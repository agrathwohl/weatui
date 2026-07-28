//! Radar ingest and resampling.
//!
//! Everything above this module sees only [`ReflectivityField`] and [`DbzGrid`].
//! The nexrad crates are 1.0.0-rc, so confining them behind this trait keeps a
//! breaking pre-release change from reaching the render or alert layers.

pub mod fetch;
pub mod grid;
pub mod ring;

use crate::geo::Coords;

pub trait ReflectivityField: Send + Sync {
    fn dbz_at(&self, at: Coords) -> Option<f32>;
    fn site_id(&self) -> &str;
    fn site_coords(&self) -> Coords;
    fn max_range_km(&self) -> f64;
    fn elevation_degrees(&self) -> f32;
}

/// Whole-field advection along a storm motion vector.
///
/// NEXRAD is observed-only, so the sole honest way to show "upcoming" is to
/// translate the most recent observation along the motion NWS published and
/// label the result as projected. Storms rotate, grow and decay; this does
/// none of that. It answers "where would this be if nothing changed", which is
/// useful for lead time and is not a forecast.
pub struct AdvectedField {
    base: std::sync::Arc<dyn ReflectivityField>,
    north_km: f64,
    east_km: f64,
}

impl AdvectedField {
    pub fn new(
        base: std::sync::Arc<dyn ReflectivityField>,
        heading_deg: f64,
        speed_kt: f64,
        minutes: f64,
    ) -> Self {
        let km = speed_kt * crate::geo::KM_PER_KNOT_HOUR * (minutes / 60.0);
        let theta = heading_deg.to_radians();
        AdvectedField { base, north_km: km * theta.cos(), east_km: km * theta.sin() }
    }
}

impl ReflectivityField for AdvectedField {
    /// Sampling the source at `at` minus the displacement moves the echo
    /// forward along the vector.
    fn dbz_at(&self, at: Coords) -> Option<f32> {
        const KM_PER_DEG_LAT: f64 = 111.19492664455873;
        let lon_scale = KM_PER_DEG_LAT * at.lat.to_radians().cos();
        let source = Coords {
            lat: at.lat - self.north_km / KM_PER_DEG_LAT,
            lon: at.lon
                - if lon_scale.abs() < f64::EPSILON { 0.0 } else { self.east_km / lon_scale },
        };
        self.base.dbz_at(source)
    }

    fn site_id(&self) -> &str {
        self.base.site_id()
    }
    fn site_coords(&self) -> Coords {
        self.base.site_coords()
    }
    fn max_range_km(&self) -> f64 {
        self.base.max_range_km()
    }
    fn elevation_degrees(&self) -> f32 {
        self.base.elevation_degrees()
    }
}

/// A rectangular half-block pixel grid. `height` counts pixels, so a terminal
/// of R rows renders 2R of them.
#[derive(Debug, Clone)]
pub struct DbzGrid {
    pub width: usize,
    pub height: usize,
    cells: Vec<Option<f32>>,
}

impl DbzGrid {
    pub fn new(width: usize, height: usize) -> Self {
        DbzGrid { width, height, cells: vec![None; width * height] }
    }

    pub fn get(&self, x: usize, y: usize) -> Option<f32> {
        if x >= self.width || y >= self.height {
            return None;
        }
        self.cells[y * self.width + x]
    }

    pub fn set(&mut self, x: usize, y: usize, value: Option<f32>) {
        if x < self.width && y < self.height {
            self.cells[y * self.width + x] = value;
        }
    }

    #[cfg(test)]
    pub fn populated_cells(&self) -> usize {
        self.cells.iter().filter(|c| c.is_some()).count()
    }

    pub fn value_range(&self) -> Option<(f32, f32)> {
        let mut it = self.cells.iter().flatten().copied();
        let first = it.next()?;
        Some(it.fold((first, first), |(lo, hi), v| (lo.min(v), hi.max(v))))
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A field returning a fixed value inside a radius, for grid tests that
    /// must not depend on network access or the nexrad crates.
    pub struct DiskField {
        pub centre: Coords,
        pub radius_km: f64,
        pub dbz: f32,
    }

    impl ReflectivityField for DiskField {
        fn dbz_at(&self, at: Coords) -> Option<f32> {
            if crate::geo::haversine_km(self.centre, at) <= self.radius_km {
                Some(self.dbz)
            } else {
                None
            }
        }
        fn site_id(&self) -> &str {
            "TEST"
        }
        fn site_coords(&self) -> Coords {
            self.centre
        }
        fn max_range_km(&self) -> f64 {
            self.radius_km
        }
        fn elevation_degrees(&self) -> f32 {
            0.5
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_grid_is_entirely_empty() {
        let g = DbzGrid::new(4, 6);
        assert_eq!(g.populated_cells(), 0);
        assert_eq!(g.value_range(), None);
        assert_eq!(g.get(0, 0), None);
    }

    #[test]
    fn set_and_get_round_trip() {
        let mut g = DbzGrid::new(4, 6);
        g.set(2, 3, Some(45.5));
        assert_eq!(g.get(2, 3), Some(45.5));
        assert_eq!(g.populated_cells(), 1);
    }

    #[test]
    fn out_of_bounds_access_is_none_rather_than_a_panic() {
        let mut g = DbzGrid::new(4, 6);
        g.set(99, 99, Some(10.0));
        assert_eq!(g.get(99, 99), None);
        assert_eq!(g.get(4, 0), None);
        assert_eq!(g.get(0, 6), None);
        assert_eq!(g.populated_cells(), 0);
    }

    #[test]
    fn advection_moves_the_echo_along_the_heading_not_against_it() {
        use crate::radar::testing::DiskField;
        use std::sync::Arc;

        let origin = Coords { lat: 35.0, lon: -97.0 };
        let base = Arc::new(DiskField { centre: origin, radius_km: 10.0, dbz: 45.0 });

        // 60 kt for 30 min is 60 * 1.852 * 0.5 = 55.56 km, not 30 km.
        let advected = AdvectedField::new(base.clone(), 0.0, 60.0, 30.0);
        let displaced = Coords { lat: 35.0 + 55.56 / 111.19492664455873, lon: -97.0 };
        assert_eq!(
            advected.dbz_at(displaced),
            Some(45.0),
            "a storm heading due north must appear north of where it was"
        );
        assert_eq!(advected.dbz_at(origin), None, "it should have vacated the origin");
    }

    #[test]
    fn advection_by_zero_minutes_is_the_identity() {
        use crate::radar::testing::DiskField;
        use std::sync::Arc;

        let origin = Coords { lat: 35.0, lon: -97.0 };
        let base = Arc::new(DiskField { centre: origin, radius_km: 10.0, dbz: 45.0 });
        let advected = AdvectedField::new(base, 137.0, 42.0, 0.0);
        assert_eq!(advected.dbz_at(origin), Some(45.0));
    }

    #[test]
    fn advection_preserves_the_underlying_site_metadata() {
        use crate::radar::testing::DiskField;
        use std::sync::Arc;

        let base = Arc::new(DiskField {
            centre: Coords { lat: 35.0, lon: -97.0 },
            radius_km: 10.0,
            dbz: 45.0,
        });
        let advected = AdvectedField::new(base, 90.0, 30.0, 15.0);
        assert_eq!(advected.site_id(), "TEST");
        assert!((advected.elevation_degrees() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn eastward_advection_moves_east() {
        use crate::radar::testing::DiskField;
        use std::sync::Arc;

        let origin = Coords { lat: 35.0, lon: -97.0 };
        let base = Arc::new(DiskField { centre: origin, radius_km: 5.0, dbz: 50.0 });
        let advected = AdvectedField::new(base, 90.0, 60.0, 60.0);
        let km_per_deg_lon = 111.19492664455873 * 35.0_f64.to_radians().cos();
        let east = Coords { lat: 35.0, lon: -97.0 + 111.19492664455873 / km_per_deg_lon };
        assert!(advected.dbz_at(east).is_some(), "should have advected roughly 111 km east");
    }

    #[test]
    fn value_range_spans_only_populated_cells() {
        let mut g = DbzGrid::new(3, 3);
        g.set(0, 0, Some(10.0));
        g.set(1, 1, Some(60.0));
        g.set(2, 2, Some(35.0));
        assert_eq!(g.value_range(), Some((10.0, 60.0)));
    }
}
