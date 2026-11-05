//! Admin boundaries: ring assembly from member ways + a fast point-in-polygon index.

use crate::merge::Coords;
use crate::pbf::{self, Boundary, WaysOut};
use anyhow::Result;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tracing::info;

type Pt = [i32; 2]; // [lat, lon] in 1e-7 degrees

#[derive(Serialize, Deserialize)]
pub struct Poly {
    pub level: u8,
    pub name: String,
    pub names: Vec<String>,
    pub cc: String,
    pub rings: Vec<Vec<Pt>>,
}

pub fn boundaries(work: &Path) -> Result<Vec<Boundary>> {
    let mut all = Vec::new();
    for c in pbf::chunk_files(&work.join("relations"))? {
        all.extend(pbf::read_chunk::<Vec<Boundary>>(&c)?.into_iter().flatten());
    }
    Ok(all)
}

pub fn boundary_way_ids(work: &Path) -> Result<HashSet<i64>> {
    Ok(boundaries(work)?.into_iter().flat_map(|b| b.ways).collect())
}

pub fn run(work: &Path) -> Result<()> {
    let bounds = boundaries(work)?;
    let mut ways: HashMap<i64, Vec<i64>> = HashMap::new();
    for c in pbf::chunk_files(&work.join("ways"))? {
        for out in pbf::read_chunk::<WaysOut>(&c)? {
            ways.extend(out.boundary_ways);
        }
    }
    let coords = Coords::open(work)?;
    let total = bounds.len();
    let polys: Vec<Poly> = bounds
        .into_par_iter()
        .filter_map(|b| {
            // A boundary cut by an extract's edge would close into a bogus shape: skip it.
            let mut lines = Vec::with_capacity(b.ways.len());
            for id in &b.ways {
                let nodes = ways.get(id)?;
                let line = nodes.iter().map(|&n| coords.get(n)).collect::<Option<Vec<Pt>>>()?;
                if line.len() >= 2 {
                    lines.push(line);
                }
            }
            let rings = assemble(lines);
            if rings.is_empty() {
                return None; // no way members (label/sub-area-only relations); counted in the summary
            }
            Some(Poly { level: b.level, name: b.name, names: b.names, cc: b.cc, rings })
        })
        .collect();
    info!("borders: {} complete polygons ({} incomplete or broken skipped)", polys.len(), total - polys.len());
    std::fs::create_dir_all(work.join("borders"))?;
    pbf::write_atomic(&work.join("borders/polys.bin"), &postcard::to_stdvec(&polys)?)
}

pub fn load(work: &Path) -> Result<Vec<Poly>> {
    Ok(postcard::from_bytes(&std::fs::read(work.join("borders/polys.bin"))?)?)
}

/// Joins line strings into closed rings. Lines that share endpoints are chained exactly;
/// leftover open chains (members missing from an extract, broken data) are joined to the
/// nearest chain end and finally closed with a straight segment.
pub fn assemble(lines: Vec<Vec<Pt>>) -> Vec<Vec<Pt>> {
    let mut rings = Vec::new();
    let mut open: Vec<Option<Vec<Pt>>> = Vec::new();
    for l in lines {
        if l.first() == l.last() && l.len() >= 4 { rings.push(l) } else { open.push(Some(l)) }
    }
    let mut by_end: HashMap<Pt, Vec<usize>> = HashMap::new();
    for (i, l) in open.iter().enumerate() {
        let l = l.as_ref().unwrap();
        by_end.entry(l[0]).or_default().push(i);
        by_end.entry(*l.last().unwrap()).or_default().push(i);
    }
    let mut chains = Vec::new();
    for i in 0..open.len() {
        let Some(mut cur) = open[i].take() else { continue };
        for _flip in 0..2 {
            loop {
                if cur.first() == cur.last() {
                    break;
                }
                let end = *cur.last().unwrap();
                let next = by_end.get(&end).and_then(|c| c.iter().copied().find(|&j| open[j].is_some()));
                let Some(j) = next else { break };
                let mut l = open[j].take().unwrap();
                if l[0] != end {
                    l.reverse();
                }
                cur.extend_from_slice(&l[1..]);
            }
            if cur.first() == cur.last() {
                break;
            }
            cur.reverse(); // try extending the other end
        }
        if cur.first() == cur.last() && cur.len() >= 4 { rings.push(cur) } else { chains.push(cur) }
    }
    // greedy nearest-end joining of what's left
    let d2 = |a: Pt, b: Pt| (a[0] as f64 - b[0] as f64).powi(2) + (a[1] as f64 - b[1] as f64).powi(2);
    while let Some(mut cur) = chains.pop() {
        loop {
            let end = *cur.last().unwrap();
            let best = chains
                .iter()
                .enumerate()
                .flat_map(|(j, c)| [(j, false, d2(end, c[0])), (j, true, d2(end, *c.last().unwrap()))])
                .min_by(|a, b| a.2.total_cmp(&b.2));
            let Some((j, rev, _)) = best else { break };
            if d2(end, cur[0]) <= best.unwrap().2 {
                break; // closing this ring is shorter than jumping to another chain
            }
            let mut c = chains.swap_remove(j);
            if rev {
                c.reverse();
            }
            cur.extend(c);
        }
        if cur.len() >= 3 {
            let first = cur[0];
            if *cur.last().unwrap() != first {
                cur.push(first);
            }
            rings.push(cur);
        }
    }
    rings
}

struct Prepared {
    bbox: [i32; 4], // min_lat, min_lon, max_lat, max_lon
    edges: Vec<[i32; 4]>,
    bands: Vec<Vec<u32>>,
    area: f64,
}

impl Prepared {
    fn new(p: &Poly) -> Self {
        let mut bbox = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
        let mut edges = Vec::new();
        for r in &p.rings {
            for w in r.windows(2) {
                edges.push([w[0][0], w[0][1], w[1][0], w[1][1]]);
            }
            for pt in r {
                bbox = [bbox[0].min(pt[0]), bbox[1].min(pt[1]), bbox[2].max(pt[0]), bbox[3].max(pt[1])];
            }
        }
        let nb = (edges.len() / 8).clamp(1, 1 << 16);
        let mut s = Prepared { bbox, edges, bands: vec![Vec::new(); nb], area: 0.0 };
        s.area = (bbox[2] - bbox[0]) as f64 * (bbox[3] - bbox[1]) as f64;
        for (i, e) in s.edges.iter().enumerate() {
            let (lo, hi) = (s.band(e[0].min(e[2])), s.band(e[0].max(e[2])));
            for b in lo..=hi {
                s.bands[b].push(i as u32);
            }
        }
        s
    }

    fn band(&self, lat: i32) -> usize {
        let h = (self.bbox[2] as i64 - self.bbox[0] as i64 + 1) as u64;
        ((lat as i64 - self.bbox[0] as i64).max(0) as u64 * self.bands.len() as u64 / h)
            .min(self.bands.len() as u64 - 1) as usize
    }

    /// Even-odd rule over all rings, so inner rings (holes) need no special handling.
    fn contains(&self, lat: i32, lon: i32) -> bool {
        if lat < self.bbox[0] || lat > self.bbox[2] || lon < self.bbox[1] || lon > self.bbox[3] {
            return false;
        }
        let mut inside = false;
        for &i in &self.bands[self.band(lat)] {
            let [alat, alon, blat, blon] = self.edges[i as usize];
            if (alat > lat) != (blat > lat) {
                let x = alon as f64 + (lat - alat) as f64 * (blon - alon) as f64 / (blat - alat) as f64;
                if (lon as f64) < x {
                    inside = !inside;
                }
            }
        }
        inside
    }
}

pub struct PolyIndex {
    polys: Vec<Prepared>,
    grid: Vec<Vec<u32>>, // 1° cells, 180 rows x 360 cols
}

fn grid_cell(lat: i32, lon: i32) -> usize {
    let r = ((lat as i64 + 900_000_000) / 10_000_000).clamp(0, 179) as usize;
    let c = ((lon as i64 + 1_800_000_000) / 10_000_000).clamp(0, 359) as usize;
    r * 360 + c
}

impl PolyIndex {
    pub fn new(polys: &[Poly]) -> Self {
        let prepared: Vec<Prepared> = polys.par_iter().map(Prepared::new).collect();
        let mut grid = vec![Vec::new(); 180 * 360];
        for (i, p) in prepared.iter().enumerate() {
            let (a, b) = (grid_cell(p.bbox[0], p.bbox[1]), grid_cell(p.bbox[2], p.bbox[3]));
            for r in a / 360..=b / 360 {
                for c in a % 360..=b % 360 {
                    grid[r * 360 + c].push(i as u32);
                }
            }
        }
        PolyIndex { polys: prepared, grid }
    }

    /// Indexes of all polygons containing the point.
    pub fn containing(&self, lat: i32, lon: i32) -> Vec<usize> {
        self.grid[grid_cell(lat, lon)]
            .iter()
            .map(|&i| i as usize)
            .filter(|&i| self.polys[i].contains(lat, lon))
            .collect()
    }

    pub fn contains(&self, i: usize, lat: i32, lon: i32) -> bool {
        self.polys[i].contains(lat, lon)
    }

    pub fn area(&self, i: usize) -> f64 {
        self.polys[i].area
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poly(rings: Vec<Vec<Pt>>) -> Poly {
        Poly { level: 2, name: "x".into(), names: vec![], cc: String::new(), rings }
    }

    #[test]
    fn assembles_split_square_and_handles_holes() {
        // square 0..100 split into two lines given in mixed directions
        let lines = vec![vec![[0, 0], [0, 100], [100, 100]], vec![[0, 0], [100, 0], [100, 100]]];
        let rings = assemble(lines);
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0].first(), rings[0].last());
        let hole = vec![[40, 40], [40, 60], [60, 60], [60, 40], [40, 40]];
        let idx = PolyIndex::new(&[poly(vec![rings[0].clone(), hole])]);
        assert_eq!(idx.containing(10, 10), vec![0]);
        assert!(idx.containing(50, 50).is_empty(), "inside the hole");
        assert!(idx.containing(150, 50).is_empty());
    }

    #[test]
    fn closes_gaps_from_missing_members() {
        // a coastline-like boundary with a missing piece between (0,100) and (10,100)
        let lines = vec![vec![[0, 0], [0, 100]], vec![[10, 100], [100, 100], [100, 0], [0, 0]]];
        let rings = assemble(lines);
        assert_eq!(rings.len(), 1);
        let idx = PolyIndex::new(&[poly(rings)]);
        assert_eq!(idx.containing(50, 50), vec![0]);
    }
}
