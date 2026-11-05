use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

/// Generator settings. Every key can be overridden with an env var `GEOBOX_<KEY>`
/// (e.g. `GEOBOX_PLANET_URL`); values are parsed as TOML, falling back to a plain string.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// `http(s)://` URL (a `<url>.md5` must exist next to it) or a local `.osm.pbf` path.
    pub planet_url: String,
    pub work_dir: PathBuf,
    pub output_dir: PathBuf,
    /// Shell command run after a successful build, e.g. `uvx hf sync ./data hf://buckets/me/world`.
    /// Empty = don't publish.
    pub publish_command: String,
    pub publish_retries: u32,
    pub tile_zoom: u8,
    /// `highway=*` values that count as streets.
    pub highway_include: Vec<String>,
    /// Extra name tags indexed as alternative names.
    pub name_tags: Vec<String>,
    /// Also index every `name:<lang>` translation.
    pub name_langs: bool,
    pub ref_tags: Vec<String>,
    /// `admin_level`s of `boundary=administrative` used for place context (2 = country).
    pub admin_levels: Vec<u8>,
    /// Translations of place names (`name:<lang>`) added to the searchable place text.
    pub place_langs: Vec<String>,
    /// Input processed per checkpoint chunk (MB of compressed PBF).
    pub chunk_mb: u64,
    pub zstd_level: i32,
    /// Worker threads, 0 = all cores.
    pub threads: usize,
    /// RAM for building the prebuilt search index (`snapshot` stage), MB.
    pub index_writer_mb: usize,
}

impl Default for Config {
    fn default() -> Self {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        Config {
            planet_url: "https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf".into(),
            work_dir: "work".into(),
            output_dir: "data".into(),
            publish_command: String::new(),
            publish_retries: 3,
            tile_zoom: 8,
            highway_include: s(&[
                "motorway",
                "trunk",
                "primary",
                "secondary",
                "tertiary",
                "motorway_link",
                "trunk_link",
                "primary_link",
                "secondary_link",
                "tertiary_link",
                "unclassified",
                "residential",
                "living_street",
                "service",
                "pedestrian",
                "track",
                "road",
                "busway",
                "footway",
                "path",
                "cycleway",
                "steps",
                "bridleway",
            ]),
            name_tags: s(&["alt_name", "old_name", "official_name", "short_name", "loc_name", "reg_name", "int_name"]),
            name_langs: true,
            ref_tags: s(&["ref", "int_ref"]),
            admin_levels: vec![2, 3, 4, 5, 6, 7, 8],
            place_langs: s(&["en"]),
            chunk_mb: 512,
            zstd_level: 12,
            threads: 0,
            index_writer_mb: 2048,
        }
    }
}

impl Config {
    pub fn load(path: Option<&str>) -> Result<Self> {
        let mut table: toml::Table = match path {
            Some(p) => {
                let text = std::fs::read_to_string(p).with_context(|| format!("reading config {p}"))?;
                toml::from_str(&text).with_context(|| format!("parsing config {p}"))?
            }
            None => toml::Table::new(),
        };
        for (k, v) in std::env::vars() {
            if let Some(key) = k.strip_prefix("GEOBOX_") {
                let value = toml::from_str::<toml::Table>(&format!("v = {v}"))
                    .ok()
                    .and_then(|mut t| t.remove("v"))
                    .unwrap_or(toml::Value::String(v));
                table.insert(key.to_lowercase(), value);
            }
        }
        let cfg: Config = table.try_into().context("invalid config")?;
        anyhow::ensure!((1..=16).contains(&cfg.tile_zoom), "tile_zoom must be 1..=16");
        anyhow::ensure!(cfg.chunk_mb > 0, "chunk_mb must be > 0");
        Ok(cfg)
    }
}
