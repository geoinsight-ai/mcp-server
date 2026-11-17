mod parquet;
mod rows;

use std::collections::BTreeMap;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::collections::{finest_gsd, queryable_field_names};
use crate::server::dggs::datetime::{api_datetime, time_query};
use crate::server::dggs::extent::{ExtentArgs, build_bbox, width_m};
use crate::server::dggs::level::choose_levels;
use self::parquet::{failed_everywhere, is_plain};
pub(super) use self::parquet::{MapSelection, add_map_requests, is_live_imagery, map_request_limit};
use self::rows::{flatten_zone_data, summarize};
use super::derived::DerivedFields;
use crate::server::dggs::zones::sample_evenly;
use crate::server::{DggsServer, MAX_ZONES_PER_QUERY, bounded, caller_bearer_token, enc, no_data_error, pretty, zone_error};

/// `get_data` returns one row per zone up to this many matched zones; above
/// it, a per-field summary instead, so the model's context stays small.
const MAX_ROWS: usize = 20;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(super) struct CollectionSelectionInput {
    #[schemars(description = "Collection id to read from, as returned by list_collections.")]
    pub(super) collection_id: String,
    #[schemars(description = "Fields to return from this collection, as listed under `fields` by get_collection. Omit to return every field of the collection — do that for imagery unless the user names specific bands, so the map offers all of them.")]
    #[serde(default)]
    pub(super) fields: Vec<String>,
    #[schemars(description = "Rename this collection's output column prefix (columns come back named \"prefix__field\"). Needed to select the same collection twice under different filters. This is not a calculated value: for those see `derived_fields`.")]
    pub(super) alias: Option<String>,
    #[schemars(description = "CQL2-text predicate restricting which of this collection's source rows contribute, e.g. \"length > 500\". Only supported for DuckDB-backed collections.")]
    pub(super) filter: Option<String>,
    /// Set when `fields` was omitted and filled with every field: the map's
    /// requests then ask for all of them rather than listing each one.
    #[serde(skip)]
    #[schemars(skip)]
    pub(super) all_fields: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetDataRequest {
    #[serde(flatten)]
    extent: ExtentArgs,

    #[schemars(description = "One or more collections to read, each with the fields to return. Fields from different collections are joined on the same DGGS zone in one call; output columns are named \"prefix__field\" (prefix = alias or collection_id).")]
    collections: Vec<CollectionSelectionInput>,

    #[schemars(description = "Values to calculate for each zone from the selected fields: a map of new column name to a CQL2-Text arithmetic expression (numbers, field names, `+ - * / % ^`, parentheses; no functions, no comparisons). Example: {\"ratio\": \"(a - b) / (a + b)\"}. With exactly one collection that has no `alias` and no `filter`, write its bare field names; otherwise write the output column names \"prefix__field\". A name must not equal a stored field and an expression cannot use another derived field. The API calculates on the zone's values, never on pixels; the result is null where an input is missing or it divides by zero. Stored values are used as they are: if a collection's description says its numbers need a scale or offset, write that into the expression. The fields an expression reads must be listed in that collection's `fields` (or `fields` omitted).")]
    #[serde(default)]
    derived_fields: BTreeMap<String, String>,

    #[schemars(description = "DGGRS grid to compute on. Omit to use the first listed collection's default grid.")]
    dggrs_id: Option<String>,
    #[schemars(description = "DGGS zone level the map draws: the Parquet requests expand to it from parent zones the grid's default depth above. Omit it (the usual case) to have it chosen from the size of the place and the resolution of the data; set it only to force coarser or finer detail.")]
    zone_level: Option<u32>,
    #[schemars(description = "Relative depth(s) below the resolved zone to expand into, for finer-than-zone_level detail within it: a single value \"5\", a range \"1-8\", or a list \"1,3,7\". Omit for one value per zone (zone-depth=0) — this can return a lot of data if set too deep.")]
    zone_depth: Option<String>,
    #[schemars(description = "Instant or interval to filter temporal data (e.g. satellite imagery), RFC 3339 like \"2024-08-01T00:00:00Z/2024-08-31T23:59:59Z\"; bare dates (\"2024-08-01/2024-08-31\") cover whole days. Pass the dates the user asked for; omit for none.")]
    datetime: Option<String>,
}

/// What one zone read of `get_data` asks for, besides the collections.
struct RowRead<'a> {
    dggrs_id: &'a str,
    zone_depth: Option<&'a str>,
    datetime: Option<&'a str>,
    derived: &'a DerivedFields,
}

impl RowRead<'_> {
    /// The query both data routes take: depth, time, and the derived fields'
    /// `alias[name]` definitions.
    fn query(&self) -> Vec<(String, String)> {
        let mut query = vec![("zone-depth".to_string(), self.zone_depth.unwrap_or("0").to_string())];
        query.extend(time_query(self.datetime).into_iter().map(|(key, value)| (key.to_string(), value)));
        query.extend(self.derived.query_pairs());
        query
    }
}

/// Borrowed `(key, value)` pairs in the form the API client takes.
fn borrowed(query: &[(String, String)]) -> Vec<(&str, String)> {
    query.iter().map(|(key, value)| (key.as_str(), value.clone())).collect()
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

/// A column of the per-collection GET under the name `get_data` reports:
/// `zone_id` and the derived fields as they are, a stored field as
/// `collection__field`.
fn column_for(collection_id: &str, derived_names: &[String], column: String, value: Value) -> (String, Value) {
    if column == "zone_id" || derived_names.contains(&column) {
        return (column, value);
    }
    (format!("{collection_id}__{column}"), value)
}

#[tool_router(router = data_router, vis = "pub(crate)")]
impl DggsServer {
    #[tool(
        title = "Get data",
        annotations(read_only_hint = true),
        description = "Get raw field values from one or more collections at a location or area — the low-level counterpart to get_insight. Use this when you already know which collection(s) and field(s) you want (optionally joining several collections' fields together, e.g. elevation plus country name, in one call), rather than going through a named insight. Unlike get_insight this does not aggregate down to one number: for up to 20 matched zones it returns one flat row per zone (`zone_id` plus one column per field, named \"prefix__field\"); for more, a per-field `summary` (min/mean/max, or the most common values) instead. A point returns the single zone containing it. It also returns `parquet_requests`: requests (method, url, body) covering the whole area, one per parent zone, for a map to fetch and draw directly — the full data, which never needs to pass through you. Zone levels are chosen for you from the size of the place and the resolution of the data, so you don't need to pick one. Each collection's `fields` can be omitted to return all of its fields (see get_collection for what they are). To calculate a value that is not stored (an index, a ratio, a difference between collections), pass `derived_fields`: the API evaluates your expression for each zone and returns it as an extra column, which the map requests draw as well; use this when no insight covers what the user asks for. If a collection has no data at a zone, the other collections' values are still returned and the failure is listed under `errors`. Works without Auth0 configured."
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

        let bbox = build_bbox(&req.extent)?;
        let datetime = req.datetime.as_deref().map(api_datetime).transpose()?;
        let datetime = datetime.as_deref();
        let derived = DerivedFields::new(req.derived_fields.clone().into_iter().collect());

        // Every collection's queryables schema: the backend has no "all
        // fields" wildcard, so a collection with no `fields` gets every
        // field in it, as explore's own data view does; and its `gi_gsd`
        // says how fine the chosen fields' data is.
        let mut collections = req.collections.clone();
        let schemas = futures::future::try_join_all(
            collections.iter().map(|c| self.queryables(&c.collection_id, caller_token)),
        )
        .await?;
        for (c, props) in collections.iter_mut().zip(&schemas) {
            if c.fields.is_empty() {
                c.fields = queryable_field_names(props);
                c.all_fields = true;
            }
            if c.fields.is_empty() {
                return Err(McpError::invalid_params(
                    format!("collection \"{}\" has no queryable fields", c.collection_id),
                    None,
                ));
            }
        }
        let finest = collections
            .iter()
            .zip(&schemas)
            .filter_map(|(c, props)| finest_gsd(props, &c.fields))
            .reduce(f64::min);

        let dggrs_id = match &req.dggrs_id {
            Some(d) => d.clone(),
            None => {
                self.default_dggrs(&collections[0].collection_id, caller_token)
                    .await?
            }
        };
        let grid = self.grid(&dggrs_id, caller_token).await;
        let live_imagery = is_live_imagery(datetime, finest);
        let levels = choose_levels(&grid, width_m(&req.extent), finest, req.zone_level, live_imagery);

        // DGGS zones are collection-independent (they depend only on the
        // DGGRS, the level and the extent), so any one of the requested
        // collections can anchor the zone lookups; the first is as good as
        // any.
        let anchor = &collections[0].collection_id;
        let zone_ids = self
            .extent_zone_ids(Some(anchor), &dggrs_id, &req.extent, &bbox, levels.answer, caller_token)
            .await?;
        if zone_ids.is_empty() {
            return Err(McpError::invalid_params(
                format!("no zones intersect that location at zone level {}", levels.answer),
                None,
            ));
        }
        let zones_matched = zone_ids.len();
        let as_rows = zones_matched <= MAX_ROWS;
        let zone_ids = sample_evenly(zone_ids, MAX_ZONES_PER_QUERY);

        let map_limit = map_request_limit(live_imagery);
        let parents = self.map_parent_ids(anchor, &dggrs_id, &req.extent, levels.parent, map_limit, caller_token);

        let read = RowRead {
            dggrs_id: &dggrs_id,
            zone_depth: req.zone_depth.as_deref(),
            datetime,
            derived: &derived,
        };
        let fetches = bounded(
            zone_ids
                .iter()
                .map(|zone_id| self.zone_row(&read, zone_id, &collections, caller_token)),
        );
        let (fetched, parents) = futures::join!(fetches, parents);
        let parents = parents?;

        let mut rows = Vec::new();
        let mut errors = Vec::new();
        for (row, zone_errors) in fetched {
            rows.extend(row.map(Value::Object));
            errors.extend(zone_errors);
        }

        if rows.is_empty() {
            return Err(no_data_error(errors));
        }

        let excluded = failed_everywhere(&collections, &errors, zone_ids.len());
        let request_collections: Vec<CollectionSelectionInput> = collections
            .iter()
            .filter(|c| !excluded.contains(&c.collection_id))
            .cloned()
            .collect();

        let mut result = serde_json::json!({
            "dggrs_id": dggrs_id,
            "zone_level": levels.answer,
            "zones_matched": zones_matched,
        });
        if as_rows {
            result["rows"] = Value::Array(rows);
        } else {
            result["summary"] = Value::Object(summarize(&rows));
            result["summary_sampled_zones"] = serde_json::json!(rows.len());
        }
        add_map_requests(
            &mut result,
            self.client.public_base_url(),
            &dggrs_id,
            &levels,
            &parents,
            MapSelection { collections: &request_collections, derived: &derived },
            datetime,
        );
        if !excluded.is_empty() {
            result["excluded_collections"] = serde_json::json!(excluded);
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
        read: &RowRead<'_>,
        zone_id: &str,
        body: &Value,
        caller_token: Option<&str>,
    ) -> Result<Value, McpError> {
        let path = format!("/dggs/{}/zones/{}/data", enc(read.dggrs_id), enc(zone_id));
        self.client
            .post_json(&path, &borrowed(&read.query()), body, caller_token)
            .await
    }

    /// One collection's row at one zone, columns named `prefix__field` as
    /// the cross-collection POST names them. A plain selection (no alias or
    /// filter) goes through the per-collection GET instead, which is the
    /// only endpoint that accepts ids like `sentinel-2-l2a` (the POST
    /// rejects hyphens with 422); its columns come back bare and are
    /// prefixed here, except the derived fields, which keep their own names.
    async fn collection_row(
        &self,
        read: &RowRead<'_>,
        zone_id: &str,
        collection: &CollectionSelectionInput,
        caller_token: Option<&str>,
    ) -> Result<serde_json::Map<String, Value>, McpError> {
        if !is_plain(collection) {
            let body = collections_body(std::slice::from_ref(collection));
            let body = self.cross_collection_data(read, zone_id, &body, caller_token).await?;
            return Ok(flatten_zone_data(zone_id, &body));
        }
        let path = format!(
            "/collections/{}/dggs/{}/zones/{}/data",
            enc(&collection.collection_id),
            enc(read.dggrs_id),
            enc(zone_id)
        );
        let derived_names = read.derived.names();
        let returned: Vec<String> = collection.fields.iter().cloned().chain(derived_names.clone()).collect();
        let mut query = read.query();
        query.push(("properties".to_string(), returned.join(",")));
        let body = self.client.get_json(&path, &borrowed(&query), caller_token).await?;
        Ok(flatten_zone_data(zone_id, &body)
            .into_iter()
            .map(|(column, value)| column_for(&collection.collection_id, &derived_names, column, value))
            .collect())
    }

    /// One zone's joined row for `get_data`. A single collection with no
    /// data at this zone fails the whole cross-collection POST (the backend
    /// 500s with e.g. "Bbox of zones does not overlap COG extent"), as does
    /// one the POST can't name (`sentinel-2-l2a`), which would also throw
    /// away every other collection's values. So on failure with several
    /// collections, read each one alone and keep what succeeds; only the
    /// collections that really failed are reported. Read alone, a collection
    /// comes without the derived fields: their expressions are written
    /// against the joined columns.
    async fn zone_row(
        &self,
        read: &RowRead<'_>,
        zone_id: &str,
        collections: &[CollectionSelectionInput],
        caller_token: Option<&str>,
    ) -> (Option<serde_json::Map<String, Value>>, Vec<Value>) {
        if let [only] = collections {
            return match self.collection_row(read, zone_id, only, caller_token).await {
                Ok(row) => (Some(row), Vec::new()),
                Err(err) => (None, vec![zone_error(zone_id, Some(&only.collection_id), &err)]),
            };
        }
        let joined = self
            .cross_collection_data(read, zone_id, &collections_body(collections), caller_token)
            .await;
        if let Ok(body) = joined {
            return (Some(flatten_zone_data(zone_id, &body)), Vec::new());
        }

        let stored_only = RowRead {
            dggrs_id: read.dggrs_id,
            zone_depth: read.zone_depth,
            datetime: read.datetime,
            derived: &DerivedFields::none(),
        };
        let mut row: Option<serde_json::Map<String, Value>> = None;
        let mut errors = Vec::new();
        for collection in collections {
            match self.collection_row(&stored_only, zone_id, collection, caller_token).await {
                Ok(values) => row.get_or_insert_with(serde_json::Map::new).extend(values),
                Err(err) => errors.push(zone_error(zone_id, Some(&collection.collection_id), &err)),
            }
        }
        (row, errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_fields_get_the_collection_prefix_and_derived_fields_do_not() {
        let derived = vec!["ratio".to_string()];
        let name = |column: &str| column_for("dem", &derived, column.to_string(), Value::Null).0;

        assert_eq!(name("band1_mean"), "dem__band1_mean");
        assert_eq!(name("ratio"), "ratio");
        assert_eq!(name("zone_id"), "zone_id");
    }

    #[test]
    fn a_zone_read_defines_its_derived_fields_in_the_query() {
        // Arrange
        let derived = DerivedFields::new(vec![("ratio".to_string(), "a / b".to_string())]);
        let read = RowRead { dggrs_id: "IVEA7H", zone_depth: None, datetime: None, derived: &derived };

        // Act
        let query = read.query();

        // Assert
        assert_eq!(
            query,
            vec![
                ("zone-depth".to_string(), "0".to_string()),
                ("alias[ratio]".to_string(), "a / b".to_string()),
            ]
        );
    }

    #[test]
    fn derived_fields_are_optional_in_the_request() {
        let without: GetDataRequest =
            serde_json::from_str(r#"{"lon": 1.0, "lat": 2.0, "collections": [{"collection_id": "dem"}]}"#).unwrap();
        let with: GetDataRequest = serde_json::from_str(
            r#"{"lon": 1.0, "lat": 2.0, "collections": [{"collection_id": "dem"}], "derived_fields": {"double": "band1_mean * 2"}}"#,
        )
        .unwrap();

        assert!(without.derived_fields.is_empty());
        assert_eq!(with.derived_fields["double"], "band1_mean * 2");
    }

    #[test]
    fn omitted_fields_deserialize_as_empty() {
        let c: CollectionSelectionInput = serde_json::from_str(r#"{"collection_id": "cadastre_parcelles"}"#).unwrap();
        assert!(c.fields.is_empty());
    }
}
