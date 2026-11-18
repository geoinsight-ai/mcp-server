use rmcp::handler::server::wrapper::Parameters;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::server::dggs::extent::{ExtentArgs, optional_bbox};
use crate::server::{DggsServer, caller_bearer_token, enc, pretty};

#[derive(Debug, Deserialize, JsonSchema)]
struct ListCollectionsRequest {
    /// The place to list collections for; omit for the whole catalogue.
    #[serde(flatten)]
    extent: ExtentArgs,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CollectionIdRequest {
    #[schemars(description = "Collection identifier, as returned by list_collections.")]
    collection_id: String,
}

/// Field names from a queryables schema, minus `zone_id` (the join key,
/// not data). What `get_data` reads when a collection's `fields` is omitted.
pub(super) fn queryable_field_names(props: &serde_json::Map<String, Value>) -> Vec<String> {
    props.keys().filter(|name| *name != "zone_id").cloned().collect()
}

/// The finest pixel size, m, among `fields` of a queryables schema
/// (`gi_gsd`, which raster collections record per band). `None` when none
/// has one, as for vector collections: their detail isn't bounded by pixels.
pub(super) fn finest_gsd<'a>(props: &serde_json::Map<String, Value>, fields: impl IntoIterator<Item = &'a String>) -> Option<f64> {
    fields
        .into_iter()
        .filter_map(|field| props.get(field)?.get("gi_gsd")?.as_f64())
        .filter(|gsd| *gsd > 0.0)
        .reduce(f64::min)
}

/// A collection's spatial extent as `[min_lon, min_lat, max_lon, max_lat]`,
/// or `None` when it covers the whole world or declares none. The backend's
/// bboxes can have their corners swapped, so they're put in order.
fn collection_bbox(collection: &Value) -> Option<[f64; 4]> {
    let raw = collection.pointer("/extent/spatial/bbox/0")?.as_array()?;
    let n: Vec<f64> = raw.iter().filter_map(Value::as_f64).collect();
    let [x0, y0, x1, y1] = n[..] else { return None };
    let bbox = [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)];
    let global = bbox[2] - bbox[0] >= 359.0 && bbox[3] - bbox[1] >= 179.0;
    (!global).then_some(bbox)
}

/// A collection's time span, `[start, end]` (either may be null), when it
/// has one. The backend wraps the values in stray SQL quotes
/// (`"'2023-09-09 18:59:41'"`); those are stripped.
fn collection_dates(collection: &Value) -> Option<Value> {
    let span = collection.pointer("/extent/temporal/interval/0")?.as_array()?;
    let clean = |v: Option<&Value>| {
        let s = v?.as_str()?.trim_matches('\'').trim();
        (!s.is_empty()).then(|| Value::String(s.replacen(' ', "T", 1)))
    };
    Some(serde_json::json!([clean(span.first()), clean(span.get(1))]))
}

/// What a model needs to pick a collection: id, title, where (`"global"`
/// when anywhere) and when. Descriptions, links and CRS are left out (the
/// full catalogue was ~2 MB); get_collection has them.
fn collection_summary(collection: &Value) -> Value {
    let round = |n: f64| (n * 100.0).round() / 100.0;
    let mut summary = serde_json::json!({
        "id": collection.get("id"),
        "title": collection.get("title"),
        "bbox": match collection_bbox(collection) {
            Some(b) => serde_json::json!(b.map(round)),
            None => serde_json::json!("global"),
        },
    });
    if let Some(dates) = collection_dates(collection) {
        summary["dates"] = dates;
    }
    summary
}

/// Whether a collection has data anywhere in `area`: global collections
/// always do.
fn covers(collection: &Value, area: &[f64; 4]) -> bool {
    match collection_bbox(collection) {
        None => true,
        Some(b) => b[0] <= area[2] && b[2] >= area[0] && b[1] <= area[3] && b[3] >= area[1],
    }
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

#[tool_router(router = collections_router, vis = "pub(crate)")]
impl DggsServer {
    #[tool(
        title = "List collections",
        annotations(title = "List collections", read_only_hint = true),
        description = "List the data collections that have data at a place: each one's id, title, bbox (\"global\" when it covers anywhere) and dates. Pass the place's point (lon/lat) or bbox, the same one you'll query — the full catalogue is close to a thousand collections, far too many to read, and only the ones overlapping the place (plus global ones) are returned. For Sentinel-2 imagery use `sentinel-2-l2a`, which combines every scene in a date range, rather than the single-scene `S2A_…`/`S2B_…` collections. Use get_collection for one collection's fields."
    )]
    async fn list_collections(
        &self,
        Parameters(req): Parameters<ListCollectionsRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, McpError> {
        let caller_token = caller_bearer_token(&ctx);
        let area = optional_bbox(&req.extent)?;
        let body = self.client.get_json("/collections", &[], caller_token.as_deref()).await?;
        let all = body.get("collections").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        let collections: Vec<Value> = all
            .iter()
            .filter(|c| area.is_none_or(|area| covers(c, &area)))
            .map(collection_summary)
            .collect();
        Ok(pretty(&serde_json::json!({
            "collections": collections,
            "matched": collections.len(),
            "total": all.len(),
        })))
    }

    #[tool(
        title = "Get collection",
        annotations(title = "Get collection", read_only_hint = true),
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

    /// A collection's queryables schema (`GET /collections/{id}/queryables`
    /// → `properties`): its field names, types, titles and `gi_*` hints.
    pub(super) async fn queryables(
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn collections_are_summarised_and_filtered_by_place() {
        let swiss = serde_json::json!({
            "id": "ch_layer", "title": "Swiss layer", "description": "long", "links": [],
            "extent": {"spatial": {"bbox": [[10.49, 47.81, 5.96, 45.82]]}},
        });
        let world = serde_json::json!({
            "id": "worldpop", "title": "WorldPop",
            "extent": {"spatial": {"bbox": [[-180.0, -90.0, 180.0, 90.0]]}, "temporal": {"interval": [["'2020-01-01 00:00:00'", null]]}},
        });
        assert_eq!(
            collection_summary(&swiss),
            serde_json::json!({"id": "ch_layer", "title": "Swiss layer", "bbox": [5.96, 45.82, 10.49, 47.81]})
        );
        assert_eq!(collection_summary(&world)["bbox"], "global");
        assert_eq!(collection_summary(&world)["dates"], serde_json::json!(["2020-01-01T00:00:00", null]));

        let jasper = [-118.2, 52.8, -118.0, 53.0];
        assert!(!covers(&swiss, &jasper));
        assert!(covers(&swiss, &[7.0, 46.0, 7.5, 46.5]));
        assert!(covers(&world, &jasper));
        assert!(covers(&serde_json::json!({"id": "no_extent"}), &jasper));
    }

    #[test]
    fn finest_gsd_is_the_smallest_pixel_among_the_chosen_fields() {
        let props: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{"B04_10m": {"gi_gsd": 10}, "B12_20m": {"gi_gsd": 20}, "B01_60m": {"gi_gsd": 60}, "name": {"type": "string"}}"#,
        )
        .unwrap();
        let fields = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(finest_gsd(&props, &fields(&["B12_20m", "B01_60m"])), Some(20.0));
        assert_eq!(finest_gsd(&props, &fields(&["B04_10m", "B12_20m"])), Some(10.0));
        assert_eq!(finest_gsd(&props, &fields(&["name"])), None);
    }
}
