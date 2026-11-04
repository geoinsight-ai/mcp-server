use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::Deserialize;

/// A point query is expanded to a bbox this wide/tall (degrees) before being
/// sent to the backend. The API's bbox filter treats an exact `min == max`
/// box as unbounded and returns effectively the whole grid instead of the
/// single containing zone — this sidesteps that.
const POINT_EPSILON_DEG: f64 = 0.0005;

/// Extent shared by `get_insight` and `get_data`: either a point (`lon`+`lat`,
/// expanded to a small bbox) or an explicit area (all four `min_*`/`max_*`
/// fields).
#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ExtentArgs {
    #[schemars(description = "Point longitude, WGS84 degrees. Provide together with `lat` for a point query; use the bbox fields instead for an area.")]
    lon: Option<f64>,
    #[schemars(description = "Point latitude, WGS84 degrees. Provide together with `lon`.")]
    lat: Option<f64>,

    #[schemars(description = "Bounding box min longitude (west). Provide all four bbox fields together for an area query, instead of lon/lat.")]
    min_lon: Option<f64>,
    #[schemars(description = "Bounding box min latitude (south).")]
    min_lat: Option<f64>,
    #[schemars(description = "Bounding box max longitude (east).")]
    max_lon: Option<f64>,
    #[schemars(description = "Bounding box max latitude (north).")]
    max_lat: Option<f64>,
}

pub(crate) fn build_bbox(extent: &ExtentArgs) -> Result<String, McpError> {
    match (extent.min_lon, extent.min_lat, extent.max_lon, extent.max_lat) {
        (Some(a), Some(b), Some(c), Some(d)) => Ok(format!("{a},{b},{c},{d}")),
        (None, None, None, None) => match (extent.lon, extent.lat) {
            (Some(lon), Some(lat)) => Ok(format!(
                "{},{},{},{}",
                lon - POINT_EPSILON_DEG,
                lat - POINT_EPSILON_DEG,
                lon + POINT_EPSILON_DEG,
                lat + POINT_EPSILON_DEG
            )),
            _ => Err(McpError::invalid_params(
                "provide either lon+lat for a point, or all four of min_lon/min_lat/max_lon/max_lat for an area",
                None,
            )),
        },
        _ => Err(McpError::invalid_params(
            "bbox fields (min_lon/min_lat/max_lon/max_lat) must be given all four together",
            None,
        )),
    }
}

/// The extent as `[min_lon, min_lat, max_lon, max_lat]` degrees (a point as
/// its tiny search box), or `None` when no extent was given at all — for
/// tools where it's optional.
pub(crate) fn optional_bbox(extent: &ExtentArgs) -> Result<Option<[f64; 4]>, McpError> {
    let given = [extent.lon, extent.lat, extent.min_lon, extent.min_lat, extent.max_lon, extent.max_lat];
    if given.iter().all(Option::is_none) {
        return Ok(None);
    }
    let bbox = build_bbox(extent)?;
    let parts: Vec<f64> = bbox.split(',').filter_map(|s| s.parse().ok()).collect();
    Ok(<[f64; 4]>::try_from(parts).ok())
}

/// The centre `(lon, lat)` of a `build_bbox` string.
pub(crate) fn centre_of(bbox: &str) -> Option<(f64, f64)> {
    let n: Vec<f64> = bbox.split(',').filter_map(|s| s.parse().ok()).collect();
    let [min_lon, min_lat, max_lon, max_lat] = n[..] else { return None };
    Some(((min_lon + max_lon) / 2.0, (min_lat + max_lat) / 2.0))
}

/// Metres per degree of latitude, and of longitude at the equator.
const METRES_PER_DEGREE: f64 = 111_320.0;

/// How wide the place is, m: the longer side of the bbox, with longitude
/// shrunk by latitude. `None` for a point, which has no width.
pub(crate) fn width_m(extent: &ExtentArgs) -> Option<f64> {
    let (Some(min_lon), Some(min_lat), Some(max_lon), Some(max_lat)) =
        (extent.min_lon, extent.min_lat, extent.max_lon, extent.max_lat)
    else {
        return None;
    };
    let mid_lat = ((min_lat + max_lat) / 2.0).to_radians();
    let east_west = (max_lon - min_lon).abs() * METRES_PER_DEGREE * mid_lat.cos();
    let north_south = (max_lat - min_lat).abs() * METRES_PER_DEGREE;
    Some(east_west.max(north_south))
}

/// `Some((lon, lat))` when the extent is a point rather than a bbox.
pub(crate) fn point_of(extent: &ExtentArgs) -> Option<(f64, f64)> {
    match (extent.min_lon, extent.min_lat, extent.max_lon, extent.max_lat) {
        (None, None, None, None) => Some((extent.lon?, extent.lat?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(
        lon: Option<f64>,
        lat: Option<f64>,
        min_lon: Option<f64>,
        min_lat: Option<f64>,
        max_lon: Option<f64>,
        max_lat: Option<f64>,
    ) -> ExtentArgs {
        ExtentArgs {
            lon,
            lat,
            min_lon,
            min_lat,
            max_lon,
            max_lat,
        }
    }

    #[test]
    fn point_expands_to_a_tiny_bbox_not_a_degenerate_one() {
        let bbox = build_bbox(&req(Some(-9.14), Some(38.72), None, None, None, None)).unwrap();
        let parts: Vec<f64> = bbox.split(',').map(|s| s.parse().unwrap()).collect();
        assert_eq!(parts.len(), 4);
        assert!(parts[0] < parts[2], "min_lon must be < max_lon, not degenerate");
        assert!(parts[1] < parts[3], "min_lat must be < max_lat, not degenerate");
    }

    #[test]
    fn explicit_bbox_passes_through_unchanged() {
        let bbox = build_bbox(&req(None, None, Some(1.0), Some(2.0), Some(3.0), Some(4.0))).unwrap();
        assert_eq!(bbox, "1,2,3,4");
    }

    #[test]
    fn neither_point_nor_bbox_is_rejected() {
        assert!(build_bbox(&req(None, None, None, None, None, None)).is_err());
    }

    #[test]
    fn partial_bbox_is_rejected() {
        assert!(build_bbox(&req(None, None, Some(1.0), Some(2.0), None, None)).is_err());
    }

    #[test]
    fn width_is_the_longer_side_in_metres() {
        // 0.3° of longitude at ~52.9°N (~20 km) vs 0.15° of latitude (~16.7 km).
        let width = width_m(&req(None, None, Some(-118.2), Some(52.8), Some(-117.9), Some(52.95))).unwrap();
        assert!((width - 20_150.0).abs() < 200.0, "{width}");
        assert_eq!(width_m(&req(Some(1.0), Some(2.0), None, None, None, None)), None);
    }

    #[test]
    fn an_optional_extent_can_be_left_out() {
        assert_eq!(optional_bbox(&req(None, None, None, None, None, None)).unwrap(), None);
        assert_eq!(
            optional_bbox(&req(None, None, Some(1.0), Some(2.0), Some(3.0), Some(4.0))).unwrap(),
            Some([1.0, 2.0, 3.0, 4.0])
        );
        assert!(optional_bbox(&req(Some(1.0), None, None, None, None, None)).is_err());
    }

    #[test]
    fn centre_of_a_bbox_is_its_midpoint() {
        let (lon, lat) = centre_of("-118.2,52.8,-118,53").unwrap();
        assert!((lon + 118.1).abs() < 1e-9 && (lat - 52.9).abs() < 1e-9);
        assert!(centre_of("1,2,3").is_none());
    }

    #[test]
    fn point_of_only_for_points() {
        assert_eq!(point_of(&req(Some(1.0), Some(2.0), None, None, None, None)), Some((1.0, 2.0)));
        assert_eq!(point_of(&req(None, None, Some(1.0), Some(2.0), Some(3.0), Some(4.0))), None);
    }
}
