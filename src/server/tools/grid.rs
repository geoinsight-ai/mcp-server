use rmcp::handler::server::wrapper::Parameters;
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::server::dggs::extent::{ExtentArgs, build_bbox, width_m};
use crate::server::dggs::level::Grid;
use crate::server::{DggsServer, caller_bearer_token, enc, pretty};

/// The grid used when a caller doesn't name one: explore's and most
/// collections' default (see `PREFERRED_DGGRS`).
const DEFAULT_DGGRS: &str = "IVEA7H";

/// `list_zones` returns at most this many zone ids; past it, the count and
/// a `truncated` flag, so a wide area at a fine level can't flood the
/// model's context.
const MAX_LISTED_ZONES: usize = 100;

#[derive(Debug, Deserialize, JsonSchema)]
struct GetZoneRequest {
    #[schemars(description = "Zone id, as returned by list_zones, get_zone (parent/children/neighbors) or get_data (zone_id).")]
    zone_id: String,
    #[schemars(description = "DGGRS grid the zone belongs to, e.g. \"IVEA7H\" (the default). Zone ids are only meaningful within their grid.")]
    dggrs_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListZonesRequest {
    #[serde(flatten)]
    extent: ExtentArgs,
    #[schemars(description = "Zone level (resolution) to list. Omit to get the place's own level: the finest whose zones still span it, so a few zones cover it; a point then gets the ~1 km zone containing it. Each level down is `aperture` times smaller (7 on IVEA7H).")]
    zone_level: Option<u32>,
    #[schemars(description = "DGGRS grid, e.g. \"IVEA7H\" (the default).")]
    dggrs_id: Option<String>,
}

/// The zone ids a zone-info response links to with `rel` (the OGC relation
/// name's last part: `dggrs-zone-parent`, `-child`, `-neighbor`), taken from
/// each link's last path segment.
fn linked_zones(info: &Value, rel: &str) -> Vec<String> {
    info.get("links")
        .and_then(Value::as_array)
        .map(|links| {
            links
                .iter()
                .filter(|l| l.get("rel").and_then(Value::as_str).is_some_and(|r| r.ends_with(rel)))
                .filter_map(|l| l.get("href")?.as_str()?.rsplit('/').next().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The zone's level: the API's own, once zone info reports it (`level`, or
/// `zoneLevel`), else estimated from the zone's area (exact on the
/// IVEA/ISEA hexagon grids, approximate on e.g. H3).
fn zone_level(info: &Value, grid: &Grid, area_m2: f64) -> u32 {
    ["level", "zoneLevel"]
        .iter()
        .find_map(|key| info.get(*key)?.as_u64())
        .and_then(|level| u32::try_from(level).ok())
        .unwrap_or_else(|| grid.level_for_area(area_m2))
}

/// `GET /dggs/{id}/zones/{zone}` reduced to what navigating needs: where the
/// zone is, how big, and its parent, children and neighbours. The polygon
/// is left out (the bbox locates it).
fn zone_summary(dggrs_id: &str, zone_id: &str, info: &Value, level: Option<u32>) -> Value {
    let round = |v: &Value| v.as_f64().map(|n| (n * 1e5).round() / 1e5);
    let coords = |key: &str| {
        info.get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|v| serde_json::json!(round(v))).collect::<Vec<_>>())
    };
    let area = info.get("areaMetersSquare").and_then(Value::as_f64);
    serde_json::json!({
        "dggrs_id": dggrs_id,
        "zone_id": info.get("id").and_then(Value::as_str).unwrap_or(zone_id),
        "zone_level": level,
        "centroid": coords("centroid"),
        "bbox": coords("bbox"),
        "area_km2": area.map(|a| (a / 1e4).round() / 100.0),
        "width_m": area.map(|a| a.sqrt().round()),
        "parent": linked_zones(info, "dggrs-zone-parent").into_iter().next(),
        "children": linked_zones(info, "dggrs-zone-child"),
        "neighbors": linked_zones(info, "dggrs-zone-neighbor"),
    })
}

#[tool_router(router = grid_router, vis = "pub(crate)")]
impl DggsServer {
    #[tool(
        description = "Describe one DGGS zone without reading any data: its level, centroid (lon, lat), bbox, area and width, and the ids of its parent (one level coarser), children (the zones one level finer that overlap it — 13 on IVEA7H, not just the 7 inside it) and neighbours (same level). Use it to move around the grid — up to a wider zone, down into finer ones, or sideways — before deciding where to query data. Works without Auth0 configured."
    )]
    async fn get_zone(
        &self,
        Parameters(req): Parameters<GetZoneRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, McpError> {
        let caller_token = caller_bearer_token(&ctx);
        let caller_token = caller_token.as_deref();
        let dggrs_id = req.dggrs_id.as_deref().unwrap_or(DEFAULT_DGGRS);
        let path = format!("/dggs/{}/zones/{}", enc(dggrs_id), enc(&req.zone_id));
        let (info, grid) = futures::join!(self.client.get_json(&path, &[], caller_token), self.grid(dggrs_id, caller_token));
        let info = info?;
        // An id the grid doesn't know still comes back 200, as a zone with
        // no area (`-0`) and id "(null)".
        let Some(area) = info.get("areaMetersSquare").and_then(Value::as_f64).filter(|a| *a > 0.0) else {
            return Err(McpError::invalid_params(
                format!("\"{}\" is not a zone of {dggrs_id}", req.zone_id),
                None,
            ));
        };
        Ok(pretty(&zone_summary(dggrs_id, &req.zone_id, &info, Some(zone_level(&info, &grid, area)))))
    }

    #[tool(
        description = "List the DGGS zones covering a place, without reading any data: the zone containing a point (lon/lat), or every zone intersecting a bbox, at one level. Omit zone_level to start at the place's own level (a few zones cover it), then go finer with a higher level or get_zone's children. Returns the zone ids (at most 100; `zones_matched` and `truncated` say if there were more) and the level's typical zone width. Works without Auth0 configured."
    )]
    async fn list_zones(
        &self,
        Parameters(req): Parameters<ListZonesRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, McpError> {
        let caller_token = caller_bearer_token(&ctx);
        let caller_token = caller_token.as_deref();
        let dggrs_id = req.dggrs_id.as_deref().unwrap_or(DEFAULT_DGGRS);
        let bbox = build_bbox(&req.extent)?;
        let grid = self.grid(dggrs_id, caller_token).await;
        let zone_level = req.zone_level.unwrap_or_else(|| grid.place_level(width_m(&req.extent)));
        if zone_level > grid.max_level {
            return Err(McpError::invalid_params(
                format!("zone_level {zone_level} is past {dggrs_id}'s finest level, {}", grid.max_level),
                None,
            ));
        }

        let zones = self
            .extent_zone_ids(None, dggrs_id, &req.extent, &bbox, zone_level, caller_token)
            .await?;
        let mut result = serde_json::json!({
            "dggrs_id": dggrs_id,
            "zone_level": zone_level,
            "zone_width_m": grid.zone_width_m(zone_level).round(),
            "zones_matched": zones.len(),
        });
        if zones.len() > MAX_LISTED_ZONES {
            result["truncated"] = Value::Bool(true);
        }
        result["zones"] = serde_json::json!(zones.into_iter().take(MAX_LISTED_ZONES).collect::<Vec<_>>());
        Ok(pretty(&result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_apis_level_wins_over_the_area_estimate() {
        let grid = Grid::new("IVEA7H", Some(6), Some(20));
        let area = 61_935_517.9;
        assert_eq!(zone_level(&serde_json::json!({}), &grid, area), 7);
        assert_eq!(zone_level(&serde_json::json!({"level": 8}), &grid, area), 8);
        assert_eq!(zone_level(&serde_json::json!({"zoneLevel": 6}), &grid, area), 6);
    }

    #[test]
    fn zone_info_is_reduced_to_position_size_and_relatives() {
        let info = serde_json::json!({
            "id": "H0-D99F-F",
            "centroid": [-118.045675265, 52.907523324],
            "bbox": [-118.1115, 52.862, -117.97979, 52.95305],
            "areaMetersSquare": 61935517.877,
            "geometry": {"type": "Polygon", "coordinates": [[]]},
            "links": [
                {"href": "/dggs/IVEA7H/zones/H0-D99F-F", "rel": "self"},
                {"href": "/dggs/IVEA7H/zones/G0-D99F-A", "rel": "http://www.opengis.net/def/rel/ogc/1.0/dggrs-zone-parent"},
                {"href": "/dggs/IVEA7H/zones/I0-29ABC9-A", "rel": "http://www.opengis.net/def/rel/ogc/1.0/dggrs-zone-child"},
                {"href": "/dggs/IVEA7H/zones/H0-D99F-B", "rel": "http://www.opengis.net/def/rel/ogc/1.0/dggrs-zone-neighbor"},
            ],
        });
        let summary = zone_summary("IVEA7H", "H0-D99F-F", &info, Some(7));
        assert_eq!(summary["zone_level"], 7);
        assert_eq!(summary["centroid"], serde_json::json!([-118.04568, 52.90752]));
        assert_eq!(summary["area_km2"], 61.94);
        assert_eq!(summary["width_m"], 7870.0);
        assert_eq!(summary["parent"], "G0-D99F-A");
        assert_eq!(summary["children"], serde_json::json!(["I0-29ABC9-A"]));
        assert_eq!(summary["neighbors"], serde_json::json!(["H0-D99F-B"]));
        assert!(summary.get("geometry").is_none());
    }
}
