use rmcp::handler::server::wrapper::Parameters;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::server::{DggsServer, caller_bearer_token, enc, pretty};

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

    /// Per-property aggregation hints (`gi_dggs_stat`, `gi_unit`) for a
    /// collection, from its queryables schema.
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
}
