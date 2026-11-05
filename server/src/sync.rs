//! Pulls the manifest, downloads changed tiles (verified), and applies them to the index
//! tile by tile. Progress is committed every `commit_every_tiles`, so an interrupted
//! bootstrap resumes where it stopped.

use crate::{Applied, Index, text};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use geobox_core::{BundleEntry, CELL_LEVELS, Manifest, ManifestEntry, bundle_of, cell_term, cell_xy};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tantivy::{IndexWriter, TantivyDocument, Term};
use tracing::{info, warn};

#[derive(Debug, Default, Clone)]
pub struct SyncReport {
    pub manifest_changed: bool,
    pub applied: usize,
    pub removed: usize,
    pub failed: usize,
}

const APPLIED_FILE: &str = "applied.tsv";
const MAX_SEGMENTS: usize = 8;
const MAX_ATTEMPTS: u32 = 10;
/// Take the whole bundle when at least this many of its tiles changed (or a third of its bytes).
const BUNDLE_MIN_TILES: usize = 50;
const MAX_BACKOFF: Duration = Duration::from_secs(300);

fn check_tile(e: &ManifestEntry, bytes: &[u8]) -> Result<()> {
    if bytes.len() as u64 != e.size || blake3::hash(bytes).to_hex().as_str() != e.file_hash {
        bail!("{}: size/hash mismatch (CDN still propagating?)", e.path);
    }
    Ok(())
}

pub(crate) fn load_applied(dir: &Path) -> Result<Applied> {
    let mut a = Applied::default();
    let Ok(text) = std::fs::read_to_string(dir.join(APPLIED_FILE)) else { return Ok(a) };
    for line in text.lines() {
        if let Some(meta) = line.strip_prefix("#meta") {
            a.meta = meta.split('\t').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.into(), v.into())).collect();
        } else if let Some((t, h)) = line.split_once('\t') {
            a.tiles.insert(t.parse().context("corrupt applied.tsv")?, h.to_string());
        }
    }
    Ok(a)
}

fn save_applied(dir: &Path, a: &Applied) -> Result<()> {
    let mut s = String::from("#meta");
    for (k, v) in &a.meta {
        s.push_str(&format!("\t{k}={v}"));
    }
    s.push('\n');
    let mut tiles: Vec<_> = a.tiles.iter().collect();
    tiles.sort();
    for (t, h) in tiles {
        s.push_str(&format!("{t}\t{h}\n"));
    }
    let tmp = dir.join("applied.tsv.tmp");
    std::fs::write(&tmp, s)?;
    std::fs::rename(tmp, dir.join(APPLIED_FILE))?;
    Ok(())
}

impl Index {
    fn is_http(&self) -> bool {
        self.cfg.base_url.starts_with("http://") || self.cfg.base_url.starts_with("https://")
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.cfg.base_url.trim_end_matches('/'), path)
    }

    /// GET with retries. Hosts rate-limit anonymous downloads (HF answers 429 after a few
    /// thousand files): on 429, 5xx or a dropped connection, back off (pausing *all* downloads
    /// on 429) and retry. Returns a 2xx response, or `None` for 304 Not Modified.
    async fn get(&self, url: &str, etag: Option<&str>) -> Result<Option<reqwest::Response>> {
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
    async fn fetch(&self, path: &str, etag: Option<&str>) -> Result<Option<(Vec<u8>, Option<String>)>> {
        if !self.is_http() {
            let p = Path::new(&self.cfg.base_url).join(path);
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
    async fn download_to(&self, path: &str, dest: &Path, size: u64, hash: &str) -> Result<()> {
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

    async fn fetch_tile(&self, e: &ManifestEntry) -> Result<Vec<u8>> {
        let (bytes, _) = self.fetch(&e.path, None).await?.context("unexpected 304")?;
        check_tile(e, &bytes)?;
        Ok(bytes)
    }

    /// Downloads one bundle and feeds the wanted tiles in it to the ingest channel.
    /// Returns the tiles it could not deliver (they fall back to single downloads).
    async fn apply_bundle(
        self: &Arc<Self>,
        b: &BundleEntry,
        tiles: Vec<ManifestEntry>,
        tx: tokio::sync::mpsc::Sender<(ManifestEntry, Result<Vec<u8>>)>,
    ) -> Vec<ManifestEntry> {
        let (file, temp) = if self.is_http() {
            let dest = self.cfg.data_dir.join("tmp").join(format!("bundle-{}.tar", b.bundle));
            let _ = tokio::fs::create_dir_all(dest.parent().unwrap()).await;
            if let Err(e) = self.download_to(&b.path, &dest, b.size, &b.file_hash).await {
                warn!("bundle {} failed ({e:#}), downloading its {} tiles one by one", b.path, tiles.len());
                let _ = tokio::fs::remove_file(&dest).await;
                return tiles;
            }
            (dest, true)
        } else {
            (Path::new(&self.cfg.base_url).join(&b.path), false)
        };
        let path = b.path.clone();
        let read = tokio::task::spawn_blocking(move || {
            let mut want: HashMap<String, ManifestEntry> = tiles.into_iter().map(|e| (e.path.clone(), e)).collect();
            let res = (|| -> Result<()> {
                let mut archive = tar::Archive::new(std::io::BufReader::new(std::fs::File::open(&file)?));
                for entry in archive.entries()? {
                    let mut entry = entry?;
                    let name = entry.path()?.to_string_lossy().into_owned();
                    if let Some(e) = want.remove(&name) {
                        let mut buf = Vec::with_capacity(e.size as usize);
                        std::io::Read::read_to_end(&mut entry, &mut buf)?;
                        let res = check_tile(&e, &buf).map(|_| buf);
                        if tx.blocking_send((e, res)).is_err() {
                            break; // ingest stopped
                        }
                    }
                }
                Ok(())
            })();
            if let Err(e) = res {
                warn!("bundle {path} unreadable ({e:#}), downloading the rest one by one");
            }
            if temp {
                let _ = std::fs::remove_file(&file);
            }
            want.into_values().collect::<Vec<_>>()
        });
        read.await.unwrap_or_default()
    }

    /// One sync round. Safe to call concurrently (rounds are serialized).
    pub async fn sync(self: &Arc<Self>) -> Result<SyncReport> {
        let _guard = self.sync_lock.lock().await;
        let etag = self.manifest_etag.lock().clone();
        let Some((bytes, new_etag)) = self.fetch("manifest.tsv", etag.as_deref()).await? else {
            return Ok(SyncReport::default());
        };
        let manifest = Manifest::parse(std::str::from_utf8(&bytes)?).map_err(anyhow::Error::msg)?;
        let applied = self.applied.lock().clone();
        let wanted: HashSet<u64> = manifest.entries.iter().map(|e| e.tile).collect();
        let todo: Vec<ManifestEntry> =
            manifest.entries.iter().filter(|e| applied.tiles.get(&e.tile) != Some(&e.row_hash)).cloned().collect();
        let removed: Vec<u64> = applied.tiles.keys().copied().filter(|t| !wanted.contains(t)).collect();
        // Nothing changed: don't spin up a writer (local-folder sources have no ETag to skip on).
        if todo.is_empty() && removed.is_empty() && self.index.searchable_segment_ids()?.len() <= MAX_SEGMENTS {
            *self.manifest_etag.lock() = new_etag;
            self.ready.store(!applied.tiles.is_empty(), Ordering::Relaxed);
            return Ok(SyncReport { manifest_changed: true, ..Default::default() });
        }
        info!(
            "sync: {} tiles in manifest, {} to apply, {} to remove",
            manifest.entries.len(),
            todo.len(),
            removed.len()
        );

        let (tx, rx) = tokio::sync::mpsc::channel(self.cfg.download_concurrency * 2);
        let me = self.clone();
        let meta = manifest.meta.clone();
        let ingest = tokio::task::spawn_blocking(move || me.ingest(rx, removed, meta));
        // A bundle when much of it changed (always on a first sync), single tiles otherwise.
        let bundle_by_id: HashMap<u64, &BundleEntry> = manifest.bundles.iter().map(|b| (b.bundle, b)).collect();
        let mut groups: BTreeMap<u64, Vec<ManifestEntry>> = BTreeMap::new();
        for e in todo {
            groups.entry(bundle_of(e.tile)).or_default().push(e);
        }
        let mut per_bundle: HashMap<u64, usize> = HashMap::new();
        for e in &manifest.entries {
            *per_bundle.entry(bundle_of(e.tile)).or_default() += 1;
        }
        let first_sync = applied.tiles.is_empty();
        let (mut singles, mut via_bundle) = (Vec::new(), Vec::new());
        for (b, tiles) in groups {
            let changed: u64 = tiles.iter().map(|e| e.size).sum();
            let worth = |be: &BundleEntry| {
                first_sync
                    || tiles.len() >= BUNDLE_MIN_TILES
                    || tiles.len() * 2 >= per_bundle[&b]
                    || changed * 3 >= be.size
            };
            match bundle_by_id.get(&b) {
                Some(be) if worth(be) => via_bundle.push(((*be).clone(), tiles)),
                _ => singles.extend(tiles),
            }
        }
        if !via_bundle.is_empty() {
            info!("sync: {} bundles, {} single tiles", via_bundle.len(), singles.len());
        }
        {
            let (me, btx) = (self.clone(), tx.clone());
            let mut bundle_jobs = futures_util::stream::iter(via_bundle)
                .map(|(be, tiles)| {
                    let (me, tx) = (me.clone(), btx.clone());
                    async move { me.apply_bundle(&be, tiles, tx).await }
                })
                .buffer_unordered(2);
            while let Some(missed) = bundle_jobs.next().await {
                singles.extend(missed);
            }
        }
        let me = self.clone();
        let mut downloads = futures_util::stream::iter(singles)
            .map(|e| {
                let me = me.clone();
                async move {
                    let r = me.fetch_tile(&e).await;
                    (e, r)
                }
            })
            .buffer_unordered(self.cfg.download_concurrency);
        while let Some(item) = downloads.next().await {
            if tx.send(item).await.is_err() {
                break; // ingest failed; its error is reported below
            }
        }
        drop(tx);
        let mut report = ingest.await??;
        report.manifest_changed = true;
        if report.failed == 0 {
            // only skip the next download when everything got applied
            *self.manifest_etag.lock() = new_etag;
        }
        Ok(report)
    }

    fn ingest(
        &self,
        mut rx: tokio::sync::mpsc::Receiver<(ManifestEntry, Result<Vec<u8>>)>,
        removed: Vec<u64>,
        meta: Vec<(String, String)>,
    ) -> Result<SyncReport> {
        let mut report = SyncReport { removed: removed.len(), ..Default::default() };
        let mut applied = self.applied.lock().clone();
        // The writer (and its RAM budget) only exists during a sync.
        let mut writer: IndexWriter = self.index.writer_with_num_threads(1, self.cfg.writer_mb << 20)?;
        for t in &removed {
            writer.delete_term(Term::from_field_u64(self.f.tile, *t));
            applied.tiles.remove(t);
        }
        let mut pending = removed.len();
        let tmp = self.cfg.data_dir.join("tmp");
        std::fs::create_dir_all(&tmp)?;
        while let Some((e, bytes)) = rx.blocking_recv() {
            match bytes.and_then(|b| self.read_tile(&e, &b, &tmp)) {
                Ok(docs) => {
                    // the tile is fully decoded before the writer is touched, so a bad file never
                    // leaves a half-replaced tile behind
                    writer.delete_term(Term::from_field_u64(self.f.tile, e.tile));
                    for d in docs {
                        writer.add_document(d)?;
                    }
                    applied.tiles.insert(e.tile, e.row_hash);
                    report.applied += 1;
                    pending += 1;
                }
                Err(err) => {
                    warn!("tile {} not applied (kept previous version): {err:#}", e.path);
                    report.failed += 1;
                }
            }
            if pending >= self.cfg.commit_every_tiles {
                self.commit(&mut writer, &applied)?;
                info!("sync: {} tiles applied so far", report.applied);
                pending = 0;
            }
        }
        applied.meta = meta;
        self.commit(&mut writer, &applied)?;
        // Every query pays per segment (term lookups, typo expansion), so after a sync leaves
        // many segments they are merged into one. Rare: weekly data adds a segment or two.
        let segments = self.index.searchable_segment_ids()?;
        if segments.len() > MAX_SEGMENTS {
            info!("sync: merging {} segments", segments.len());
            writer.merge(&segments).wait()?;
        }
        writer.wait_merging_threads()?;
        self.reload()?;
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.cache.invalidate_all();
        if !applied.tiles.is_empty() {
            self.ready.store(true, Ordering::Relaxed);
        }
        metrics::gauge!("geobox_tiles").set(applied.tiles.len() as f64);
        info!("sync: done ({} applied, {} removed, {} failed)", report.applied, report.removed, report.failed);
        Ok(report)
    }

    fn commit(&self, writer: &mut IndexWriter, applied: &Applied) -> Result<()> {
        writer.commit()?;
        save_applied(&self.cfg.data_dir, applied)?;
        *self.applied.lock() = applied.clone();
        self.reload()?;
        Ok(())
    }

    fn read_tile(&self, e: &ManifestEntry, zst: &[u8], tmp: &Path) -> Result<Vec<TantivyDocument>> {
        let db_path = tmp.join(format!("{}.db", e.tile));
        std::fs::write(&db_path, zstd::decode_all(zst)?)?;
        let docs = (|| -> Result<Vec<TantivyDocument>> {
            let db = rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let mut st = db.prepare(
                "SELECT r.name, r.alt, r.ref, r.class, r.lat, r.lon, p.display, p.search, p.country_code
                 FROM roads r JOIN places p ON p.id = r.place_id",
            )?;
            let mut docs = Vec::new();
            let mut an = text::analyzer();
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                docs.push(self.make_doc(
                    &mut an,
                    e.tile,
                    &r.get::<_, String>(0)?,
                    &r.get::<_, String>(1)?,
                    &r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?.clamp(0, 9) as u64,
                    r.get(4)?,
                    r.get(5)?,
                    [&r.get::<_, String>(6)?, &r.get::<_, String>(7)?, &r.get::<_, String>(8)?],
                ));
            }
            Ok(docs)
        })();
        let _ = std::fs::remove_file(&db_path);
        docs
    }

    #[allow(clippy::too_many_arguments)]
    fn make_doc(
        &self,
        an: &mut tantivy::tokenizer::TextAnalyzer,
        tile: u64,
        name: &str,
        alt: &str,
        refs: &str,
        class: u64,
        lat: i32,
        lon: i32,
        place: [&str; 3],
    ) -> TantivyDocument {
        let f = &self.f;
        let mut d = TantivyDocument::default();
        let alt_values: Vec<&str> = alt.lines().filter_map(|l| l.split_once('=')).map(|(_, v)| v).collect();
        let (n, a, r, p) =
            (text::prep(name), text::prep(&alt_values.join(" \n ")), text::ref_text(refs), text::prep(place[1]));
        d.add_text(f.names, format!("{n}\n{a}\n{r}"));
        d.add_text(f.all, format!("{n}\n{a}\n{r}\n{p}"));
        d.add_text(f.name, n);
        d.add_text(f.alt, a);
        d.add_text(f.refs, r);
        d.add_text(f.place, p);
        let mut all_words = Vec::new();
        for n in std::iter::once(name).chain(alt_values.iter().copied()) {
            let words = text::tokens(an, n);
            if !words.is_empty() {
                d.add_text(f.exact, text::exact_key(&words));
            }
            all_words.extend(words);
        }
        d.add_text(f.prefix, text::prefixes(&all_words));
        for z in CELL_LEVELS {
            let (x, y) = cell_xy(lat, lon, z);
            d.add_u64(f.cell, cell_term(z, x, y));
        }
        d.add_u64(f.tile, tile);
        d.add_i64(f.lat, lat as i64);
        d.add_i64(f.lon, lon as i64);
        d.add_u64(f.class, class);
        d.add_text(f.d_name, name);
        d.add_text(f.d_ref, refs);
        d.add_text(f.d_place, place[0]);
        d.add_text(f.d_cc, place[2]);
        d.add_text(f.d_alt, alt);
        d
    }
}
