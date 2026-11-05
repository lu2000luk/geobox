//! geobox server library: keeps a local Tantivy index in sync with published tiles and
//! answers street searches. Embed it directly (`Index::open`, `sync`, `search`) or run the
//! HTTP binary in this crate.

pub mod config;
mod search;
mod sync;
mod text;

pub use config::{Config, Weights};
pub use search::{Hit, Query};
pub use sync::SyncReport;

use anyhow::Result;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use tantivy::directory::MmapDirectory;
use tantivy::schema::{
    Field, INDEXED, IndexRecordOption, NumericOptions, STORED, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::{IndexReader, ReloadPolicy};

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("server overloaded, retry shortly")]
    Overloaded,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Bump when the index schema or text processing changes.
const INDEX_VERSION: &str = "2";

#[derive(Clone, Copy)]
pub(crate) struct Fields {
    pub name: Field,
    pub alt: Field,
    pub refs: Field,
    pub place: Field,
    /// Leading letters of every word in the street's names ("rom" for "Via Roma").
    pub prefix: Field,
    /// Whole normalized names as single terms, for an exact-name bonus.
    pub exact: Field,
    /// Every word of name, alt names, refs and place in one field: matching is plain term
    /// lookups (fast to skip through) instead of an OR over four fields.
    pub all: Field,
    /// Every word of the street's own names (name, alt names, refs).
    pub names: Field,
    pub cell: Field,
    pub tile: Field,
    pub lat: Field,
    pub lon: Field,
    pub class: Field,
    pub d_name: Field,
    pub d_ref: Field,
    pub d_place: Field,
    pub d_cc: Field,
    pub d_alt: Field,
}

fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let text = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default().set_tokenizer(text::TOKENIZER).set_index_option(IndexRecordOption::WithFreqs),
    );
    // already-normalized terms: no tokenizing beyond whitespace, no length normalization
    let raw = |tok: &str| {
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(tok)
                .set_index_option(IndexRecordOption::Basic)
                .set_fieldnorms(false),
        )
    };
    let fast = NumericOptions::default().set_fast().set_stored();
    let f = Fields {
        name: b.add_text_field("name", text.clone()),
        alt: b.add_text_field("alt", text.clone()),
        refs: b.add_text_field("ref", text.clone()),
        place: b.add_text_field("place", text),
        prefix: b.add_text_field("prefix", raw(text::WS_TOKENIZER)),
        exact: b.add_text_field("exact", raw("raw")),
        all: b.add_text_field("all", raw(text::TOKENIZER)),
        names: b.add_text_field("names", raw(text::TOKENIZER)),
        cell: b.add_u64_field("cell", INDEXED),
        tile: b.add_u64_field("tile", INDEXED),
        lat: b.add_i64_field("lat", fast.clone()),
        lon: b.add_i64_field("lon", fast.clone()),
        class: b.add_u64_field("class", fast),
        d_name: b.add_text_field("d_name", STORED),
        d_ref: b.add_text_field("d_ref", STORED),
        d_place: b.add_text_field("d_place", STORED),
        d_cc: b.add_text_field("d_cc", STORED),
        d_alt: b.add_text_field("d_alt", STORED),
    };
    (b.build(), f)
}

/// Snapshot of what has been applied locally: tile -> row hash, plus manifest metadata.
#[derive(Default, Clone)]
pub(crate) struct Applied {
    pub tiles: HashMap<u64, String>,
    pub meta: Vec<(String, String)>,
}

pub struct Index {
    pub(crate) cfg: Config,
    pub(crate) index: tantivy::Index,
    pub(crate) reader: IndexReader,
    /// Fast-field columns per segment of the current searcher (refreshed by `reload`).
    pub(crate) columns: parking_lot::RwLock<search::ColumnCache>,
    pub(crate) f: Fields,
    pub(crate) abbrev: text::Abbrev,
    pub(crate) cache: moka::sync::Cache<String, Arc<Vec<Hit>>>,
    pub(crate) generation: AtomicU64,
    pub(crate) applied: Mutex<Applied>,
    pub(crate) ready: AtomicBool,
    pub(crate) manifest_etag: Mutex<Option<String>>,
    /// Shared back-off: no download starts before this instant (set on HTTP 429).
    pub(crate) download_pause: Mutex<Option<std::time::Instant>>,
    pub(crate) sync_lock: tokio::sync::Mutex<()>,
    pub(crate) permits: Arc<tokio::sync::Semaphore>,
    pub(crate) queued: AtomicUsize,
    pub(crate) http: reqwest::Client,
}

impl Index {
    /// Opens (or creates) the local index in `cfg.data_dir`. Does not download anything.
    pub fn open(cfg: Config) -> Result<Arc<Index>> {
        let dir = cfg.data_dir.join("index");
        // An index from an older layout is rebuilt from the tiles instead of failing to open.
        let version_file = cfg.data_dir.join("index-version");
        if std::fs::read_to_string(&version_file).ok().as_deref() != Some(INDEX_VERSION) {
            if dir.exists() {
                tracing::warn!("local index has an old layout: rebuilding it from the tiles");
            }
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_file(cfg.data_dir.join("applied.tsv"));
            std::fs::create_dir_all(&cfg.data_dir)?;
            std::fs::write(&version_file, INDEX_VERSION)?;
        }
        std::fs::create_dir_all(&dir)?;
        let (schema, f) = schema();
        let index = tantivy::Index::open_or_create(MmapDirectory::open(&dir)?, schema)?;
        index.tokenizers().register(text::TOKENIZER, text::analyzer());
        index.tokenizers().register(text::WS_TOKENIZER, tantivy::tokenizer::WhitespaceTokenizer::default());
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .doc_store_cache_num_blocks(cfg.docstore_cache_blocks)
            .try_into()?;
        let abbrev = match &cfg.abbrev_file {
            Some(p) => text::Abbrev::parse(&std::fs::read_to_string(p)?),
            None => text::Abbrev::parse(include_str!("../abbrev.tsv")),
        };
        let applied = sync::load_applied(&cfg.data_dir)?;
        let cache = moka::sync::Cache::builder()
            .weigher(|k: &String, v: &Arc<Vec<Hit>>| {
                (k.len() + v.iter().map(Hit::approx_size).sum::<usize>()).min(u32::MAX as usize) as u32
            })
            .max_capacity(cfg.cache_mb << 20)
            .build();
        let ready = !applied.tiles.is_empty();
        let columns = parking_lot::RwLock::new(search::load_columns(&reader.searcher()));
        Ok(Arc::new(Index {
            columns,
            permits: Arc::new(tokio::sync::Semaphore::new(cfg.search_threads)),
            http: reqwest::Client::builder()
                .user_agent("geobox-server")
                .timeout(std::time::Duration::from_secs(300))
                .build()?,
            cfg,
            index,
            reader,
            f,
            abbrev,
            cache,
            generation: AtomicU64::new(0),
            applied: Mutex::new(applied),
            ready: AtomicBool::new(ready),
            manifest_etag: Mutex::new(None),
            download_pause: Mutex::new(None),
            sync_lock: tokio::sync::Mutex::new(()),
            queued: AtomicUsize::new(0),
        }))
    }

    /// Makes committed changes searchable (and refreshes the cached columns).
    pub(crate) fn reload(&self) -> Result<()> {
        self.reader.reload()?;
        *self.columns.write() = search::load_columns(&self.reader.searcher());
        Ok(())
    }

    /// True once a full data set is loaded (after the first successful sync).
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    pub fn tile_count(&self) -> usize {
        self.applied.lock().tiles.len()
    }

    pub fn road_count(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Manifest metadata of the applied data (e.g. `osm_timestamp`).
    pub fn meta(&self, key: &str) -> Option<String> {
        self.applied.lock().meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    pub fn data_dir(&self) -> &Path {
        &self.cfg.data_dir
    }
}
