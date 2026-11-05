# How it works

```
 planet.osm.pbf ──► geobox-generator ──► data/ ──► your publish command ──► public bucket/CDN
                                                                              │
                     clients ◄── HTTP ◄── geobox-server (local index) ◄── sync┘
```

## Data published by the generator

### Tiles

The world is split into an equirectangular grid of 2^z × 2^z cells (`tile_zoom`, default 8, so a
cell is ~1.4° × 0.7°). Tiles are numbered along a Hilbert curve, which gives nearby regions
nearby numbers. Each tile with streets becomes one zstd-compressed SQLite file:

```sql
CREATE TABLE places(id INTEGER PRIMARY KEY, display TEXT, search TEXT, country_code TEXT);
CREATE TABLE roads(id INTEGER PRIMARY KEY, name TEXT, alt TEXT, ref TEXT,
                   class INTEGER, lat INTEGER, lon INTEGER, place_id INTEGER);
```

- `lat`/`lon`: 1e-7 degrees (OSM's native precision).
- `alt`: `tag=value` lines (`alt_name=…`, `name:en=…`).
- `ref`: `;`-separated.
- `display`: suburb, city, region, country, as shown to users.
- `search`: every name and translation of those places, plus the other admin areas the street is in.

The file name `tiles/<z>-<x>-<y>-<hash>.db.zst` includes a hash of the rows. A file with the
same rows always has the same name, so unchanged regions are never uploaded or downloaded again.
You can open a tile with any SQLite tool after `zstd -d`.

### Bundles

A bundle is a plain tar of the `tiles/…` files of one bundle id. The id is `tile >> 10`: an aligned
run of 1024 Hilbert numbers, which is a square of up to 1024 tiles. Membership depends only on the
tile number, so a new or deleted tile never moves other tiles to a different bundle. The tars are
deterministic (sorted entries, zero timestamps), so an unchanged bundle keeps its name. The planet
has 60 bundles, from a few KB to ~130 MB.

### Prebuilt index

The generator's last stage indexes all tiles with the server's own code and publishes the result as
`index/<version>-<hash>.tar.zst`: the index files plus the list of tiles they contain. The header
of the manifest names it (`index_path`, `index_blake3`, `index_size`, `index_version`). A server
starting without data installs it and only fetches tiles that changed afterwards. `index_version`
must match the server's index format, otherwise the server builds the index from tiles.

### Manifest

`manifest.tsv` is tab-separated UTF-8:

```
#geobox-manifest v1	osm_timestamp=1792976403	generated=1793871402	input_md5=…	tile_zoom=8
<tile>	<path>	<row_hash>	<file_blake3>	<size>
…
#bundle	<bundle_id>	<path>	<file_blake3>	<size>
…
```

- Paths are relative to the manifest. A path containing `..`, a leading `/` or `\` is rejected.
- A server applies a tile when its `row_hash` differs from the one it has, and removes tiles that
  are no longer listed.

## Generator pipeline

The stage-by-stage details are in [generator.md](generator.md#stages-and-checkpoints).

- **Streets, not segments.** OSM splits a street into many ways. Ways with the same normalized name
  (or the same `ref` when unnamed) are joined when they share an end node or their ends are within
  ~100 m. That catches dual carriageways and gaps at junctions. On Italy this removed ~20% of the
  duplicates.
- **Center.** The middle node of the segment closest to the street's average position, so it always
  lies on the street.
- **Place context.** Point-in-polygon against `boundary=administrative` polygons:
  - country = level 2, region = levels 3–5, city = the highest level ≥ 6;
  - polygon edges are indexed by latitude band, so each test only checks nearby edges;
  - the nearest `place=suburb|quarter|neighbourhood` node inside that city is added;
  - streets in places without a city polygon fall back to the nearest `city|town|village|hamlet` node.

## Server index

The server converts tiles into a [Tantivy](https://github.com/quickwit-oss/tantivy) index on disk.
It is memory-mapped, so the OS file cache does the caching and the process stays small. Per street:

- **Scoring fields** (BM25): `name`, `alt`, `ref` (plus a separator-free form: `I-95` → `i95`),
  `place`.
- **Matching fields**: `all` (every word of the four above) and `names` (only the street's own
  names). Each query word becomes a single term lookup here, which is fast to skip through.
- **`prefix`**: the first 1–20 letters of every word of the street's names, for search-as-you-type.
- **`exact`**: the whole normalized name, for an exact-name bonus.
- **`cell`**: grid cells at zoom 6, 8, 10 and 12, for searching outward from the user.
- **Fast fields**: `lat`, `lon`, `class`.

Text preparation, identical for streets and queries:
- lowercase, accents folded (`ß` → `ss`), dots dropped (`p.za` → `pza`);
- Chinese, Japanese and Korean runs (written without spaces) are split into overlapping character
  pairs.

## Search pipeline

1. **Tokenize** the query as above. At most 8 words.
2. **Plan each word.**
   - Expand abbreviations (`server/abbrev.tsv`: `st` → street|saint, `-str` → `-strasse`, …) and keep
     the expansions that exist in the index.
   - A bare number becomes optional: it adds score when it matches ("Route 66") and never blocks a
     match (house numbers).
   - The last word, if the query doesn't end with a space, is also a prefix: it matches street-name
     words starting with it, plus the most frequent place names starting with it.
   - An unknown word is either a typo or noise. With 4+ letters it is expanded to the most frequent
     indexed words within 1 typo (2 from 11 letters). Shorter unknown words and numbers are dropped.
3. **Match.** Every required word must match the street's names or its place, in any order, and at
   least one must match the street's own names. Without that last rule, "new york" would return
   every street in the city.
4. **Search outward from the point.**
   - Try the 3×3 cells at zoom 12 (~5 km) around the user, then zoom 10, 8, 6 (~2000 km), then the
     whole world, stopping as soon as there are enough results.
   - One more ring is searched after that, so a better text match slightly farther away can still
     win. That extra ring is skipped for very common words, and never goes worldwide.
   - If the rarest word matches fewer than 3,000 streets worldwide, a single worldwide pass is used
     instead: it's cheaper than rings.
5. **Fall back** only when nothing was found.
   - First allow typos in every rare word (one found in ≤ 200 streets). A word in hundreds of
     street names is real, not the misspelled one.
   - Then try leaving out one word at a time (a wrong city, an extra word).
   - Earlier tiers always rank first.
6. **Cache.** Results are cached by query, position rounded to ~1 km, limit and language. The
   cache is cleared whenever new data is applied.

## Ranking

```
score = w_text · text − w_dist · ln(1 + km) − w_class · class/9
```

- **`text`** is BM25 with field weights: name 3, ref 2.5, alt 1.5, place 1. Then:
  - +2 for a prefix match;
  - +4 when the query is exactly one of the street's names;
  - a fixed +1 × field weight for typo matches, so a rare look-alike can't outscore a real word.
- **Very common words** (more than 200k streets) don't add text score. Their weight would be near
  zero, and computing it costs more than the rest of the search.
- **Distance** uses a log scale, so 1 km vs 2 km matters but 4,000 vs 4,100 km doesn't.
- **Defaults**: `w_text 1`, `w_dist 1.5`, `w_class 1`. Override with `GEOBOX_W_*`.

## Known limits

- Only streets (`highway=*`), not parks or POIs. Change `highway_include` in the generator to add
  other way types.
- Roads crossing the antimeridian get a plain average as center (a handful worldwide).
- Words glued together ("SaintAnne" for "Saint Anne") aren't split.
- A query made only of very common words with no position ("road", "123 St") scans many matches
  and can take 100–300 ms. With a position it's fast.
- The meaning of admin levels 6–8 varies by country, so "city" may be a district or a
  municipality.
