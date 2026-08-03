//! Radar ingest and resampling.
//!
//! Everything above this module sees only [`RadarField`] and [`DbzGrid`].
//! The nexrad crates are 1.0.0-rc, so confining them behind this trait keeps a
//! breaking pre-release change from reaching the render or alert layers.

pub mod cells;
pub mod fetch;
pub mod hrrr;
pub mod interp;
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
    EchoTop,
    VerticallyIntegratedLiquid,
}

impl RadarProduct {
    #[cfg(test)]
    pub const ALL: &'static [RadarProduct] = &[
        RadarProduct::Reflectivity,
        RadarProduct::Velocity,
        RadarProduct::CorrelationCoefficient,
        RadarProduct::DifferentialReflectivity,
        RadarProduct::SpectrumWidth,
        RadarProduct::EchoTop,
        RadarProduct::VerticallyIntegratedLiquid,
    ];

    pub fn label(self) -> &'static str {
        match self {
            RadarProduct::Reflectivity => "reflectivity",
            RadarProduct::Velocity => "velocity",
            RadarProduct::CorrelationCoefficient => "corr coeff",
            RadarProduct::DifferentialReflectivity => "diff refl",
            RadarProduct::SpectrumWidth => "spectrum width",
            RadarProduct::EchoTop => "echo top",
            RadarProduct::VerticallyIntegratedLiquid => "VIL",
        }
    }

    pub fn units(self) -> &'static str {
        match self {
            RadarProduct::Reflectivity => "dBZ",
            RadarProduct::Velocity => "m/s",
            RadarProduct::CorrelationCoefficient => "",
            RadarProduct::DifferentialReflectivity => "dB",
            RadarProduct::SpectrumWidth => "m/s",
            RadarProduct::EchoTop => "km",
            RadarProduct::VerticallyIntegratedLiquid => "kg/m2",
        }
    }

    /// Whether a reading earns the right to cover the colour base. An overlay
    /// drawing every gate would just hide it, so each product keeps only its
    /// diagnostic tail: correlation collapse, strong radial motion, broad
    /// spectra.
    pub fn is_notable(self, value: f32) -> bool {
        match self {
            RadarProduct::Reflectivity => value >= 35.0,
            RadarProduct::Velocity => value.abs() >= 20.0,
            RadarProduct::CorrelationCoefficient => {
                value < crate::radar::fetch::DEFAULT_MIN_CORRELATION
            }
            RadarProduct::DifferentialReflectivity => !(-1.0..3.0).contains(&value),
            RadarProduct::SpectrumWidth => value >= 6.0,
            RadarProduct::EchoTop => value >= 9.0,
            RadarProduct::VerticallyIntegratedLiquid => value >= 25.0,
        }
    }

    /// How a column of elevation cuts collapses to one value. Reflectivity
    /// takes the column maximum (composite reflectivity, matching HRRR REFC).
    /// The dual-pol and Doppler moments read the lowest cut: debris, rotation
    /// and drop shape are near-ground phenomena, and a column minimum for
    /// correlation would report the worst gate aloft as ground truth.
    pub fn reduction(self) -> ColumnReduction {
        match self {
            RadarProduct::Reflectivity
            | RadarProduct::SpectrumWidth
            | RadarProduct::EchoTop
            | RadarProduct::VerticallyIntegratedLiquid => ColumnReduction::Max,
            RadarProduct::CorrelationCoefficient
            | RadarProduct::Velocity
            | RadarProduct::DifferentialReflectivity => ColumnReduction::LowestCut,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnReduction {
    Max,
    LowestCut,
}

/// A radar field that can be sampled anywhere for any supported moment.
///
/// Carries no site or range: those are properties of a radar, and
/// an HRRR forecast grid has neither. Keeping them here forced the forecast
/// implementation to invent values, which is how a caller ends up drawing
/// coverage geometry for something that has no coverage.
pub trait RadarField: Send + Sync {
    fn value_at(&self, at: Coords, product: RadarProduct) -> Option<f32>;
    fn supports(&self, product: RadarProduct) -> bool;
    fn source_label(&self) -> &str;
    fn elevation_degrees(&self) -> f32;

    #[cfg(test)]
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
    fn ordinary_readings_are_not_notable_enough_to_cover_the_base() {
        for (product, ordinary) in [
            (RadarProduct::Reflectivity, 20.0),
            (RadarProduct::Velocity, 5.0),
            (RadarProduct::Velocity, -5.0),
            (RadarProduct::CorrelationCoefficient, 0.99),
            (RadarProduct::DifferentialReflectivity, 0.5),
            (RadarProduct::SpectrumWidth, 2.0),
            (RadarProduct::EchoTop, 4.0),
            (RadarProduct::VerticallyIntegratedLiquid, 5.0),
        ] {
            assert!(!product.is_notable(ordinary), "{product:?} at {ordinary}");
        }
    }

    #[test]
    fn diagnostic_readings_are_notable() {
        for (product, diagnostic) in [
            (RadarProduct::Reflectivity, 55.0),
            (RadarProduct::Velocity, 35.0),
            (RadarProduct::Velocity, -35.0),
            (RadarProduct::CorrelationCoefficient, 0.4),
            (RadarProduct::DifferentialReflectivity, 5.0),
            (RadarProduct::DifferentialReflectivity, -2.0),
            (RadarProduct::SpectrumWidth, 9.0),
            (RadarProduct::EchoTop, 12.0),
            (RadarProduct::VerticallyIntegratedLiquid, 45.0),
        ] {
            assert!(product.is_notable(diagnostic), "{product:?} at {diagnostic}");
        }
    }

    /// Velocity is signed, so a threshold applied to the raw value rather than
    /// its magnitude would show outbound rotation and hide inbound.
    #[test]
    fn velocity_notability_is_symmetric_about_zero() {
        for speed in [15.0f32, 20.0, 25.0, 60.0] {
            assert_eq!(
                RadarProduct::Velocity.is_notable(speed),
                RadarProduct::Velocity.is_notable(-speed),
                "{speed} m/s"
            );
        }
    }

    /// Regression: the dual-pol and Doppler moments describe the near-ground
    /// air, so reducing them over the whole column reports a gate kilometres
    /// above the one being asked about.
    #[test]
    fn near_ground_moments_read_the_lowest_cut() {
        for product in [
            RadarProduct::CorrelationCoefficient,
            RadarProduct::Velocity,
            RadarProduct::DifferentialReflectivity,
        ] {
            assert_eq!(product.reduction(), ColumnReduction::LowestCut, "{product:?}");
        }
    }

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
