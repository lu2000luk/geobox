//! Shared pieces between the generator and the server: grid math, road classes,
//! the tile file schema and the manifest format.

/// Zoom levels of the `cell` terms each road is indexed under (used for ring search).
pub const CELL_LEVELS: [u8; 4] = [6, 8, 10, 12];

/// SQLite schema of one tile file. Coordinates are OSM's native 1e-7 degrees.
pub const TILE_SCHEMA: &str = "
CREATE TABLE places(id INTEGER PRIMARY KEY, display TEXT NOT NULL, search TEXT NOT NULL, country_code TEXT NOT NULL);
CREATE TABLE roads(id INTEGER PRIMARY KEY, name TEXT NOT NULL, alt TEXT NOT NULL, ref TEXT NOT NULL,
                   class INTEGER NOT NULL, lat INTEGER NOT NULL, lon INTEGER NOT NULL, place_id INTEGER NOT NULL);
";

/// Equirectangular grid cell of a point at zoom `z` (2^z columns over 360°, 2^z rows over 180°).
pub fn cell_xy(lat_e7: i32, lon_e7: i32, z: u8) -> (u32, u32) {
    let n = 1u64 << z;
    let x = ((lon_e7 as i64 + 1_800_000_000) as u64 * n / 3_600_000_000).min(n - 1);
    let y = ((900_000_000 - lat_e7 as i64).max(0) as u64 * n / 1_800_000_000).min(n - 1);
    (x as u32, y as u32)
}

/// Packs a grid cell into one u64 term: zoom in the top byte, then x, then y.
pub fn cell_term(z: u8, x: u32, y: u32) -> u64 {
    (z as u64) << 56 | (x as u64) << 28 | y as u64
}

/// Hilbert curve index of (x, y) on a 2^z grid. Nearby cells get nearby indexes.
pub fn hilbert(z: u8, mut x: u32, mut y: u32) -> u64 {
    let n = 1u64 << z;
    let mut d = 0u64;
    let mut s = n / 2;
    while s > 0 {
        let rx = (x as u64 & s > 0) as u64;
        let ry = (y as u64 & s > 0) as u64;
        d += s * s * ((3 * rx) ^ ry);
        rot(n, &mut x, &mut y, rx, ry);
        s /= 2;
    }
    d
}

/// Inverse of [`hilbert`].
pub fn hilbert_inv(z: u8, d: u64) -> (u32, u32) {
    let n = 1u64 << z;
    let (mut x, mut y) = (0u32, 0u32);
    let mut t = d;
    let mut s = 1u64;
    while s < n {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        rot(s, &mut x, &mut y, rx, ry);
        x += (s * rx) as u32;
        y += (s * ry) as u32;
        t /= 4;
        s *= 2;
    }
    (x, y)
}

fn rot(n: u64, x: &mut u32, y: &mut u32, rx: u64, ry: u64) {
    if ry == 0 {
        if rx == 1 {
            *x = (n - 1) as u32 - *x;
            *y = (n - 1) as u32 - *y;
        }
        std::mem::swap(x, y);
    }
}

/// Importance class of a `highway=*` value: 0 = motorway (most important) .. 9 = paths.
pub fn highway_class(v: &str) -> u8 {
    match v.strip_suffix("_link").unwrap_or(v) {
        "motorway" => 0,
        "trunk" => 1,
        "primary" => 2,
        "secondary" => 3,
        "tertiary" => 4,
        "unclassified" | "residential" | "road" | "busway" => 5,
        "living_street" | "pedestrian" => 6,
        "service" => 7,
        "track" => 8,
        _ => 9,
    }
}

/// Great-circle distance in meters between two points in 1e-7 degrees.
pub fn haversine_m(lat1: i32, lon1: i32, lat2: i32, lon2: i32) -> f64 {
    let r = |v: i32| (v as f64 * 1e-7).to_radians();
    let (p1, p2) = (r(lat1), r(lat2));
    let dp = p2 - p1;
    let dl = r(lon2) - r(lon1);
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6_371_008.8 * a.sqrt().asin()
}

/// One published tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub tile: u64,
    pub path: String,
    pub row_hash: String,
    pub file_hash: String,
    pub size: u64,
}

/// Tiles are also published in bundles (one tar per aligned run of 2^BUNDLE_SHIFT Hilbert
/// indexes, i.e. a square of up to 1024 tiles) so a first sync is a few large downloads.
/// Membership depends only on the tile id, so a changed tile never moves other tiles' bundles.
pub const BUNDLE_SHIFT: u32 = 10;

pub fn bundle_of(tile: u64) -> u64 {
    tile >> BUNDLE_SHIFT
}

/// One published bundle: a plain tar of `tiles/...` files (already zstd-compressed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleEntry {
    pub bundle: u64,
    pub path: String,
    pub file_hash: String,
    pub size: u64,
}

/// `manifest.tsv`: a `#geobox-manifest v1` header line with `key=value` metadata, one
/// tab-separated line per tile, and one `#bundle` line per bundle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub meta: Vec<(String, String)>,
    pub entries: Vec<ManifestEntry>,
    pub bundles: Vec<BundleEntry>,
}

const MANIFEST_MAGIC: &str = "#geobox-manifest v1";
const BUNDLE_MARK: &str = "#bundle";

fn safe_path(p: &str) -> bool {
    !p.contains("..") && !p.starts_with('/') && !p.contains('\\')
}

impl Manifest {
    pub fn to_tsv(&self) -> String {
        let mut s = String::from(MANIFEST_MAGIC);
        for (k, v) in &self.meta {
            s.push_str(&format!("\t{k}={v}"));
        }
        s.push('\n');
        for e in &self.entries {
            s.push_str(&format!("{}\t{}\t{}\t{}\t{}\n", e.tile, e.path, e.row_hash, e.file_hash, e.size));
        }
        for b in &self.bundles {
            s.push_str(&format!("{BUNDLE_MARK}\t{}\t{}\t{}\t{}\n", b.bundle, b.path, b.file_hash, b.size));
        }
        s
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        let header = lines.next().ok_or("empty manifest")?;
        let mut parts = header.split('\t');
        if parts.next() != Some(MANIFEST_MAGIC) {
            return Err("not a geobox manifest (bad header)".into());
        }
        let meta = parts.filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect();
        let (mut entries, mut bundles) = (Vec::new(), Vec::new());
        for (i, line) in lines.enumerate().filter(|(_, l)| !l.is_empty()) {
            let c: Vec<&str> = line.split('\t').collect();
            let bad = || format!("manifest line {}: malformed", i + 2);
            if c[0] == BUNDLE_MARK {
                if c.len() != 5 || !safe_path(c[2]) {
                    return Err(bad());
                }
                bundles.push(BundleEntry {
                    bundle: c[1].parse().map_err(|_| bad())?,
                    path: c[2].to_string(),
                    file_hash: c[3].to_string(),
                    size: c[4].parse().map_err(|_| bad())?,
                });
                continue;
            }
            if c.len() != 5 || !safe_path(c[1]) {
                return Err(bad());
            }
            entries.push(ManifestEntry {
                tile: c[0].parse().map_err(|_| bad())?,
                path: c[1].to_string(),
                row_hash: c[2].to_string(),
                file_hash: c[3].to_string(),
                size: c[4].parse().map_err(|_| bad())?,
            });
        }
        Ok(Manifest { meta, entries, bundles })
    }

    pub fn meta(&self, key: &str) -> Option<&str> {
        self.meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hilbert_roundtrip_and_locality() {
        for z in [1u8, 4, 8] {
            let n = 1u32 << z;
            let mut seen = std::collections::HashSet::new();
            for x in 0..n {
                for y in 0..n {
                    let d = hilbert(z, x, y);
                    assert!(d < (n as u64).pow(2) && seen.insert(d));
                    assert_eq!(hilbert_inv(z, d), (x, y));
                }
            }
            // consecutive indexes are adjacent cells
            for d in 1..(n as u64).pow(2) {
                let (a, b) = (hilbert_inv(z, d - 1), hilbert_inv(z, d));
                assert_eq!(a.0.abs_diff(b.0) + a.1.abs_diff(b.1), 1);
            }
        }
    }

    #[test]
    fn cells_cover_the_world() {
        assert_eq!(cell_xy(900_000_000, -1_800_000_000, 8), (0, 0));
        assert_eq!(cell_xy(-900_000_000, 1_800_000_000, 8), (255, 255));
        assert_eq!(cell_xy(0, 0, 1), (1, 1));
    }

    #[test]
    fn manifest_roundtrip_and_rejects_bad_paths() {
        let m = Manifest {
            meta: vec![("osm_timestamp".into(), "2026-11-01T00:00:00Z".into())],
            bundles: vec![BundleEntry { bundle: 0, path: "bundles/0-ef.tar".into(), file_hash: "ef".into(), size: 99 }],
            entries: vec![ManifestEntry {
                tile: 7,
                path: "tiles/8-1-2-ab.db.zst".into(),
                row_hash: "ab".into(),
                file_hash: "cd".into(),
                size: 9,
            }],
        };
        assert_eq!(Manifest::parse(&m.to_tsv()).unwrap(), m);
        assert!(Manifest::parse("#geobox-manifest v1\n1\t../x\ta\tb\t1\n").is_err());
        assert!(Manifest::parse("hello").is_err());
    }

    #[test]
    fn distance_sanity() {
        // Rome -> Milan is about 477 km
        let d = haversine_m(419_028_000, 124_964_000, 454_642_000, 91_900_000);
        assert!((d - 477_000.0).abs() < 5_000.0, "{d}");
        assert_eq!(highway_class("motorway_link"), 0);
        assert_eq!(highway_class("footway"), 9);
    }
}
