//! geobox generator: OSM PBF -> per-tile SQLite files + manifest -> your publish command.

mod borders;
mod bundles;
mod config;
mod download;
mod merge;
mod pbf;
mod tiles;

use anyhow::{Context, Result, bail};
use config::Config;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use tracing::{error, info, warn};

const STAGES: [&str; 8] = ["index", "relations", "ways", "nodes", "merge", "borders", "tiles", "bundles"];

const USAGE: &str = "usage: geobox-generator [--config <file.toml>] [--from-stage <stage>]
stages: index relations ways nodes merge borders tiles bundles
every config key can be overridden with GEOBOX_<KEY> env vars";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt().with_target(false).with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout())).init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:?}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let (mut config_path, mut from_stage) = (None, None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => config_path = Some(args.next().context(USAGE)?),
            "--from-stage" => from_stage = Some(args.next().context(USAGE)?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => bail!("unknown argument {a}\n{USAGE}"),
        }
    }
    let cfg = Arc::new(Config::load(config_path.as_deref())?);
    let from = match &from_stage {
        Some(s) => Some(STAGES.iter().position(|x| x == s).with_context(|| format!("unknown stage {s}\n{USAGE}"))?),
        None => None,
    };
    if cfg.threads > 0 {
        rayon::ThreadPoolBuilder::new().num_threads(cfg.threads).build_global()?;
    }
    std::fs::create_dir_all(&cfg.work_dir)?;
    std::fs::create_dir_all(&cfg.output_dir)?;
    let lock = std::fs::File::create(cfg.work_dir.join("generator.lock"))?;
    if lock.try_lock().is_err() {
        bail!("another generator run is using {}", cfg.work_dir.display());
    }

    let (pbf_path, md5) = download::fetch(&cfg.planet_url, &cfg.work_dir).await?;
    let built = cfg.work_dir.join("built.md5");
    let published = cfg.work_dir.join("published.md5");
    let up_to_date =
        from.is_none() && read(&built).as_deref() == Some(md5.as_str()) && cfg.output_dir.join("manifest.tsv").exists();
    if up_to_date {
        info!("output already built from input {md5}");
    } else {
        build(&cfg, &pbf_path, &md5, from).await?;
        std::fs::write(&built, &md5)?;
    }
    if cfg.publish_command.trim().is_empty() {
        info!("no publish_command set, output left in {}", cfg.output_dir.display());
    } else if read(&published).as_deref() == Some(md5.as_str()) {
        info!("already published");
    } else {
        publish(&cfg).await?;
        std::fs::write(&published, &md5)?;
    }
    Ok(())
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

/// Runs `f` unless the stage already completed for this input. A stage dir left over
/// from a different input is wiped; one left by a crash on the same input is resumed.
async fn stage<F>(cfg: &Config, md5: &str, name: &'static str, force: bool, f: F) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    let dir = cfg.work_dir.join(name);
    let marker = cfg.work_dir.join(format!("{name}.done"));
    if !force && read(&marker).as_deref() == Some(md5) {
        info!("stage {name}: already done");
        return Ok(());
    }
    let _ = std::fs::remove_file(&marker);
    let input = dir.join("input.md5");
    if force || read(&input).as_deref() != Some(md5) {
        let _ = std::fs::remove_dir_all(&dir);
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&input, md5)?;
    info!("stage {name}: running");
    let t = std::time::Instant::now();
    tokio::task::spawn_blocking(f).await?.with_context(|| format!("stage {name} failed"))?;
    std::fs::write(&marker, md5)?;
    info!("stage {name}: done in {:.1}s", t.elapsed().as_secs_f64());
    Ok(())
}

fn load_index(work: &Path) -> Result<pbf::PbfIndex> {
    Ok(postcard::from_bytes(&std::fs::read(work.join("index/index.bin"))?)?)
}

async fn build(cfg: &Arc<Config>, pbf_path: &Path, md5: &str, from: Option<usize>) -> Result<()> {
    let force = |name: &str| from.is_some_and(|f| STAGES.iter().position(|s| *s == name).unwrap() >= f);
    let work: PathBuf = cfg.work_dir.clone();
    let pbf_path = pbf_path.to_path_buf();

    let (w, p) = (work.clone(), pbf_path.clone());
    stage(cfg, md5, "index", force("index"), move || {
        let idx = pbf::build_index(&p)?;
        pbf::write_atomic(&w.join("index/index.bin"), &postcard::to_stdvec(&idx)?)
    })
    .await?;

    let (w, p, c) = (work.clone(), pbf_path.clone(), cfg.clone());
    stage(cfg, md5, "relations", force("relations"), move || {
        let idx = load_index(&w)?;
        pbf::chunked_pass(&p, &idx, idx.relations(), &w.join("relations"), c.chunk_mb, "relations", |pb| {
            pbf::relations_of(pb, &c.admin_levels, &c.place_langs)
        })
    })
    .await?;

    let (w, p, c) = (work.clone(), pbf_path.clone(), cfg.clone());
    stage(cfg, md5, "ways", force("ways"), move || {
        let idx = load_index(&w)?;
        let boundary_ways = borders::boundary_way_ids(&w)?;
        let highways: HashSet<String> = c.highway_include.iter().cloned().collect();
        let name_tags: HashSet<String> = c.name_tags.iter().cloned().collect();
        let filter = pbf::WayFilter {
            highways: &highways,
            name_tags: &name_tags,
            name_langs: c.name_langs,
            ref_tags: &c.ref_tags,
            boundary_ways: &boundary_ways,
        };
        pbf::chunked_pass(&p, &idx, idx.ways(), &w.join("ways"), c.chunk_mb, "ways", |pb| pbf::ways_of(pb, &filter))
    })
    .await?;

    let (w, p, c) = (work.clone(), pbf_path.clone(), cfg.clone());
    stage(cfg, md5, "nodes", force("nodes"), move || {
        let idx = load_index(&w)?;
        let dir = w.join("nodes");
        let needed_path = dir.join("needed.bin");
        if !needed_path.exists() {
            let mut needed = Vec::new();
            for chunk in pbf::chunk_files(&w.join("ways"))? {
                for out in pbf::read_chunk::<pbf::WaysOut>(&chunk)? {
                    needed.extend(out.roads.iter().flat_map(|r| [r.first, r.mid, r.last]));
                    needed.extend(out.boundary_ways.into_iter().flat_map(|(_, n)| n));
                }
            }
            needed.sort_unstable();
            needed.dedup();
            anyhow::ensure!(needed.len() < u32::MAX as usize, "too many needed nodes");
            info!("nodes: {} node locations needed", needed.len());
            pbf::write_atomic(&needed_path, bytemuck::cast_slice(&needed))?;
        }
        {
            let needed = merge::Mapped::<i64>::open(&needed_path)?;
            let needed = needed.as_slice();
            pbf::chunked_pass(&p, &idx, idx.nodes(), &dir.join("chunks"), c.chunk_mb, "nodes", |pb| {
                pbf::nodes_of(pb, needed, &c.place_langs)
            })?;
            let mut coords = vec![[merge::MISSING; 2]; needed.len()];
            let mut places = Vec::new();
            for chunk in pbf::chunk_files(&dir.join("chunks"))? {
                for out in pbf::read_chunk::<pbf::NodesOut>(&chunk)? {
                    for (i, lat, lon) in out.found {
                        coords[i as usize] = [lat, lon];
                    }
                    places.extend(out.places);
                }
            }
            let missing = coords.iter().filter(|c| c[0] == merge::MISSING).count();
            if missing > 0 {
                warn!("nodes: {missing} referenced nodes not in the input (normal for extracts)");
            }
            pbf::write_atomic(&dir.join("coords.bin"), bytemuck::cast_slice(&coords))?;
            pbf::write_atomic(
                &dir.join("chunk-places.bin"),
                &postcard::to_stdvec(&vec![pbf::NodesOut { found: vec![], places }])?,
            )?;
        }
        let _ = std::fs::remove_dir_all(dir.join("chunks"));
        Ok(())
    })
    .await?;

    let w = work.clone();
    stage(cfg, md5, "merge", force("merge"), move || merge::run(&w)).await?;
    let w = work.clone();
    stage(cfg, md5, "borders", force("borders"), move || borders::run(&w)).await?;

    let (w, c) = (work.clone(), cfg.clone());
    let md5_owned = md5.to_string();
    stage(cfg, md5, "tiles", force("tiles"), move || {
        let idx = load_index(&w)?;
        let generated = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        let meta = vec![
            ("osm_timestamp".to_string(), idx.timestamp.to_string()),
            ("generated".to_string(), generated.to_string()),
            ("input_md5".to_string(), md5_owned),
            ("tile_zoom".to_string(), c.tile_zoom.to_string()),
        ];
        tiles::run(&w, &c.output_dir, c.tile_zoom, c.zstd_level, meta)
    })
    .await?;

    let out = cfg.output_dir.clone();
    stage(cfg, md5, "bundles", force("bundles"), move || bundles::run(&out)).await?;
    Ok(())
}

async fn publish(cfg: &Config) -> Result<()> {
    let cmd = &cfg.publish_command;
    for attempt in 1..=cfg.publish_retries.max(1) {
        info!("publish (attempt {attempt}): {cmd}");
        let mut c = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.arg("/C").arg(cmd);
            c
        } else {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(cmd);
            c
        };
        let status = c.status().await.context("could not start publish_command")?;
        if status.success() {
            info!("published");
            return Ok(());
        }
        warn!("publish_command exited with {status}");
        if attempt < cfg.publish_retries {
            tokio::time::sleep(std::time::Duration::from_secs(30 << attempt.min(5))).await;
        }
    }
    bail!("publish failed after {} attempts", cfg.publish_retries.max(1))
}
