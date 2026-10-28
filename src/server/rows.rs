use serde_json::Value;

use super::aggregate::reduce;

/// Flattens one zone's DGGS-JSON (`values: { field: [{depth, data, shape}] }`
/// plus links and grid metadata repeated per zone) into a single row:
/// `{zone_id, field: value}` at one depth with one value, `field: [values]`
/// for one depth expanded into sub-zones, `field: {depth: [values]}` for
/// several depths. Most of the original bytes were wrapper, not data.
pub(super) fn flatten_zone_data(zone_id: &str, body: &Value) -> serde_json::Map<String, Value> {
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

/// Per-field summary across rows, for when there are too many zones to
/// return as rows: min/mean/max for numeric fields, the most common values
/// for anything else. Sub-zone arrays (`zone_depth` > 0) count each element.
pub(super) fn summarize(rows: &[Value]) -> serde_json::Map<String, Value> {
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

/// Up to `max` ids spread evenly across `ids`, instead of the first `max`.
/// The backend lists zones in id order, which is spatially clustered, so
/// truncating to the head of the list samples one corner of the area.
pub(super) fn sample_evenly(ids: Vec<String>, max: usize) -> Vec<String> {
    if ids.len() <= max {
        return ids;
    }
    let step = ids.len() as f64 / max as f64;
    (0..max).map(|i| ids[(i as f64 * step) as usize].clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn sample_evenly_spreads_across_the_list() {
        let ids: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        assert_eq!(sample_evenly(ids.clone(), 20), ids);
        assert_eq!(sample_evenly(ids, 5), vec!["0", "2", "4", "6", "8"]);
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
}
