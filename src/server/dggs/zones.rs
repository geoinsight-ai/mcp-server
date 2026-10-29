use rmcp::ErrorData as McpError;
use serde_json::Value;

use super::extent::{ExtentArgs, point_of};
use crate::server::{DggsServer, enc};

/// The backend's zone-depth expansion when a grid doesn't report its own
/// `default_depth` (IVEA7H reports 6).
const FALLBACK_DEFAULT_DEPTH: u32 = 6;

/// DGGRS ids to prefer, in order, when a collection supports several and the
/// caller didn't pin one. See `default_dggrs`.
const PREFERRED_DGGRS: &[&str] = &["IVEA7H"];

impl DggsServer {
    /// The first DGGRS a collection supports, used as the default grid when
    /// the caller doesn't pin one.
    pub(crate) async fn default_dggrs(&self, collection_id: &str, caller_token: Option<&str>) -> Result<String, McpError> {
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

    /// How many levels the backend expands a zone by when `zone-depth` is
    /// omitted (`GET /dggs/{id}` → `default_depth`). Parquet requests omit it so the
    /// backend's cache applies; this tells us which level that lands on.
    pub(crate) async fn default_depth(&self, dggrs_id: &str, caller_token: Option<&str>) -> u32 {
        let path = format!("/dggs/{}", enc(dggrs_id));
        self.client
            .get_json(&path, &[], caller_token)
            .await
            .ok()
            .and_then(|body| body.get("default_depth").and_then(Value::as_u64))
            .map(|d| d as u32)
            .unwrap_or(FALLBACK_DEFAULT_DEPTH)
    }

    /// Zones covering the extent at `zone_level`: the one containing a point,
    /// or every zone intersecting a bbox.
    pub(crate) async fn extent_zone_ids(
        &self,
        collection_id: &str,
        dggrs_id: &str,
        extent: &ExtentArgs,
        bbox: &str,
        zone_level: u32,
        caller_token: Option<&str>,
    ) -> Result<Vec<String>, McpError> {
        match point_of(extent) {
            Some(point) => {
                self.point_zone_ids(collection_id, dggrs_id, bbox, zone_level, point, caller_token)
                    .await
            }
            None => self.list_zone_ids(collection_id, dggrs_id, bbox, zone_level, caller_token).await,
        }
    }

    /// Zone ids at `zone_level` intersecting `bbox`, within a collection's grid.
    pub(crate) async fn list_zone_ids(
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

    /// The one zone at `zone_level` whose polygon contains (`lon`, `lat`).
    /// `list_zone_ids` on a point's epsilon box typically returns the 2-3
    /// neighbouring zones that box overlaps; asking for their polygons
    /// (`f=geojson`) lets a point query answer for the zone the point is
    /// actually in. Falls back to every overlapping zone if none contains it
    /// (a point exactly on a shared edge).
    pub(crate) async fn point_zone_ids(
        &self,
        collection_id: &str,
        dggrs_id: &str,
        bbox: &str,
        zone_level: u32,
        (lon, lat): (f64, f64),
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
            ("f", "geojson".to_string()),
        ];
        let body = self.client.get_json(&path, &query, caller_token).await?;
        let features = body
            .get("features")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let id_of = |f: &Value| f.get("id").and_then(Value::as_str).map(str::to_string);
        let containing = features.iter().find(|f| {
            f.pointer("/geometry/coordinates/0")
                .and_then(Value::as_array)
                .is_some_and(|ring| ring_contains(ring, lon, lat))
        });
        Ok(match containing.and_then(id_of) {
            Some(id) => vec![id],
            None => features.iter().filter_map(id_of).collect(),
        })
    }
}

/// Ray-casting point-in-polygon test against a GeoJSON linear ring
/// (`[[lon, lat], ...]`). Ignores antimeridian wrap, which no zone this
/// server samples at a point query's resolution straddles in practice.
fn ring_contains(ring: &[Value], lon: f64, lat: f64) -> bool {
    let points: Vec<(f64, f64)> = ring
        .iter()
        .filter_map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
        .collect();
    let mut inside = false;
    for (i, &(xi, yi)) in points.iter().enumerate() {
        let (xj, yj) = points[(i + points.len() - 1) % points.len()];
        if (yi > lat) != (yj > lat) && lon < (xj - xi) * (lat - yi) / (yj - yi) + xi {
            inside = !inside;
        }
    }
    inside
}

/// Up to `max` ids spread evenly across `ids`, instead of the first `max`.
/// The backend lists zones in id order, which is spatially clustered, so
/// truncating to the head of the list samples one corner of the area.
pub(crate) fn sample_evenly(ids: Vec<String>, max: usize) -> Vec<String> {
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
    fn ring_contains_distinguishes_inside_from_outside() {
        let square: Vec<Value> = serde_json::from_str("[[0,0],[2,0],[2,2],[0,2],[0,0]]").unwrap();
        assert!(ring_contains(&square, 1.0, 1.0));
        assert!(!ring_contains(&square, 3.0, 1.0));
        assert!(!ring_contains(&square, 1.0, -0.5));
    }

    #[test]
    fn sample_evenly_spreads_across_the_list() {
        let ids: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        assert_eq!(sample_evenly(ids.clone(), 20), ids);
        assert_eq!(sample_evenly(ids, 5), vec!["0", "2", "4", "6", "8"]);
    }
}
