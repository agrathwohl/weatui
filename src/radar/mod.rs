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

/// A selectable radar moment.
///
/// A forecast grid only carries reflectivity, so `supports` lets the display
/// tell "this product is empty here" apart from "this source cannot produce
/// this product at all".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadarProduct {
    Reflectivity,
    Velocity,
    CorrelationCoefficient,
    DifferentialReflectivity,
    SpectrumWidth,
}

impl RadarProduct {
    pub const ALL: &'static [RadarProduct] = &[
        RadarProduct::Reflectivity,
        RadarProduct::Velocity,
        RadarProduct::CorrelationCoefficient,
        RadarProduct::DifferentialReflectivity,
        RadarProduct::SpectrumWidth,
    ];

    pub fn label(self) -> &'static str {
        match self {
            RadarProduct::Reflectivity => "reflectivity",
            RadarProduct::Velocity => "velocity",
            RadarProduct::CorrelationCoefficient => "corr coeff",
            RadarProduct::DifferentialReflectivity => "diff refl",
            RadarProduct::SpectrumWidth => "spectrum width",
        }
    }

    pub fn units(self) -> &'static str {
        match self {
            RadarProduct::Reflectivity => "dBZ",
            RadarProduct::Velocity => "m/s",
            RadarProduct::CorrelationCoefficient => "",
            RadarProduct::DifferentialReflectivity => "dB",
            RadarProduct::SpectrumWidth => "m/s",
        }
    }

    pub fn next(self) -> Self {
        let all = Self::ALL;
        let i = all.iter().position(|p| *p == self).unwrap_or(0);
        all[(i + 1) % all.len()]
    }

    /// How a column of elevation cuts collapses to one value.
    ///
    /// Not uniform across products. Reflectivity takes the column maximum
    /// because that is composite reflectivity, which is what HRRR forecasts.
    /// Correlation takes the minimum, because a debris signature is a
    /// *collapse* in correlation and averaging would erase it. Velocity and
    /// differential reflectivity come from the lowest cut, since near-ground
    /// rotation and drop shape are what matter and aloft values would mask
    /// them.
    pub fn reduction(self) -> ColumnReduction {
        match self {
            RadarProduct::Reflectivity | RadarProduct::SpectrumWidth => ColumnReduction::Max,
            RadarProduct::CorrelationCoefficient => ColumnReduction::Min,
            RadarProduct::Velocity | RadarProduct::DifferentialReflectivity => {
                ColumnReduction::LowestCut
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnReduction {
    Max,
    Min,
    LowestCut,
}

/// A radar field that can be sampled anywhere for any supported moment.
///
/// Deliberately carries no site or range: those are properties of a radar, and
/// an HRRR forecast grid has neither. Keeping them here forced the forecast
/// implementation to invent values, which is how a caller ends up drawing
/// coverage geometry for something that has no coverage.
pub trait RadarField: Send + Sync {
    fn value_at(&self, at: Coords, product: RadarProduct) -> Option<f32>;
    fn supports(&self, product: RadarProduct) -> bool;
    fn source_label(&self) -> &str;
    fn elevation_degrees(&self) -> f32;

    fn dbz_at(&self, at: Coords) -> Option<f32> {
        self.value_at(at, RadarProduct::Reflectivity)
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

    impl RadarField for DiskField {
        fn value_at(&self, at: Coords, product: RadarProduct) -> Option<f32> {
            if product != RadarProduct::Reflectivity {
                return None;
            }
            if crate::geo::haversine_km(self.centre, at) <= self.radius_km {
                Some(self.dbz)
            } else {
                None
            }
        }

        fn supports(&self, product: RadarProduct) -> bool {
            product == RadarProduct::Reflectivity
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
