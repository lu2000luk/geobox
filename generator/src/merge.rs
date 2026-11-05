//! Way segments -> roads: same name (or same ref) + shared end node = one road.

use crate::pbf::{self, RawWay, WaysOut};
use anyhow::Result;
use memmap2::Mmap;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

pub const MISSING: i32 = i32::MIN;
const BUCKETS: usize = 64;

#[derive(Serialize, Deserialize)]
pub struct Road {
    pub name: String,
    /// `tag=value` lines (alt names and translations)
    pub alt: String,
    /// `;`-separated refs
    pub refs: String,
    pub class: u8,
    pub lat: i32,
    pub lon: i32,
}

/// Read-only typed view of a raw array file (memory-mapped, so it can exceed RAM).
pub struct Mapped<T> {
    map: Option<Mmap>,
    _t: std::marker::PhantomData<T>,
}

impl<T: bytemuck::Pod> Mapped<T> {
    pub fn open(path: &Path) -> Result<Self> {
        let f = std::fs::File::open(path)?;
        // SAFETY: the file is written once by this program and never modified while mapped.
        let map = if f.metadata()?.len() == 0 { None } else { Some(unsafe { Mmap::map(&f)? }) };
        Ok(Mapped { map, _t: Default::default() })
    }
    pub fn as_slice(&self) -> &[T] {
        self.map.as_ref().map_or(&[], |m| bytemuck::cast_slice(m))
    }
}

/// Sorted node ids that are needed + their coordinates (`[MISSING; 2]` when absent from the input).
pub struct Coords {
    ids: Mapped<i64>,
    coords: Mapped<[i32; 2]>,
}

impl Coords {
    pub fn open(work: &Path) -> Result<Self> {
        Ok(Coords {
            ids: Mapped::open(&work.join("nodes/needed.bin"))?,
            coords: Mapped::open(&work.join("nodes/coords.bin"))?,
        })
    }
    pub fn get(&self, id: i64) -> Option<[i32; 2]> {
        let i = self.ids.as_slice().binary_search(&id).ok()?;
        Some(self.coords.as_slice()[i]).filter(|c| c[0] != MISSING)
    }
}

fn group_key(w: &RawWay) -> String {
    if w.name.is_empty() {
        format!("\u{1}{}", w.refs.join(";").to_lowercase())
    } else {
        w.name.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

pub fn run(work: &Path) -> Result<()> {
    let dir = work.join("merge");
    std::fs::create_dir_all(&dir)?;
    // 1. partition ways into buckets by group key so each bucket fits in memory
    if !dir.join("partition.done").exists() {
        let mut writers = (0..BUCKETS)
            .map(|b| Ok(BufWriter::new(std::fs::File::create(dir.join(format!("ways-{b:02}.bin")))?)))
            .collect::<Result<Vec<_>>>()?;
        for chunk in pbf::chunk_files(&work.join("ways"))? {
            for out in pbf::read_chunk::<WaysOut>(&chunk)? {
                for w in out.roads {
                    let mut h = std::hash::DefaultHasher::new();
                    group_key(&w).hash(&mut h);
                    let bytes = postcard::to_stdvec(&w)?;
                    let wr = &mut writers[h.finish() as usize % BUCKETS];
                    wr.write_all(&(bytes.len() as u32).to_le_bytes())?;
                    wr.write_all(&bytes)?;
                }
            }
        }
        for mut w in writers {
            w.flush()?;
        }
        std::fs::write(dir.join("partition.done"), "")?;
    }
    // 2. merge each bucket
    let coords = Coords::open(work)?;
    (0..BUCKETS).into_par_iter().try_for_each(|b| -> Result<()> {
        let out = dir.join(format!("roads-{b:02}.bin"));
        if out.exists() {
            return Ok(());
        }
        let mut ways = read_records(&dir.join(format!("ways-{b:02}.bin")))?;
        ways.sort_by_cached_key(group_key);
        let mut roads = Vec::new();
        for group in ways.chunk_by(|a, b| group_key(a) == group_key(b)) {
            merge_group(group, &coords, &mut roads);
        }
        pbf::write_atomic(&out, &postcard::to_stdvec(&roads)?)
    })
}

fn read_records(path: &Path) -> Result<Vec<RawWay>> {
    let mut r = BufReader::new(std::fs::File::open(path)?);
    let mut out = Vec::new();
    let mut len = [0u8; 4];
    let mut buf = Vec::new();
    loop {
        match r.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        buf.resize(u32::from_le_bytes(len) as usize, 0);
        r.read_exact(&mut buf)?;
        out.push(postcard::from_bytes(&buf)?);
    }
    Ok(out)
}

pub fn read_roads(work: &Path) -> Result<Vec<Road>> {
    let mut all = Vec::new();
    for b in 0..BUCKETS {
        let bytes = std::fs::read(work.join(format!("merge/roads-{b:02}.bin")))?;
        all.extend(postcard::from_bytes::<Vec<Road>>(&bytes)?);
    }
    Ok(all)
}

fn find(p: &mut [usize], mut i: usize) -> usize {
    while p[i] != i {
        p[i] = p[p[i]];
        i = p[i];
    }
    i
}

fn merge_group(ways: &[RawWay], coords: &Coords, out: &mut Vec<Road>) {
    let mut parent: Vec<usize> = (0..ways.len()).collect();
    let mut ends: HashMap<i64, usize> = HashMap::new();
    for (i, w) in ways.iter().enumerate() {
        for node in [w.first, w.last] {
            match ends.get(&node) {
                Some(&j) => {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    parent[a] = b;
                }
                None => {
                    ends.insert(node, i);
                }
            }
        }
    }
    // Also join segments whose ends are close but not shared (dual carriageways, a gap at a
    // junction, a differently named bridge in between): ~100 m grid + exact distance check.
    const NEAR_E7: i64 = 9_000; // ~100 m of latitude
    type EndGrid = HashMap<(i32, i32), Vec<(usize, [i32; 2])>>;
    let mut grid = EndGrid::new();
    for (i, w) in ways.iter().enumerate() {
        for c in [w.first, w.last].into_iter().filter_map(|n| coords.get(n)) {
            let cos = (c[0] as f64 * 1e-7).to_radians().cos();
            let key = (c[0] / NEAR_E7 as i32, c[1] / NEAR_E7 as i32);
            for dy in -1..=1 {
                for dx in -1..=1 {
                    for &(j, o) in grid.get(&(key.0 + dy, key.1 + dx)).into_iter().flatten() {
                        let dlat = (c[0] - o[0]) as f64;
                        let dlon = (c[1] - o[1]) as f64 * cos;
                        if dlat * dlat + dlon * dlon < (NEAR_E7 * NEAR_E7) as f64 {
                            let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                            parent[a] = b;
                        }
                    }
                }
            }
            grid.entry(key).or_default().push((i, c));
        }
    }
    let mut comps: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..ways.len() {
        let r = find(&mut parent, i);
        comps.entry(r).or_default().push(i);
    }
    let mut comps: Vec<Vec<usize>> = comps.into_values().collect();
    comps.sort_by_key(|c| c[0]);
    for members in comps {
        let pts: Vec<[i32; 2]> = members.iter().filter_map(|&i| coords.get(ways[i].mid)).collect();
        if pts.is_empty() {
            continue; // nodes outside the extract
        }
        // shortcut: plain mean, wrong for roads crossing the antimeridian (a handful worldwide).
        let (mlat, mlon) = pts.iter().fold((0.0, 0.0), |(a, b), p| (a + p[0] as f64, b + p[1] as f64));
        let (mlat, mlon) = (mlat / pts.len() as f64, mlon / pts.len() as f64);
        let cos = (mlat * 1e-7).to_radians().cos();
        let center = pts
            .iter()
            .min_by(|a, b| {
                let d = |p: &[i32; 2]| (p[0] as f64 - mlat).powi(2) + ((p[1] as f64 - mlon) * cos).powi(2);
                d(a).total_cmp(&d(b))
            })
            .unwrap();
        let mut names: HashMap<&str, usize> = HashMap::new();
        for &i in &members {
            *names.entry(ways[i].name.as_str()).or_default() += 1;
        }
        let name = names.into_iter().max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0))).unwrap().0.to_string();
        let alt: BTreeSet<String> = members
            .iter()
            .flat_map(|&i| ways[i].alt.iter())
            .filter(|(_, v)| *v != name)
            .map(|(k, v)| format!("{k}={}", v.replace('\n', " ")))
            .collect();
        let refs: BTreeSet<&str> = members.iter().flat_map(|&i| ways[i].refs.iter().map(String::as_str)).collect();
        out.push(Road {
            name,
            alt: alt.into_iter().collect::<Vec<_>>().join("\n"),
            refs: refs.into_iter().collect::<Vec<_>>().join(";"),
            class: members.iter().map(|&i| ways[i].class).min().unwrap(),
            lat: center[0],
            lon: center[1],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn way(name: &str, first: i64, mid: i64, last: i64) -> RawWay {
        RawWay { name: name.into(), alt: vec![], refs: vec![], class: 5, first, mid, last }
    }

    /// Node i sits at lat = i * ~90 m (+ `far` for i >= 10), lon 0.
    fn coords(name: &str, far: i32) -> Coords {
        let dir = tempdir(name);
        let ids: Vec<i64> = (1..=20).collect();
        let co: Vec<[i32; 2]> = ids.iter().map(|&i| [i as i32 * 8_000 + if i >= 10 { far } else { 0 }, 0]).collect();
        std::fs::create_dir_all(dir.join("nodes")).unwrap();
        std::fs::write(dir.join("nodes/needed.bin"), bytemuck::cast_slice(&ids)).unwrap();
        std::fs::write(dir.join("nodes/coords.bin"), bytemuck::cast_slice(&co)).unwrap();
        Coords::open(&dir).unwrap()
    }

    #[test]
    fn joins_touching_segments_and_keeps_far_ones_apart() {
        let coords = coords("far", 10_000_000);
        // 1-2-3 and 3-4-5 share node 3 (one road); 10-11-12 is a different road ~1° away
        let ways = [way("Via Roma", 1, 2, 3), way("Via Roma", 3, 4, 5), way("Via Roma", 10, 11, 12)];
        let mut out = Vec::new();
        merge_group(&ways, &coords, &mut out);
        assert_eq!(out.len(), 2);
        assert!([2 * 8_000, 4 * 8_000].contains(&out[0].lat));
        assert_eq!(out[1].lat, 11 * 8_000 + 10_000_000);
    }

    #[test]
    fn joins_segments_with_nearby_ends() {
        // 5 and 6 are ~90 m apart: a gap at a junction
        let coords = coords("near", 0);
        let ways = [way("Via Roma", 1, 3, 5), way("Via Roma", 6, 7, 8), way("Via Roma", 15, 17, 19)];
        let mut out = Vec::new();
        merge_group(&ways, &coords, &mut out);
        assert_eq!(out.len(), 2, "first two joined, third is 700 m away");
    }

    fn tempdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("geobox-merge-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
