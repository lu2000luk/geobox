//! End-to-end: synthetic tiles on disk -> sync -> search behaviours.

use geobox_core::{Manifest, ManifestEntry, TILE_SCHEMA, cell_xy, hilbert};
use geobox_server::{Config, Index, Query};
use std::path::Path;

// (name, alt, ref, class, lat, lon, place display, place search, cc)
type Row = (&'static str, &'static str, &'static str, i64, f64, f64, &'static str, &'static str, &'static str);

const ROWS: &[Row] = &[
    ("Via Roma", "", "", 5, 41.9010, 12.4970, "Roma, Lazio, Italia", "Roma Rome Lazio Italia Italy", "IT"),
    ("Via Roma", "", "", 5, 45.4640, 9.1900, "Milano, Lombardia, Italia", "Milano Milan Lombardia Italia Italy", "IT"),
    ("Via Roma", "", "", 5, 38.1150, 13.3610, "Palermo, Sicilia, Italia", "Palermo Sicilia Italia Italy", "IT"),
    (
        "Via Giuseppe Garibaldi",
        "",
        "",
        5,
        41.8900,
        12.4700,
        "Roma, Lazio, Italia",
        "Roma Rome Lazio Italia Italy",
        "IT",
    ),
    (
        "Piazza Garibaldi",
        "",
        "",
        6,
        40.8530,
        14.2720,
        "Napoli, Campania, Italia",
        "Napoli Naples Campania Italia Italy",
        "IT",
    ),
    (
        "Autostrada del Sole",
        "",
        "A1;E35",
        0,
        43.7700,
        11.2500,
        "Firenze, Toscana, Italia",
        "Firenze Florence Toscana Italia Italy",
        "IT",
    ),
    (
        "Central Park West",
        "",
        "",
        4,
        40.7800,
        -73.9700,
        "Upper West Side, Manhattan, New York, United States",
        "Upper West Side Manhattan New York United States",
        "US",
    ),
    ("Main Street", "", "", 5, 40.7000, -74.0000, "New York, United States", "New York United States", "US"),
    ("Main Street", "", "", 5, 34.0500, -118.2500, "Los Angeles, United States", "Los Angeles United States", "US"),
    (
        "Baker Street",
        "name:fr=Rue Baker",
        "",
        5,
        51.5230,
        -0.1570,
        "Marylebone, London, England, United Kingdom",
        "Marylebone London England United Kingdom",
        "GB",
    ),
    ("Hauptstraße", "", "", 5, 52.5000, 13.4000, "Berlin, Deutschland", "Berlin Deutschland Germany", "DE"),
    ("Avenue de la Costa", "", "", 4, 43.7400, 7.4200, "Monte-Carlo, Monaco", "Monte-Carlo Monaco", "MC"),
    (
        "Via 12 Ottobre",
        "",
        "",
        5,
        44.4100,
        8.9300,
        "Genova, Liguria, Italia",
        "Genova Genoa Liguria Italia Italy",
        "IT",
    ),
    (
        "Route 66",
        "",
        "US 66",
        3,
        35.2000,
        -101.8000,
        "Amarillo, Texas, United States",
        "Amarillo Texas United States",
        "US",
    ),
    ("Via di Casal Boccone", "", "", 5, 41.9650, 12.5500, "Roma, Lazio, Italia", "Roma Rome Lazio Italia Italy", "IT"),
    // a second, unjoined piece of the same street ~250 m away: must show up once
    ("Via di Casal Boccone", "", "", 5, 41.9665, 12.5522, "Roma, Lazio, Italia", "Roma Rome Lazio Italia Italy", "IT"),
    (
        "Via Di Sorrento",
        "",
        "",
        5,
        26.4024,
        -80.1731,
        "Palm Beach County, Florida, United States",
        "Palm Beach County Florida United States",
        "US",
    ),
];

fn write_tiles(dir: &Path) {
    std::fs::create_dir_all(dir.join("tiles")).unwrap();
    let mut by_tile: std::collections::BTreeMap<u64, Vec<&Row>> = Default::default();
    for r in ROWS {
        let (x, y) = cell_xy((r.4 * 1e7) as i32, (r.5 * 1e7) as i32, 8);
        by_tile.entry(hilbert(8, x, y)).or_default().push(r);
    }
    let mut entries = Vec::new();
    for (tile, rows) in by_tile {
        let db_path = dir.join(format!("{tile}.db"));
        let db = rusqlite::Connection::open(&db_path).unwrap();
        db.execute_batch(TILE_SCHEMA).unwrap();
        for (i, r) in rows.iter().enumerate() {
            db.execute("INSERT INTO places VALUES (?,?,?,?)", rusqlite::params![i as i64, r.6, r.7, r.8]).unwrap();
            db.execute(
                "INSERT INTO roads VALUES (?,?,?,?,?,?,?,?)",
                rusqlite::params![i as i64, r.0, r.1, r.2, r.3, (r.4 * 1e7) as i64, (r.5 * 1e7) as i64, i as i64],
            )
            .unwrap();
        }
        drop(db);
        let z = zstd::encode_all(&std::fs::read(&db_path).unwrap()[..], 3).unwrap();
        std::fs::remove_file(&db_path).unwrap();
        let path = format!("tiles/{tile}.db.zst");
        std::fs::write(dir.join(&path), &z).unwrap();
        entries.push(ManifestEntry {
            tile,
            path,
            row_hash: blake3::hash(&z).to_hex().to_string(),
            file_hash: blake3::hash(&z).to_hex().to_string(),
            size: z.len() as u64,
        });
    }
    // bundles, like the generator's: a plain tar of tiles/... per bundle id
    let mut bundles = Vec::new();
    let mut by_bundle: std::collections::BTreeMap<u64, Vec<&ManifestEntry>> = Default::default();
    for e in &entries {
        by_bundle.entry(geobox_core::bundle_of(e.tile)).or_default().push(e);
    }
    std::fs::create_dir_all(dir.join("bundles")).unwrap();
    for (b, es) in by_bundle {
        let mut tar = tar::Builder::new(Vec::new());
        for e in es {
            let data = std::fs::read(dir.join(&e.path)).unwrap();
            let mut h = tar::Header::new_ustar();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            tar.append_data(&mut h, &e.path, &data[..]).unwrap();
        }
        let bytes = tar.into_inner().unwrap();
        let path = format!("bundles/{b}.tar");
        std::fs::write(dir.join(&path), &bytes).unwrap();
        bundles.push(geobox_core::BundleEntry {
            bundle: b,
            path,
            file_hash: blake3::hash(&bytes).to_hex().to_string(),
            size: bytes.len() as u64,
        });
    }
    let m = Manifest { meta: vec![("osm_timestamp".into(), "1".into())], entries, bundles };
    std::fs::write(dir.join("manifest.tsv"), m.to_tsv()).unwrap();
}

async fn setup() -> (tempfile::TempDir, std::sync::Arc<Index>) {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("published");
    write_tiles(&data);
    let cfg =
        Config { base_url: data.to_string_lossy().into(), data_dir: tmp.path().join("server"), ..Config::default() };
    let idx = Index::open(cfg).unwrap();
    assert!(!idx.is_ready());
    let r = idx.sync().await.unwrap();
    assert_eq!(r.failed, 0);
    assert!(idx.is_ready());
    (tmp, idx)
}

async fn top(idx: &std::sync::Arc<Index>, q: &str, point: Option<(f64, f64)>) -> Vec<(String, String)> {
    let hits = idx.search(Query { q: q.into(), point, limit: 5, lang: None }).await.unwrap();
    hits.into_iter().map(|h| (h.display_name, h.place)).collect()
}

#[tokio::test]
async fn smart_search_behaviours() {
    let (_tmp, idx) = setup().await;
    assert_eq!(idx.road_count(), ROWS.len() as u64);
    let milan = Some((45.47, 9.18));
    let rome = Some((41.9, 12.5));

    let r = top(&idx, "via roma", milan).await;
    assert!(r[0].1.starts_with("Milano"), "nearest Via Roma first: {r:?}");
    let r = top(&idx, "via roma", rome).await;
    assert!(r[0].1.starts_with("Roma"), "{r:?}");

    // place words beat distance; unrelated point
    let r = top(&idx, "central park new york", rome).await;
    assert_eq!(r[0].0, "Central Park West", "{r:?}");
    // house number + postcode ignored, city word picks the right Via Roma
    let r = top(&idx, "Via Roma 12, 00185 Roma", None).await;
    assert!(r[0].1.starts_with("Roma"), "{r:?}");
    let r = top(&idx, "via roma milan", rome).await;
    assert!(r[0].1.starts_with("Milano"), "{r:?}");
    // word order, typos, abbreviations, suffix rules, refs, prefixes
    assert_eq!(top(&idx, "street baker", None).await[0].0, "Baker Street");
    assert_eq!(top(&idx, "baker stret", None).await[0].0, "Baker Street");
    assert_eq!(top(&idx, "p.za garibaldi", None).await[0].0, "Piazza Garibaldi");
    assert_eq!(top(&idx, "hauptstr berlin", None).await[0].0, "Hauptstraße");
    assert_eq!(top(&idx, "a1", None).await[0].0, "Autostrada del Sole");
    assert_eq!(top(&idx, "costa av monaco", None).await[0].0, "Avenue de la Costa");
    assert_eq!(top(&idx, "via giuseppe gar", None).await[0].0, "Via Giuseppe Garibaldi");
    // main street: distance decides
    let r = top(&idx, "main st", Some((34.0, -118.0))).await;
    assert!(r[0].1.starts_with("Los Angeles"), "{r:?}");
    // numbers inside names still count, house numbers never block a match
    assert_eq!(top(&idx, "via 12 ottobre", None).await[0].0, "Via 12 Ottobre");
    let r = top(&idx, "via roma 12", milan).await;
    assert!(r[0].0 == "Via Roma" && r[0].1.starts_with("Milano"), "{r:?}");
    assert_eq!(top(&idx, "route 66", None).await[0].0, "Route 66");
    assert_eq!(top(&idx, "66", None).await[0].0, "Route 66");
    // the user-reported query, with a far-away street sharing the generic words
    assert_eq!(top(&idx, "via di casal boccone", Some((41.7658, 12.2814))).await[0].0, "Via di Casal Boccone");
    assert_eq!(top(&idx, "via di casal boccone roma", None).await[0].0, "Via di Casal Boccone");
    let r = top(&idx, "casal boccone", None).await;
    assert_eq!(r.iter().filter(|x| x.0 == "Via di Casal Boccone").count(), 1, "pieces merged: {r:?}");
    // noise only -> nothing; unknown street -> nothing
    assert!(top(&idx, "345 00185", None).await.is_empty());
    assert!(top(&idx, "zzzzqqq", None).await.is_empty());

    // language display + distance reporting
    let h = idx
        .search(Query { q: "baker street".into(), point: Some((51.52, -0.16)), limit: 1, lang: Some("fr".into()) })
        .await
        .unwrap();
    assert_eq!(h[0].display_name, "Rue Baker");
    assert!(h[0].distance_m.unwrap() < 1000.0);
}

#[tokio::test]
async fn sync_over_http_survives_rate_limiting() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("published");
    write_tiles(&data);
    // one corrupt bundle: its tiles must come one by one instead
    let m = Manifest::parse(&std::fs::read_to_string(data.join("manifest.tsv")).unwrap()).unwrap();
    std::fs::write(data.join(&m.bundles[0].path), b"not a tar").unwrap();
    // a host that answers 429 to the first three requests, then serves the files
    let hits = std::sync::Arc::new(AtomicUsize::new(0));
    let tile_hits = std::sync::Arc::new(AtomicUsize::new(0));
    let (root, counter, tile_counter) = (data.clone(), hits.clone(), tile_hits.clone());
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let (root, counter, tile_counter) = (root.clone(), counter.clone(), tile_counter.clone());
        async move {
            use axum::response::IntoResponse;
            if uri.path().starts_with("/tiles/") {
                tile_counter.fetch_add(1, Ordering::SeqCst);
            }
            if counter.fetch_add(1, Ordering::SeqCst) < 3 {
                return (axum::http::StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], "slow down")
                    .into_response();
            }
            match tokio::fs::read(root.join(uri.path().trim_start_matches('/'))).await {
                Ok(b) => b.into_response(),
                Err(_) => axum::http::StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let cfg = Config { base_url: format!("http://{addr}"), data_dir: tmp.path().join("server"), ..Config::default() };
    let idx = Index::open(cfg).unwrap();
    let r = idx.sync().await.unwrap();
    assert_eq!(r.failed, 0, "{r:?}");
    assert_eq!(idx.road_count(), ROWS.len() as u64);
    assert!(hits.load(Ordering::SeqCst) > 3);
    // first sync uses bundles; only the corrupt bundle's tiles were fetched one by one
    let corrupt_tiles = m.entries.iter().filter(|e| geobox_core::bundle_of(e.tile) == m.bundles[0].bundle).count();
    assert_eq!(tile_hits.load(Ordering::SeqCst), corrupt_tiles);
}

#[tokio::test]
async fn resync_is_incremental_and_removes_tiles() {
    let (tmp, idx) = setup().await;
    let data = tmp.path().join("published");
    // nothing changed -> nothing applied
    let r = idx.sync().await.unwrap();
    assert_eq!((r.applied, r.removed), (0, 0));
    // drop one tile from the manifest -> its roads disappear
    let mut m = Manifest::parse(&std::fs::read_to_string(data.join("manifest.tsv")).unwrap()).unwrap();
    let gone = m.entries.remove(0);
    std::fs::write(data.join("manifest.tsv"), m.to_tsv()).unwrap();
    let before = idx.road_count();
    let r = idx.sync().await.unwrap();
    assert_eq!((r.applied, r.removed), (0, 1));
    assert!(idx.road_count() < before, "tile {} removed", gone.tile);
    // reopening keeps state
    drop(idx);
    let cfg =
        Config { base_url: data.to_string_lossy().into(), data_dir: tmp.path().join("server"), ..Config::default() };
    let idx = Index::open(cfg).unwrap();
    assert!(idx.is_ready());
    assert_eq!(idx.sync().await.unwrap().applied, 0);
}

#[tokio::test]
async fn prebuilt_index_bootstrap() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("published");
    write_tiles(&data);
    // generator side: build + publish the index
    let (path, hash, size) = geobox_server::build_snapshot(&data, &tmp.path().join("build"), 64).await.unwrap();
    let mut m = Manifest::parse(&std::fs::read_to_string(data.join("manifest.tsv")).unwrap()).unwrap();
    geobox_server::add_to_manifest(&mut m, &path, &hash, size);
    std::fs::write(data.join("manifest.tsv"), m.to_tsv()).unwrap();

    // GEOBOX_BUILD_INDEX=1: no download, the normal sync builds it
    let opt_out = Config {
        base_url: data.to_string_lossy().into(),
        data_dir: tmp.path().join("own"),
        build_index: true,
        ..Config::default()
    };
    assert!(!geobox_server::bootstrap(&opt_out).await.unwrap());

    // server side: a fresh data dir gets the prebuilt index and is ready without indexing anything
    let cfg = Config { base_url: data.to_string_lossy().into(), data_dir: tmp.path().join("srv"), ..Config::default() };
    assert!(geobox_server::bootstrap(&cfg).await.unwrap());
    assert!(!geobox_server::bootstrap(&cfg).await.unwrap(), "second start: local data already there");
    let idx = Index::open(cfg).unwrap();
    assert!(idx.is_ready());
    assert_eq!(idx.road_count(), ROWS.len() as u64);
    let r = idx.sync().await.unwrap();
    assert_eq!((r.applied, r.failed), (0, 0), "snapshot already matches the manifest");
    assert_eq!(top(&idx, "baker street", None).await[0].0, "Baker Street");
}
