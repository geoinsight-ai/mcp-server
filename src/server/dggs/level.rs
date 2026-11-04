//! Which DGGS levels a question is answered and drawn at, derived from the
//! size of the place asked about and the resolution of the data instead of a
//! fixed default: a town and a province, or 10 m imagery and a 1 km model,
//! each get zones that suit them.

/// Earth's surface area, m². Every zone of an equal-area grid at a level gets
/// an even share of it.
const EARTH_AREA_M2: f64 = 5.100_656e14;

/// A point query is treated as a place this wide, m, so it still gets parent
/// zones (and their children) around it on the map.
const POINT_PLACE_M: f64 = 1_000.0;

/// How many levels below the place's own level an area's answer is sampled
/// at: two levels of an aperture-7 grid is ~49 zones across the place, which
/// stays inside `MAX_ZONES_PER_QUERY` while still spreading over the area.
const ANSWER_EXTRA_LEVELS: u32 = 2;

/// Used when a grid's `/dggs/{id}` description can't be read.
const FALLBACK_DEFAULT_DEPTH: u32 = 6;
const FALLBACK_MAX_LEVEL: u32 = 20;
/// IVEA7H, ISEA7H and H3 are all aperture 7.
const FALLBACK_APERTURE: u32 = 7;

/// What the level choice needs to know about a grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Grid {
    /// Each level splits a zone's area this many ways.
    pub(crate) aperture: u32,
    /// Levels the backend expands a parent zone by when `zone-depth` is
    /// omitted (and can serve from its cache).
    pub(crate) default_depth: u32,
    pub(crate) max_level: u32,
}

impl Grid {
    /// `aperture` comes from the grid id (see [`aperture_of`]); the rest from
    /// the grid's own description, `None` where it gave nothing.
    pub(crate) fn new(dggrs_id: &str, default_depth: Option<u32>, max_level: Option<u32>) -> Self {
        Self {
            aperture: aperture_of(dggrs_id).unwrap_or(FALLBACK_APERTURE),
            default_depth: default_depth.unwrap_or(FALLBACK_DEFAULT_DEPTH),
            max_level: max_level.unwrap_or(FALLBACK_MAX_LEVEL),
        }
    }

    /// Typical width, m, of one zone at `level`: the square root of its share
    /// of Earth's area. Icosahedral hexagon grids have `10·aperture^level + 2`
    /// zones per level (12 of them pentagons).
    pub(crate) fn zone_width_m(&self, level: u32) -> f64 {
        let zones = 10.0 * f64::from(self.aperture).powi(level as i32) + 2.0;
        (EARTH_AREA_M2 / zones).sqrt()
    }

    /// The finest level whose zones are still at least `width_m` wide.
    fn finest_level_at_least(&self, width_m: f64) -> u32 {
        (0..=self.max_level)
            .rev()
            .find(|&level| self.zone_width_m(level) >= width_m)
            .unwrap_or(0)
    }

    /// The level of a place `place_width_m` wide (`None` for a point): the
    /// finest whose zones still span it, as `choose_levels` starts from.
    pub(crate) fn place_level(&self, place_width_m: Option<f64>) -> u32 {
        self.finest_level_at_least(place_width_m.unwrap_or(POINT_PLACE_M))
    }

    /// The level whose zones are closest in area to `area_m2`. Zone info
    /// doesn't say its level; on an equal-area grid the area gives it away
    /// (each level is `aperture` times smaller). Only an estimate on grids
    /// that don't follow `zone_width_m`'s count, like H3.
    pub(crate) fn level_for_area(&self, area_m2: f64) -> u32 {
        let width = area_m2.max(0.0).sqrt();
        (0..=self.max_level)
            .min_by(|&a, &b| {
                let off = |level: u32| (self.zone_width_m(level) / width).ln().abs();
                off(a).total_cmp(&off(b))
            })
            .unwrap_or(0)
    }
}

/// The aperture written into a grid id: the digit right before its first
/// `H`, e.g. 7 in `IVEA7H`, 3 in `ISEA3HDGGRID`.
fn aperture_of(dggrs_id: &str) -> Option<u32> {
    let h = dggrs_id.find('H')?;
    dggrs_id[..h].chars().last()?.to_digit(10)
}

/// The levels one question is answered and drawn at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Levels {
    /// Parent zones the map loads, one Parquet request each.
    pub(crate) parent: u32,
    /// What the map draws: each parent expanded to this level.
    pub(crate) children: u32,
    /// Zones the answer for the model is sampled at: the zone containing a
    /// point, or a few dozen across an area.
    pub(crate) answer: u32,
    /// `zone-depth` to send with the Parquet requests: `None` lets the
    /// backend expand by its default depth (and use its cache), which only
    /// reaches `children` when the parent sits that far above it.
    pub(crate) request_depth: Option<u32>,
}

/// Picks the levels for a place `place_width_m` wide (`None` for a point)
/// whose finest data is `finest_gsd_m` per pixel (`None` for vector data,
/// which has no resolution of its own):
///
/// - the place's level is the finest whose zone still spans the place;
/// - children sit the grid's default depth below that, so the parents cover
///   the place and expand by the backend's cached default,
/// - but never finer than the data, where children would only repeat the
///   same pixel; parents then move up to stay that depth above.
///
/// With `data_first` (live imagery, see `map_request_limit`), children sit
/// at the data's own level whatever the place's size: the backend mosaics
/// such data on the spot, and a parent sized to a large place reads every
/// band over far more pixels (all of Sentinel-2 over a level-5 parent ran
/// past the gateway's ~50 s; a level-7 one is 1/49 of that area). The map
/// then shows fewer, smaller parents around the centre of the place.
///
/// `children_override` is the caller's own `zone_level`, when given.
pub(crate) fn choose_levels(
    grid: &Grid,
    place_width_m: Option<f64>,
    finest_gsd_m: Option<f64>,
    children_override: Option<u32>,
    data_first: bool,
) -> Levels {
    let place = grid.finest_level_at_least(place_width_m.unwrap_or(POINT_PLACE_M));
    let data = finest_gsd_m
        .filter(|gsd| *gsd > 0.0)
        .map_or(grid.max_level, |gsd| grid.finest_level_at_least(gsd));

    let by_place = || if data_first { data } else { (place + grid.default_depth).min(data) };
    let children = children_override.unwrap_or_else(by_place).min(grid.max_level);
    let answer = match place_width_m {
        None => children,
        Some(_) => (place + ANSWER_EXTRA_LEVELS).min(children),
    };
    let (parent, request_depth) = if children >= grid.default_depth {
        (children - grid.default_depth, None)
    } else {
        (0, Some(children))
    };

    Levels {
        parent,
        children,
        answer,
        request_depth,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ivea7h() -> Grid {
        Grid::new("IVEA7H", Some(6), Some(20))
    }

    #[test]
    fn aperture_is_read_from_the_grid_id() {
        assert_eq!(aperture_of("IVEA7H"), Some(7));
        assert_eq!(aperture_of("ISEA3HDGGRID"), Some(3));
        assert_eq!(aperture_of("rHEALPix"), None);
        assert_eq!(Grid::new("rHEALPix", None, None).aperture, FALLBACK_APERTURE);
    }

    #[test]
    fn zone_widths_match_the_measured_ivea7h_zones() {
        // Measured from IVEA7H zone polygons: ~20.8 km, ~1.12 km, ~61 m.
        let grid = ivea7h();
        assert!((grid.zone_width_m(6) - 20_800.0).abs() < 300.0);
        assert!((grid.zone_width_m(9) - 1_123.0).abs() < 20.0);
        assert!((grid.zone_width_m(12) - 61.0).abs() < 2.0);
    }

    #[test]
    fn a_zones_level_comes_from_its_area() {
        // H0-D99F-F, an IVEA7H level-7 zone, is 61.9 km².
        assert_eq!(ivea7h().level_for_area(61_935_517.9), 7);
        assert_eq!(ivea7h().level_for_area(1_260_000.0), 9);
        assert_eq!(ivea7h().place_level(None), 9);
        assert_eq!(ivea7h().place_level(Some(28_000.0)), 5);
    }

    #[test]
    fn a_town_on_sentinel_2_stops_at_the_imagery_resolution() {
        // ~3 km town, 10 m pixels: level 7 (~7.9 km) spans it, so the place
        // alone would allow children at 13 (~23 m); 14 (~8.7 m) would be
        // finer than a pixel, so 13 is also the data's limit.
        let levels = choose_levels(&ivea7h(), Some(3_000.0), Some(10.0), None, false);
        assert_eq!(levels, Levels { parent: 7, children: 13, answer: 9, request_depth: None });
    }

    #[test]
    fn coarse_data_pulls_the_children_and_parents_up() {
        // Same town, 1 km model output: children stop at level 9 (~1.1 km)
        // and parents move up to stay six levels above them.
        let levels = choose_levels(&ivea7h(), Some(3_000.0), Some(1_000.0), None, false);
        assert_eq!(levels, Levels { parent: 3, children: 9, answer: 9, request_depth: None });
    }

    #[test]
    fn a_large_vector_area_follows_the_place_size() {
        // ~100 km park, no pixel size: level 4 spans it, children 6 below.
        let levels = choose_levels(&ivea7h(), Some(100_000.0), None, None, false);
        assert_eq!(levels, Levels { parent: 4, children: 10, answer: 6, request_depth: None });
    }

    #[test]
    fn a_point_answers_at_the_drawn_level() {
        let levels = choose_levels(&ivea7h(), None, Some(16.0), None, false);
        assert_eq!(levels.children, 13);
        assert_eq!(levels.answer, levels.children);
        assert_eq!(levels.parent, 7);
    }

    #[test]
    fn the_callers_level_wins_and_shallow_levels_send_their_depth() {
        let levels = choose_levels(&ivea7h(), Some(3_000.0), Some(10.0), Some(4), false);
        assert_eq!(levels, Levels { parent: 0, children: 4, answer: 4, request_depth: Some(4) });
    }

    #[test]
    fn live_imagery_is_drawn_at_its_own_resolution() {
        // Jasper's ~28 km bbox would put parents at level 5 by place; 10 m
        // imagery goes to its native level 13 instead, parents at 7.
        let levels = choose_levels(&ivea7h(), Some(28_000.0), Some(10.0), None, true);
        assert_eq!((levels.parent, levels.children, levels.answer), (7, 13, 7));
        let by_place = choose_levels(&ivea7h(), Some(28_000.0), Some(10.0), None, false);
        assert_eq!((by_place.parent, by_place.children), (5, 11));
    }

    #[test]
    fn a_continent_sized_place_starts_at_level_zero() {
        let levels = choose_levels(&ivea7h(), Some(20_000_000.0), None, None, false);
        assert_eq!((levels.parent, levels.children), (0, 6));
    }
}
