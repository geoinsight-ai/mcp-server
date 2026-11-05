# ogc-dggs-mcp

An MCP (Model Context Protocol) server, written in Rust with [rmcp](https://github.com/modelcontextprotocol/rust-sdk),
that exposes geoinsight's [`ogc-dggs-api`](../ogc-dggs-api) as **task-shaped** tools for
LLM agents — not a 1:1 mirror of the REST API.

The API's real value is answering "what's the NDVI here" or "any burn scar here",
not "list me some DGGS zones" — DGGRS/zone mechanics are how the data is indexed, not
something a caller should have to reason about. So `get_insight` does the whole job
internally (resolve concept → collection → resolution → covering zone(s) → per-property
aggregation) and returns one clean answer. Zones, DGGRS grids, schemas and admin/order
routes are deliberately **not** exposed as tools at all.

## Tools

| Tool | What it does | Backend calls it composes |
| --- | --- | --- |
| `list_insights` | List available concepts (NDVI, burn scar analysis, ...) | `GET /concepts` |
| `get_insight` | The main tool: one aggregated value of an insight at a point or over an area | `GET /concepts/{id}`, `GET /collections/{id}/dggs`, `GET .../zones`, `GET .../zones/{id}/data` (×N, concurrently), `GET /collections/{id}/queryables` |
| `get_data` | Raw, unaggregated field values from one or more named collections at a location, optionally joined in one call | `GET /collections/{id}/dggs`, `GET .../zones`, `POST /dggs/{id}/zones/{id}/data` (×N, concurrently) |
| `list_collections` | Browse raw dataset metadata directly (secondary/advanced) | `GET /collections` |
| `get_collection` | Describe one dataset, including its `fields` (name, type, title, unit, aggregation stat) | `GET /collections/{id}`, `GET /collections/{id}/queryables` |

`list_insights` and `get_insight` need this server's Auth0 client-credentials configured
— they call `/concepts`, the one route the backend guards with a bearer token.
`get_data`, `list_collections` and `get_collection` are open and work without it.

### How `get_insight` works

Given an insight (by name or id) and a location (`lon`/`lat`, or a
`min_lon`/`min_lat`/`max_lon`/`max_lat` box):

1. Resolve the insight to a concept (`GET /concepts/{id}`), which carries which
   collection(s) serve it and the bands it reads from each
   (`sources[].inputs`, e.g. B8A and B12 for burn scars). Only those bands are
   fetched and reported; a collection the concept doesn't list falls back to all fields.
2. Find the DGGRS grid for that collection (`GET /collections/{id}/dggs`, preferring
   `IVEA7H` — see gotcha below — unless overridden).
3. Choose the zone levels from the place and the data (see
   [Zone levels](#zone-levels)), then find the zone(s) at the answer level covering the
   location. A point resolves to the
   **single zone containing it** (zone polygons via `f=geojson`, point-in-polygon), not
   the 2-3 neighbours its small search box overlaps; an area to every zone it
   intersects, sampled **evenly** across the list if there are more than 64.
4. Fetch each zone's own value (`GET .../zones/{id}/data?zone-depth=0`, one call per
   zone, run concurrently). A zone that fails is listed under `errors` and left out of
   the reduction; only if every zone fails is the call an error.
5. Reduce each property across the sampled zones using **that property's own
   configured statistic** (`gi_dggs_stat` from `GET /collections/{id}/queryables` — e.g.
   `mean` for a numeric property like NDVI, `majority` for a categorical one like a
   country name), not a hardcoded guess. Numeric properties support
   `mean`/`sum`/`min`/`max`/`count`; anything non-numeric falls back to a majority vote
   across the sampled zones.

The result is one JSON object: the insight, which collection/grid/resolution were used,
how many zones were sampled, a `values` map of `{ value, stat, unit, sampled_zones }`
per band, `errors` when some zones failed, and the same map fields as `get_data`
(`parent_level`, `children_level`, `collections`, `parquet_requests`), so a map can draw
the insight's bands over the whole place.

### `get_data`: raw values for the model, Parquet requests for the map

`get_insight` is concept-first and aggregated; `get_data` is the lower-level sibling for
when you already know the collection(s)/field(s) you want. Given a location and a
`collections` array (each `{ collection_id, fields?, alias?, filter? }`, mirroring the
backend's cross-collection body; leave `fields` out to get every field in the collection's
`/queryables` schema), it returns two things: a **small answer** for the
model, and **Parquet requests** a map fetches and draws itself, so the full data never
passes through the model or this server.

1. Picks a DGGRS grid (first listed collection's `GET .../dggs`, preferring `IVEA7H`
   when available — see gotcha below — unless overridden).
2. Chooses the zone levels from the place and the data (see [Zone levels](#zone-levels)).
3. Resolves the location at the answer level. A point resolves to the **single zone
   containing it** (zone polygons via `f=geojson`, point-in-polygon), not the 2-3
   neighbours its small search box overlaps; an area to every zone it intersects.
4. **Small answer:**
   - 20 matched zones or fewer → `rows`: one flat row per zone,
     `{ zone_id, prefix__field: value, ... }` (`prefix` = `alias` or `collection_id`),
     from `POST /dggs/{dggrs_id}/zones/{zone_id}/data` per zone.
   - More → `summary`: per field, min/mean/max for numbers or the most common values
     otherwise, computed over up to 64 zones sampled evenly across the area
     (`summary_sampled_zones`).
5. **`parquet_requests`:** one Parquet request per parent zone covering the location (up to 16;
   `requests_truncated` beyond that), each `{ parent_zone_id, method, url, body? }`. One
   collection with no alias/filter → the per-collection `GET .../zones/{parent}/data`;
   otherwise the cross-collection `POST`, so several collections come back as one
   combined view. `zone-depth` is omitted so the backend expands each parent by its
   default depth (and can serve it from its cache) — about 118,000 children per parent on
   IVEA7H.
6. If a collection fails at a zone, the zone is retried one collection at a time so the
   others' values still come back; each failure is listed under `errors` with the
   backend's `hint`. A collection that fails at **every** sampled zone is left out of the
   Parquet requests (`excluded_collections`). Only if no zone returns anything is the call an error.

### Zone levels

Neither tool uses a fixed zone level. Each picks three from the size of the place
(the bbox's longer side; a point counts as 1 km) and the finest pixel size of the
chosen fields (`gi_gsd` in the collection's queryables, which raster collections record
per band; vector collections have none):

- **children** (what the map draws) = the place's level plus the grid's default depth
  (`defaultDepth` from `GET /dggs/{id}`, 6 on IVEA7H), but never finer than the
  data's pixel size: below that, children would only repeat the same pixel. The
  place's level is the finest whose zones are still as wide as the place.
- **parent** (one Parquet request each) = children minus the default depth, so the
  backend expands each parent by its cached default. Children shallower than the
  default depth use level-0 parents and send `zone-depth` explicitly.
- **answer** (what the small answer is sampled at) = the zone containing a point at
  the children level, or two levels below the place's level for an area (~49 zones).

Zone widths come from the grid's aperture (read from its id, e.g. 7 in `IVEA7H`):
an icosahedral grid has `10·aperture^level + 2` equal-area zones per level. On IVEA7H
that is ~21 km at level 6, ~1.1 km at 9 and ~61 m at 12. For example, a ~3 km town on
10 m Sentinel-2 bands draws level-13 children (~23 m) under level-7 parents, while a
~100 km park on vector data draws level 10 under level 4. Passing `zone_level`
overrides the children level.

`zone_depth` (default `"0"`, one value per zone) passes straight through to the
backend's own zone-depth expansion if you want finer detail *within* a resolved zone;
be aware that expanding it can return a lot of values very fast (thousands, not
gotcha-free — see below).

**Known backend gotchas** (found via live testing against `api-dev.geoinsight.ai`,
independent of this server, worth their own bug reports against `ogc-dggs-api`):

- The bbox filter treats an exact `min == max` (degenerate/point) box as unbounded and
  returns effectively the whole global grid instead of the single containing zone. Both
  `get_insight` and `get_data` work around this by expanding a point to a ±0.0005° box
  before querying.
- The DGGRID-backed grids (`ISEA3HDGGRID` — which happens to sort first in every
  collection's DGGRS list — and likely its siblings) can 500 on zone data at finer
  resolutions with `"No regions: bbox is empty"`, reproducible with plain `curl`,
  unrelated to this server. Both `get_insight` and `get_data` default to `IVEA7H`
  instead of "whichever grid sorts first" for exactly this reason; override `dggrs_id`
  explicitly if you need a different grid and hit this.
- `GET .../data` without an explicit `zone-depth` picks a server-chosen `defaultDepth`
  that can be surprisingly deep (thousands of values for one small zone) — both tools
  always pass `zone-depth=0` unless you override it, to keep responses bounded.

- Live STAC collections (e.g. `sentinel-2-l2a`) make the API search Copernicus once per
  zone request, and Copernicus rate-limits bursts — a 64-zone area query on Jasper
  National Park was refused with `429 Too Many Requests`, which the API passes on as a
  **500** whose hint contains "Too Many Requests". A burst of 8 still had half refused, while 4
  got through, so this server keeps at most 4 per-zone requests in flight per tool call,
  and retries a rate-limited response (a real 429, or a 5xx with that text) up to 3 times,
  honouring `Retry-After` or backing off ~1s/2s/4s with jitter so a refused burst doesn't
  retry all at once.

**Not handled yet**: place names ("Lisbon") aren't geocoded — both tools take
coordinates only, relying on the calling agent's own geographic knowledge to turn a
place into a point/bbox. Revisit if that turns out not to be precise enough.

Every tool returns JSON, pretty-printed. Non-2xx responses become an MCP tool error
whose `data` field carries the backend's own error body (the API includes a helpful
`hint` field, e.g. listing valid DGGRS ids on a 404).

## Configuration

All configuration is via environment variables.

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `OGC_DGGS_API_BASE_URL` | no | `http://localhost:3001` | Base URL of the ogc-dggs-api instance to call |
| `OGC_DGGS_API_PUBLIC_URL` | no | `OGC_DGGS_API_BASE_URL` | The same API's URL as the caller reaches it, used for `get_data`'s `parquet_requests`. Set it when `OGC_DGGS_API_BASE_URL` is an internal address (e.g. in-cluster DNS in k8s) |
| `AUTH0_DOMAIN` | only for `list_insights`/`get_insight` | — | Auth0 tenant domain, e.g. `geoinsight.us.auth0.com` |
| `AUTH0_CLIENT_ID` | only for `list_insights`/`get_insight` | — | Auth0 M2M application client id |
| `AUTH0_CLIENT_SECRET` | only for `list_insights`/`get_insight` | — | Auth0 M2M application client secret |
| `AUTH0_AUDIENCE` | only for `list_insights`/`get_insight` | — | API identifier configured on the Auth0 API (same value the ogc-dggs-api validates as `AUTH0_AUDIENCE`) |
| `RUST_LOG` | no | `info` | Standard `tracing` filter |

The server uses the Auth0 client-credentials (machine-to-machine) grant: it fetches a
token from `https://{AUTH0_DOMAIN}/oauth/token` and caches/refreshes it automatically.
All four `AUTH0_*` variables must be set together or auth is left disabled (`get_data`,
`list_collections`/`get_collection` still work without it; `list_insights`/`get_insight`
will fail with the backend's own 401).

## Build

```sh
cargo build --release
```

The binary is `target/release/ogc-dggs-mcp`.

## Run with Claude Code / Claude Desktop

Add to your MCP config (e.g. `claude_desktop_config.json`, or via `claude mcp add`):

```json
{
  "mcpServers": {
    "ogc-dggs": {
      "command": "/absolute/path/to/target/release/ogc-dggs-mcp",
      "env": {
        "OGC_DGGS_API_BASE_URL": "https://api-dev.geoinsight.ai",
        "AUTH0_DOMAIN": "your-tenant.us.auth0.com",
        "AUTH0_CLIENT_ID": "...",
        "AUTH0_CLIENT_SECRET": "...",
        "AUTH0_AUDIENCE": "..."
      }
    }
  }
}
```

Point `OGC_DGGS_API_BASE_URL` at whichever deployment you want:
`http://localhost:30001` (local dev), `https://api-dev.geoinsight.ai`, or
`https://api-mr.geoinsight.ai`.

The server talks JSON-RPC over stdio — it must not print anything to stdout itself;
all logs go to stderr via `tracing`.

## Project layout

- `src/config.rs` — env var loading
- `src/auth.rs` — Auth0 client-credentials token fetch/cache
- `src/client.rs` — thin reqwest wrapper: builds requests, attaches auth, maps HTTP
  errors to MCP tool errors
- `src/bearer_auth.rs` — validates an HTTP caller's Auth0 bearer token (JWKS/RS256)
- `src/http_transport.rs` — the HTTP transport: `/mcp` behind the bearer check, `/healthz`
- `src/server/` — the `DggsServer` MCP server (`mod.rs`: the server and its combined tool
  router):
  - `tools/` — what MCP clients see, each file with the helpers only it uses:
    - `insights.rs` — `list_insights`, `get_insight`: concept resolution, per-zone values,
      per-property reduction
    - `collections.rs` — `list_collections`, `get_collection`: metadata and fields from
      `/queryables`
    - `data/` — `get_data`: per-zone fetching with per-collection fallback (`mod.rs`),
      flat rows, summary and sampling (`rows.rs`), parent-zone Parquet requests
      (`parquet.rs`)
    - `stats.rs` — `get_call_stats`
  - `dggs/` — internal, shared by the tools: `extent.rs` (point/bbox arguments),
    `level.rs` (zone levels from place size and pixel size), `zones.rs` (grid and zone
    lookups), `aggregate.rs` (`mean`/`sum`/`min`/`max`/
    `count` and majority vote)
- `src/main.rs` — wiring: picks the stdio or HTTP transport
