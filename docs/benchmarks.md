# Benchmarks

## Results

Full planet (OSM planet of late October 2026): 27.1 M streets, 337,944 admin polygons,
13,215 tiles.

- **Machine**: Windows 11 desktop, 16 threads, 32 GB RAM, NVMe.
- **Setup**: server and benchmark on the same machine. Search measured at a fixed 100 req/s for
  60 s, 6,000 requests.

### Generator

| | |
|---|---|
| Total (14 threads) | ~25 min, plus ~3 min to md5 the 90 GB input |
| Output | 13,215 tiles (555 MB) + 60 bundles (562 MB) |
| Work files | ~13 GB |

### Server

| | |
|---|---|
| First sync from a local folder | ~6 min + ~1.5 min index merge |
| Index size | 2.7 GB |
| First sync from Hugging Face in a 2-CPU / 1 GB container | peak 466 MB RAM |
| Idle (30 s sampled) | ~20 MB RSS, 0.000% CPU |

### Search latency (server-side work dominates; HTTP over loopback)

| 100 req/s | p50 | p95 | p99 | max | CPU |
|---|---|---|---|---|---|
| Uncached queries | 0.6 ms | 15 ms | 31–34 ms | ~80 ms | ~0.25 core |
| Repeated (cached) queries | 0.5 ms | 0.6 ms | 0.7 ms | ~1 ms | ~0.03 core |

No requests were shed or failed. The slowest uncached queries are typo lookups (50–80 ms).
Queries made only of very common words with no position can take 100–300 ms; with a position they
are fast.

Throughput scales with CPU: an uncached search costs ~2.5 ms of CPU on average, so one core
handles roughly 400 uncached searches/s. Repeated queries are much cheaper. Above capacity the
server answers `503` immediately instead of queueing, which keeps latency bounded; add replicas
for more.

### Quality (right street ranked #1 / within the top 5)

| Query kind | Example | top 1 | top 5 |
|---|---|---|---|
| exact name | `Via Giuseppe Garibaldi` | 86% | 98% |
| name + city | `Via Roma Milano` | 95% | 100% |
| address | `Via Roma 12, 00185 Roma` | 93% | 99.5% |
| abbreviation | `P.za Garibaldi`, `Main St` | 84% | 97% |
| shuffled words | `Garibaldi Giuseppe Via` | 84% | 97% |
| one typo | `Via Giusepe Garibaldi` | 73% | 84% |
| half-typed | `Via Giuseppe Gar` | 51% | 73% |

70% of queries come with a position near the street, 20% with a position elsewhere and 10% with
none. "Right street" means:
- with a nearby position: the exact street (within 50 m);
- otherwise: the same name in the named city, or the same name at all.

Half-typed queries are often truly ambiguous ("Via Pa" near a town has many valid answers), so
their numbers understate real-world quality.

## Reproducing

`geobox-bench` samples streets from published tiles and generates queries from them:
- the exact name, a prefix, one typo, an abbreviation, shuffled words;
- the name plus a house number, postcode and city;
- the name plus a city.

It then:
1. optionally samples the server's idle memory/CPU (by PID);
2. checks quality with one request per query;
3. load-tests at fixed rates without slowing down when the server does (so slow responses aren't
   hidden). Each rate gets an uncached pass, then a cached one.

```sh
geobox-server &                      # GEOBOX_BASE_URL=./data  (or the public URL)
geobox-bench --data ./data --url http://127.0.0.1:8080 --pid <server pid> \
             --rps 100,300,1000 --conns 1000 --secs 60 --queries 3000
```

| Flag | Default | |
|---|---|---|
| `--data` | (required) | Folder with `manifest.tsv` + `tiles/`, used to build the queries |
| `--url` | `http://127.0.0.1:8080` | Server to test |
| `--pid` | none | Server process id, for memory/CPU sampling (same machine only) |
| `--rps` | `100` | Comma-separated request rates, run one after another |
| `--conns` | `64` | Simulated users, each with its own keep-alive connection |
| `--secs` | `60` | Duration of each load pass |
| `--queries` | `3000` | Streets sampled (one query each) |
| `--idle-secs` | `30` | Idle sampling time (with `--pid`) |
| `--quality` | `1` | `0` skips the quality pass |
| `--seed` | `1` | Query sampling seed |

Set `BENCH_SHOW_FAILURES=1` to print every quality miss. The server's slow-query log
(`GEOBOX_SLOW_QUERY_MS`) shows where time goes for individual queries.
