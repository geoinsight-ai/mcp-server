mod parquet;
mod rows;

use futures::future::join_all;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::collections::queryable_field_names;
use crate::server::dggs::extent::{ExtentArgs, build_bbox};
use self::parquet::{MAX_PARQUET_REQUESTS, failed_everywhere, parent_level, parquet_request};
use self::rows::{flatten_zone_data, sample_evenly, summarize};
use crate::server::{DggsServer, MAX_ZONES_PER_QUERY, caller_bearer_token, enc, pretty};

/// `get_data`'s zone level when the caller doesn't specify one. `get_insight`
/// instead reads a concept-specific resolution, but `get_data` has no concept
/// to read it from.
const DEFAULT_ZONE_LEVEL: u32 = 9;

/// `get_data` returns one row per zone up to this many matched zones; above
/// it, a per-field summary instead, so the model's context stays small.
const MAX_ROWS: usize = 20;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct CollectionSelectionInput {
    #[schemars(description = "Collection id to read from, as returned by list_collections.")]
    pub(super) collection_id: String,
    #[schemars(description = "Fields to return from this collection, as listed under `fields` by get_collection. Omit to return every field of the collection.")]
    #[serde(default)]
    pub(super) fields: Vec<String>,
    #[schemars(description = "Rename this collection's output column prefix (columns come back named \"prefix__field\"). Needed to select the same collection twice under different filters.")]
    pub(super) alias: Option<String>,
    #[schemars(description = "CQL2-text predicate restricting which of this collection's source rows contribute, e.g. \"length > 500\". Only supported for DuckDB-backed collections.")]
    pub(super) filter: Option<String>,
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

/// The cross-collection POST body for `collections`.
pub(super) fn collections_body(collections: &[CollectionSelectionInput]) -> Value {
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

#[tool_router(router = data_router, vis = "pub(crate)")]
impl DggsServer {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_fields_deserialize_as_empty() {
        let c: CollectionSelectionInput = serde_json::from_str(r#"{"collection_id": "cadastre_parcelles"}"#).unwrap();
        assert!(c.fields.is_empty());
    }
}
