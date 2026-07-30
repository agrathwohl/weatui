//! Radar ingest and resampling.
//!
//! Everything above this module sees only [`ReflectivityField`] and [`DbzGrid`].
//! The nexrad crates are 1.0.0-rc, so confining them behind this trait keeps a
//! breaking pre-release change from reaching the render or alert layers.

pub mod fetch;
pub mod hrrr;
pub mod grid;
pub mod ring;

use crate::geo::Coords;

/// A field of reflectivity that can be sampled anywhere.
///
/// Deliberately carries no site or range: those are properties of a radar, and
/// an HRRR forecast grid has neither. Keeping them here forced the forecast
/// implementation to invent values, which is how a caller ends up drawing
/// coverage geometry for something that has no coverage.
pub trait ReflectivityField: Send + Sync {
    fn dbz_at(&self, at: Coords) -> Option<f32>;
    fn source_label(&self) -> &str;
    fn elevation_degrees(&self) -> f32;
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
        fn source_label(&self) -> &str {
            "TEST"
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
    fn value_range_spans_only_populated_cells() {
        let mut g = DbzGrid::new(3, 3);
        g.set(0, 0, Some(10.0));
        g.set(1, 1, Some(60.0));
        g.set(2, 2, Some(35.0));
        assert_eq!(g.value_range(), Some((10.0, 60.0)));
    }
}
