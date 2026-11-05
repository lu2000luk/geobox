//! Prebuilt index: the generator builds the search index once and publishes it, and a new server
//! downloads it instead of indexing ~27 M streets itself (`GEOBOX_BUILD_INDEX=1` opts out).

use crate::fetch::Fetcher;
use crate::sync::load_applied;
use crate::{Config, INDEX_VERSION, Index};
use anyhow::{Context, Result};
use geobox_core::Manifest;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use tracing::info;

/// Published folder of prebuilt index archives.
const SNAPSHOT_DIR: &str = "index";

/// Builds the search index for the tiles in `published` (a generator output folder) inside
/// `work`, and packs it as `published/index/<version>-<hash>.tar.zst`.
/// Returns (published path, blake3, size). Older archives in that folder are removed.
pub async fn build_snapshot(published: &Path, work: &Path, writer_mb: usize) -> Result<(String, String, u64)> {
    let _ = std::fs::remove_dir_all(work);
    std::fs::create_dir_all(work)?;
    let cfg = Config {
        base_url: published.to_string_lossy().into_owned(),
        data_dir: work.to_path_buf(),
        writer_mb,
        commit_every_tiles: usize::MAX,
        build_index: true,
        ..Config::default()
    };
    let index = Index::open(cfg)?;
    let report = index.sync().await?;
    anyhow::ensure!(report.failed == 0, "{} tiles could not be indexed", report.failed);
    index.compact()?;
    info!("snapshot: indexed {} streets", index.road_count());
    drop(index);

    let (work, out_dir) = (work.to_path_buf(), published.join(SNAPSHOT_DIR));
    tokio::task::spawn_blocking(move || -> Result<(String, String, u64)> {
        std::fs::create_dir_all(&out_dir)?;
        let tmp = out_dir.join("snapshot.tmp");
        {
            let mut z = zstd::Encoder::new(BufWriter::new(std::fs::File::create(&tmp)?), 9)?;
            {
                let mut tar = tar::Builder::new(&mut z);
                for f in std::fs::read_dir(work.join("index"))? {
                    let p = f?.path();
                    let name = p.file_name().unwrap().to_string_lossy().into_owned();
                    if p.is_file() && !name.ends_with(".lock") {
                        tar.append_path_with_name(&p, format!("index/{name}"))?;
                    }
                }
                tar.append_path_with_name(work.join("applied.tsv"), "applied.tsv")?;
                tar.finish()?;
            }
            z.finish()?.flush()?;
        }
        let mut h = blake3::Hasher::new();
        h.update_reader(std::fs::File::open(&tmp)?)?;
        let hash = h.finalize().to_hex().to_string();
        let size = std::fs::metadata(&tmp)?.len();
        let name = format!("{INDEX_VERSION}-{}.tar.zst", &hash[..16]);
        for f in std::fs::read_dir(&out_dir)? {
            let p = f?.path();
            if p != tmp {
                std::fs::remove_file(p)?;
            }
        }
        std::fs::rename(&tmp, out_dir.join(&name))?;
        Ok((format!("{SNAPSHOT_DIR}/{name}"), hash, size))
    })
    .await?
}

/// Records a built snapshot in the manifest header (`index_*` keys).
pub fn add_to_manifest(m: &mut Manifest, path: &str, hash: &str, size: u64) {
    m.meta.retain(|(k, _)| !k.starts_with("index_"));
    m.meta.extend([
        ("index_path".to_string(), path.to_string()),
        ("index_blake3".to_string(), hash.to_string()),
        ("index_size".to_string(), size.to_string()),
        ("index_version".to_string(), INDEX_VERSION.to_string()),
    ]);
}

/// On a server without local data, downloads and installs the published prebuilt index into
/// `cfg.data_dir`. Returns `false` (and changes nothing) when local data exists, when
/// `cfg.build_index` is set, or when no compatible prebuilt index is published; the normal sync
/// then builds the index from tiles.
pub async fn bootstrap(cfg: &Config) -> Result<bool> {
    let dir = &cfg.data_dir;
    let have_data = std::fs::read_to_string(dir.join("index-version")).ok().as_deref() == Some(INDEX_VERSION)
        && !load_applied(dir)?.meta.is_empty();
    if cfg.build_index || have_data {
        return Ok(false);
    }
    let f = Fetcher::new(&cfg.base_url)?;
    let (bytes, _) = f.fetch("manifest.tsv", None).await?.context("manifest: unexpected 304")?;
    let m = Manifest::parse(std::str::from_utf8(&bytes)?).map_err(anyhow::Error::msg)?;
    let (Some(path), Some(hash), Some(size)) =
        (m.meta("index_path"), m.meta("index_blake3"), m.meta("index_size").and_then(|s| s.parse::<u64>().ok()))
    else {
        info!("no prebuilt index published: building the index from tiles");
        return Ok(false);
    };
    if m.meta("index_version") != Some(INDEX_VERSION) {
        info!("published index has another format version: building the index from tiles");
        return Ok(false);
    }
    std::fs::create_dir_all(dir.join("tmp"))?;
    let (archive, temp): (PathBuf, bool) = if f.is_http() {
        let tmp = dir.join("tmp").join("prebuilt-index.tar.zst");
        info!("downloading prebuilt index: {path} ({} MB)", size >> 20);
        f.download_to(path, &tmp, size, hash).await?;
        (tmp, true)
    } else {
        (f.local(path), false)
    };
    let dir = dir.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        info!("unpacking prebuilt index");
        let stage = dir.join("tmp").join("unpack");
        let _ = std::fs::remove_dir_all(&stage);
        std::fs::create_dir_all(&stage)?;
        let mut a = tar::Archive::new(zstd::Decoder::new(std::fs::File::open(&archive)?)?);
        for entry in a.entries()? {
            entry?.unpack_in(&stage)?; // refuses paths escaping `stage`
        }
        anyhow::ensure!(
            stage.join("index").join("meta.json").exists() && stage.join("applied.tsv").exists(),
            "prebuilt index archive is incomplete"
        );
        // install: nothing in data_dir is touched until the archive unpacked completely
        let _ = std::fs::remove_dir_all(dir.join("index"));
        let _ = std::fs::remove_file(dir.join("applied.tsv"));
        std::fs::rename(stage.join("index"), dir.join("index"))?;
        std::fs::rename(stage.join("applied.tsv"), dir.join("applied.tsv"))?;
        std::fs::write(dir.join("index-version"), INDEX_VERSION)?;
        let _ = std::fs::remove_dir_all(&stage);
        if temp {
            let _ = std::fs::remove_file(&archive);
        }
        Ok(())
    })
    .await??;
    info!("prebuilt index installed");
    Ok(true)
}
