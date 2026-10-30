use std::collections::HashMap;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::server::dggs::aggregate::{majority, reduce};
use crate::server::dggs::extent::{ExtentArgs, build_bbox};
use crate::server::dggs::zones::sample_evenly;
use crate::server::{DggsServer, MAX_ZONES_PER_QUERY, bounded, caller_bearer_token, enc, pretty, zone_error};

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

/// Each sampled zone's property values, grouped by property, plus an
/// `errors` entry for every zone that failed — so one bad zone costs its own
/// samples, not the whole answer.
fn collect_zone_values(
    zone_ids: &[String],
    results: Vec<Result<HashMap<String, Value>, McpError>>,
    collection_id: &str,
) -> (HashMap<String, Vec<Value>>, Vec<Value>) {
    let mut per_property: HashMap<String, Vec<Value>> = HashMap::new();
    let mut errors = Vec::new();
    for (zone_id, result) in zone_ids.iter().zip(results) {
        match result {
            Ok(zone) => {
                for (col, value) in zone {
                    per_property.entry(col).or_default().push(value);
                }
            }
            Err(err) => errors.push(zone_error(zone_id, Some(collection_id), &err)),
        }
    }
    (per_property, errors)
}

/// One property's samples reduced to `{value, stat, sampled_zones}`. Zones
/// with no value (`null`, e.g. a cloud-masked NDVI pixel) are left out
/// first: otherwise one null among numbers would make the property look
/// categorical and get a majority vote over floats instead of its
/// configured statistic. `sampled_zones` counts only zones with a value.
fn aggregate_property(samples: &[Value], stat: Option<String>) -> Value {
    let present: Vec<Value> = samples.iter().filter(|v| !v.is_null()).cloned().collect();
    let nums: Option<Vec<f64>> = present.iter().map(Value::as_f64).collect();
    let (value, stat_name) = match nums {
        Some(nums) if !nums.is_empty() => (
            serde_json::json!(reduce(stat.as_deref(), &nums)),
            stat.unwrap_or_else(|| "mean".to_string()),
        ),
        _ => (majority(&present), stat.unwrap_or_else(|| "majority".to_string())),
    };
    serde_json::json!({
        "value": value,
        "stat": stat_name,
        "sampled_zones": present.len(),
    })
}

#[tool_router(router = insights_router, vis = "pub(crate)")]
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
        description = "Get an insight's value for a location or area — the main data-access tool. Pass an insight (e.g. \"ndvi\", \"burn_scar_analysis\"; see list_insights) and either a point (lon/lat) or an area (min_lon/min_lat/max_lon/max_lat). This resolves the insight's data source and DGGS resolution, samples the zone(s) covering the location (the single zone containing a point; up to 64 zones spread evenly across an area), and returns one aggregated value per property using that property's own configured statistic (mean, max, ...) — you get a location, not a list of DGGS zones to interpret yourself. Zones that fail are listed under `errors`; the value comes from the zones that succeeded."
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

        // A point resolves to the one zone containing it, not the 2-3
        // neighbours its small search box overlaps (see `extent_zone_ids`).
        let zone_ids = self
            .extent_zone_ids(&collection_id, &dggrs_id, &req.extent, &bbox, zone_level, caller_token)
            .await?;
        if zone_ids.is_empty() {
            return Err(McpError::invalid_params(
                format!("no zones of collection \"{collection_id}\" intersect that location at zone level {zone_level}"),
                None,
            ));
        }
        let zones_matched = zone_ids.len();
        let zones_truncated = zones_matched > MAX_ZONES_PER_QUERY;
        let zone_ids = sample_evenly(zone_ids, MAX_ZONES_PER_QUERY);

        let stats = self.property_stats(&collection_id, caller_token).await?;

        let fetches = zone_ids
            .iter()
            .map(|zone_id| self.zone_values(&collection_id, &dggrs_id, zone_id, req.datetime.as_deref(), caller_token));
        let results = bounded(fetches).await;

        // Each property is collected raw first, then reduced according to
        // its own type: numeric properties average/sum/min/max per
        // `gi_dggs_stat`, categorical ones (e.g. `gi_dggs_stat: "majority"`
        // on a country name) take the most common value across zones.
        // A zone that fails is reported under `errors` rather than failing
        // the whole answer; only if every zone fails is the call an error.
        let (per_property, errors) = collect_zone_values(&zone_ids, results, &collection_id);
        if errors.len() == zone_ids.len() {
            return Err(McpError::invalid_params(
                "no data for any sampled zone; see `errors` for each failure",
                Some(serde_json::json!({ "errors": errors })),
            ));
        }

        let mut values = serde_json::Map::new();
        for (col, samples) in &per_property {
            let (stat, unit) = stats.get(col).cloned().unwrap_or((None, None));
            let mut summary = aggregate_property(samples, stat);
            summary["unit"] = serde_json::json!(unit);
            values.insert(col.clone(), summary);
        }

        let mut result = serde_json::json!({
            "insight": { "id": concept_id, "name": concept_name },
            "collection_id": collection_id,
            "dggrs_id": dggrs_id,
            "zone_level": zone_level,
            "zones_sampled": zone_ids.len(),
            "zones_matched": zones_matched,
            "zones_truncated": zones_truncated,
            "values": values,
        });
        if !errors.is_empty() {
            result["errors"] = Value::Array(errors);
        }
        Ok(pretty(&result))
    }

    /// Resolve a caller-supplied insight name/id to its full concept detail
    /// (`GET /concepts/{id}`), which carries the collections that serve it
    /// and the DGGS resolution to sample at. Matches numeric ids directly;
    /// otherwise looks the name up (case-insensitive, exact match preferred
    /// over substring) against `GET /concepts`.
    async fn resolve_concept(&self, insight: &str, caller_token: Option<&str>) -> Result<Value, McpError> {
        if let Ok(id) = insight.parse::<u64>() {
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

    async fn property_stats(
        &self,
        collection_id: &str,
        caller_token: Option<&str>,
    ) -> Result<HashMap<String, (Option<String>, Option<String>)>, McpError> {
        Ok(self
            .queryables(collection_id, caller_token)
            .await?
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_zone_is_reported_not_fatal() {
        let zones = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let ok = |v: f64| Ok(HashMap::from([("ndvi".to_string(), serde_json::json!(v))]));
        let failed = Err(McpError::internal_error(
            "ogc-dggs-api returned 500",
            Some(serde_json::json!({"hint": "No regions: bbox is empty"})),
        ));
        let (values, errors) = collect_zone_values(&zones, vec![ok(0.2), failed, ok(0.4)], "sentinel-2-l2a");

        assert_eq!(values["ndvi"], vec![serde_json::json!(0.2), serde_json::json!(0.4)]);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["zone_id"], "B");
        assert_eq!(errors[0]["collection_id"], "sentinel-2-l2a");
        assert_eq!(errors[0]["hint"], "No regions: bbox is empty");
    }

    #[test]
    fn nulls_are_skipped_not_voted_on() {
        let samples = serde_json::json!([0.2, null, 0.4, null]);
        let summary = aggregate_property(samples.as_array().unwrap(), Some("mean".to_string()));
        assert_eq!(summary["value"], 0.30000000000000004);
        assert_eq!(summary["stat"], "mean");
        assert_eq!(summary["sampled_zones"], 2);
    }

    #[test]
    fn categorical_values_take_the_majority_ignoring_nulls() {
        let samples = serde_json::json!([null, "CA", "US", "CA", null, null]);
        let summary = aggregate_property(samples.as_array().unwrap(), None);
        assert_eq!(summary["value"], "CA");
        assert_eq!(summary["stat"], "majority");
        assert_eq!(summary["sampled_zones"], 3);
    }

    #[test]
    fn all_null_samples_give_a_null_value() {
        let samples = serde_json::json!([null, null]);
        let summary = aggregate_property(samples.as_array().unwrap(), Some("mean".to_string()));
        assert_eq!(summary["value"], Value::Null);
        assert_eq!(summary["sampled_zones"], 0);
    }
}
