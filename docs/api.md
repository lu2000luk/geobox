# HTTP API

Base: `http://<host>:8080`. Responses are JSON (gzip if the client accepts it). CORS is open.

## `GET /v1/search`

| Parameter | Required | Meaning |
|---|---|---|
| `q` | yes | What the user typed, 1–200 characters |
| `lat`, `lon` | no (both or neither) | User position in degrees. Nearer streets rank higher |
| `limit` | no | 1–50, default 10 |
| `lang` | no | Language code such as `en`, `de` or `zh-Hans`. `display_name` uses the street's `name:<lang>` when present |

`q` is matched forgivingly:

- **Any word order**: `street baker` finds Baker Street.
- **Abbreviations**: `st`, `ave`, `p.za`, `c.so`, `hauptstr` and [many more](../server/abbrev.tsv).
- **Typos**: `baker stret`.
- **Half-typed last word** (search as you type): `via giuseppe gar`.
- **Place words** narrow the search: `via roma milano`, `central park west new york`.
- **Address noise is ignored**: `Via Roma 12, 00185 Roma` (house number and postcode).
- **Refs and alternative or old names**: `A1`, `I-95` or `i95`, alternative and translated names.

If nothing matches every word, the server retries with typo tolerance, then with one word left
out. Results that match every word always come first.

### Response

```json
{
  "results": [
    {
      "name": "Baker Street",
      "display_name": "Baker Street",
      "ref": "A41",
      "class": 3,
      "place": "Marylebone, London, England, United Kingdom",
      "country_code": "GB",
      "lat": 51.5208,
      "lon": -0.1571,
      "distance_m": 412.0,
      "score": 14.2
    }
  ],
  "data_timestamp": 1792976403,
  "attribution": "Data © OpenStreetMap contributors, ODbL (https://www.openstreetmap.org/copyright)"
}
```

| Field | Meaning |
|---|---|
| `name` | OSM `name` (empty for ref-only roads) |
| `display_name` | `name:<lang>` if requested and available, else `name`, else `ref` |
| `ref` | Road numbers, `;`-separated. Omitted when empty |
| `class` | Road importance: 0 motorway, 1 trunk, 2 primary, 3 secondary, 4 tertiary, 5 residential/unclassified, 6 pedestrian/living street, 7 service, 8 track, 9 paths |
| `place` | Suburb, city, region, country, as far as known |
| `country_code` | ISO 3166-1 alpha-2, empty if unknown |
| `lat`, `lon` | A point on the street, near its middle |
| `distance_m` | Distance from `lat`/`lon`. Only present when a position was given |
| `score` | Relative ranking score; only meaningful within one response |
| `data_timestamp` | OSM data date (unix seconds) of the loaded data |
| `attribution` | Required by the ODbL: show it where results are displayed |

### Errors

| Status | When |
|---|---|
| 400 | Invalid parameters: `{"error": "..."}` |
| 503 | Overloaded: too many searches in flight. Has `Retry-After: 1`. Retry, or add replicas |
| 504 | Request took longer than 10 s |
| 500 | Internal error (logged) |

An empty `results` list is a normal answer: nothing matched.

## `GET /healthz`

`200 ok` while the process is running.

## `GET /readyz`

`200 ready` once data is loaded; `503` during the first sync.

## `GET /metrics`

Prometheus text format. See [server.md](server.md#operations).
