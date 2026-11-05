# Server

`geobox-server` keeps a local search index in sync with published data and answers searches
over HTTP. It needs no credentials, only a public URL (or a local folder). With no
configuration it serves the public world data set:

```sh
geobox-server                                   # = GEOBOX_BASE_URL=https://huggingface.co/buckets/lu2000luk/world/resolve
GEOBOX_BASE_URL=./data geobox-server            # your own data, from a folder or URL
```

On the first start (empty `GEOBOX_DATA_DIR`) it downloads the **prebuilt index** the generator
published (`index/*.tar.zst`, verified by hash) and installs it, so no indexing happens on the server.
After that it only applies changed tiles. Set `GEOBOX_BUILD_INDEX=1` to build the index locally from
the tiles instead (also the automatic fallback when no compatible prebuilt index is published or its
download fails). It serves right away: `/healthz` is 200, and searches and `/readyz` return 503 until
the data is loaded. The HTTP API is in [api.md](api.md).

## Configuration

All settings are environment variables.

| Variable | Default | Meaning |
|---|---|---|
| `GEOBOX_BASE_URL` | `https://huggingface.co/buckets/lu2000luk/world/resolve` | Where `manifest.tsv` is published: `https://…` or a local directory. The default is the public world data set |
| `GEOBOX_DATA_DIR` | `server-data` | Local index and sync state (planet: ~3.3 GB during the first sync, 2.7 GB after) |
| `GEOBOX_BIND` | `0.0.0.0:8080` | Listen address |
| `GEOBOX_POLL_SECS` | `3600` | How often to check the manifest (conditional GET: nearly free when unchanged) |
| `GEOBOX_SEARCH_THREADS` | CPU count, at least 4 | Searches run in parallel (more than the CPUs, so searches waiting on disk don't idle the CPU) |
| `GEOBOX_QUEUE_FACTOR` | `4` | Up to `threads × factor` more may wait; beyond that requests get 503 at once |
| `GEOBOX_CACHE_MB` | `8` | Result cache (repeated queries from nearby users are answered in <1 ms) |
| `GEOBOX_PAGE_CACHE_MB` | `24` | Index and code pages kept in memory (Linux), plus 4.5 MB per request/s up to 448 MB (the cap right away while requests are being shed). Beyond that they are dropped and re-read from disk; after 30 s idle everything is dropped (see [memory](#memory)) |
| `GEOBOX_WRITER_MB` | `64` | Indexing memory, allocated only during a sync (min 16) |
| `GEOBOX_DOCSTORE_CACHE_BLOCKS` | `16` | Cached blocks of stored results |
| `GEOBOX_DOWNLOAD_CONCURRENCY` | `8` | Parallel tile downloads (bundles: 2 at a time) |
| `GEOBOX_COMMIT_EVERY_TILES` | `200` | Sync checkpoint interval |
| `GEOBOX_SLOW_QUERY_MS` | `200` | Log searches slower than this, with a per-stage breakdown |
| `GEOBOX_BUILD_INDEX` | off | `1` = build the index locally from tiles instead of downloading the prebuilt one |
| `GEOBOX_ABBREV_FILE` | built-in | Custom abbreviation table (format: [`server/abbrev.tsv`](../server/abbrev.tsv)) |
| `GEOBOX_W_TEXT` / `GEOBOX_W_DIST` / `GEOBOX_W_CLASS` | `1.0` / `1.5` / `1.0` | Ranking weights (see [how it works](how-it-works.md#ranking)) |

## Syncing

- **Polling.** Every `GEOBOX_POLL_SECS` the server re-reads `manifest.tsv`. It sends `If-None-Match`, so
  an unchanged manifest costs one tiny request.
- **First start.** The prebuilt index is downloaded, checked against its hash, unpacked into a staging
  folder and only then moved into place, together with the list of tiles it contains. An interrupted
  download starts over on the next start; nothing half-written is ever used.
- **What gets downloaded.** Only tiles whose content hash changed are fetched. Each changed area
  comes either as a **bundle** or as single tiles:
  - A bundle is one tar of up to 1024 tiles. It's used on a first sync, or when 50+ tiles, half the
    tiles, or a third of the bytes of that bundle changed.
  - Otherwise just the changed tiles are fetched.
  - The planet's first sync is 60 bundle downloads (~560 MB). A typical weekly update is a few
    hundred small files.
- **Verification and safe replacement.** Every file is verified (size + blake3) before use. A
  region is replaced in one step: its old streets are deleted and the new ones added together, so
  searches never see half a region.
- **Failures.** A failed download keeps the previous version and is retried on the next round,
  with backoff of 30 s, doubling up to the poll interval. HTTP 429 (rate limited) and 5xx responses
  are retried within the round, honouring `Retry-After`, and pause all downloads together. A broken
  bundle falls back to its single tiles.
- **Checkpoints.** Progress is committed every `GEOBOX_COMMIT_EVERY_TILES` tiles, so a restart
  during the first sync continues where it stopped.
- **Index merging.** After a sync the index is merged into a single segment when it has more than 8.
  Queries pay per segment, so this keeps them fast. The planet merge takes about 1.5 minutes.
- **Upgrades.** A server upgrade that changes the index layout rebuilds the index from the tiles
  automatically.

## Resource use

Measured on the full planet. Details are in [benchmarks.md](benchmarks.md).

| | |
|---|---|
| Idle | ~10 MB process RSS (~20 MB container) 30 s after the last search, 0% CPU (one manifest check per hour) |
| Searching, 100 req/s uncached | ~0.25 CPU core; p99 31–34 ms |
| First sync (2 CPUs, 1 GB limit) | peak ~470 MB |

### Memory

The index is memory-mapped: its pages live in the OS file cache. Left alone, the kernel keeps every
page a search touched until the container's limit is reached, and they all count as the
container's memory. On Linux the server bounds them itself:

- **Under load**: resident index pages are kept under `GEOBOX_PAGE_CACHE_MB` plus 4.5 MB per
  request/s (averaged over 10 s, capped at 448 MB): ~50 MB at 5 req/s, ~200 MB at 40. Beyond that,
  pages are dropped and re-read from disk when needed. While any search is being shed (`503`), the
  budget is the cap, so trimming never costs throughput. When traffic drops, the budget shrinks
  within ~10 s.
- **Idle** (30 s without searches): the pages, the result cache and freed heap memory are all
  handed back to the OS.

Give it fast storage (SSD/NVMe): searches read from disk more often under the budget.

**Load shedding**: when more than `threads × (factor + 1)` searches are in flight, new ones get
`503` with `Retry-After: 1` immediately instead of queueing. Latency stays bounded and memory stays flat.
To handle more traffic, add replicas behind a load balancer. Each replica keeps its own copy,
synced from the same public URL, and nothing is shared between them.

## Operations

- `GET /healthz`: 200 while the process runs (liveness).
- `GET /readyz`: 200 once data is loaded, 503 during the first sync (readiness).
- `GET /metrics`: Prometheus:
  - `geobox_search_requests_total{status="ok|overloaded|error"}`
  - `geobox_search_seconds` (histogram)
  - `geobox_cache_hits_total`
  - `geobox_tiles`
  - `geobox_last_sync_success_timestamp_seconds`. Alert if it falls far behind your publish
    schedule.
- Logs go to stdout. Slow queries are logged as warnings, with the time spent in each step and
  each index search.
- SIGTERM / Ctrl-C shut down gracefully (in-flight requests finish).

## Docker

```sh
docker build -t geobox-server .
docker run -d -p 8080:8080 -v geobox:/data \
  -e GEOBOX_BASE_URL=https://huggingface.co/buckets/lu2000luk/world/resolve \
  geobox-server
```

The image is distroless, runs as non-root, and keeps its data in the `/data` volume. Use
`/healthz` and `/readyz` as liveness/readiness probes.

## systemd

```ini
[Unit]
Description=geobox street search
After=network-online.target

[Service]
Environment=GEOBOX_BASE_URL=https://huggingface.co/buckets/lu2000luk/world/resolve
Environment=GEOBOX_DATA_DIR=/var/lib/geobox
ExecStart=/usr/local/bin/geobox-server
DynamicUser=yes
StateDirectory=geobox
Restart=always
MemoryMax=1G

[Install]
WantedBy=multi-user.target
```

## As a Rust library

```rust
use geobox_server::{Config, Index, Query};

let index = Index::open(Config { base_url: "./data".into(), ..Config::default() })?;
index.sync().await?; // call periodically, e.g. from a tokio interval
let hits = index
    .search(Query { q: "baker st london".into(), point: Some((51.52, -0.15)), limit: 5, lang: None })
    .await?;
for h in hits {
    println!("{} ({}) {:.0} m", h.display_name, h.place, h.distance_m.unwrap_or_default());
}
```

`search` is async and applies the same load shedding as the HTTP server
(`SearchError::Overloaded`). `search_blocking` runs a search on the current thread.
