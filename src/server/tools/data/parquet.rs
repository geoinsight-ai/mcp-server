use serde_json::Value;

use super::{CollectionSelectionInput, collections_body};
use crate::server::dggs::datetime::time_query;
use crate::server::dggs::level::Levels;
use crate::server::dggs::zones::ParentZones;
use crate::server::enc;

/// Caps how many parent-zone Parquet requests one call returns. Each one
/// expands to `default_depth` levels of children (~7^6 on IVEA7H), so this
/// bounds what the map is asked to load. Provisional until we know what the
/// map draws comfortably.
const MAX_PARQUET_REQUESTS: usize = 16;

/// The cap for live imagery: raster data asked for over a date range, which
/// the backend mosaics from upstream scenes on the spot. Each such parent
/// pulls every scene's full-size band files through GDAL; four all-band
/// Sentinel-2 level-5 parents at once segfaulted api-mr (exit 139,
/// 2026-11-03). Live imagery is drawn at its own level (see
/// `choose_levels`), so these parents are small (~8 km at IVEA7H level 7).
const MAX_LIVE_IMAGERY_REQUESTS: usize = 3;

/// Live imagery: raster data (it has a pixel size) asked for over a date
/// range, which the backend mosaics from upstream scenes per request.
pub(in crate::server::tools) fn is_live_imagery(datetime: Option<&str>, finest_gsd: Option<f64>) -> bool {
    datetime.is_some() && finest_gsd.is_some()
}

/// How many parent-zone requests the map gets: fewer for live imagery, the
/// usual cap otherwise.
pub(in crate::server::tools) fn map_request_limit(live_imagery: bool) -> usize {
    if live_imagery {
        MAX_LIVE_IMAGERY_REQUESTS
    } else {
        MAX_PARQUET_REQUESTS
    }
}

/// The map's share of a `get_data`/`get_insight` result, added to `result`:
/// the levels it draws (`parent_level`, `children_level`), the collections
/// it loads, and one Parquet request per parent zone, up to its limit. Both
/// tools share this so a map client reads them the same way.
pub(in crate::server::tools) fn add_map_requests(
    result: &mut Value,
    base_url: &str,
    dggrs_id: &str,
    levels: &Levels,
    parents: &ParentZones,
    collections: &[CollectionSelectionInput],
    datetime: Option<&str>,
) {
    let requests: Vec<Value> = parents
        .ids
        .iter()
        .take(parents.limit)
        .map(|parent| parquet_request(base_url, dggrs_id, parent, collections, datetime, levels.request_depth))
        .collect();

    result["parent_level"] = serde_json::json!(levels.parent);
    result["children_level"] = serde_json::json!(levels.children);
    result["collections"] = serde_json::json!(collections.iter().map(|c| &c.collection_id).collect::<Vec<_>>());
    result["parquet_requests"] = Value::Array(requests);
    if parents.ids.len() > parents.limit {
        result["requests_truncated"] = Value::Bool(true);
        result["parent_zones_matched"] = serde_json::json!(parents.ids.len());
    }
}

/// One Parquet request covering `parent_zone_id`, for the
/// caller to fetch itself. A single collection with no alias/filter uses the
/// plain per-collection GET (which also accepts ids the cross-collection
/// endpoint rejects, like `sentinel-2-l2a`): with every field when the
/// caller didn't pick any, so the map can switch between all the bands,
/// otherwise just the picked ones. Anything else uses the cross-collection
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
    query.extend(time_query(datetime).into_iter().map(|(k, v)| format!("{k}={}", enc(&v))));
    if let Some(depth) = zone_depth {
        query.push(format!("zone-depth={depth}"));
    }
    let query = query.join("&");

    match collections {
        [only] if is_plain(only) => {
            let properties = if only.all_fields {
                String::new()
            } else {
                format!("&properties={}", enc(&only.fields.join(",")))
            };
            serde_json::json!({
                "parent_zone_id": parent_zone_id,
                "method": "GET",
                "url": format!(
                    "{base_url}/collections/{}/dggs/{}/zones/{}/data?{query}{properties}",
                    enc(&only.collection_id),
                    enc(dggrs_id),
                    enc(parent_zone_id)
                ),
            })
        }
        _ => serde_json::json!({
            "parent_zone_id": parent_zone_id,
            "method": "POST",
            "url": format!("{base_url}/dggs/{}/zones/{}/data?{query}", enc(dggrs_id), enc(parent_zone_id)),
            "body": collections_body(collections),
        }),
    }
}

/// A selection the per-collection GET can serve: no alias to rename its
/// columns and no filter, which only the cross-collection POST takes.
pub(super) fn is_plain(collection: &CollectionSelectionInput) -> bool {
    collection.alias.is_none() && collection.filter.is_none()
}

/// Collections that failed at every one of `sampled` zones (e.g. a Canadian
/// DEM queried in Portugal, if the backend 500s rather than returning
/// nulls). Every combined Parquet request would fail the same way, so these
/// are left out of the Parquet requests and named instead.
pub(super) fn failed_everywhere(collections: &[CollectionSelectionInput], errors: &[Value], sampled: usize) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(id: &str, fields: &[&str]) -> CollectionSelectionInput {
        CollectionSelectionInput {
            collection_id: id.to_string(),
            fields: fields.iter().map(|f| f.to_string()).collect(),
            alias: None,
            filter: None,
            all_fields: false,
        }
    }

    #[test]
    fn map_requests_carry_levels_collections_and_a_capped_request_list() {
        let levels = Levels { parent: 7, children: 13, answer: 9, request_depth: None };
        let parents = ParentZones { ids: (0..20).map(|i| format!("P{i}")).collect(), limit: MAX_PARQUET_REQUESTS };
        let mut result = serde_json::json!({});
        add_map_requests(&mut result, "https://api.example", "IVEA7H", &levels, &parents, &[selection("sentinel-2-l2a", &["B04_10m"])], None);

        assert_eq!(result["parent_level"], 7);
        assert_eq!(result["children_level"], 13);
        assert_eq!(result["collections"], serde_json::json!(["sentinel-2-l2a"]));
        assert_eq!(result["parquet_requests"].as_array().unwrap().len(), MAX_PARQUET_REQUESTS);
        assert_eq!(result["requests_truncated"], true);
        assert_eq!(result["parent_zones_matched"], 20);
    }

    #[test]
    fn single_collection_request_is_a_plain_get() {
        let l = parquet_request("https://api.example", "IVEA7H", "C0-1A", &[selection("sentinel-2-l2a", &["B04"])], None, None);
        assert_eq!(l["method"], "GET");
        assert_eq!(
            l["url"],
            "https://api.example/collections/sentinel-2-l2a/dggs/IVEA7H/zones/C0-1A/data?f=parquet&properties=B04"
        );
        assert!(l.get("body").is_none());

        let all = CollectionSelectionInput { all_fields: true, ..selection("sentinel-2-l2a", &["B04", "B08"]) };
        let l = parquet_request("https://api.example", "IVEA7H", "C0-1A", &[all], None, None);
        assert_eq!(l["url"], "https://api.example/collections/sentinel-2-l2a/dggs/IVEA7H/zones/C0-1A/data?f=parquet");
    }

    #[test]
    fn live_imagery_gets_fewer_parents() {
        assert!(is_live_imagery(Some("2024-08-01T00:00:00Z/.."), Some(10.0)));
        assert!(!is_live_imagery(None, Some(10.0)));
        assert!(!is_live_imagery(Some("2024-08-01T00:00:00Z/.."), None));
        assert_eq!(map_request_limit(true), MAX_LIVE_IMAGERY_REQUESTS);
        assert_eq!(map_request_limit(false), MAX_PARQUET_REQUESTS);
    }

    #[test]
    fn several_collections_request_is_one_combined_post() {
        let cols = [selection("canada_dem", &["band1_mean"]), selection("overture_admin0", &["country_name"])];
        let l = parquet_request("https://api.example", "IVEA7H", "C0-1A", &cols, Some("2024-06-01T00:00:00Z"), Some(4));
        assert_eq!(l["method"], "POST");
        assert_eq!(
            l["url"],
            "https://api.example/dggs/IVEA7H/zones/C0-1A/data?f=parquet&datetime=2024-06-01T00%3A00%3A00Z&stac-search-strategy=mosaic&zone-depth=4"
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
}
