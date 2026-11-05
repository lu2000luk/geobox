//! Place context per road, then one SQLite file per tile + `manifest.tsv`.

use crate::borders::{self, PolyIndex};
use crate::merge::{self, Road};
use crate::pbf::{self, NodesOut, PlaceNode};
use anyhow::Result;
use geobox_core::{Manifest, ManifestEntry, cell_xy, hilbert, hilbert_inv};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tracing::info;

/// Search radius in meters per place kind (see `pbf::place_kind`).
const PLACE_RADIUS_M: [f64; 8] = [15_000.0, 7_000.0, 3_000.0, 1_500.0, 8_000.0, 3_000.0, 2_000.0, 1_500.0];

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
enum Who {
    Poly(u32),
    Place(u32),
}

#[derive(Hash, PartialEq, Eq, Clone)]
struct CtxKey {
    suburb: Option<Who>,
    city: Option<Who>,
    state: Option<Who>,
    country: Option<Who>,
    others: Vec<Who>,
}

struct PlaceGrid {
    places: Vec<PlaceNode>,
    cells: HashMap<(i32, i32), Vec<u32>>, // 0.1 degree cells
}

fn pcell(lat: i32, lon: i32) -> (i32, i32) {
    (lat.div_euclid(1_000_000), lon.div_euclid(1_000_000))
}

impl PlaceGrid {
    fn new(places: Vec<PlaceNode>) -> Self {
        let mut cells: HashMap<_, Vec<u32>> = HashMap::new();
        for (i, p) in places.iter().enumerate() {
            cells.entry(pcell(p.lat, p.lon)).or_default().push(i as u32);
        }
        PlaceGrid { places, cells }
    }

    /// Nearest place of the given kinds, ranked by distance / that kind's radius.
    fn nearest(
        &self,
        lat: i32,
        lon: i32,
        kinds: std::ops::RangeInclusive<u8>,
        ok: impl Fn(&PlaceNode) -> bool,
    ) -> Option<u32> {
        let max_r = kinds.clone().map(|k| PLACE_RADIUS_M[k as usize]).fold(0.0, f64::max);
        let dlat = (max_r / 11_100.0).ceil() as i32 + 1;
        let cos = (lat as f64 * 1e-7).to_radians().cos().max(0.05);
        let dlon = ((max_r / (11_100.0 * cos)).ceil() as i32 + 1).min(1800);
        let (cy, cx) = pcell(lat, lon);
        let mut best: Option<(f64, u32)> = None;
        for y in cy - dlat..=cy + dlat {
            for x in cx - dlon..=cx + dlon {
                for &i in self.cells.get(&(y, x)).into_iter().flatten() {
                    let p = &self.places[i as usize];
                    if !kinds.contains(&p.kind) || !ok(p) {
                        continue;
                    }
                    let r = geobox_core::haversine_m(lat, lon, p.lat, p.lon) / PLACE_RADIUS_M[p.kind as usize];
                    if r <= 1.0 && best.is_none_or(|b| r < b.0) {
                        best = Some((r, i));
                    }
                }
            }
        }
        best.map(|b| b.1)
    }
}

pub fn run(work: &Path, out: &Path, zoom: u8, zstd_level: i32, meta: Vec<(String, String)>) -> Result<()> {
    let polys = borders::load(work)?;
    let pidx = PolyIndex::new(&polys);
    let mut places = Vec::new();
    for c in pbf::chunk_files(&work.join("nodes"))? {
        places.extend(pbf::read_chunk::<NodesOut>(&c)?.into_iter().flat_map(|n| n.places));
    }
    let grid = PlaceGrid::new(places);
    // shortcut: all roads in RAM (~150 B each, ~7 GB for the planet); partition by tile if this outgrows 32 GB.
    let roads = merge::read_roads(work)?;
    info!("tiles: {} roads, {} polygons, {} place nodes", roads.len(), polys.len(), grid.places.len());

    let keys: Vec<CtxKey> = roads
        .par_iter()
        .map(|r| {
            let inside = pidx.containing(r.lat, r.lon);
            let smallest = |f: &dyn Fn(u8) -> bool| {
                inside
                    .iter()
                    .copied()
                    .filter(|&i| f(polys[i].level))
                    .min_by(|&a, &b| pidx.area(a).total_cmp(&pidx.area(b)))
            };
            let country = smallest(&|l| l == 2);
            let state = smallest(&|l| (3..=5).contains(&l));
            let city_level = inside.iter().map(|&i| polys[i].level).filter(|&l| l >= 6).max();
            let city = city_level.and_then(|cl| smallest(&|l| l == cl));
            let chosen = [country, state, city];
            let mut others: Vec<Who> =
                inside.iter().filter(|i| !chosen.contains(&Some(**i))).map(|&i| Who::Poly(i as u32)).collect();
            others.sort_by_key(|w| match w {
                Who::Poly(i) | Who::Place(i) => *i,
            });
            CtxKey {
                // a suburb must lie in the road's own city, not across a border
                suburb: grid
                    .nearest(r.lat, r.lon, 4..=7, |p| city.is_none_or(|c| pidx.contains(c, p.lat, p.lon)))
                    .map(Who::Place),
                city: city
                    .map(|i| Who::Poly(i as u32))
                    .or_else(|| grid.nearest(r.lat, r.lon, 0..=3, |_| true).map(Who::Place)),
                state: state.map(|i| Who::Poly(i as u32)),
                country: country.map(|i| Who::Poly(i as u32)),
                others,
            }
        })
        .collect();
    let mut ctx_ids: HashMap<CtxKey, u32> = HashMap::new();
    let mut ctx_list: Vec<CtxKey> = Vec::new();
    let road_ctx: Vec<u32> = keys
        .into_iter()
        .map(|k| {
            *ctx_ids.entry(k.clone()).or_insert_with(|| {
                ctx_list.push(k);
                ctx_list.len() as u32 - 1
            })
        })
        .collect();
    drop(ctx_ids);
    let names = |w: &Who| -> &[String] {
        match *w {
            Who::Poly(i) => &polys[i as usize].names,
            Who::Place(i) => &grid.places[i as usize].names,
        }
    };
    let ctx_text: Vec<(String, String, String)> = ctx_list
        .par_iter()
        .map(|k| {
            let mut display: Vec<&str> = Vec::new();
            for w in [&k.suburb, &k.city, &k.state, &k.country].into_iter().flatten() {
                let n = names(w)[0].as_str();
                if !display.contains(&n) {
                    display.push(n);
                }
            }
            let mut seen = HashSet::new();
            let search: Vec<&str> = [&k.suburb, &k.city, &k.state, &k.country]
                .into_iter()
                .flatten()
                .chain(&k.others)
                .flat_map(|w| names(w).iter().map(String::as_str))
                .filter(|n| seen.insert(*n))
                .collect();
            let cc = match k.country {
                Some(Who::Poly(i)) => polys[i as usize].cc.clone(),
                _ => String::new(),
            };
            (display.join(", "), search.join(" "), cc)
        })
        .collect();

    // group roads by tile, rows in a stable order (hilbert at z16, then text)
    let mut order: Vec<(u64, u64, usize)> = roads
        .par_iter()
        .enumerate()
        .map(|(i, r)| {
            let (x, y) = cell_xy(r.lat, r.lon, zoom);
            let (fx, fy) = cell_xy(r.lat, r.lon, 16);
            (hilbert(zoom, x, y), hilbert(16, fx, fy), i)
        })
        .collect();
    order.par_sort_unstable_by(|a, b| {
        (a.0, a.1).cmp(&(b.0, b.1)).then_with(|| {
            let (ra, rb) = (&roads[a.2], &roads[b.2]);
            (&ra.name, &ra.refs, ra.lat, ra.lon, &ra.alt).cmp(&(&rb.name, &rb.refs, rb.lat, rb.lon, &rb.alt))
        })
    });
    let groups: Vec<&[(u64, u64, usize)]> = order.chunk_by(|a, b| a.0 == b.0).collect();

    let tiles_dir = out.join("tiles");
    let tmp_dir = work.join("tiles-tmp");
    std::fs::create_dir_all(&tiles_dir)?;
    std::fs::create_dir_all(&tmp_dir)?;
    let entries: Vec<ManifestEntry> = groups
        .par_iter()
        .map(|g| write_tile(g, &roads, &road_ctx, &ctx_text, zoom, zstd_level, out, &tmp_dir))
        .collect::<Result<_>>()?;

    let keep: HashSet<String> = entries.iter().map(|e| e.path.clone()).collect();
    for f in std::fs::read_dir(&tiles_dir)? {
        let p = f?.path();
        let rel = format!("tiles/{}", p.file_name().unwrap().to_string_lossy());
        if !keep.contains(&rel) {
            std::fs::remove_file(&p)?;
        }
    }
    let manifest = Manifest { meta, entries, bundles: Vec::new() }; // bundles: next stage
    pbf::write_atomic(&out.join("manifest.tsv"), manifest.to_tsv().as_bytes())?;
    info!("tiles: wrote {} tiles", manifest.entries.len());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_tile(
    group: &[(u64, u64, usize)],
    roads: &[Road],
    road_ctx: &[u32],
    ctx_text: &[(String, String, String)],
    zoom: u8,
    zstd_level: i32,
    out: &Path,
    tmp_dir: &Path,
) -> Result<ManifestEntry> {
    let tile = group[0].0;
    let mut local: HashMap<u32, i64> = HashMap::new();
    let mut places: Vec<&(String, String, String)> = Vec::new();
    let mut rows = Vec::with_capacity(group.len());
    for &(_, _, i) in group {
        let c = road_ctx[i];
        let pid = *local.entry(c).or_insert_with(|| {
            places.push(&ctx_text[c as usize]);
            places.len() as i64 - 1
        });
        rows.push((&roads[i], pid));
    }
    let canon: Vec<(&str, &str, &str, u8, i32, i32, i64)> = rows
        .iter()
        .map(|(r, p)| (r.name.as_str(), r.alt.as_str(), r.refs.as_str(), r.class, r.lat, r.lon, *p))
        .collect();
    let row_hash = blake3::hash(&postcard::to_stdvec(&(&places, &canon))?).to_hex().to_string();
    let (x, y) = hilbert_inv(zoom, tile);
    let path = format!("tiles/{zoom}-{x}-{y}-{}.db.zst", &row_hash[..16]);
    let full = out.join(&path);
    let bytes = if full.exists() {
        std::fs::read(&full)? // unchanged since the last run: same rows, same file
    } else {
        let db_path = tmp_dir.join(format!("{tile}.db"));
        let _ = std::fs::remove_file(&db_path);
        {
            let mut db = rusqlite::Connection::open(&db_path)?;
            db.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA page_size=4096;")?;
            db.execute_batch(geobox_core::TILE_SCHEMA)?;
            let tx = db.transaction()?;
            {
                let mut ins = tx.prepare("INSERT INTO places VALUES (?,?,?,?)")?;
                for (i, (d, s, cc)) in places.iter().enumerate() {
                    ins.execute(rusqlite::params![i as i64, d, s, cc])?;
                }
                let mut ins = tx.prepare("INSERT INTO roads VALUES (?,?,?,?,?,?,?,?)")?;
                for (i, (name, alt, refs, class, lat, lon, pid)) in canon.iter().enumerate() {
                    ins.execute(rusqlite::params![i as i64, name, alt, refs, class, lat, lon, pid])?;
                }
            }
            tx.commit()?;
        }
        let raw = std::fs::read(&db_path)?;
        std::fs::remove_file(&db_path)?;
        let z = zstd::encode_all(&raw[..], zstd_level)?;
        pbf::write_atomic(&full, &z)?;
        z
    };
    Ok(ManifestEntry {
        tile,
        path,
        row_hash,
        file_hash: blake3::hash(&bytes).to_hex().to_string(),
        size: bytes.len() as u64,
    })
}
