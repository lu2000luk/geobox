//! HTTP (or local folder) access to the published data, with retries and a shared back-off
//! for rate limiting. Used by syncing and by the prebuilt-index bootstrap.

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use parking_lot::Mutex;
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::info;

const MAX_ATTEMPTS: u32 = 10;
const MAX_BACKOFF: Duration = Duration::from_secs(300);

pub(crate) struct Fetcher {
    pub base_url: String,
    http: reqwest::Client,
    /// Shared back-off: no download starts before this instant (set on HTTP 429).
    download_pause: Mutex<Option<Instant>>,
}

impl Fetcher {
    pub fn new(base_url: &str) -> Result<Self> {
        Ok(Fetcher {
            base_url: base_url.to_string(),
            http: reqwest::Client::builder()
                .user_agent("geobox-server")
                .connect_timeout(Duration::from_secs(30))
                .read_timeout(Duration::from_secs(120))
                .build()?,
            download_pause: Mutex::new(None),
        })
    }

    pub fn is_http(&self) -> bool {
        self.base_url.starts_with("http://") || self.base_url.starts_with("https://")
    }

    /// Local path of a published file when the base is a folder.
    pub fn local(&self, path: &str) -> std::path::PathBuf {
        Path::new(&self.base_url).join(path)
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), path)
    }

    /// GET with retries. Hosts rate-limit anonymous downloads (HF answers 429 after a few
    /// thousand files): on 429, 5xx or a dropped connection, back off (pausing *all* downloads
    /// on 429) and retry. Returns a 2xx response, or `None` for 304 Not Modified.
    pub async fn get(&self, url: &str, etag: Option<&str>) -> Result<Option<reqwest::Response>> {
        let mut delay = Duration::from_secs(5);
        for attempt in 1.. {
            self.wait_for_download_pause().await;
            let mut req = self.http.get(url);
            if let Some(e) = etag {
                req = req.header(reqwest::header::IF_NONE_MATCH, e);
            }
            let wait = match req.send().await {
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_MODIFIED => return Ok(None),
                Ok(resp)
                    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS || resp.status().is_server_error() =>
                {
                    let after = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok()?.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let wait = after.unwrap_or(delay).min(MAX_BACKOFF);
                    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        self.pause_downloads(wait);
                    }
                    if attempt >= MAX_ATTEMPTS {
                        bail!("GET {url}: {} after {attempt} attempts", resp.status());
                    }
                    wait
                }
                Ok(resp) => return Ok(Some(resp.error_for_status().with_context(|| format!("GET {url}"))?)),
                Err(e) if attempt < MAX_ATTEMPTS => {
                    tracing::debug!("GET {url}: {e}");
                    delay
                }
                Err(e) => return Err(e).with_context(|| format!("GET {url} after {attempt} attempts")),
            };
            tokio::time::sleep(wait).await;
            delay = (delay * 2).min(MAX_BACKOFF);
        }
        unreachable!()
    }

    /// Fetches a small file under the base URL. `None` = not modified since `etag`.
    pub async fn fetch(&self, path: &str, etag: Option<&str>) -> Result<Option<(Vec<u8>, Option<String>)>> {
        if !self.is_http() {
            let p = Path::new(&self.base_url).join(path);
            return Ok(Some((tokio::fs::read(&p).await.with_context(|| format!("reading {}", p.display()))?, None)));
        }
        let url = self.url(path);
        for attempt in 1..=3 {
            let Some(resp) = self.get(&url, etag).await? else { return Ok(None) };
            let etag = resp.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()).map(String::from);
            match resp.bytes().await {
                Ok(b) => return Ok(Some((b.to_vec(), etag))),
                Err(e) if attempt < 3 => tracing::debug!("GET {url}: body: {e}"),
                Err(e) => return Err(e).with_context(|| format!("GET {url}")),
            }
        }
        unreachable!()
    }

    /// Streams a (large) published file to `dest`, checking its size and blake3 hash.
    pub async fn download_to(&self, path: &str, dest: &Path, size: u64, hash: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let url = self.url(path);
        let mut last_err = None;
        for _ in 0..3 {
            let resp = self.get(&url, None).await?.context("unexpected 304")?;
            let mut file = tokio::fs::File::create(dest).await?;
            let mut hasher = blake3::Hasher::new();
            let mut stream = resp.bytes_stream();
            let mut n = 0u64;
            let mut ok = true;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(c) => {
                        hasher.update(&c);
                        n += c.len() as u64;
                        file.write_all(&c).await?;
                    }
                    Err(e) => {
                        last_err = Some(anyhow::Error::from(e));
                        ok = false;
                        break;
                    }
                }
            }
            file.flush().await?;
            if ok && n == size && hasher.finalize().to_hex().as_str() == hash {
                return Ok(());
            }
            if ok {
                last_err = Some(anyhow::anyhow!("{path}: size/hash mismatch"));
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("{path}: download failed")))
    }

    /// Makes every download wait at least `d` (shared, so one 429 slows all workers down).
    fn pause_downloads(&self, d: Duration) {
        let until = Instant::now() + d;
        let mut p = self.download_pause.lock();
        if p.is_none_or(|cur| cur < until) {
            *p = Some(until);
            info!("download rate-limited by the data host, pausing {d:?}");
        }
    }

    async fn wait_for_download_pause(&self) {
        loop {
            let until = *self.download_pause.lock();
            match until {
                Some(t) if t > Instant::now() => tokio::time::sleep_until(t.into()).await,
                _ => return,
            }
        }
    }
}
