use rmcp::ErrorData as McpError;
use serde_json::Value;

use super::extent::{ExtentArgs, build_bbox, centre_of, point_of};
use super::level::Grid;
use crate::server::{DggsServer, enc};

/// The parent zones covering a place, and how many of them the map gets.
pub(crate) struct ParentZones {
    pub(crate) ids: Vec<String>,
    pub(crate) limit: usize,
}

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

    /// What the level choice needs to know about a grid, from its
    /// description (`GET /dggs/{id}` → `defaultDepth`, `maxRefinementLevel`).
    /// A grid that can't be described falls back to IVEA7H's values rather
    /// than failing the question.
    pub(crate) async fn grid(&self, dggrs_id: &str, caller_token: Option<&str>) -> Grid {
        let path = format!("/dggs/{}", enc(dggrs_id));
        let body = self.client.get_json(&path, &[], caller_token).await.ok();
        let number = |key: &str| {
            body.as_ref()
                .and_then(|b| b.get(key))
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
        };
        Grid::new(dggrs_id, number("defaultDepth"), number("maxRefinementLevel"))
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

    /// The parent zones the map loads, for `extent_zone_ids`' extent at
    /// `zone_level`. When there are more than `limit` (the map only gets that
    /// many), they're ordered nearest the centre of the place first, so the
    /// map shows where the user asked about rather than the first zones in
    /// the API's id order (which skipped half of Jasper town).
    pub(crate) async fn map_parent_ids(
        &self,
        collection_id: &str,
        dggrs_id: &str,
        extent: &ExtentArgs,
        zone_level: u32,
        limit: usize,
        caller_token: Option<&str>,
    ) -> Result<ParentZones, McpError> {
        let bbox = build_bbox(extent)?;
        let mut ids = self
            .extent_zone_ids(collection_id, dggrs_id, extent, &bbox, zone_level, caller_token)
            .await?;
        if ids.len() <= limit {
            return Ok(ParentZones { ids, limit });
        }
        let Some(centre) = centre_of(&bbox) else { return Ok(ParentZones { ids, limit }) };
        // Only an ordering hint: if the polygons can't be read, the API's
        // order stands.
        if let Ok(centres) = self.zone_centres(collection_id, dggrs_id, &bbox, zone_level, caller_token).await {
            let distance = |id: &String| {
                centres
                    .iter()
                    .find(|(zone, _)| zone == id)
                    .map_or(f64::INFINITY, |(_, c)| squared_distance(*c, centre))
            };
            ids.sort_by(|a, b| distance(a).total_cmp(&distance(b)));
        }
        Ok(ParentZones { ids, limit })
    }

    /// Each zone at `zone_level` intersecting `bbox` with the mean of its
    /// polygon's vertices, close enough to its centre for ordering zones by
    /// nearness.
    async fn zone_centres(
        &self,
        collection_id: &str,
        dggrs_id: &str,
        bbox: &str,
        zone_level: u32,
        caller_token: Option<&str>,
    ) -> Result<Vec<(String, (f64, f64))>, McpError> {
        let path = format!("/collections/{}/dggs/{}/zones", enc(collection_id), enc(dggrs_id));
        let query = vec![
            ("bbox", bbox.to_string()),
            ("zone-level", zone_level.to_string()),
            ("f", "geojson".to_string()),
        ];
        let body = self.client.get_json(&path, &query, caller_token).await?;
        let features = body.get("features").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(features
            .iter()
            .filter_map(|f| {
                let id = f.get("id")?.as_str()?.to_string();
                let ring = f.pointer("/geometry/coordinates/0")?.as_array()?;
                Some((id, ring_centre(ring)?))
            })
            .collect())
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

/// The mean of a GeoJSON ring's vertices (the closing vertex repeats the
/// first, which barely shifts it).
fn ring_centre(ring: &[Value]) -> Option<(f64, f64)> {
    let points: Vec<(f64, f64)> = ring
        .iter()
        .filter_map(|p| Some((p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
        .collect();
    let n = points.len() as f64;
    (n > 0.0).then(|| {
        let (lon, lat) = points.iter().fold((0.0, 0.0), |(x, y), (px, py)| (x + px, y + py));
        (lon / n, lat / n)
    })
}

/// Squared distance between two lon/lat points, with longitude shrunk by
/// latitude so east-west and north-south compare fairly. Only for ordering.
fn squared_distance((lon_a, lat_a): (f64, f64), (lon_b, lat_b): (f64, f64)) -> f64 {
    let shrink = ((lat_a + lat_b) / 2.0).to_radians().cos();
    let (dx, dy) = ((lon_a - lon_b) * shrink, lat_a - lat_b);
    dx * dx + dy * dy
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
    fn nearer_zones_sort_first() {
        let ring = serde_json::json!([[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]]);
        assert_eq!(ring_centre(ring.as_array().unwrap()), Some((1.0, 1.0)));
        // At 60°N a degree of longitude is half a degree of latitude.
        let east = squared_distance((1.0, 60.0), (0.0, 60.0));
        let north = squared_distance((0.0, 60.6), (0.0, 60.0));
        assert!(east < north);
    }

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
