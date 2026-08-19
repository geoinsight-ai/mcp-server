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

    /// Per-property aggregation hints (`gi_dggs_stat`, `gi_unit`) for a
    /// collection, from its queryables schema.
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
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(value, _)| value)
        .unwrap_or(Value::Null)
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
    #[schemars(description = "Fields to return from this collection's schema (see get_collection, or its queryables link). Required, non-empty — there is no wildcard.")]
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
    #[schemars(description = "DGGS zone level (resolution) to resolve the location to. Defaults to 9 if omitted; override for coarser/finer sampling.")]
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
        description = "Get raw field values from one or more collections at a location or area — the low-level counterpart to get_insight. Use this when you already know which collection(s) and field(s) you want (optionally joining several collections' fields together, e.g. elevation plus country name, in one call), rather than going through a named insight. Unlike get_insight this does not aggregate down to one number: it returns the backend's own per-zone DGGS-JSON data, one entry per sampled zone. Works without Auth0 configured."
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

        let dggrs_id = match &req.dggrs_id {
            Some(d) => d.clone(),
            None => {
                self.default_dggrs(&req.collections[0].collection_id, caller_token)
                    .await?
            }
        };
        let zone_level = req.zone_level.unwrap_or(DEFAULT_ZONE_LEVEL);
        let bbox = build_bbox(&req.extent)?;

        // DGGS child zones are collection-independent (they depend only on
        // the DGGRS, the parent zone and the depth), so any one of the
        // requested collections can anchor the zone lookup; the first is as
        // good as any.
        let mut zone_ids = self
            .list_zone_ids(&req.collections[0].collection_id, &dggrs_id, &bbox, zone_level, caller_token)
            .await?;
        if zone_ids.is_empty() {
            return Err(McpError::invalid_params(
                format!("no zones intersect that location at zone level {zone_level}"),
                None,
            ));
        }
        let zones_matched = zone_ids.len();
        let zones_truncated = zones_matched > MAX_ZONES_PER_QUERY;
        zone_ids.truncate(MAX_ZONES_PER_QUERY);

        let body = serde_json::json!({
            "collections": req.collections.iter().map(|c| serde_json::json!({
                "id": c.collection_id,
                "fields": c.fields,
                "alias": c.alias,
                "filter": c.filter,
            })).collect::<Vec<_>>(),
        });

        let fetches = zone_ids.iter().map(|zone_id| {
            self.cross_collection_data(
                &dggrs_id,
                zone_id,
                &body,
                req.zone_depth.as_deref(),
                req.datetime.as_deref(),
                caller_token,
            )
        });
        let results: Vec<Value> = join_all(fetches).await.into_iter().collect::<Result<_, _>>()?;

        let result = serde_json::json!({
            "dggrs_id": dggrs_id,
            "zone_level": zone_level,
            "zones_sampled": results.len(),
            "zones_matched": zones_matched,
            "zones_truncated": zones_truncated,
            "results": results,
        });
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
        description = "Describe a single data collection: its extent, temporal range, and links (advanced/secondary — use get_insight for concept-driven queries)."
    )]
    async fn get_collection(
        &self,
        Parameters(req): Parameters<CollectionIdRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, McpError> {
        let caller_token = caller_bearer_token(&ctx);
        let path = format!("/collections/{}", enc(&req.collection_id));
        let body = self.client.get_json(&path, &[], caller_token.as_deref()).await?;
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
    fn majority_of_empty_is_null() {
        assert_eq!(majority(&[]), Value::Null);
    }
}
