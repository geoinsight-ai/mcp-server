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
| `get_collection` | Describe one dataset directly (secondary/advanced) | `GET /collections/{id}` |

`list_insights` and `get_insight` need this server's Auth0 client-credentials configured
— they call `/concepts`, the one route the backend guards with a bearer token.
`get_data`, `list_collections` and `get_collection` are open and work without it.

### How `get_insight` works

Given an insight (by name or id) and a location (`lon`/`lat`, or a
`min_lon`/`min_lat`/`max_lon`/`max_lat` box):

1. Resolve the insight to a concept (`GET /concepts/{id}`), which carries which
   collection(s) serve it and the DGGS resolution to sample at
   (`processing[].dggs_refinement_level`).
2. Find the DGGRS grid for that collection (`GET /collections/{id}/dggs`, first entry
   unless overridden).
3. Find the zone(s) at that resolution covering the location (`GET .../zones` with a
   bbox filter — a point is expanded to a small epsilon box first; see gotcha below).
4. Fetch each zone's own value (`GET .../zones/{id}/data?zone-depth=0`, one call per
   zone, capped at 64 and run concurrently).
5. Reduce each property across the sampled zones using **that property's own
   configured statistic** (`gi_dggs_stat` from `GET /collections/{id}/queryables` — e.g.
   `mean` for a numeric property like NDVI, `majority` for a categorical one like a
   country name), not a hardcoded guess. Numeric properties support
   `mean`/`sum`/`min`/`max`/`count`; anything non-numeric falls back to a majority vote
   across the sampled zones.

The result is one JSON object: the insight, which collection/grid/resolution were used,
how many zones were sampled, and a `values` map of `{ value, stat, unit, sampled_zones }`
per property.

### `get_data`: raw values, your choice of collection(s)

`get_insight` is concept-first and aggregated; `get_data` is the lower-level sibling for
when you already know the collection(s)/field(s) you want and want the actual per-zone
values, not a single reduced number. Given a location and a `collections` array (each
`{ collection_id, fields, alias?, filter? }`, mirroring the backend's cross-collection
body), it:

1. Picks a DGGRS grid (first listed collection's `GET .../dggs`, preferring `IVEA7H`
   when available — see gotcha below — unless overridden).
2. Resolves the location to zone(s) at `zone_level` (default 9), same as `get_insight`.
3. `POST`s `/dggs/{dggrs_id}/zones/{zone_id}/data` once per zone (capped at 64,
   concurrent), with the `collections` array as the body. Fields from different
   collections come back joined on the same zone, as `prefix__field` columns
   (`prefix` = `alias` or `collection_id`) — this is how you pull e.g. elevation and a
   country name together in one call instead of two separate lookups.
4. Returns the backend's own DGGS-JSON per sampled zone, unmodified — no aggregation,
   by design.

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
| `OGC_DGGS_API_BASE_URL` | no | `http://localhost:30001` | Base URL of the ogc-dggs-api instance to call |
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
- `src/server.rs` — the `DggsServer` tool router: 5 task-shaped `#[tool]`s, backed by
  private helper methods (concept resolution, DGGRS/zone lookup, per-zone data fetch,
  queryables-driven reduction) that compose several backend calls per tool
- `src/main.rs` — wiring + stdio transport
