//! geobox server library: keeps a local Tantivy index in sync with published tiles and
//! answers street searches. Embed it directly (`Index::open`, `sync`, `search`) or run the
//! HTTP binary in this crate.

pub mod config;
mod fetch;
#[cfg(target_os = "linux")]
mod mem;
mod search;
mod snapshot;
mod sync;
mod text;

pub use config::{Config, Weights};
#[cfg(target_os = "linux")]
pub use mem::resident_file_bytes;
pub use search::{Hit, Query};
pub use snapshot::{add_to_manifest, bootstrap, build_snapshot};
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
use tantivy::{IndexReader, ReloadPolicy, Searcher};

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("server overloaded, retry shortly")]
    Overloaded,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Bump when the index schema or text processing changes.
pub const INDEX_VERSION: &str = "2";

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
    /// Closed when idle (unmapping the index), reopened by the next search.
    pub(crate) reader: Mutex<Option<IndexReader>>,
    /// Fast-field columns per segment of the current searcher (loaded by the first search,
    /// refreshed by `reload`, dropped when idle).
    pub(crate) columns: parking_lot::RwLock<search::ColumnCache>,
    pub(crate) f: Fields,
    pub(crate) abbrev: text::Abbrev,
    pub(crate) cache: moka::sync::Cache<String, Arc<Vec<Hit>>>,
    pub(crate) generation: AtomicU64,
    pub(crate) applied: Mutex<Applied>,
    pub(crate) ready: AtomicBool,
    pub(crate) manifest_etag: Mutex<Option<String>>,
    pub(crate) sync_lock: tokio::sync::Mutex<()>,
    pub(crate) permits: Arc<tokio::sync::Semaphore>,
    pub(crate) queued: AtomicUsize,
    pub(crate) fetcher: fetch::Fetcher,
    /// Where `trim_memory` stopped paging out (an address in the index mappings).
    #[cfg(target_os = "linux")]
    pub(crate) trim_cursor: AtomicUsize,
}

fn open_reader(index: &tantivy::Index, cfg: &Config) -> Result<IndexReader> {
    Ok(index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .doc_store_cache_num_blocks(cfg.docstore_cache_blocks)
        .try_into()?)
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
        // Searches touch a few small spots of a multi-GB index; default readahead pulls ~128 KB around
        // each one, filling the page cache (and the container's memory graph) with bytes never read.
        #[cfg(unix)]
        let mmap = MmapDirectory::open_with_madvice(&dir, tantivy::directory::Advice::Random)?;
        #[cfg(not(unix))]
        let mmap = MmapDirectory::open(&dir)?;
        let index = tantivy::Index::open_or_create(mmap, schema)?;
        index.tokenizers().register(text::TOKENIZER, text::analyzer());
        index.tokenizers().register(text::WS_TOKENIZER, tantivy::tokenizer::WhitespaceTokenizer::default());
        let reader = open_reader(&index, &cfg)?;
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
        // manifest metadata is saved only when a sync round finishes, so a server restarted in the
        // middle of its first sync stays "not ready" until that sync completes
        let ready = !applied.meta.is_empty();
        Ok(Arc::new(Index {
            // loaded by the first search
            columns: Default::default(),
            permits: Arc::new(tokio::sync::Semaphore::new(cfg.search_threads)),
            fetcher: fetch::Fetcher::new(&cfg.base_url)?,
            cfg,
            index,
            reader: Mutex::new(Some(reader)),
            f,
            abbrev,
            cache,
            generation: AtomicU64::new(0),
            applied: Mutex::new(applied),
            ready: AtomicBool::new(ready),
            manifest_etag: Mutex::new(None),
            sync_lock: tokio::sync::Mutex::new(()),
            queued: AtomicUsize::new(0),
            #[cfg(target_os = "linux")]
            trim_cursor: AtomicUsize::new(0),
        }))
    }

    /// Makes committed changes searchable (and refreshes the cached columns).
    pub(crate) fn reload(&self) -> Result<()> {
        let reader = self.reader.lock().clone();
        // a closed reader reopens on the latest commit anyway
        *self.columns.write() = match reader {
            Some(r) => {
                r.reload()?;
                search::load_columns(&r.searcher())
            }
            None => Default::default(),
        };
        Ok(())
    }

    /// The current searcher, reopening the index if it was closed while idle.
    pub(crate) fn searcher(&self) -> Result<Searcher> {
        let mut reader = self.reader.lock();
        if reader.is_none() {
            *reader = Some(open_reader(&self.index, &self.cfg)?);
        }
        Ok(reader.as_ref().expect("just opened").searcher())
    }

    /// True once a full data set is loaded (after the first successful sync).
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    pub fn tile_count(&self) -> usize {
        self.applied.lock().tiles.len()
    }

    pub fn road_count(&self) -> u64 {
        self.searcher().map_or(0, |s| s.num_docs())
    }

    /// Manifest metadata of the applied data (e.g. `osm_timestamp`).
    pub fn meta(&self, key: &str) -> Option<String> {
        self.applied.lock().meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    pub fn data_dir(&self) -> &Path {
        &self.cfg.data_dir
    }

    /// Drops what an idle server can: cached results and columns, the open index (its mappings and
    /// their page tables), and every file page still mapped (code). All of it comes back on demand.
    #[cfg(target_os = "linux")]
    pub fn release_memory(&self) -> std::io::Result<()> {
        // paged out while still mapped: once unmapped, the pages would linger as plain file cache
        let maps = mem::file_mappings(None)?;
        for (a, b, _) in &maps {
            mem::page_out(*a, *b);
        }
        // a sync holds its own searchers; they unmap when it finishes
        *self.reader.lock() = None;
        *self.columns.write() = Default::default();
        self.cache.invalidate_all();
        self.cache.run_pending_tasks();
        let dirs = [self.cfg.data_dir.clone(), self.cfg.data_dir.join("index")];
        let data = dirs.iter().filter_map(|d| std::fs::read_dir(d).ok()).flatten().flatten().map(|e| e.path());
        let mut files: Vec<_> = maps.into_iter().map(|m| m.2).chain(data).collect();
        files.sort();
        files.dedup();
        for f in files {
            mem::drop_cached(&f);
        }
        Ok(())
    }

    /// Pages out index pages until at most `target` bytes of file pages stay resident. It sweeps on
    /// from where the last call stopped, so pages that many searches share survive a while.
    #[cfg(target_os = "linux")]
    pub fn trim_memory(&self, target: u64) -> std::io::Result<()> {
        const CHUNK: usize = 64 << 20;
        let dir = std::fs::canonicalize(self.cfg.data_dir.join("index"))?;
        let chunks: Vec<(usize, usize)> = mem::file_mappings(Some(&dir))?
            .into_iter()
            .flat_map(|(a, b, _)| (a..b).step_by(CHUNK).map(move |c| (c, (c + CHUNK).min(b))))
            .collect();
        let cursor = self.trim_cursor.load(Ordering::Relaxed);
        let start = chunks.iter().position(|&(a, _)| a >= cursor).unwrap_or(0);
        for &(a, b) in chunks[start..].iter().chain(&chunks[..start]) {
            mem::page_out(a, b);
            self.trim_cursor.store(b, Ordering::Relaxed);
            if mem::resident_file_bytes() <= target {
                break;
            }
        }
        Ok(())
    }
}
