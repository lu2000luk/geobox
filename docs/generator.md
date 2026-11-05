# Generator

`geobox-generator` turns an OpenStreetMap `.osm.pbf` file into the files a server downloads,
then runs your publish command.

```sh
geobox-generator                          # defaults: planet from planet.openstreetmap.org
geobox-generator --config generator.toml  # see generator.example.toml
geobox-generator --from-stage tiles       # redo a stage and everything after it
geobox-generator --help
```

Every config key can also be set as an environment variable `GEOBOX_<KEY>`, for example
`GEOBOX_PLANET_URL=...` or `GEOBOX_THREADS=8`. Values are parsed as TOML, so lists work too:
`GEOBOX_ADMIN_LEVELS="[2, 4, 8]"`. All keys and defaults are listed in
[`generator.example.toml`](../generator.example.toml).

## Inputs

- **Planet**: `https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf` (~90 GB, updated
  weekly). Mirrors with the same file work too and are often much faster, e.g.
  `https://mirror.init7.net/openstreetmap/pbf/planet-latest.osm.pbf` (~65 MB/s from Europe). A
  download interrupted on one mirror can be resumed on another: the partial file is kept.
- **Extracts** for testing or regional deployments, e.g. Geofabrik:
  `https://download.geofabrik.de/europe/italy-latest.osm.pbf`.
- **A local file**: `planet_url = "/path/to/file.osm.pbf"`.

For URLs, `<url>.md5` must exist. The download is resumable and checked against that md5. If the
md5 matches the last successful build, the run finishes immediately ("already built") and only
publishes if the last publish failed.

## Stages and checkpoints

| Stage | What it does | Planet time* |
|---|---|---|
| `index` | Reads only the PBF block headers, then binary-searches where the node / way / relation sections start (the planet is sorted), so each later pass reads only its own section | 11 s |
| `relations` | `boundary=administrative` relations at `admin_levels`: names, translations, ISO country code, member ways | 8 s |
| `ways` | Named or `ref`'d `highway=*` ways (first/middle/last node, names, refs, road class), plus the node lists of boundary ways | 5 min |
| `nodes` | Coordinates of exactly the needed nodes (227 M for the planet), plus `place=*` nodes (suburbs, villages…) | 8 min |
| `merge` | Joins way segments into streets: same name (or same ref) and a shared end node or ends within ~100 m. Center = the segment midpoint closest to the street's average position | 1.7 min |
| `borders` | Builds polygons from boundary ways. Boundaries with missing members (e.g. at an extract's edge) are skipped instead of guessed | 47 s |
| `tiles` | Place context per street (suburb, city, region, country), then one SQLite file per tile and `manifest.tsv` | 6 min |
| `bundles` | Packs tiles into up to 1024-tile tar bundles and lists them in the manifest | 1–11 s |
| `snapshot` | Builds the server's search index from the tiles (with the server's own code), merges it into one segment and publishes it as `index/<version>-<hash>.tar.zst`, so servers don't have to index on first start. RAM: `index_writer_mb` (default 2048) | 12 min (1.8 GB archive) |

\*16-thread desktop, 14 threads used, NVMe, 32 GB RAM. Before `index`, a local input file is
md5-hashed (~3 min for 90 GB).

**Resuming**: each stage writes `<work_dir>/<stage>.done` when it finishes. The long passes also
write a checkpoint every `chunk_mb` of input. Re-running after a crash continues from the last
chunk. Starting with a different input (new md5) wipes the old intermediate files automatically.
A lock file stops two runs from using the same `work_dir` at once.

**Memory**: the planet peaks at roughly 15 GB. The large arrays (node ids and coordinates) are
memory-mapped files, so machines with less RAM still finish, just slower. Disk: the input
plus ~13 GB in `work_dir` plus ~1.2 GB of output.

## Output

```
data/
  manifest.tsv                      # what servers read first
  tiles/8-<x>-<y>-<rowhash>.db.zst  # one zstd-compressed SQLite file per region
  bundles/<id>-<hash>.tar           # the same tiles, up to 1024 per tar
  index/<version>-<hash>.tar.zst    # prebuilt search index for new servers
```

File names contain a hash of their content. A region whose streets didn't change keeps exactly
the same file name, so your sync tool skips it and servers never download it again. The
manifest format is in [how it works](how-it-works.md#manifest).

## Publishing

The generator never uploads anything itself. It runs `publish_command` after a successful
build, retrying `publish_retries` times with backoff. Any tool that copies a folder works:

```toml
publish_command = "uvx hf sync ./data hf://buckets/<you>/<bucket> --delete"
publish_command = "aws s3 sync ./data s3://<bucket> --delete"
publish_command = "rclone sync ./data <remote>:<bucket>"
```

- Use the "delete" option of your tool, otherwise renamed files from older builds pile up in the bucket.
  A new manifest never references them.
- Servers tolerate a manifest that arrives before its files: a missing file is retried on the next
  poll and the old version stays in use. For the cleanest switch, upload everything except
  `manifest.tsv` first, then the manifest:
  `uvx hf sync ./data hf://buckets/<you>/<bucket> --exclude manifest.tsv && uvx hf sync ./data hf://buckets/<you>/<bucket>`
- The bucket must be publicly readable. For Hugging Face buckets, the URL servers use is
  `https://huggingface.co/buckets/<you>/<bucket>/resolve`.

## Scheduling

Run it weekly, after the planet is published (typically Monday). If the input hasn't changed,
the run does nothing, unless the last publish failed, in which case it publishes again.

```cron
0 6 * * 2  cd /srv/geobox && ./geobox-generator --config generator.toml >> generator.log 2>&1
```

Exit code 0 = built and published (or already up to date). Non-zero = failed. The previous output
and the published data are untouched, and the next run resumes.

## GitHub Actions

`.github/workflows/generate.yml` is triggered manually (Actions → generate data → Run workflow).
It builds the data and uploads `data/` as an artifact kept for 1 day. The planet needs a large or
self-hosted runner (`runner` input); standard runners are fine for extracts.

## Docker

```sh
docker build --target generator -t geobox-generator .
docker run -v $PWD:/work -e GEOBOX_PLANET_URL=... geobox-generator
```

The image has no upload tools. Build on top of it to add `aws`, `rclone` or `uv`.
