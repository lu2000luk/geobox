//! Packs tiles into bundle tarballs (see `geobox_core::BUNDLE_SHIFT`) and lists them in the
//! manifest. Tars are byte-for-byte deterministic, so an unchanged bundle keeps its name and is
//! never re-uploaded.

use crate::pbf;
use anyhow::{Context, Result};
use geobox_core::{BundleEntry, Manifest, ManifestEntry, bundle_of};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use tracing::info;

pub fn run(out: &Path) -> Result<()> {
    let path = out.join("manifest.tsv");
    let mut manifest = Manifest::parse(&std::fs::read_to_string(&path).context("reading manifest.tsv")?)
        .map_err(anyhow::Error::msg)?;
    let mut groups: BTreeMap<u64, Vec<&ManifestEntry>> = BTreeMap::new();
    for e in &manifest.entries {
        groups.entry(bundle_of(e.tile)).or_default().push(e);
    }
    let dir = out.join("bundles");
    std::fs::create_dir_all(&dir)?;
    let bundles: Vec<BundleEntry> =
        groups.par_iter().map(|(&b, tiles)| write_bundle(out, b, tiles)).collect::<Result<_>>()?;
    let keep: HashSet<&str> = bundles.iter().map(|b| b.path.as_str()).collect();
    for f in std::fs::read_dir(&dir)? {
        let p = f?.path();
        if !keep.contains(format!("bundles/{}", p.file_name().unwrap().to_string_lossy()).as_str()) {
            std::fs::remove_file(&p)?;
        }
    }
    let total: u64 = bundles.iter().map(|b| b.size).sum();
    info!("bundles: {} bundles, {} MB", bundles.len(), total >> 20);
    manifest.bundles = bundles;
    pbf::write_atomic(&path, manifest.to_tsv().as_bytes())
}

fn write_bundle(out: &Path, bundle: u64, tiles: &[&ManifestEntry]) -> Result<BundleEntry> {
    let mut tiles = tiles.to_vec();
    tiles.sort_by(|a, b| a.path.cmp(&b.path));
    let mut tar = tar::Builder::new(Vec::new());
    for e in tiles {
        let data = std::fs::read(out.join(&e.path)).with_context(|| format!("reading {}", e.path))?;
        let mut h = tar::Header::new_ustar();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(0);
        h.set_uid(0);
        h.set_gid(0);
        h.set_entry_type(tar::EntryType::Regular);
        tar.append_data(&mut h, &e.path, &data[..])?;
    }
    let bytes = tar.into_inner()?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let path = format!("bundles/{bundle}-{}.tar", &hash[..16]);
    let full = out.join(&path);
    if !full.exists() {
        pbf::write_atomic(&full, &bytes)?;
    }
    Ok(BundleEntry { bundle, path, file_hash: hash, size: bytes.len() as u64 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundles_are_deterministic_and_grouped() {
        let out = std::env::temp_dir().join(format!("geobox-bundle-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(out.join("tiles")).unwrap();
        let mut entries = Vec::new();
        for tile in [1u64, 2, 5000] {
            let path = format!("tiles/{tile}.db.zst");
            std::fs::write(out.join(&path), format!("tile {tile}")).unwrap();
            entries.push(ManifestEntry { tile, path, row_hash: "r".into(), file_hash: "f".into(), size: 6 });
        }
        let m = Manifest { meta: vec![], entries, bundles: vec![] };
        std::fs::write(out.join("manifest.tsv"), m.to_tsv()).unwrap();
        run(&out).unwrap();
        let first = Manifest::parse(&std::fs::read_to_string(out.join("manifest.tsv")).unwrap()).unwrap();
        assert_eq!(first.bundles.len(), 2, "tiles 1,2 share bundle 0; 5000 is in bundle 4");
        run(&out).unwrap();
        let second = Manifest::parse(&std::fs::read_to_string(out.join("manifest.tsv")).unwrap()).unwrap();
        assert_eq!(first.bundles, second.bundles, "same input -> same bundle names");
        let mut a = tar::Archive::new(std::fs::File::open(out.join(&first.bundles[0].path)).unwrap());
        let names: Vec<String> =
            a.entries().unwrap().map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["tiles/1.db.zst", "tiles/2.db.zst"]);
    }
}
