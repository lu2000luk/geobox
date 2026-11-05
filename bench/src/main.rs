//! geobox benchmark: builds a realistic query set from published tiles, checks result
//! quality, then load-tests a running server at a fixed request rate (open loop, so slow
//! responses are not hidden) while sampling the server's memory and CPU.
//!
//! usage: bench --data <dir with manifest.tsv> --url http://127.0.0.1:8080 [--pid <server pid>]
//!              [--rps 100,300,1000] [--conns 1000] [--secs 60] [--queries 3000] [--idle-secs 30]
//!              [--quality 1] [--seed 1]
//! --conns simulates that many users, each with its own keep-alive connection.

use anyhow::{Context, Result};
use geobox_core::{Manifest, haversine_m};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
struct Road {
    name: String,
    lat: i32,
    lon: i32,
    city: String,
}

#[derive(Clone)]
struct Case {
    kind: &'static str,
    q: String,
    point: Option<(f64, f64)>,
    road: Road,
    /// what counts as correct: exact road, same name in the same city, or just same name
    strict: Strictness,
}

#[derive(Clone, Copy, PartialEq)]
enum Strictness {
    Exact,
    City,
    Name,
}

struct Args {
    data: String,
    url: String,
    pid: Option<u32>,
    rps: Vec<u32>,
    conns: usize,
    quality: bool,
    secs: u64,
    queries: usize,
    idle_secs: u64,
    seed: u64,
}

fn args() -> Result<Args> {
    let mut a = Args {
        data: String::new(),
        url: "http://127.0.0.1:8080".into(),
        pid: None,
        rps: vec![100],
        conns: 64,
        quality: true,
        secs: 60,
        queries: 3000,
        idle_secs: 30,
        seed: 1,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().with_context(|| format!("missing value for {k}"))?;
        match k.as_str() {
            "--data" => a.data = v,
            "--url" => a.url = v,
            "--pid" => a.pid = Some(v.parse()?),
            "--rps" => a.rps = v.split(',').map(|r| r.trim().parse()).collect::<Result<_, _>>()?,
            "--conns" => a.conns = v.parse()?,
            "--quality" => a.quality = v != "0",
            "--secs" => a.secs = v.parse()?,
            "--queries" => a.queries = v.parse()?,
            "--idle-secs" => a.idle_secs = v.parse()?,
            "--seed" => a.seed = v.parse()?,
            _ => anyhow::bail!("unknown argument {k}"),
        }
    }
    anyhow::ensure!(!a.data.is_empty(), "--data is required");
    Ok(a)
}

/// Reservoir-samples named roads from all tiles.
fn sample_roads(dir: &Path, n: usize, rng: &mut fastrand::Rng) -> Result<Vec<Road>> {
    let m = Manifest::parse(&std::fs::read_to_string(dir.join("manifest.tsv"))?).map_err(anyhow::Error::msg)?;
    let tmp = std::env::temp_dir().join(format!("geobox-bench-{}.db", std::process::id()));
    let mut out: Vec<Road> = Vec::new();
    let mut seen = 0usize;
    for e in &m.entries {
        std::fs::write(&tmp, zstd::decode_all(&std::fs::read(dir.join(&e.path))?[..])?)?;
        let db = rusqlite::Connection::open(&tmp)?;
        let mut st = db.prepare(
            "SELECT r.name, r.lat, r.lon, p.display FROM roads r JOIN places p ON p.id = r.place_id WHERE r.name != ''",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let display: String = r.get(3)?;
            // display is "suburb, city, state, country"; the city is the 2nd-to-last-but-one when present
            let parts: Vec<&str> = display.split(", ").collect();
            let city = if parts.len() >= 3 { parts[parts.len() - 3] } else { parts[0] }.to_string();
            let road = Road { name: r.get(0)?, lat: r.get(1)?, lon: r.get(2)?, city };
            seen += 1;
            if out.len() < n {
                out.push(road);
            } else {
                let j = rng.usize(..seen);
                if j < n {
                    out[j] = road;
                }
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Ok(out)
}

const ABBREV: &[(&str, &str)] = &[
    ("Via ", "V. "),
    ("Viale ", "V.le "),
    ("Piazza ", "P.za "),
    ("Corso ", "C.so "),
    ("Largo ", "L.go "),
    ("Vicolo ", "Vic. "),
    ("Street", "St"),
    ("Avenue", "Ave"),
    ("Road", "Rd"),
    ("Boulevard", "Bd"),
    ("Straße", "Str."),
    ("strasse", "str."),
    ("Strada ", "Str. "),
    ("Calle ", "C/ "),
];

fn make_cases(roads: &[Road], rng: &mut fastrand::Rng) -> Vec<Case> {
    let mut cases = Vec::new();
    for (i, r) in roads.iter().enumerate() {
        let near = || {
            let d = 0.015; // up to ~1.5 km away
            Some((r.lat as f64 / 1e7 + (fastrand::f64() - 0.5) * d, r.lon as f64 / 1e7 + (fastrand::f64() - 0.5) * d))
        };
        let other = &roads[(i * 7919 + 13) % roads.len()];
        // 70% near the road, 20% somewhere else in the data set, 10% no point
        let roll = rng.u8(0..10);
        let point = match roll {
            0..=6 => near(),
            7 | 8 => Some((other.lat as f64 / 1e7, other.lon as f64 / 1e7)),
            _ => None,
        };
        let words: Vec<&str> = r.name.split_whitespace().collect();
        let longest = words.iter().copied().max_by_key(|w| w.chars().count()).unwrap_or("");
        let kind = match i % 8 {
            0 | 1 => "exact",
            2 => "prefix",
            3 => "typo",
            4 => "abbrev",
            5 => "shuffled",
            6 => "address",
            _ => "city",
        };
        let q = match kind {
            "prefix" => {
                let chars: Vec<char> = r.name.chars().collect();
                let cut = (chars.len() * 2 / 3).max(4).min(chars.len());
                chars[..cut].iter().collect()
            }
            "typo" if longest.chars().count() >= 5 => {
                let mut c: Vec<char> = longest.chars().collect();
                let k = rng.usize(1..c.len() - 1);
                c.remove(k);
                r.name.replacen(longest, &c.iter().collect::<String>(), 1)
            }
            "abbrev" => ABBREV
                .iter()
                .find(|(a, _)| r.name.contains(a))
                .map_or(r.name.clone(), |(a, b)| r.name.replacen(a, b, 1)),
            "shuffled" if words.len() >= 2 => words.iter().rev().copied().collect::<Vec<_>>().join(" "),
            "address" => format!("{} {}, {:05} {}", r.name, rng.u32(1..200), rng.u32(10000..99999), r.city),
            "city" => format!("{} {}", r.name, r.city),
            _ => r.name.clone(),
        };
        let strict = if roll <= 6 {
            Strictness::Exact
        } else if matches!(kind, "address" | "city") {
            Strictness::City
        } else {
            Strictness::Name
        };
        cases.push(Case { kind, q, point, road: r.clone(), strict });
    }
    cases
}

fn url_for(base: &str, c: &Case) -> String {
    let mut u = reqwest::Url::parse(&format!("{base}/v1/search")).unwrap();
    u.query_pairs_mut().append_pair("q", &c.q).append_pair("limit", "5");
    if let Some((la, lo)) = c.point {
        u.query_pairs_mut().append_pair("lat", &format!("{la:.6}")).append_pair("lon", &format!("{lo:.6}"));
    }
    u.to_string()
}

fn correct(c: &Case, results: &[serde_json::Value]) -> (bool, bool) {
    let ok = |h: &serde_json::Value| {
        let name = h["name"].as_str().unwrap_or("");
        if name != c.road.name {
            return false;
        }
        match c.strict {
            Strictness::Exact => {
                let (la, lo) = (h["lat"].as_f64().unwrap_or(0.0), h["lon"].as_f64().unwrap_or(0.0));
                haversine_m((la * 1e7).round() as i32, (lo * 1e7).round() as i32, c.road.lat, c.road.lon) < 50.0
            }
            Strictness::City => h["place"].as_str().unwrap_or("").contains(&c.road.city),
            Strictness::Name => true,
        }
    };
    (results.first().is_some_and(ok), results.iter().take(5).any(ok))
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    if v.is_empty() { 0.0 } else { v[((v.len() as f64 * p) as usize).min(v.len() - 1)] }
}

struct Sampler {
    sys: sysinfo::System,
    pid: sysinfo::Pid,
}

impl Sampler {
    fn new(pid: u32) -> Self {
        Sampler { sys: sysinfo::System::new().expect("sysinfo"), pid: sysinfo::Pid::from_u32(pid) }
    }
    /// (rss MB, accumulated cpu seconds)
    fn sample(&mut self) -> Option<(f64, f64)> {
        self.sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[self.pid]), true);
        let p = self.sys.process(self.pid)?;
        Some((p.memory() as f64 / 1048576.0, p.accumulated_cpu_time() as f64 / 1000.0))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = args()?;
    let mut rng = fastrand::Rng::with_seed(a.seed);
    fastrand::seed(a.seed);
    let t = Instant::now();
    let roads = sample_roads(Path::new(&a.data), a.queries, &mut rng)?;
    let cases = Arc::new(make_cases(&roads, &mut rng));
    println!("built {} cases from {} in {:.1}s", cases.len(), a.data, t.elapsed().as_secs_f64());
    let client = reqwest::Client::builder().pool_max_idle_per_host(256).timeout(Duration::from_secs(30)).build()?;

    if let Some(pid) = a.pid {
        let mut s = Sampler::new(pid);
        let (rss0, cpu0) = s.sample().context("server pid not found")?;
        tokio::time::sleep(Duration::from_secs(a.idle_secs)).await;
        let (rss1, cpu1) = s.sample().context("server exited")?;
        println!(
            "idle: rss {rss0:.0} -> {rss1:.0} MB, cpu {:.3}% of one core over {}s",
            (cpu1 - cpu0) / a.idle_secs as f64 * 100.0,
            a.idle_secs
        );
    }

    // quality: sequential, one request per case
    let mut by_kind: BTreeMap<&str, (usize, usize, usize)> = BTreeMap::new();
    for c in cases.iter().filter(|_| a.quality) {
        let v: serde_json::Value = client.get(url_for(&a.url, c)).send().await?.json().await?;
        let results = v["results"].as_array().map(|v| v.as_slice()).unwrap_or(&[]);
        let (t1, t5) = correct(c, results);
        if !t5 && std::env::var("BENCH_SHOW_FAILURES").is_ok() {
            let got: Vec<String> = results.iter().take(3).map(|h| format!("{} [{}]", h["name"], h["place"])).collect();
            println!(
                "  MISS {:8} q={:?} point={:?} want={:?} [{}] got={got:?}",
                c.kind,
                c.q,
                c.point.is_some(),
                c.road.name,
                c.road.city
            );
        }
        let e = by_kind.entry(c.kind).or_default();
        e.0 += 1;
        e.1 += t1 as usize;
        e.2 += t5 as usize;
    }
    println!("quality (top1 / top5):");
    let (mut clean, mut messy) = ((0, 0), (0, 0));
    for (k, (n, t1, t5)) in &by_kind {
        println!("  {k:9} n={n:5}  {:5.1}%  {:5.1}%", *t1 as f64 * 100.0 / *n as f64, *t5 as f64 * 100.0 / *n as f64);
        let bucket = if *k == "exact" { &mut clean } else { &mut messy };
        bucket.0 += n;
        bucket.1 += t5;
    }
    println!(
        "  clean top5 {:.1}%   messy top5 {:.1}%",
        clean.1 as f64 * 100.0 / clean.0.max(1) as f64,
        messy.1 as f64 * 100.0 / messy.0.max(1) as f64
    );

    // load: for each rate, a cold pass (queries the server has not cached) then a warm one
    let users: Vec<reqwest::Client> = (0..a.conns.max(1))
        .map(|_| reqwest::Client::builder().pool_max_idle_per_host(1).timeout(Duration::from_secs(30)).build())
        .collect::<Result<_, _>>()?;
    for (step, &rps) in a.rps.iter().enumerate() {
        for pass in ["cold", "warm"] {
            let total = (rps as u64 * a.secs) as usize;
            let start = Instant::now() + Duration::from_millis(200);
            let mut tasks = Vec::with_capacity(total);
            let mut sampler = a.pid.map(Sampler::new);
            let cpu_start = sampler.as_mut().and_then(|s| s.sample()).map(|x| x.1);
            let mut max_rss: f64 = 0.0;
            for i in 0..total {
                let due = start + Duration::from_secs_f64(i as f64 / rps as f64);
                let mut c = cases[(i * 31) % cases.len()].clone();
                if pass == "cold" {
                    // the quality pass already cached these exact queries; shift points for new cache keys
                    let shift = 0.0371 * (step + 1) as f64;
                    c.point = c.point.map(|(la, lo)| (la + shift, lo - shift));
                }
                let (client, url) = (users[i % users.len()].clone(), url_for(&a.url, &c));
                let label = format!("{} {:?}{}", c.kind, c.q, if c.point.is_some() { " @point" } else { "" });
                tasks.push(tokio::spawn(async move {
                    tokio::time::sleep_until(due.into()).await;
                    // Timers can fire late (Windows: ~15 ms granularity), so service time is measured
                    // from the actual send; the lag is reported separately to show the client keeps up.
                    let sent = Instant::now();
                    let r = client.get(url).send().await;
                    // 0 = ok, 1 = shed (503), 2 = other error
                    let outcome = match r {
                        Ok(r) if r.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE => 1,
                        Ok(r) if r.status().is_success() => {
                            if r.bytes().await.is_ok() {
                                0
                            } else {
                                2
                            }
                        }
                        _ => 2,
                    };
                    (sent.elapsed().as_secs_f64() * 1000.0, (sent - due).as_secs_f64() * 1000.0, outcome, label)
                }));
                if i % rps as usize == 0 {
                    if let Some(s) = sampler.as_mut()
                        && let Some((rss, _)) = s.sample()
                    {
                        max_rss = max_rss.max(rss);
                    }
                    tokio::task::yield_now().await;
                }
            }
            let mut lat = Vec::with_capacity(total);
            let mut lag = Vec::with_capacity(total);
            let mut slowest: Vec<(f64, String)> = Vec::new();
            let (mut shed, mut errors) = (0, 0);
            for t in tasks {
                let (ms, lag_ms, outcome, label) = t.await?;
                shed += (outcome == 1) as usize;
                slowest.push((ms, label));
                lag.push(lag_ms);
                if let Some(s) = sampler.as_mut()
                    && lat.len() % 500 == 0
                    && let Some((rss, _)) = s.sample()
                {
                    max_rss = max_rss.max(rss);
                }
                lat.push(ms);
                errors += (outcome == 2) as usize;
            }
            let elapsed = start.elapsed().as_secs_f64();
            let cpu = match (cpu_start, sampler.as_mut().and_then(|s| s.sample())) {
                (Some(c0), Some((rss, c1))) => {
                    max_rss = max_rss.max(rss);
                    format!("  server cpu {:.0}% of one core, max rss {max_rss:.0} MB", (c1 - c0) / elapsed * 100.0)
                }
                _ => String::new(),
            };
            let mean = lat.iter().sum::<f64>() / lat.len().max(1) as f64;
            println!(
                "load {pass}: {total} req @ {rps} rps, {} users  p50 {:.1} ms  p95 {:.1} ms  p99 {:.1} ms  max {:.1} ms  mean {mean:.1} ms  shed(503) {shed}  errors {errors}  (client send lag p99 {:.1} ms){cpu}",
                users.len(),
                pct(&mut lat, 0.50),
                pct(&mut lat, 0.95),
                pct(&mut lat, 0.99),
                pct(&mut lat, 1.0),
                pct(&mut lag, 0.99),
            );
            slowest.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (ms, label) in slowest.iter().take(5) {
                println!("    slow {ms:7.1} ms  {label}");
            }
        }
    }
    Ok(())
}
