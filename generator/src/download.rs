//! Fetching the input planet/extract: resumable download + md5 verification.

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use md5::{Digest, Md5};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::info;

pub fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Ensures the input PBF is present and verified. Returns its path and md5 (hex).
pub async fn fetch(url: &str, work: &Path) -> Result<(PathBuf, String)> {
    if !is_url(url) {
        let path = PathBuf::from(url);
        let md5 = md5_file(&path).await.with_context(|| format!("reading {url}"))?;
        return Ok((path, md5));
    }
    let client = reqwest::Client::builder().user_agent("geobox-generator").build()?;
    let remote_md5 = client.get(format!("{url}.md5")).send().await?.error_for_status()?.text().await?;
    let remote_md5 = remote_md5.split_whitespace().next().unwrap_or_default().to_lowercase();
    anyhow::ensure!(remote_md5.len() == 32, "bad md5 file at {url}.md5");

    let dest = work.join("input.osm.pbf");
    let verified = work.join("input.md5");
    if dest.exists() && std::fs::read_to_string(&verified).ok().as_deref() == Some(remote_md5.as_str()) {
        info!("input already downloaded and verified ({remote_md5})");
        return Ok((dest, remote_md5));
    }

    let part = work.join("input.osm.pbf.part");
    // `download_resumable` continues a `.part` file left behind by a crash or retry.
    // content_length() is the (empty) body size for HEAD, so read the header itself
    let head = client.head(url).send().await?.error_for_status()?;
    let total: Option<u64> =
        head.headers().get(reqwest::header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse().ok());
    preflight_disk(work, total.unwrap_or(0))?;
    for attempt in 1..=5u32 {
        match download_resumable(&client, url, &part, total).await {
            Ok(()) => break,
            Err(e) if attempt < 5 => {
                tracing::warn!("download attempt {attempt} failed: {e:#}, retrying");
                tokio::time::sleep(std::time::Duration::from_secs(10 * attempt as u64)).await;
            }
            Err(e) => return Err(e),
        }
    }
    let got = md5_file(&part).await?;
    if got != remote_md5 {
        let _ = std::fs::remove_file(&part);
        bail!("md5 mismatch for {url}: expected {remote_md5}, got {got} (partial file removed)");
    }
    std::fs::rename(&part, &dest)?;
    std::fs::write(&verified, &remote_md5)?;
    Ok((dest, remote_md5))
}

async fn download_resumable(client: &reqwest::Client, url: &str, part: &Path, total: Option<u64>) -> Result<()> {
    let have = tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
    if total == Some(have) {
        return Ok(());
    }
    let mut req = client.get(url);
    if have > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let resp = req.send().await?.error_for_status()?;
    let resumed = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let mut file =
        tokio::fs::OpenOptions::new().create(true).write(true).append(resumed).truncate(!resumed).open(part).await?;
    let mut done = if resumed { have } else { 0 };
    info!("downloading {url} from byte {done} of {}", total.map_or("?".into(), |t| t.to_string()));
    let mut stream = resp.bytes_stream();
    let mut last_log = std::time::Instant::now();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        if last_log.elapsed().as_secs() >= 30 {
            info!("downloaded {} MB", done >> 20);
            last_log = std::time::Instant::now();
        }
    }
    file.flush().await?;
    if let Some(t) = total {
        anyhow::ensure!(done == t, "download ended early at {done} of {t} bytes");
    }
    Ok(())
}

async fn md5_file(path: &Path) -> Result<String> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut h = Md5::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Fails fast when the disk obviously can't hold the input plus intermediate files.
fn preflight_disk(work: &Path, input_size: u64) -> Result<()> {
    let need = input_size + input_size / 2 + (2u64 << 30);
    let work = std::fs::canonicalize(work).context("resolving work_dir")?;
    let disks = sysinfo::Disks::new_with_refreshed_list()?;
    let free = disks
        .iter()
        .filter(|d| work.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space());
    if let Some(free) = free {
        anyhow::ensure!(
            free >= need,
            "not enough disk space in {}: {} GB free, ~{} GB needed for a {} GB input",
            work.display(),
            free >> 30,
            need >> 30,
            input_size >> 30
        );
    }
    let ram = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_memory(sysinfo::MemoryRefreshKind::everything()),
    )?
    .total_memory();
    if input_size > (20u64 << 30) && ram < (16u64 << 30) {
        tracing::warn!(
            "only {} GB RAM for a {} GB input: the build will page to disk and be slow",
            ram >> 30,
            input_size >> 30
        );
    }
    Ok(())
}
