use std::collections::HashMap;
use std::sync::Arc;

use futures::future::join_all;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::ApiClient;

/// A point query is expanded to a bbox this wide/tall (degrees) before being
/// sent to the backend. The API's bbox filter treats an exact `min == max`
/// box as unbounded and returns effectively the whole grid instead of the
/// single containing zone — this sidesteps that.
const POINT_EPSILON_DEG: f64 = 0.0005;

/// Caps how many zones a single `get_insight`/`get_data` call will fan out
/// to, so an oversized area (or too-fine a zone level) can't trigger
/// thousands of backend requests.
const MAX_ZONES_PER_QUERY: usize = 64;

/// `get_data`'s zone level when the caller doesn't specify one. `get_insight`
/// instead reads a concept-specific resolution, but `get_data` has no concept
/// to read it from.
const DEFAULT_ZONE_LEVEL: u32 = 9;

/// `get_data` returns one row per zone up to this many matched zones; above
/// it, a per-field summary instead, so the model's context stays small.
const MAX_ROWS: usize = 20;

/// The backend's zone-depth expansion when a grid doesn't report its own
/// `default_depth` (IVEA7H reports 6).
const FALLBACK_DEFAULT_DEPTH: u32 = 6;

/// Caps how many parent-zone Parquet requests one `get_data` call returns.
/// Each one expands to `default_depth` levels of children (~7^6 on IVEA7H),
/// so this bounds what the map is asked to load. Provisional until we know
/// what the map draws comfortably.
const MAX_PARQUET_REQUESTS: usize = 16;

/// DGGRS ids to prefer, in order, when a collection supports several and the
/// caller didn't pin one. See `default_dggrs`.
const PREFERRED_DGGRS: &[&str] = &["IVEA7H"];

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn enc(segment: &str) -> String {
    urlencoding::encode(segment).into_owned()
}

/// The caller's own bearer token, when this request arrived over the
/// customer-facing HTTP transport (`src/http_transport.rs`) with a validated
/// `Authorization` header. `None` on stdio — nothing ever inserts
/// `http::request::Parts` into a stdio request's extensions, so this simply
/// falls through to the server's own Auth0 client-credentials in `ApiClient`.
/// This extraction is infallible by construction: `RequestContext<RoleServer>`
/// and `Extensions::get` never error, they just return `None` when absent —
/// unlike `rmcp::handler::server::tool::Extension<T>`, which *requires* `T`
/// to be present and would break every stdio call if used here instead.
fn caller_bearer_token(ctx: &RequestContext<RoleServer>) -> Option<String> {
    let parts = ctx.extensions.get::<http::request::Parts>()?;
    let header = parts.headers.get(http::header::AUTHORIZATION)?;
    let value = header.to_str().ok()?;
    Some(value.strip_prefix("Bearer ").unwrap_or(value).to_string())
}

#[derive(Clone)]
pub struct DggsServer {
    client: Arc<ApiClient>,
    // Read by the generated `ServerHandler` impl from `#[tool_handler]`;
    // dead-code analysis doesn't see through the macro expansion.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl DggsServer {
    pub fn new(client: ApiClient) -> Self {
        Self {
            client: Arc::new(client),
            tool_router: Self::tool_router(),
        }
    }

    /// Resolve a caller-supplied insight name/id to its full concept detail
    /// (`GET /concepts/{id}`), which carries the collections that serve it
    /// and the DGGS resolution to sample at. Matches numeric ids directly;
    /// otherwise looks the name up (case-insensitive, exact match preferred
    /// over substring) against `GET /concepts`.
    async fn resolve_concept(&self, insight: &str, caller_token: Option<&str>) -> Result<Value, McpError> {
        if let Ok(id) = insight.parse::<u8>() {
            return self
                .client
                .get_json(&format!("/concepts/{id}"), &[], caller_token)
                .await;
        }

        let concepts = self.client.get_json("/concepts", &[], caller_token).await?;
        let list = concepts
            .get("concepts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let needle = insight.to_lowercase();
        let field_eq = |c: &Value, field: &str, needle: &str| {
            c.get(field)
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case(needle))
        };
        let field_contains = |c: &Value, field: &str, needle: &str| {
            c.get(field)
                .and_then(Value::as_str)
                .is_some_and(|s| s.to_lowercase().contains(needle))
        };

        let exact: Vec<&Value> = list
            .iter()
            .filter(|c| field_eq(c, "name", &needle) || field_eq(c, "ui_label", &needle))
            .collect();
        let candidates = if !exact.is_empty() {
            exact
        } else {
            list.iter()
                .filter(|c| field_contains(c, "name", &needle) || field_contains(c, "ui_label", &needle))
                .collect()
        };

        let names = |items: &[&Value]| -> String {
            items
                .iter()
                .filter_map(|c| c.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(", ")
        };

        match candidates.as_slice() {
            [] => {
                let available = names(&list.iter().collect::<Vec<_>>());
                Err(McpError::invalid_params(
                    format!("no insight matches \"{insight}\". Available: {available}"),
                    None,
                ))
            }
            [single] => {
                let id = single
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| McpError::internal_error("concept is missing its id", None))?;
                self.client
                    .get_json(&format!("/concepts/{id}"), &[], caller_token)
                    .await
            }
            many => Err(McpError::invalid_params(
                format!(
                    "\"{insight}\" matches multiple insights: {}. Use a more specific name or the numeric id.",
                    names(many)
                ),
                None,
            )),
        }
    }

    /// The first DGGRS a collection supports, used as the default grid when
    /// the caller doesn't pin one.
    async fn default_dggrs(&self, collection_id: &str, caller_token: Option<&str>) -> Result<String, McpError> {
        let path = format!("/collections/{}/dggs", enc(collection_id));
        let body = self.client.get_json(&path, &[], caller_token).await?;
        let ids: Vec<&str> = body
            .get("dggrs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|d| d.get("id")).filter_map(Value::as_str).collect())
            .unwrap_or_default();

        // Every collection tends to list every DGGRS the server supports,
        // including DGGRID-backed grids (e.g. ISEA3HDGGRID, which sorts
        // first) that can 500 on zone data at finer resolutions ("No
        // regions: bbox is empty", confirmed independently of this server).
        // Prefer a known-good grid instead of whichever happens to sort
        // first; PREFERRED_DGGRS is a deliberate choice, not the default.
        PREFERRED_DGGRS
            .iter()
            .find(|preferred| ids.contains(preferred))
            .or_else(|| ids.first())
            .map(|id| id.to_string())
            .ok_or_else(|| {
                McpError::invalid_params(format!("collection \"{collection_id}\" has no DGGRS available"), None)
            })
    }

    /// How many levels the backend expands a zone by when `zone-depth` is
    /// omitted (`GET /dggs/{id}` → `default_depth`). Parquet requests omit it so the
    /// backend's cache applies; this tells us which level that lands on.
    async fn default_depth(&self, dggrs_id: &str, caller_token: Option<&str>) -> u32 {
        let path = format!("/dggs/{}", enc(dggrs_id));
        self.client
            .get_json(&path, &[], caller_token)
            .await
            .ok()
            .and_then(|body| body.get("default_depth").and_then(Value::as_u64))
            .map(|d| d as u32)
            .unwrap_or(FALLBACK_DEFAULT_DEPTH)
    }

    /// Zones covering the extent at `zone_level`: the one containing a point,
    /// or every zone intersecting a bbox.
    async fn extent_zone_ids(
        &self,
        collection_id: &str,
        dggrs_id: &str,
        extent: &ExtentArgs,
        bbox: &str,
        zone_level: u32,
        caller_token: Option<&str>,
    ) -> Result<Vec<String>, McpError> {
        match point_of(extent) {
            Some(point) => {
                self.point_zone_ids(collection_id, dggrs_id, bbox, zone_level, point, caller_token)
                    .await
            }
            None => self.list_zone_ids(collection_id, dggrs_id, bbox, zone_level, caller_token).await,
        }
    }

    /// Zone ids at `zone_level` intersecting `bbox`, within a collection's grid.
    async fn list_zone_ids(
        &self,
        collection_id: &str,
        dggrs_id: &str,
        bbox: &str,
        zone_level: u32,
        caller_token: Option<&str>,
    ) -> Result<Vec<String>, McpError> {
        let path = format!(
            "/collections/{}/dggs/{}/zones",
            enc(collection_id),
            enc(dggrs_id)
        );
        let query = vec![
            ("bbox", bbox.to_string()),
            ("zone-level", zone_level.to_string()),
        ];
        let body = self.client.get_json(&path, &query, caller_token).await?;
        Ok(body
            .get("zones")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|z| z.as_str().map(str::to_string))
            .collect())
    }

    /// The one zone at `zone_level` whose polygon contains (`lon`, `lat`).
    /// `list_zone_ids` on a point's epsilon box typically returns the 2-3
    /// neighbouring zones that box overlaps; asking for their polygons
    /// (`f=geojson`) lets a point query answer for the zone the point is
    /// actually in. Falls back to every overlapping zone if none contains it
    /// (a point exactly on a shared edge).
    async fn point_zone_ids(
        &self,
        collection_id: &str,
        dggrs_id: &str,
        bbox: &str,
        zone_level: u32,
        (lon, lat): (f64, f64),
        caller_token: Option<&str>,
    ) -> Result<Vec<String>, McpError> {
        let path = format!(
            "/collections/{}/dggs/{}/zones",
            enc(collection_id),
            enc(dggrs_id)
        );
        let query = vec![
            ("bbox", bbox.to_string()),
            ("zone-level", zone_level.to_string()),
            ("f", "geojson".to_string()),
        ];
        let body = self.client.get_json(&path, &query, caller_token).await?;
        let features = body
            .get("features")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let id_of = |f: &Value| f.get("id").and_then(Value::as_str).map(str::to_string);
        let containing = features.iter().find(|f| {
            f.pointer("/geometry/coordinates/0")
                .and_then(Value::as_array)
                .is_some_and(|ring| ring_contains(ring, lon, lat))
        });
        Ok(match containing.and_then(id_of) {
            Some(id) => vec![id],
            None => features.iter().filter_map(id_of).collect(),
        })
    }

    /// Per-property aggregation hints (`gi_dggs_stat`, `gi_unit`) for a
    /// collection, from its queryables schema.
    /// A collection's queryables schema (`GET /collections/{id}/queryables`
    /// → `properties`): its field names, types, titles and `gi_*` hints.
    async fn queryables(
        &self,
        collection_id: &str,
        caller_token: Option<&str>,
    ) -> Result<serde_json::Map<String, Value>, McpError> {
        let path = format!("/collections/{}/queryables", enc(collection_id));
        let body = self.client.get_json(&path, &[], caller_token).await?;
        Ok(body
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default())
    }

    async fn property_stats(
        &self,
        collection_id: &str,
        caller_token: Option<&str>,
    ) -> Result<HashMap<String, (Option<String>, Option<String>)>, McpError> {
        let path = format!("/collections/{}/queryables", enc(collection_id));
        let body = self.client.get_json(&path, &[], caller_token).await?;
        let props = body
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        Ok(props
            .into_iter()
            .map(|(name, schema)| {
                let stat = schema.get("gi_dggs_stat").and_then(Value::as_str).map(str::to_string);
                let unit = schema.get("gi_unit").and_then(Value::as_str).map(str::to_string);
                (name, (stat, unit))
            })
            .collect())
    }

    /// A single zone's own property values (`zone-depth=0`: one value per
    /// property, not an expansion into sub-zones).
    async fn zone_values(
        &self,
        collection_id: &str,
        dggrs_id: &str,
        zone_id: &str,
        datetime: Option<&str>,
        caller_token: Option<&str>,
    ) -> Result<HashMap<String, Value>, McpError> {
        let path = format!(
            "/collections/{}/dggs/{}/zones/{}/data",
            enc(collection_id),
            enc(dggrs_id),
            enc(zone_id)
        );
        let mut query = vec![("zone-depth", "0".to_string())];
        if let Some(dt) = datetime {
            query.push(("datetime", dt.to_string()));
        }
        let body = self.client.get_json(&path, &query, caller_token).await?;
        let values = body
            .get("values")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        Ok(values
            .into_iter()
            .filter_map(|(col, depths)| {
                let value = depths
                    .as_array()?
                    .first()?
                    .get("data")?
                    .as_array()?
                    .first()?
                    .clone();
                Some((col, value))
            })
            .collect())
    }

    /// `POST /dggs/{dggrs_id}/zones/{zone_id}/data`: fields from one or more
    /// collections, joined on this one zone, columns named `prefix__field`.
    /// Unlike `zone_values`, this is collection-independent by construction
    /// (the body names its own collections) and returns the raw DGGS-JSON
    /// body rather than a flattened map, since `get_data` doesn't reduce.
    async fn cross_collection_data(
        &self,
        dggrs_id: &str,
        zone_id: &str,
        body: &Value,
        zone_depth: Option<&str>,
        datetime: Option<&str>,
        caller_token: Option<&str>,
    ) -> Result<Value, McpError> {
        let path = format!("/dggs/{}/zones/{}/data", enc(dggrs_id), enc(zone_id));
        let mut query = vec![("zone-depth", zone_depth.unwrap_or("0").to_string())];
        if let Some(dt) = datetime {
            query.push(("datetime", dt.to_string()));
        }
        self.client.post_json(&path, &query, body, caller_token).await
    }

    /// One zone's joined row for `get_data`. A single collection with no
    /// data at this zone fails the whole cross-collection POST (the backend
    /// 500s with e.g. "Bbox of zones does not overlap COG extent"), which
    /// would also throw away every other collection's values. So on failure
    /// with several collections, retry each one alone and keep what
    /// succeeds; only the collections that really failed are reported.
    async fn zone_row(
        &self,
        dggrs_id: &str,
        zone_id: &str,
        collections: &[CollectionSelectionInput],
        zone_depth: Option<&str>,
        datetime: Option<&str>,
        caller_token: Option<&str>,
    ) -> (Option<serde_json::Map<String, Value>>, Vec<Value>) {
        let joined = self
            .cross_collection_data(dggrs_id, zone_id, &collections_body(collections), zone_depth, datetime, caller_token)
            .await;
        let err = match joined {
            Ok(body) => return (Some(flatten_zone_data(zone_id, &body)), Vec::new()),
            Err(err) => err,
        };
        if collections.len() == 1 {
            return (None, vec![zone_error(zone_id, Some(&collections[0]), &err)]);
        }

        let mut row: Option<serde_json::Map<String, Value>> = None;
        let mut errors = Vec::new();
        for collection in collections {
            let body = collections_body(std::slice::from_ref(collection));
            match self
                .cross_collection_data(dggrs_id, zone_id, &body, zone_depth, datetime, caller_token)
                .await
            {
                Ok(body) => row
                    .get_or_insert_with(serde_json::Map::new)
                    .extend(flatten_zone_data(zone_id, &body)),
                Err(err) => errors.push(zone_error(zone_id, Some(collection), &err)),
            }
        }
        (row, errors)
    }
}

/// Field names from a queryables schema, minus `zone_id` (the join key,
/// not data). What `get_data` reads when a collection's `fields` is omitted.
fn queryable_field_names(props: &serde_json::Map<String, Value>) -> Vec<String> {
    props.keys().filter(|name| *name != "zone_id").cloned().collect()
}

/// A queryables schema as a compact field list for `get_collection`: what
/// each field is and how it aggregates across zones, so the model can pick
/// fields and explain values (e.g. "Total Parcel Area", summed).
fn field_summaries(props: &serde_json::Map<String, Value>) -> Vec<Value> {
    props
        .iter()
        .filter(|(name, _)| *name != "zone_id")
        .map(|(name, schema)| {
            let mut field = serde_json::Map::new();
            field.insert("name".to_string(), Value::String(name.clone()));
            for (key, out) in [("type", "type"), ("title", "title"), ("gi_dggs_stat", "stat"), ("gi_unit", "unit")] {
                if let Some(v) = schema.get(key).filter(|v| !v.is_null()) {
                    field.insert(out.to_string(), v.clone());
                }
            }
            Value::Object(field)
        })
        .collect()
}

/// The cross-collection POST body for `collections`.
fn collections_body(collections: &[CollectionSelectionInput]) -> Value {
    serde_json::json!({
        "collections": collections.iter().map(|c| serde_json::json!({
            "id": c.collection_id,
            "fields": c.fields,
            "alias": c.alias,
            "filter": c.filter,
        })).collect::<Vec<_>>(),
    })
}

/// A failed zone/collection fetch, reported alongside the rows that did
/// succeed. Carries the backend's own `hint` when it gave one.
fn zone_error(zone_id: &str, collection: Option<&CollectionSelectionInput>, err: &McpError) -> Value {
    let hint = err.data.as_ref().and_then(|d| d.get("hint")).cloned();
    serde_json::json!({
        "zone_id": zone_id,
        "collection_id": collection.map(|c| &c.collection_id),
        "message": err.message,
        "hint": hint,
    })
}

/// Flattens one zone's DGGS-JSON (`values: { field: [{depth, data, shape}] }`
/// plus links and grid metadata repeated per zone) into a single row:
/// `{zone_id, field: value}` at one depth with one value, `field: [values]`
/// for one depth expanded into sub-zones, `field: {depth: [values]}` for
/// several depths. Most of the original bytes were wrapper, not data.
fn flatten_zone_data(zone_id: &str, body: &Value) -> serde_json::Map<String, Value> {
    let mut row = serde_json::Map::new();
    let id = body.get("zoneId").and_then(Value::as_str).unwrap_or(zone_id);
    row.insert("zone_id".to_string(), Value::String(id.to_string()));

    let Some(values) = body.get("values").and_then(Value::as_object) else {
        return row;
    };
    for (field, depths) in values {
        let depths = depths.as_array().map(Vec::as_slice).unwrap_or_default();
        let data_of = |d: &Value| d.get("data").cloned().unwrap_or(Value::Null);
        let value = match depths {
            [single] => match data_of(single) {
                Value::Array(mut data) if data.len() == 1 => data.remove(0),
                data => data,
            },
            many => Value::Object(
                many.iter()
                    .map(|d| {
                        let depth = d.get("depth").map(Value::to_string).unwrap_or_default();
                        (depth, data_of(d))
                    })
                    .collect(),
            ),
        };
        row.insert(field.clone(), value);
    }
    row
}

/// Parent level for the Parquet requests, and the `zone-depth` to send with
/// them. A request omits `zone-depth` so the backend expands its parent by
/// `default_depth` (and can serve it from cache); that only reaches
/// `children_level` when the parent sits `default_depth` levels above it.
/// For a children level shallower than that, the parent is level 0 and the
/// depth is sent explicitly.
fn parent_level(children_level: u32, default_depth: u32) -> (u32, Option<u32>) {
    if children_level >= default_depth {
        (children_level - default_depth, None)
    } else {
        (0, Some(children_level))
    }
}

/// One Parquet request covering `parent_zone_id`, for the
/// caller to fetch itself. A single collection with no alias/filter uses the
/// plain per-collection GET (which also accepts ids the cross-collection
/// endpoint rejects, like `sentinel-2-l2a`); otherwise the cross-collection
/// POST, so several collections come back as one combined view.
fn parquet_request(
    base_url: &str,
    dggrs_id: &str,
    parent_zone_id: &str,
    collections: &[CollectionSelectionInput],
    datetime: Option<&str>,
    zone_depth: Option<u32>,
) -> Value {
    let mut query = vec!["f=parquet".to_string()];
    if let Some(dt) = datetime {
        query.push(format!("datetime={}", enc(dt)));
    }
    if let Some(depth) = zone_depth {
        query.push(format!("zone-depth={depth}"));
    }
    let query = query.join("&");

    match collections {
        [only] if only.alias.is_none() && only.filter.is_none() => serde_json::json!({
            "parent_zone_id": parent_zone_id,
            "method": "GET",
            "url": format!(
                "{base_url}/collections/{}/dggs/{}/zones/{}/data?{query}",
                enc(&only.collection_id),
                enc(dggrs_id),
                enc(parent_zone_id)
            ),
        }),
        _ => serde_json::json!({
            "parent_zone_id": parent_zone_id,
            "method": "POST",
            "url": format!("{base_url}/dggs/{}/zones/{}/data?{query}", enc(dggrs_id), enc(parent_zone_id)),
            "body": collections_body(collections),
        }),
    }
}

/// Collections that failed at every one of `sampled` zones (e.g. a Canadian
/// DEM queried in Portugal, if the backend 500s rather than returning
/// nulls). Every combined Parquet request would fail the same way, so these
/// are left out of the Parquet requests and named instead.
fn failed_everywhere(collections: &[CollectionSelectionInput], errors: &[Value], sampled: usize) -> Vec<String> {
    collections
        .iter()
        .filter(|c| {
            let failures = errors
                .iter()
                .filter(|e| e["collection_id"].as_str() == Some(c.collection_id.as_str()))
                .count();
            failures == sampled
        })
        .map(|c| c.collection_id.clone())
        .collect()
}

/// Per-field summary across rows, for when there are too many zones to
/// return as rows: min/mean/max for numeric fields, the most common values
/// for anything else. Sub-zone arrays (`zone_depth` > 0) count each element.
fn summarize(rows: &[Value]) -> serde_json::Map<String, Value> {
    let mut samples: Vec<(String, Vec<Value>)> = Vec::new();
    for row in rows.iter().filter_map(Value::as_object) {
        for (field, value) in row.iter().filter(|(field, _)| *field != "zone_id") {
            let values = match value {
                Value::Array(items) => items.clone(),
                other => vec![other.clone()],
            };
            let values = values.into_iter().filter(|v| !v.is_null());
            match samples.iter_mut().find(|(name, _)| name == field) {
                Some((_, existing)) => existing.extend(values),
                None => samples.push((field.clone(), values.collect())),
            }
        }
    }

    samples
        .into_iter()
        .map(|(field, values)| {
            let nums: Option<Vec<f64>> = values.iter().map(Value::as_f64).collect();
            let summary = match nums {
                Some(nums) if !nums.is_empty() => serde_json::json!({
                    "min": reduce(Some("min"), &nums),
                    "mean": reduce(Some("mean"), &nums),
                    "max": reduce(Some("max"), &nums),
                    "count": nums.len(),
                }),
                _ => {
                    let mut counts: Vec<(Value, usize)> = Vec::new();
                    for v in &values {
                        match counts.iter_mut().find(|(existing, _)| existing == v) {
                            Some((_, count)) => *count += 1,
                            None => counts.push((v.clone(), 1)),
                        }
                    }
                    let distinct = counts.len();
                    // Stable sort: ties keep first-seen order, like `majority`.
                    counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
                    serde_json::json!({
                        "top": counts.into_iter().take(5)
                            .map(|(value, count)| serde_json::json!({ "value": value, "count": count }))
                            .collect::<Vec<_>>(),
                        "distinct": distinct,
                        "count": values.len(),
                    })
                }
            };
            (field, summary)
        })
        .collect()
}

/// Ray-casting point-in-polygon test against a GeoJSON linear ring
/// (`[[lon, lat], ...]`). Ignores antimeridian wrap, which no zone this
/// server samples at a point query's resolution straddles in practice.
fn ring_contains(ring: &[Value], lon: f64, lat: f64) -> bool {
    let points: Vec<(f64, f64)> = ring
        .iter()
        .filter_map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
        .collect();
    let mut inside = false;
    for (i, &(xi, yi)) in points.iter().enumerate() {
        let (xj, yj) = points[(i + points.len() - 1) % points.len()];
        if (yi > lat) != (yj > lat) && lon < (xj - xi) * (lat - yi) / (yj - yi) + xi {
            inside = !inside;
        }
    }
    inside
}

/// Up to `max` ids spread evenly across `ids`, instead of the first `max`.
/// The backend lists zones in id order, which is spatially clustered, so
/// truncating to the head of the list samples one corner of the area.
fn sample_evenly(ids: Vec<String>, max: usize) -> Vec<String> {
    if ids.len() <= max {
        return ids;
    }
    let step = ids.len() as f64 / max as f64;
    (0..max).map(|i| ids[(i as f64 * step) as usize].clone()).collect()
}

fn reduce(stat: Option<&str>, values: &[f64]) -> f64 {
    match stat {
        Some("sum") => values.iter().sum(),
        Some("min") => values.iter().cloned().fold(f64::INFINITY, f64::min),
        Some("max") => values.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        Some("count") => values.len() as f64,
        // "mean" and anything unrecognized both default to a plain average.
        _ => values.iter().sum::<f64>() / values.len() as f64,
    }
}

/// Most frequent value across sampled zones, for non-numeric properties
/// (e.g. `gi_dggs_stat: "majority"` on a categorical column like a country
/// name). Ties break on whichever value was seen first.
fn majority(values: &[Value]) -> Value {
    let mut counts: Vec<(Value, usize)> = Vec::new();
    for v in values {
        match counts.iter_mut().find(|(existing, _)| existing == v) {
            Some((_, count)) => *count += 1,
            None => counts.push((v.clone(), 1)),
        }
    }
    // Not `max_by_key`: it returns the *last* of equal maxima, which would
    // break ties on the value seen last instead.
    let mut best: Option<(Value, usize)> = None;
    for (value, count) in counts {
        if best.as_ref().is_none_or(|(_, best_count)| count > *best_count) {
            best = Some((value, count));
        }
    }
    best.map(|(value, _)| value).unwrap_or(Value::Null)
}

/// Extent shared by `get_insight` and `get_data`: either a point (`lon`+`lat`,
/// expanded to a small bbox) or an explicit area (all four `min_*`/`max_*`
/// fields).
#[derive(Debug, Deserialize, JsonSchema)]
struct ExtentArgs {
    #[schemars(description = "Point longitude, WGS84 degrees. Provide together with `lat` for a point query; use the bbox fields instead for an area.")]
    lon: Option<f64>,
    #[schemars(description = "Point latitude, WGS84 degrees. Provide together with `lon`.")]
    lat: Option<f64>,

    #[schemars(description = "Bounding box min longitude (west). Provide all four bbox fields together for an area query, instead of lon/lat.")]
    min_lon: Option<f64>,
    #[schemars(description = "Bounding box min latitude (south).")]
    min_lat: Option<f64>,
    #[schemars(description = "Bounding box max longitude (east).")]
    max_lon: Option<f64>,
    #[schemars(description = "Bounding box max latitude (north).")]
    max_lat: Option<f64>,
}

fn build_bbox(extent: &ExtentArgs) -> Result<String, McpError> {
    match (extent.min_lon, extent.min_lat, extent.max_lon, extent.max_lat) {
        (Some(a), Some(b), Some(c), Some(d)) => Ok(format!("{a},{b},{c},{d}")),
        (None, None, None, None) => match (extent.lon, extent.lat) {
            (Some(lon), Some(lat)) => Ok(format!(
                "{},{},{},{}",
                lon - POINT_EPSILON_DEG,
                lat - POINT_EPSILON_DEG,
                lon + POINT_EPSILON_DEG,
                lat + POINT_EPSILON_DEG
            )),
            _ => Err(McpError::invalid_params(
                "provide either lon+lat for a point, or all four of min_lon/min_lat/max_lon/max_lat for an area",
                None,
            )),
        },
        _ => Err(McpError::invalid_params(
            "bbox fields (min_lon/min_lat/max_lon/max_lat) must be given all four together",
            None,
        )),
    }
}

/// `Some((lon, lat))` when the extent is a point rather than a bbox.
fn point_of(extent: &ExtentArgs) -> Option<(f64, f64)> {
    match (extent.min_lon, extent.min_lat, extent.max_lon, extent.max_lat) {
        (None, None, None, None) => Some((extent.lon?, extent.lat?)),
        _ => None,
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetInsightRequest {
    #[schemars(description = "Which insight/concept to compute, e.g. \"ndvi\", \"burn_scar_analysis\". Matches by numeric id or by name/label (case-insensitive). See list_insights for what's available.")]
    insight: String,

    #[serde(flatten)]
    extent: ExtentArgs,

    #[schemars(description = "Override which collection to read. Omit to use the insight's default (first) linked collection.")]
    collection_id: Option<String>,
    #[schemars(description = "Override which DGGRS (grid) to compute on. Omit to use the collection's default grid.")]
    dggrs_id: Option<String>,
    #[schemars(description = "Override the DGGS zone level (resolution) to sample at. Omit to use the insight's configured resolution.")]
    zone_level: Option<u32>,
    #[schemars(description = "RFC 3339 instant or interval to filter temporal data (e.g. satellite imagery), such as \"2024-06-01T00:00:00Z\" or \"2024-06-01T00:00:00Z/2024-06-30T23:59:59Z\".")]
    datetime: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CollectionIdRequest {
    #[schemars(description = "Collection identifier, as returned by list_collections.")]
    collection_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct CollectionSelectionInput {
    #[schemars(description = "Collection id to read from, as returned by list_collections.")]
    collection_id: String,
    #[schemars(description = "Fields to return from this collection, as listed under `fields` by get_collection. Omit to return every field of the collection.")]
    #[serde(default)]
    fields: Vec<String>,
    #[schemars(description = "Rename this collection's output column prefix (columns come back named \"prefix__field\"). Needed to select the same collection twice under different filters.")]
    alias: Option<String>,
    #[schemars(description = "CQL2-text predicate restricting which of this collection's source rows contribute, e.g. \"length > 500\". Only supported for DuckDB-backed collections.")]
    filter: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetDataRequest {
    #[serde(flatten)]
    extent: ExtentArgs,

    #[schemars(description = "One or more collections to read, each with the fields to return. Fields from different collections are joined on the same DGGS zone in one call; output columns are named \"prefix__field\" (prefix = alias or collection_id).")]
    collections: Vec<CollectionSelectionInput>,

    #[schemars(description = "DGGRS grid to compute on. Omit to use the first listed collection's default grid.")]
    dggrs_id: Option<String>,
    #[schemars(description = "DGGS zone level (resolution) of the data: rows and summary are at this level, and the Parquet requests expand to it from parent zones the grid's default depth above. Defaults to 9 if omitted; override for coarser/finer data.")]
    zone_level: Option<u32>,
    #[schemars(description = "Relative depth(s) below the resolved zone to expand into, for finer-than-zone_level detail within it: a single value \"5\", a range \"1-8\", or a list \"1,3,7\". Omit for one value per zone (zone-depth=0) — this can return a lot of data if set too deep.")]
    zone_depth: Option<String>,
    #[schemars(description = "RFC 3339 instant or interval to filter temporal data (e.g. satellite imagery).")]
    datetime: Option<String>,
}

#[tool_router]
impl DggsServer {
    #[tool(
        description = "List the insights/concepts (e.g. NDVI, burn scar analysis) this server can compute, with a short description of each. Use get_insight to actually compute one for a location."
    )]
    async fn list_insights(&self, ctx: RequestContext<RoleServer>) -> Result<String, McpError> {
        let caller_token = caller_bearer_token(&ctx);
        let body = self.client.get_json("/concepts", &[], caller_token.as_deref()).await?;
        Ok(pretty(&body))
    }

    #[tool(
        description = "Get an insight's value for a location or area — the main data-access tool. Pass an insight (e.g. \"ndvi\", \"burn_scar_analysis\"; see list_insights) and either a point (lon/lat) or an area (min_lon/min_lat/max_lon/max_lat). This resolves the insight's data source and DGGS resolution, samples the zone(s) covering the location, and returns one aggregated value per property using that property's own configured statistic (mean, max, ...) — you get a location, not a list of DGGS zones to interpret yourself."
    )]
    async fn get_insight(
        &self,
        Parameters(req): Parameters<GetInsightRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, McpError> {
        let caller_token = caller_bearer_token(&ctx);
        let caller_token = caller_token.as_deref();

        let concept = self.resolve_concept(&req.insight, caller_token).await?;
        let concept_id = concept.get("id").and_then(Value::as_u64);
        let concept_name = concept
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&req.insight)
            .to_string();

        let collection_id = match &req.collection_id {
            Some(cid) => cid.clone(),
            None => concept
                .get("collections")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    McpError::invalid_params(
                        format!("insight \"{concept_name}\" has no data collections configured; pass collection_id explicitly"),
                        None,
                    )
                })?,
        };

        let zone_level = match req.zone_level {
            Some(z) => z,
            None => concept
                .get("processing")
                .and_then(Value::as_array)
                .and_then(|p| p.first())
                .and_then(|p| p.get("dggs_refinement_level"))
                .and_then(Value::as_u64)
                .map(|v| v as u32)
                .ok_or_else(|| {
                    McpError::invalid_params(
                        format!("insight \"{concept_name}\" has no configured resolution; pass zone_level explicitly"),
                        None,
                    )
                })?,
        };

        let dggrs_id = match &req.dggrs_id {
            Some(d) => d.clone(),
            None => self.default_dggrs(&collection_id, caller_token).await?,
        };

        let bbox = build_bbox(&req.extent)?;

        let mut zone_ids = self
            .list_zone_ids(&collection_id, &dggrs_id, &bbox, zone_level, caller_token)
            .await?;
        if zone_ids.is_empty() {
            return Err(McpError::invalid_params(
                format!("no zones of collection \"{collection_id}\" intersect that location at zone level {zone_level}"),
                None,
            ));
        }
        let zones_matched = zone_ids.len();
        let zones_truncated = zones_matched > MAX_ZONES_PER_QUERY;
        zone_ids.truncate(MAX_ZONES_PER_QUERY);

        let stats = self.property_stats(&collection_id, caller_token).await?;

        let fetches = zone_ids
            .iter()
            .map(|zone_id| self.zone_values(&collection_id, &dggrs_id, zone_id, req.datetime.as_deref(), caller_token));
        let results = join_all(fetches).await;

        // Each property is collected raw first, then reduced according to
        // its own type: numeric properties average/sum/min/max per
        // `gi_dggs_stat`, categorical ones (e.g. `gi_dggs_stat: "majority"`
        // on a country name) take the most common value across zones.
        let mut per_property: HashMap<String, Vec<Value>> = HashMap::new();
        for result in results {
            for (col, value) in result? {
                per_property.entry(col).or_default().push(value);
            }
        }

        let mut values = serde_json::Map::new();
        for (col, samples) in &per_property {
            let (stat, unit) = stats.get(col).cloned().unwrap_or((None, None));
            let all_numeric = samples.iter().all(|v| v.as_f64().is_some());
            let (value, stat_name) = if all_numeric {
                let nums: Vec<f64> = samples.iter().filter_map(Value::as_f64).collect();
                (
                    serde_json::json!(reduce(stat.as_deref(), &nums)),
                    stat.unwrap_or_else(|| "mean".to_string()),
                )
            } else {
                (majority(samples), stat.unwrap_or_else(|| "majority".to_string()))
            };
            values.insert(
                col.clone(),
                serde_json::json!({
                    "value": value,
                    "stat": stat_name,
                    "unit": unit,
                    "sampled_zones": samples.len(),
                }),
            );
        }

        let result = serde_json::json!({
            "insight": { "id": concept_id, "name": concept_name },
            "collection_id": collection_id,
            "dggrs_id": dggrs_id,
            "zone_level": zone_level,
            "zones_sampled": zone_ids.len(),
            "zones_matched": zones_matched,
            "zones_truncated": zones_truncated,
            "values": values,
        });
        Ok(pretty(&result))
    }

    #[tool(
        description = "Get raw field values from one or more collections at a location or area — the low-level counterpart to get_insight. Use this when you already know which collection(s) and field(s) you want (optionally joining several collections' fields together, e.g. elevation plus country name, in one call), rather than going through a named insight. Unlike get_insight this does not aggregate down to one number: for up to 20 matched zones it returns one flat row per zone (`zone_id` plus one column per field, named \"prefix__field\"); for more, a per-field `summary` (min/mean/max, or the most common values) instead. A point returns the single zone containing it. It also returns `parquet_requests`: requests (method, url, body) covering the whole area, one per parent zone, for a map to fetch and draw directly — the full data, which never needs to pass through you. Each collection's `fields` can be omitted to return all of its fields (see get_collection for what they are). If a collection has no data at a zone, the other collections' values are still returned and the failure is listed under `errors`. Works without Auth0 configured."
    )]
    async fn get_data(
        &self,
        Parameters(req): Parameters<GetDataRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, McpError> {
        let caller_token = caller_bearer_token(&ctx);
        let caller_token = caller_token.as_deref();

        if req.collections.is_empty() {
            return Err(McpError::invalid_params(
                "`collections` must list at least one collection",
                None,
            ));
        }

        // The backend has no "all fields" wildcard, so a collection with no
        // `fields` gets every field in its queryables schema, as explore's
        // own data view does.
        let mut collections = req.collections.clone();
        for c in collections.iter_mut().filter(|c| c.fields.is_empty()) {
            c.fields = queryable_field_names(&self.queryables(&c.collection_id, caller_token).await?);
            if c.fields.is_empty() {
                return Err(McpError::invalid_params(
                    format!("collection \"{}\" has no queryable fields", c.collection_id),
                    None,
                ));
            }
        }

        let dggrs_id = match &req.dggrs_id {
            Some(d) => d.clone(),
            None => {
                self.default_dggrs(&collections[0].collection_id, caller_token)
                    .await?
            }
        };
        // TODO: derive from each collection's resolution once collections
        // record one (`gi_resolution_m`); until then, the caller's level or 9.
        let children_level = req.zone_level.unwrap_or(DEFAULT_ZONE_LEVEL);
        let bbox = build_bbox(&req.extent)?;

        // DGGS zones are collection-independent (they depend only on the
        // DGGRS, the level and the extent), so any one of the requested
        // collections can anchor the zone lookups; the first is as good as
        // any.
        let anchor = &collections[0].collection_id;
        let (zone_ids, default_depth) = futures::join!(
            self.extent_zone_ids(anchor, &dggrs_id, &req.extent, &bbox, children_level, caller_token),
            self.default_depth(&dggrs_id, caller_token),
        );
        let zone_ids = zone_ids?;
        if zone_ids.is_empty() {
            return Err(McpError::invalid_params(
                format!("no zones intersect that location at zone level {children_level}"),
                None,
            ));
        }
        let zones_matched = zone_ids.len();
        let as_rows = zones_matched <= MAX_ROWS;
        let zone_ids = sample_evenly(zone_ids, MAX_ZONES_PER_QUERY);

        let (parent_level, request_depth) = parent_level(children_level, default_depth);
        let parent_ids = self.extent_zone_ids(anchor, &dggrs_id, &req.extent, &bbox, parent_level, caller_token);

        let fetches = join_all(zone_ids.iter().map(|zone_id| {
            self.zone_row(
                &dggrs_id,
                zone_id,
                &collections,
                req.zone_depth.as_deref(),
                req.datetime.as_deref(),
                caller_token,
            )
        }));
        let (fetched, parent_ids) = futures::join!(fetches, parent_ids);
        let parent_ids = parent_ids?;

        let mut rows = Vec::new();
        let mut errors = Vec::new();
        for (row, zone_errors) in fetched {
            rows.extend(row.map(Value::Object));
            errors.extend(zone_errors);
        }

        if rows.is_empty() {
            return Err(McpError::invalid_params(
                "no data for any sampled zone; see `errors` for each failure",
                Some(serde_json::json!({ "errors": errors })),
            ));
        }

        let excluded = failed_everywhere(&collections, &errors, zone_ids.len());
        let request_collections: Vec<CollectionSelectionInput> = collections
            .iter()
            .filter(|c| !excluded.contains(&c.collection_id))
            .cloned()
            .collect();

        let requests_truncated = parent_ids.len() > MAX_PARQUET_REQUESTS;
        let parquet_requests: Vec<Value> = parent_ids
            .iter()
            .take(MAX_PARQUET_REQUESTS)
            .map(|parent| {
                parquet_request(
                    self.client.base_url(),
                    &dggrs_id,
                    parent,
                    &request_collections,
                    req.datetime.as_deref(),
                    request_depth,
                )
            })
            .collect();

        let mut result = serde_json::json!({
            "dggrs_id": dggrs_id,
            "children_level": children_level,
            "parent_level": parent_level,
            "zones_matched": zones_matched,
        });
        if as_rows {
            result["rows"] = Value::Array(rows);
        } else {
            result["summary"] = Value::Object(summarize(&rows));
            result["summary_sampled_zones"] = serde_json::json!(rows.len());
        }
        result["parquet_requests"] = Value::Array(parquet_requests);
        if !excluded.is_empty() {
            result["excluded_collections"] = serde_json::json!(excluded);
        }
        if requests_truncated {
            result["requests_truncated"] = Value::Bool(true);
            result["parent_zones_matched"] = serde_json::json!(parent_ids.len());
        }
        if !errors.is_empty() {
            result["errors"] = Value::Array(errors);
        }
        Ok(pretty(&result))
    }

    #[tool(
        description = "List the data collections available on this server (advanced/secondary — most questions are better answered with get_insight, which picks the right collection for an insight automatically)."
    )]
    async fn list_collections(&self, ctx: RequestContext<RoleServer>) -> Result<String, McpError> {
        let caller_token = caller_bearer_token(&ctx);
        let body = self.client.get_json("/collections", &[], caller_token.as_deref()).await?;
        Ok(pretty(&body))
    }

    #[tool(
        description = "Describe a single data collection: its extent, temporal range, links, and `fields` — each field's name, type, title, unit and how it aggregates across zones (`stat`). Use the field names with get_data, or omit them there to get every field."
    )]
    async fn get_collection(
        &self,
        Parameters(req): Parameters<CollectionIdRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, McpError> {
        let caller_token = caller_bearer_token(&ctx);
        let caller_token = caller_token.as_deref();
        let path = format!("/collections/{}", enc(&req.collection_id));
        let (collection, queryables) = futures::join!(
            self.client.get_json(&path, &[], caller_token),
            self.queryables(&req.collection_id, caller_token),
        );
        let mut body = collection?;
        // The fields are an addition: a collection whose queryables fail to
        // load is still described.
        if let Ok(props) = queryables {
            body["fields"] = Value::Array(field_summaries(&props));
        }
        Ok(pretty(&body))
    }

    #[tool(
        description = "How many HTTP requests this server has sent to ogc-dggs-api so far this session. Useful for understanding real load: a single get_insight/get_data call can fan out to many backend requests (one per sampled zone, plus lookups), not just one."
    )]
    async fn get_call_stats(&self) -> String {
        pretty(&serde_json::json!({
            "requests_to_ogc_dggs_api": self.client.request_count(),
        }))
    }
}

#[tool_handler]
impl ServerHandler for DggsServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Tools for geoinsight's ogc-dggs-api. Start with list_insights to see available \
             concepts (NDVI, burn scar analysis, ...), then get_insight(insight, lon/lat or bbox) \
             to get one aggregated value for a location or area — it handles DGGS zone \
             resolution and cross-zone aggregation internally. If you already know which \
             collection(s)/field(s) you want instead of a named insight, use get_data \
             (optionally joining fields from several collections in one call) for the raw, \
             unaggregated per-zone values. list_collections/get_collection are a secondary \
             path for browsing raw dataset metadata. In stdio mode, list_insights and \
             get_insight need this server's own Auth0 client-credentials configured (they call \
             the bearer-token-gated /concepts route); get_data, list_collections and \
             get_collection work without it. In HTTP mode, every request needs its own bearer \
             token instead, which is forwarded to ogc-dggs-api as-is. get_call_stats reports how \
             many backend HTTP requests this session has made in total, since a single \
             get_insight/get_data call can fan out to many.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(
        lon: Option<f64>,
        lat: Option<f64>,
        min_lon: Option<f64>,
        min_lat: Option<f64>,
        max_lon: Option<f64>,
        max_lat: Option<f64>,
    ) -> ExtentArgs {
        ExtentArgs {
            lon,
            lat,
            min_lon,
            min_lat,
            max_lon,
            max_lat,
        }
    }

    #[test]
    fn point_expands_to_a_tiny_bbox_not_a_degenerate_one() {
        let bbox = build_bbox(&req(Some(-9.14), Some(38.72), None, None, None, None)).unwrap();
        let parts: Vec<f64> = bbox.split(',').map(|s| s.parse().unwrap()).collect();
        assert_eq!(parts.len(), 4);
        assert!(parts[0] < parts[2], "min_lon must be < max_lon, not degenerate");
        assert!(parts[1] < parts[3], "min_lat must be < max_lat, not degenerate");
    }

    #[test]
    fn explicit_bbox_passes_through_unchanged() {
        let bbox = build_bbox(&req(None, None, Some(1.0), Some(2.0), Some(3.0), Some(4.0))).unwrap();
        assert_eq!(bbox, "1,2,3,4");
    }

    #[test]
    fn neither_point_nor_bbox_is_rejected() {
        assert!(build_bbox(&req(None, None, None, None, None, None)).is_err());
    }

    #[test]
    fn partial_bbox_is_rejected() {
        assert!(build_bbox(&req(None, None, Some(1.0), Some(2.0), None, None)).is_err());
    }

    #[test]
    fn reduce_uses_the_configured_statistic() {
        let values = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(reduce(Some("mean"), &values), 2.5);
        assert_eq!(reduce(Some("sum"), &values), 10.0);
        assert_eq!(reduce(Some("min"), &values), 1.0);
        assert_eq!(reduce(Some("max"), &values), 4.0);
        assert_eq!(reduce(Some("count"), &values), 4.0);
        // Unknown or absent stat hints default to a plain average.
        assert_eq!(reduce(None, &values), 2.5);
        assert_eq!(reduce(Some("bogus"), &values), 2.5);
    }

    #[test]
    fn majority_picks_the_most_common_value() {
        let values = vec![
            Value::String("CA".to_string()),
            Value::String("CA".to_string()),
            Value::String("US".to_string()),
        ];
        assert_eq!(majority(&values), Value::String("CA".to_string()));
    }

    #[test]
    fn majority_tie_breaks_on_first_seen() {
        let values = vec![
            Value::String("CA".to_string()),
            Value::String("US".to_string()),
            Value::String("US".to_string()),
            Value::String("CA".to_string()),
        ];
        assert_eq!(majority(&values), Value::String("CA".to_string()));
    }

    #[test]
    fn flatten_single_value_per_field_to_scalars() {
        let body = serde_json::json!({
            "zoneId": "J0-2D3068-B",
            "dggrs": "[ogc-dggrs:IVEA7H]",
            "links": [{"href": "https://example.invalid", "rel": "source"}],
            "depths": [0],
            "values": {
                "dem__band1_mean": [{"data": [1424], "depth": 0, "shape": {"count": 1, "subZones": 1}}],
                "country__country_name": [{"data": ["Canada"], "depth": 0, "shape": {"count": 1, "subZones": 1}}],
            },
        });
        let row = flatten_zone_data("fallback", &body);
        assert_eq!(
            Value::Object(row),
            serde_json::json!({"zone_id": "J0-2D3068-B", "dem__band1_mean": 1424, "country__country_name": "Canada"})
        );
    }

    #[test]
    fn flatten_keeps_sub_zone_arrays_and_multiple_depths() {
        let body = serde_json::json!({
            "values": {
                "a": [{"data": [1, 2, 3], "depth": 1}],
                "b": [{"data": [1], "depth": 0}, {"data": [1, 2], "depth": 1}],
            },
        });
        let row = flatten_zone_data("Z1", &body);
        assert_eq!(row["zone_id"], "Z1");
        assert_eq!(row["a"], serde_json::json!([1, 2, 3]));
        assert_eq!(row["b"], serde_json::json!({"0": [1], "1": [1, 2]}));
    }

    #[test]
    fn ring_contains_distinguishes_inside_from_outside() {
        let square: Vec<Value> = serde_json::from_str("[[0,0],[2,0],[2,2],[0,2],[0,0]]").unwrap();
        assert!(ring_contains(&square, 1.0, 1.0));
        assert!(!ring_contains(&square, 3.0, 1.0));
        assert!(!ring_contains(&square, 1.0, -0.5));
    }

    #[test]
    fn sample_evenly_spreads_across_the_list() {
        let ids: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        assert_eq!(sample_evenly(ids.clone(), 20), ids);
        assert_eq!(sample_evenly(ids, 5), vec!["0", "2", "4", "6", "8"]);
    }

    #[test]
    fn point_of_only_for_points() {
        assert_eq!(point_of(&req(Some(1.0), Some(2.0), None, None, None, None)), Some((1.0, 2.0)));
        assert_eq!(point_of(&req(None, None, Some(1.0), Some(2.0), Some(3.0), Some(4.0))), None);
    }

    fn selection(id: &str, fields: &[&str]) -> CollectionSelectionInput {
        CollectionSelectionInput {
            collection_id: id.to_string(),
            fields: fields.iter().map(|f| f.to_string()).collect(),
            alias: None,
            filter: None,
        }
    }

    #[test]
    fn parent_level_sits_default_depth_above_children() {
        assert_eq!(parent_level(9, 6), (3, None));
        assert_eq!(parent_level(6, 6), (0, None));
        // Shallower than the default depth: parent is level 0, depth explicit.
        assert_eq!(parent_level(4, 6), (0, Some(4)));
    }

    #[test]
    fn single_collection_request_is_a_plain_get() {
        let l = parquet_request("https://api.example", "IVEA7H", "C0-1A", &[selection("sentinel-2-l2a", &["B04"])], None, None);
        assert_eq!(l["method"], "GET");
        assert_eq!(
            l["url"],
            "https://api.example/collections/sentinel-2-l2a/dggs/IVEA7H/zones/C0-1A/data?f=parquet"
        );
        assert!(l.get("body").is_none());
    }

    #[test]
    fn several_collections_request_is_one_combined_post() {
        let cols = [selection("canada_dem", &["band1_mean"]), selection("overture_admin0", &["country_name"])];
        let l = parquet_request("https://api.example", "IVEA7H", "C0-1A", &cols, Some("2024-06-01T00:00:00Z"), Some(4));
        assert_eq!(l["method"], "POST");
        assert_eq!(
            l["url"],
            "https://api.example/dggs/IVEA7H/zones/C0-1A/data?f=parquet&datetime=2024-06-01T00%3A00%3A00Z&zone-depth=4"
        );
        assert_eq!(l["body"]["collections"][1]["id"], "overture_admin0");
    }

    #[test]
    fn only_collections_failing_at_every_zone_are_excluded() {
        let cols = [selection("canada_dem", &["band1_mean"]), selection("overture_admin0", &["country_name"])];
        let err = |zone: &str, id: &str| serde_json::json!({"zone_id": zone, "collection_id": id});
        let everywhere = [err("a", "canada_dem"), err("b", "canada_dem")];
        assert_eq!(failed_everywhere(&cols, &everywhere, 2), vec!["canada_dem"]);
        let somewhere = [err("a", "canada_dem")];
        assert!(failed_everywhere(&cols, &somewhere, 2).is_empty());
    }

    #[test]
    fn queryables_become_field_names_and_summaries() {
        let props: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{
                "zone_id": {"type": "string"},
                "parcel_area_m2": {"type": "number", "title": "Total Parcel Area", "gi_dggs_stat": "sum", "gi_unit": null},
                "parcel_section": {"type": "string", "title": "Cadastral section", "gi_dggs_stat": "majority"}
            }"#,
        )
        .unwrap();
        assert_eq!(queryable_field_names(&props), vec!["parcel_area_m2", "parcel_section"]);
        assert_eq!(
            Value::Array(field_summaries(&props)),
            serde_json::json!([
                {"name": "parcel_area_m2", "type": "number", "title": "Total Parcel Area", "stat": "sum"},
                {"name": "parcel_section", "type": "string", "title": "Cadastral section", "stat": "majority"}
            ])
        );
    }

    #[test]
    fn omitted_fields_deserialize_as_empty() {
        let c: CollectionSelectionInput = serde_json::from_str(r#"{"collection_id": "cadastre_parcelles"}"#).unwrap();
        assert!(c.fields.is_empty());
    }

    #[test]
    fn summarize_numbers_and_categories() {
        let rows = vec![
            serde_json::json!({"zone_id": "a", "elev": 10, "country": "CA", "sub": [1, 2]}),
            serde_json::json!({"zone_id": "b", "elev": 30, "country": "CA", "sub": [3]}),
            serde_json::json!({"zone_id": "c", "elev": null, "country": "US", "sub": [6]}),
        ];
        let s = summarize(&rows);
        assert!(!s.contains_key("zone_id"));
        assert_eq!(s["elev"], serde_json::json!({"min": 10.0, "mean": 20.0, "max": 30.0, "count": 2}));
        assert_eq!(s["sub"]["mean"], 3.0);
        assert_eq!(s["country"]["top"][0], serde_json::json!({"value": "CA", "count": 2}));
        assert_eq!(s["country"]["distinct"], 2);
    }

    #[test]
    fn majority_of_empty_is_null() {
        assert_eq!(majority(&[]), Value::Null);
    }
}
