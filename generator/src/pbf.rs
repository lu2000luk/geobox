//! Passes over the PBF file. Every pass is split into chunks of ~`chunk_mb` input;
//! each finished chunk is written to disk, so a crashed run resumes at the last chunk.

use anyhow::{Context, Result};
use osmpbf::{BlobReader, BlobType, ByteOffset, PrimitiveBlock, RelMemberType};
use rayon::prelude::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use tracing::info;

#[derive(Serialize, Deserialize)]
pub struct PbfIndex {
    pub offsets: Vec<u64>,
    pub sizes: Vec<u64>,
    /// First blob of the way / relation section (only meaningful when `sorted`).
    pub way_start: usize,
    pub rel_start: usize,
    pub sorted: bool,
    /// `osmosis_replication_timestamp` of the file, unix seconds (0 if absent).
    pub timestamp: i64,
}

impl PbfIndex {
    pub fn nodes(&self) -> Range<usize> {
        if self.sorted { 0..self.way_start } else { 0..self.offsets.len() }
    }
    pub fn ways(&self) -> Range<usize> {
        if self.sorted { self.way_start..self.rel_start } else { 0..self.offsets.len() }
    }
    pub fn relations(&self) -> Range<usize> {
        if self.sorted { self.rel_start..self.offsets.len() } else { 0..self.offsets.len() }
    }
}

/// Scans blob headers only (no decompression), then binary-searches the section starts.
pub fn build_index(pbf: &Path) -> Result<PbfIndex> {
    let mut r = BlobReader::seekable_from_path(pbf)?;
    let mut probe = BlobReader::seekable_from_path(pbf)?;
    let (mut offsets, mut sizes) = (Vec::new(), Vec::new());
    let (mut sorted, mut timestamp) = (false, 0);
    while let Some(h) = r.next_header_skip_blob() {
        let (header, off) = h?;
        let off = off.context("blob without offset")?;
        match header.blob_type() {
            BlobType::OsmData => {
                offsets.push(off.0);
                sizes.push(header.get_blob_size() as u64);
            }
            BlobType::OsmHeader => {
                let hb = probe.blob_from_offset(off)?.to_headerblock()?;
                sorted = hb.optional_features().iter().chain(hb.required_features()).any(|f| f == "Sort.Type_then_ID");
                timestamp = hb.osmosis_replication_timestamp().unwrap_or(0);
            }
            _ => {}
        }
    }
    let mut kind = |i: usize| -> Result<u8> {
        let b = probe.blob_from_offset(ByteOffset(offsets[i]))?.to_primitiveblock()?;
        for g in b.groups() {
            if g.dense_nodes().next().is_some() || g.nodes().next().is_some() {
                return Ok(0);
            }
            if g.ways().next().is_some() {
                return Ok(1);
            }
            if g.relations().next().is_some() {
                return Ok(2);
            }
        }
        Ok(0)
    };
    let mut first_at_least = |k: u8| -> Result<usize> {
        let (mut lo, mut hi) = (0, offsets.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if kind(mid)? < k { lo = mid + 1 } else { hi = mid }
        }
        Ok(lo)
    };
    let (way_start, rel_start) = if sorted { (first_at_least(1)?, first_at_least(2)?) } else { (0, 0) };
    info!(
        "pbf index: {} blobs, sorted={sorted}, ways from blob {way_start}, relations from {rel_start}",
        offsets.len()
    );
    Ok(PbfIndex { offsets, sizes, way_start, rel_start, sorted, timestamp })
}

/// Runs `f` on every block of `range` in parallel and writes the per-block results
/// to `dir/chunk-NNNNN.bin`. Chunks that already exist are skipped (resume).
pub fn chunked_pass<T, F>(
    pbf: &Path,
    idx: &PbfIndex,
    range: Range<usize>,
    dir: &Path,
    chunk_mb: u64,
    name: &str,
    f: F,
) -> Result<()>
where
    T: Serialize + Send,
    F: Fn(&PrimitiveBlock) -> T + Sync,
{
    std::fs::create_dir_all(dir)?;
    let mut chunks = Vec::new();
    let (mut start, mut acc) = (range.start, 0u64);
    for i in range.clone() {
        acc += idx.sizes[i];
        if acc >= chunk_mb << 20 {
            chunks.push(start..i + 1);
            (start, acc) = (i + 1, 0);
        }
    }
    if start < range.end || chunks.is_empty() {
        chunks.push(start..range.end);
    }
    for (ci, cr) in chunks.iter().enumerate() {
        let out = dir.join(format!("chunk-{ci:05}.bin"));
        if out.exists() {
            continue;
        }
        let mut blobs = Vec::with_capacity(cr.len());
        if !cr.is_empty() {
            let mut r = BlobReader::seekable_from_path(pbf)?;
            r.seek(ByteOffset(idx.offsets[cr.start]))?;
            while blobs.len() < cr.len() {
                let b = r.next().context("unexpected end of pbf")??;
                if matches!(b.get_type(), BlobType::OsmData) {
                    blobs.push(b);
                }
            }
        }
        let results: Vec<T> =
            blobs.par_iter().map(|b| b.to_primitiveblock().map(|pb| f(&pb))).collect::<Result<_, _>>()?;
        drop(blobs);
        write_atomic(&out, &postcard::to_stdvec(&results)?)?;
        info!("{name}: chunk {}/{} done", ci + 1, chunks.len());
    }
    Ok(())
}

/// All chunk files of a pass, in order.
pub fn chunk_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("chunk-")))
        .collect();
    v.sort();
    Ok(v)
}

pub fn read_chunk<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let bytes = std::fs::read(path)?;
    postcard::from_bytes(&bytes).with_context(|| format!("corrupt chunk {}", path.display()))
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------- pass outputs ----------

#[derive(Serialize, Deserialize)]
pub struct Boundary {
    pub level: u8,
    pub name: String,
    /// `name` plus the configured `name:<lang>` translations.
    pub names: Vec<String>,
    pub cc: String,
    pub ways: Vec<i64>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RawWay {
    pub name: String,
    pub alt: Vec<(String, String)>,
    pub refs: Vec<String>,
    pub class: u8,
    pub first: i64,
    pub mid: i64,
    pub last: i64,
}

#[derive(Serialize, Deserialize, Default)]
pub struct WaysOut {
    pub roads: Vec<RawWay>,
    pub boundary_ways: Vec<(i64, Vec<i64>)>,
}

/// place kinds: 0 city, 1 town, 2 village, 3 hamlet, 4 borough, 5 suburb, 6 quarter, 7 neighbourhood
#[derive(Serialize, Deserialize)]
pub struct PlaceNode {
    pub kind: u8,
    pub names: Vec<String>,
    pub lat: i32,
    pub lon: i32,
}

#[derive(Serialize, Deserialize, Default)]
pub struct NodesOut {
    /// (index into `needed`, lat, lon)
    pub found: Vec<(u32, i32, i32)>,
    pub places: Vec<PlaceNode>,
}

pub fn place_kind(v: &str) -> Option<u8> {
    Some(match v {
        "city" => 0,
        "town" => 1,
        "village" => 2,
        "hamlet" => 3,
        "borough" => 4,
        "suburb" => 5,
        "quarter" => 6,
        "neighbourhood" => 7,
        _ => return None,
    })
}

// ---------- per-block extractors ----------

pub fn relations_of(pb: &PrimitiveBlock, levels: &[u8], langs: &[String]) -> Vec<Boundary> {
    let mut out = Vec::new();
    for g in pb.groups() {
        for rel in g.relations() {
            let (mut admin, mut level, mut ty_ok) = (false, None, false);
            let (mut name, mut names, mut cc) = (None, Vec::new(), [None, None, None]);
            for (k, v) in rel.tags() {
                match k {
                    "boundary" => admin = v == "administrative",
                    "admin_level" => level = v.parse::<u8>().ok(),
                    "type" => ty_ok = v == "boundary" || v == "multipolygon",
                    "name" => name = Some(v),
                    "ISO3166-1:alpha2" => cc[0] = Some(v),
                    "ISO3166-1" => cc[1] = Some(v),
                    "country_code" => cc[2] = Some(v),
                    _ => {
                        if let Some(l) = k.strip_prefix("name:")
                            && langs.iter().any(|x| x == l)
                        {
                            names.push(v.to_string());
                        }
                    }
                }
            }
            let (Some(level), Some(name)) = (level, name) else { continue };
            if !admin || !ty_ok || !levels.contains(&level) {
                continue;
            }
            let ways = rel
                .members()
                .filter(|m| m.member_type == RelMemberType::Way && matches!(m.role(), Ok("outer" | "inner" | "")))
                .map(|m| m.member_id)
                .collect();
            names.insert(0, name.to_string());
            names.dedup();
            let cc = if level == 2 {
                cc.iter().flatten().next().map(|c| c.to_uppercase()).unwrap_or_default()
            } else {
                String::new()
            };
            out.push(Boundary { level, name: name.to_string(), names, cc, ways });
        }
    }
    out
}

pub struct WayFilter<'a> {
    pub highways: &'a HashSet<String>,
    pub name_tags: &'a HashSet<String>,
    pub name_langs: bool,
    pub ref_tags: &'a [String],
    pub boundary_ways: &'a HashSet<i64>,
}

/// `name:xx`, `name:xxx`, `name:xx-Latn`… but not `name:etymology` & co.
fn is_lang_key(k: &str) -> bool {
    k.strip_prefix("name:").is_some_and(|l| {
        let (base, rest) = l.split_once(['-', '_']).unwrap_or((l, ""));
        (2..=3).contains(&base.len())
            && base.bytes().all(|b| b.is_ascii_lowercase())
            && rest.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

pub fn ways_of(pb: &PrimitiveBlock, f: &WayFilter) -> WaysOut {
    let mut out = WaysOut::default();
    for g in pb.groups() {
        for w in g.ways() {
            if f.boundary_ways.contains(&w.id()) {
                out.boundary_ways.push((w.id(), w.refs().collect()));
            }
            let (mut class, mut name) = (None, "");
            let (mut alt, mut refs) = (Vec::new(), Vec::new());
            for (k, v) in w.tags() {
                if k == "highway" {
                    if f.highways.contains(v) {
                        class = Some(geobox_core::highway_class(v));
                    }
                } else if k == "name" {
                    name = v;
                } else if f.ref_tags.iter().any(|t| t == k) {
                    refs.extend(v.split(';').map(str::trim).filter(|s| !s.is_empty()).map(String::from));
                } else if f.name_tags.contains(k) || (f.name_langs && is_lang_key(k)) {
                    alt.push((k.to_string(), v.to_string()));
                }
            }
            let Some(class) = class else { continue };
            let name = name.trim();
            if name.is_empty() && refs.is_empty() {
                continue;
            }
            alt.retain(|(_, v)| v != name);
            let nodes: Vec<i64> = w.refs().collect();
            if nodes.len() < 2 {
                continue;
            }
            out.roads.push(RawWay {
                name: name.to_string(),
                alt,
                refs,
                class,
                first: nodes[0],
                mid: nodes[nodes.len() / 2],
                last: nodes[nodes.len() - 1],
            });
        }
    }
    out
}

/// First index `>= from` whose value is `>= id`, assuming everything before `from` is `< id`.
fn gallop(s: &[i64], from: usize, id: i64) -> usize {
    let (mut lo, mut step) = (from, 1);
    while lo + step < s.len() && s[lo + step] < id {
        lo += step;
        step *= 2;
    }
    let hi = (lo + step + 1).min(s.len());
    lo + s[lo.min(hi)..hi].partition_point(|&x| x < id)
}

pub fn nodes_of(pb: &PrimitiveBlock, needed: &[i64], langs: &[String]) -> NodesOut {
    let mut out = NodesOut::default();
    let (mut from, mut prev) = (0usize, i64::MIN);
    let mut visit = |id: i64, lat: i32, lon: i32, tags: &mut dyn Iterator<Item = (&str, &str)>| {
        if id < prev {
            from = 0; // block not sorted by id: restart the search
        }
        prev = id;
        let j = gallop(needed, from, id);
        from = j;
        if j < needed.len() && needed[j] == id {
            out.found.push((j as u32, lat, lon));
        }
        let (mut kind, mut names) = (None, Vec::new());
        for (k, v) in tags {
            if k == "place" {
                kind = place_kind(v);
            } else if k == "name" {
                names.insert(0, v.to_string());
            } else if let Some(l) = k.strip_prefix("name:")
                && langs.iter().any(|x| x == l)
            {
                names.push(v.to_string());
            }
        }
        if let Some(kind) = kind
            && !names.is_empty()
        {
            out.places.push(PlaceNode { kind, names, lat, lon });
        }
    };
    for g in pb.groups() {
        for n in g.dense_nodes() {
            visit(n.id(), n.decimicro_lat(), n.decimicro_lon(), &mut n.tags());
        }
        for n in g.nodes() {
            visit(n.id(), n.decimicro_lat(), n.decimicro_lon(), &mut n.tags());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gallop_finds_lower_bound() {
        let s = [1i64, 3, 5, 7, 9, 11, 13, 15, 17];
        for id in 0..20 {
            let want = s.partition_point(|&x| x < id);
            assert_eq!(gallop(&s, 0, id), want, "id {id}");
            if want > 2 {
                assert_eq!(gallop(&s, 2, id), want, "id {id} from 2");
            }
        }
        assert_eq!(gallop(&[], 0, 5), 0);
    }

    #[test]
    fn lang_keys() {
        assert!(is_lang_key("name:en") && is_lang_key("name:zh-Hans") && is_lang_key("name:fur"));
        assert!(!is_lang_key("name:etymology") && !is_lang_key("name:etymology:wikidata") && !is_lang_key("name"));
    }
}
