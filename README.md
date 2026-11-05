# geobox

Worldwide street search built from OpenStreetMap. Type a street the way people do, with typos,
abbreviations, house numbers, postcodes or a city name in any order, and get the right street,
nearest first.

```
GET /v1/search?q=via roma 12, 00185 roma
GET /v1/search?q=baker st&lat=51.52&lon=-0.15
GET /v1/search?q=hauptstr berlin
```

```json
{
  "results": [
    { "name": "Baker Street", "display_name": "Baker Street", "class": 3,
      "place": "Marylebone, London, England, United Kingdom", "country_code": "GB",
      "lat": 51.5208, "lon": -0.1571, "distance_m": 412.0, "score": 14.2 }
  ],
  "data_timestamp": 1792976403,
  "attribution": "Data © OpenStreetMap contributors, ODbL (https://www.openstreetmap.org/copyright)"
}
```

## The pieces

| Binary | What it does |
|---|---|
| `geobox-generator` | Turns an OSM `.osm.pbf` file (the whole planet or any extract) into small per-region SQLite files, bundles and a `manifest.tsv`, then runs **your** publish command (Hugging Face, S3, rclone…). Resumable and checkpointed. |
| `geobox-server` | Downloads the published files from a public URL (no credentials needed), keeps a local search index in sync by fetching only the regions that changed, and serves an HTTP API. It is also a Rust library (`geobox_server::Index`). |
| `geobox-bench` | Builds realistic queries from real streets, measures result quality, and load-tests a server. |

## Quick start

**Option A: use the data that is already published** (no generator needed):

```sh
GEOBOX_BASE_URL=https://huggingface.co/buckets/lu2000luk/world/resolve geobox-server
# first start: downloads ~560 MB in 60 files and builds a ~2.7 GB index, which takes a few minutes
curl "http://localhost:8080/v1/search?q=central%20park%20west%20new%20york"
```

**Option B: build your own data.**

```sh
# 1. generate from a small extract (seconds) or the planet (~25 min, ~32 GB RAM)
GEOBOX_PLANET_URL=https://download.geofabrik.de/europe/monaco-latest.osm.pbf geobox-generator

# 2. publish ./data anywhere public, e.g. a Hugging Face bucket
uvx hf sync ./data hf://buckets/<you>/<bucket>

# 3. serve it (or point GEOBOX_BASE_URL at the local ./data folder)
GEOBOX_BASE_URL=https://huggingface.co/buckets/<you>/<bucket>/resolve geobox-server
```

## Building

```sh
cargo build --release
# binaries: target/release/geobox-generator, geobox-server, geobox-bench
docker build -t geobox-server .                          # server image (distroless, ~150 MB)
docker build --target generator -t geobox-generator .     # generator image
```

Requires Rust 1.89+. Everything else (SQLite, zstd) is compiled in.

## Documentation

- [Generator](docs/generator.md): configuration, stages and checkpoints, publishing, scheduling, hardware.
- [Server](docs/server.md): configuration, syncing, operations, Docker, use as a library.
- [HTTP API](docs/api.md): endpoints, parameters, responses, errors.
- [How it works](docs/how-it-works.md): data format, search pipeline, ranking.
- [Benchmarks](docs/benchmarks.md): measured performance and how to reproduce it.

## Numbers (full planet, 27.1 M streets)

| | |
|---|---|
| Generator | ~25 min on 14 threads; output 555 MB of tiles + 562 MB of bundles |
| Server first sync | 60 downloads, ~6–8 min; index 2.7 GB on disk |
| Search at 100 req/s, uncached | p50 0.6 ms, p95 15 ms, p99 31–34 ms, ~0.25 CPU core |
| Search, cached | p99 0.7 ms |
| Idle | ~20 MB RAM, 0% CPU |
| Quality (right street in top 5) | 98% exact names, 92% messy queries |

Details and caveats are in [benchmarks](docs/benchmarks.md).

## License

Data: © OpenStreetMap contributors, [ODbL](https://www.openstreetmap.org/copyright). Every API
response carries an `attribution` field; show it wherever results are displayed.
