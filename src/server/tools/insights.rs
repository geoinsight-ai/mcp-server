use std::collections::HashMap;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::collections::{finest_gsd, queryable_field_names};
use super::data::{CollectionSelectionInput, add_map_requests, is_live_imagery, map_request_limit};
use crate::server::dggs::aggregate::{majority, reduce};
use crate::server::dggs::datetime::{api_datetime, time_query};
use crate::server::dggs::extent::{ExtentArgs, build_bbox, width_m};
use crate::server::dggs::level::choose_levels;
use crate::server::dggs::zones::sample_evenly;
use crate::server::{DggsServer, MAX_ZONES_PER_QUERY, bounded, caller_bearer_token, enc, no_data_error, pretty, zone_error};

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
    #[schemars(description = "DGGS zone level the map draws. Omit it (the usual case) to have it chosen from the size of the place and the resolution of the insight's data; set it only to force coarser or finer detail.")]
    zone_level: Option<u32>,
    #[schemars(description = "Instant or interval to filter temporal data (e.g. satellite imagery), RFC 3339 such as \"2024-06-01T00:00:00Z\" or \"2024-06-01T00:00:00Z/2024-06-30T23:59:59Z\"; bare dates (\"2024-06-01/2024-06-30\") cover whole days. Pass the dates the user asked for; omit for none.")]
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

/// Each property's aggregation hint and unit (`gi_dggs_stat`, `gi_unit`)
/// from a queryables schema.
fn property_stats(props: &serde_json::Map<String, Value>) -> HashMap<String, (Option<String>, Option<String>)> {
    props
        .iter()
        .map(|(name, schema)| {
            let stat = schema.get("gi_dggs_stat").and_then(Value::as_str).map(str::to_string);
            let unit = schema.get("gi_unit").and_then(Value::as_str).map(str::to_string);
            (name.clone(), (stat, unit))
        })
        .collect()
}

/// The bands a concept reads from `collection_id` (`collections[].properties`
/// in its detail), e.g. B8A and B12 for burn scars. Empty when the concept
/// doesn't list that collection.
fn concept_fields(concept: &Value, collection_id: &str) -> Vec<String> {
    concept
        .get("collections")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|c| c.get("id").and_then(Value::as_str) == Some(collection_id))
        .and_then(|c| c.get("properties"))
        .and_then(Value::as_object)
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default()
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
        description = "Get an insight's value for a location or area — the main data-access tool. Pass an insight (e.g. \"ndvi\", \"burn_scar_analysis\"; see list_insights) and either a point (lon/lat) or an area (min_lon/min_lat/max_lon/max_lat). This resolves the insight's data source and the bands it reads, picks DGGS zone levels from the size of the place and the data's resolution, samples the zone(s) covering the location (the single zone containing a point; up to 64 zones spread evenly across an area), and returns one aggregated value per band using that band's own configured statistic (mean, max, ...) — you get a location, not a list of DGGS zones to interpret yourself. It also returns `parquet_requests` for a map to draw the insight's data over the whole place itself; that data never passes through you. Zones that fail are listed under `errors`; the value comes from the zones that succeeded."
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

        let dggrs_id = match &req.dggrs_id {
            Some(d) => d.clone(),
            None => self.default_dggrs(&collection_id, caller_token).await?,
        };
        let bbox = build_bbox(&req.extent)?;
        let datetime = req.datetime.as_deref().map(api_datetime).transpose()?;
        let datetime = datetime.as_deref();

        let (props, grid) = futures::join!(
            self.queryables(&collection_id, caller_token),
            self.grid(&dggrs_id, caller_token),
        );
        let props = props?;
        // The insight's own bands, when the concept lists them for this
        // collection; otherwise (e.g. an overridden collection) all fields.
        let fields = match concept_fields(&concept, &collection_id) {
            fields if fields.is_empty() => queryable_field_names(&props),
            fields => fields,
        };
        let finest = finest_gsd(&props, &fields);
        let live_imagery = is_live_imagery(datetime, finest);
        let levels = choose_levels(&grid, width_m(&req.extent), finest, req.zone_level, live_imagery);
        let map_limit = map_request_limit(live_imagery);

        // A point resolves to the one zone containing it, not the 2-3
        // neighbours its small search box overlaps (see `extent_zone_ids`).
        let zone_ids = self
            .extent_zone_ids(Some(&collection_id), &dggrs_id, &req.extent, &bbox, levels.answer, caller_token)
            .await?;
        if zone_ids.is_empty() {
            return Err(McpError::invalid_params(
                format!(
                    "no zones of collection \"{collection_id}\" intersect that location at zone level {}",
                    levels.answer
                ),
                None,
            ));
        }
        let zones_matched = zone_ids.len();
        let zones_truncated = zones_matched > MAX_ZONES_PER_QUERY;
        let zone_ids = sample_evenly(zone_ids, MAX_ZONES_PER_QUERY);

        let fetches = bounded(
            zone_ids
                .iter()
                .map(|zone_id| self.zone_values(&collection_id, &dggrs_id, zone_id, datetime, caller_token)),
        );
        let parents =
            self.map_parent_ids(&collection_id, &dggrs_id, &req.extent, levels.parent, map_limit, caller_token);
        let (results, parents) = futures::join!(fetches, parents);
        let parents = parents?;

        // Each property is collected raw first, then reduced according to
        // its own type: numeric properties average/sum/min/max per
        // `gi_dggs_stat`, categorical ones (e.g. `gi_dggs_stat: "majority"`
        // on a country name) take the most common value across zones.
        // A zone that fails is reported under `errors` rather than failing
        // the whole answer; only if every zone fails is the call an error.
        let (mut per_property, errors) = collect_zone_values(&zone_ids, results, &collection_id);
        if errors.len() == zone_ids.len() {
            return Err(no_data_error(errors));
        }
        per_property.retain(|property, _| fields.contains(property));

        let stats = property_stats(&props);
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
            "zone_level": levels.answer,
            "zones_sampled": zone_ids.len(),
            "zones_matched": zones_matched,
            "zones_truncated": zones_truncated,
            "values": values,
        });
        let map_collections = [CollectionSelectionInput {
            collection_id: collection_id.clone(),
            fields,
            alias: None,
            filter: None,
            all_fields: false,
        }];
        add_map_requests(
            &mut result,
            self.client.public_base_url(),
            &dggrs_id,
            &levels,
            &parents,
            &map_collections,
            datetime,
        );
        if !errors.is_empty() {
            result["errors"] = Value::Array(errors);
        }
        Ok(pretty(&result))
    }

    /// Resolve a caller-supplied insight name/id to its full concept detail
    /// (`GET /concepts/{id}`), which carries the collections that serve it
    /// and the bands it reads from each. Matches numeric ids directly;
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
        query.extend(time_query(datetime));
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
    fn concept_fields_are_the_bands_listed_for_that_collection() {
        let concept = serde_json::json!({
            "collections": [
                {"id": "sentinel-2-l2a", "properties": {"B8A_20m": {"gi_gsd": 20}, "B12_20m": {"gi_gsd": 20}}},
                {"id": "other", "properties": {"x": {}}}
            ]
        });
        let mut fields = concept_fields(&concept, "sentinel-2-l2a");
        fields.sort();
        assert_eq!(fields, vec!["B12_20m", "B8A_20m"]);
        assert!(concept_fields(&concept, "canada_dem").is_empty());
        assert!(concept_fields(&serde_json::json!({}), "sentinel-2-l2a").is_empty());
    }

    #[test]
    fn property_stats_read_the_stat_and_unit_hints() {
        let props: serde_json::Map<String, Value> =
            serde_json::from_str(r#"{"elev": {"gi_dggs_stat": "mean", "gi_unit": "m"}, "name": {}}"#).unwrap();
        let stats = property_stats(&props);
        assert_eq!(stats["elev"], (Some("mean".to_string()), Some("m".to_string())));
        assert_eq!(stats["name"], (None, None));
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
