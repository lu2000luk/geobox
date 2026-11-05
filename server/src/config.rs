use anyhow::{Context, Result};
use std::path::PathBuf;
use std::str::FromStr;

/// Server settings, read from `GEOBOX_*` env vars (see README for the full list).
#[derive(Debug, Clone)]
pub struct Config {
    /// Public base URL of the published data (`https://.../`) or a local directory.
    pub base_url: String,
    pub data_dir: PathBuf,
    pub bind: String,
    pub poll_secs: u64,
    /// Searches running at once. Requests beyond `search_threads * queue_factor` get "overloaded".
    pub search_threads: usize,
    pub queue_factor: usize,
    pub cache_mb: u64,
    pub writer_mb: usize,
    pub docstore_cache_blocks: usize,
    pub download_concurrency: usize,
    pub commit_every_tiles: usize,
    pub abbrev_file: Option<PathBuf>,
    /// Queries slower than this are logged with a per-stage breakdown.
    pub slow_query_ms: u64,
    pub weights: Weights,
}

/// Ranking: `score = text * w_text - w_dist * ln(1 + km) - w_class * class/9`. Results matching every
/// query word always come before results found with a word dropped.
#[derive(Debug, Clone, Copy)]
pub struct Weights {
    pub text: f32,
    pub dist: f32,
    pub class: f32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            base_url: String::new(),
            data_dir: "server-data".into(),
            bind: "0.0.0.0:8080".into(),
            poll_secs: 3600,
            search_threads: std::thread::available_parallelism().map_or(2, |n| n.get()),
            queue_factor: 4,
            cache_mb: 32,
            writer_mb: 64,
            docstore_cache_blocks: 16,
            download_concurrency: 8,
            commit_every_tiles: 200,
            abbrev_file: None,
            slow_query_ms: 200,
            weights: Weights { text: 1.0, dist: 1.5, class: 1.0 },
        }
    }
}

fn env<T: FromStr>(key: &str, default: T) -> Result<T> {
    match std::env::var(format!("GEOBOX_{key}")) {
        Ok(v) => v.parse().ok().with_context(|| format!("invalid GEOBOX_{key}={v}")),
        Err(_) => Ok(default),
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let d = Config::default();
        let c = Config {
            base_url: env("BASE_URL", d.base_url)?,
            data_dir: env("DATA_DIR", d.data_dir)?,
            bind: env("BIND", d.bind)?,
            poll_secs: env("POLL_SECS", d.poll_secs)?,
            search_threads: env("SEARCH_THREADS", d.search_threads)?,
            queue_factor: env("QUEUE_FACTOR", d.queue_factor)?,
            cache_mb: env("CACHE_MB", d.cache_mb)?,
            writer_mb: env("WRITER_MB", d.writer_mb)?,
            docstore_cache_blocks: env("DOCSTORE_CACHE_BLOCKS", d.docstore_cache_blocks)?,
            download_concurrency: env("DOWNLOAD_CONCURRENCY", d.download_concurrency)?,
            commit_every_tiles: env("COMMIT_EVERY_TILES", d.commit_every_tiles)?,
            abbrev_file: std::env::var("GEOBOX_ABBREV_FILE").ok().map(PathBuf::from),
            slow_query_ms: env("SLOW_QUERY_MS", d.slow_query_ms)?,
            weights: Weights {
                text: env("W_TEXT", d.weights.text)?,
                dist: env("W_DIST", d.weights.dist)?,
                class: env("W_CLASS", d.weights.class)?,
            },
        };
        anyhow::ensure!(!c.base_url.is_empty(), "GEOBOX_BASE_URL is required (public data URL or local directory)");
        anyhow::ensure!(
            c.search_threads > 0 && c.writer_mb >= 16,
            "GEOBOX_SEARCH_THREADS must be > 0 and GEOBOX_WRITER_MB >= 16"
        );
        Ok(c)
    }
}
