use std::collections::HashMap;

use futures::future::join_all;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::aggregate::{majority, reduce};
use super::extent::{ExtentArgs, build_bbox};
use super::{DggsServer, MAX_ZONES_PER_QUERY, caller_bearer_token, enc, pretty};

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

impl DggsServer {
    /// Resolve a caller-supplied insight name/id to its full concept detail
    /// (`GET /concepts/{id}`), which carries the collections that serve it
    /// and the DGGS resolution to sample at. Matches numeric ids directly;
    /// otherwise looks the name up (case-insensitive, exact match preferred
    /// over substring) against `GET /concepts`.
    pub(super) async fn resolve_concept(&self, insight: &str, caller_token: Option<&str>) -> Result<Value, McpError> {
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

    pub(super) async fn property_stats(
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
    pub(super) async fn zone_values(
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
}


#[tool_router(router = insight_router, vis = "pub(super)")]
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
}
