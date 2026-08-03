//! Motion interpolation between consecutive forecast frames.
//!
//! Hourly forecast steps make storms jump a full hour of travel per frame.
//! These synthetic in-between frames advect both neighbours along one bulk
//! motion vector and blend them, so playback shows movement the model
//! predicted. They are derived frames, not model output: `source_label`
//! says so.

use crate::geo::Coords;
use crate::radar::{RadarField, RadarProduct};
use std::sync::Arc;

/// Sample spacing for motion estimation, degrees (~7 km).
const SHIFT_GRID_STEP: f64 = 0.0625;
const SHIFT_GRID_N: i64 = 24;
/// Search radius in grid cells (~56 km, covers ~120 km/h over 30 min).
const SHIFT_SEARCH: i64 = 8;
/// Echo below this does not vote on motion.
const SHIFT_MIN_DBZ: f32 = 20.0;

pub struct InterpolatedField {
    pub before: Arc<dyn RadarField>,
    pub after: Arc<dyn RadarField>,
    /// 0 at `before`'s valid time, 1 at `after`'s.
    pub alpha: f32,
    /// Bulk feature displacement from `before` to `after`, degrees.
    pub shift: (f64, f64),
    label: String,
}

impl InterpolatedField {
    pub fn new(
        before: Arc<dyn RadarField>,
        after: Arc<dyn RadarField>,
        alpha: f32,
        shift: (f64, f64),
    ) -> Self {
        InterpolatedField { before, after, alpha, shift, label: "HRRR interp".to_string() }
    }
}

impl RadarField for InterpolatedField {
    fn value_at(&self, at: Coords, product: RadarProduct) -> Option<f32> {
        let a = self.alpha as f64;
        let from_before = self.before.value_at(
            Coords { lat: at.lat - a * self.shift.0, lon: at.lon - a * self.shift.1 },
            product,
        );
        let from_after = self.after.value_at(
            Coords {
                lat: at.lat + (1.0 - a) * self.shift.0,
                lon: at.lon + (1.0 - a) * self.shift.1,
            },
            product,
        );
        match (from_before, from_after) {
            (Some(x), Some(y)) => Some(x + (y - x) * self.alpha),
            (Some(x), None) => Some(x * (1.0 - self.alpha)),
            (None, Some(y)) => Some(y * self.alpha),
            (None, None) => None,
        }
    }

    fn supports(&self, product: RadarProduct) -> bool {
        self.before.supports(product) && self.after.supports(product)
    }

    fn source_label(&self) -> &str {
        &self.label
    }

    fn elevation_degrees(&self) -> f32 {
        self.before.elevation_degrees()
    }
}

/// Bulk displacement from `before` to `after` by maximising echo overlap on a
/// coarse grid around `centre`. One vector for the whole field: storms with
/// different individual motions share the mean steering flow closely enough
/// for a display interpolation.
pub fn estimate_shift(
    before: &dyn RadarField,
    after: &dyn RadarField,
    centre: Coords,
) -> (f64, f64) {
    let sample = |field: &dyn RadarField, ix: i64, iy: i64| -> f32 {
        field
            .value_at(
                Coords {
                    lat: centre.lat + iy as f64 * SHIFT_GRID_STEP,
                    lon: centre.lon + ix as f64 * SHIFT_GRID_STEP,
                },
                RadarProduct::Reflectivity,
            )
            .filter(|v| *v >= SHIFT_MIN_DBZ)
            .unwrap_or(0.0)
    };

    let mut a = Vec::with_capacity((SHIFT_GRID_N * 2 + 1).pow(2) as usize);
    for iy in -SHIFT_GRID_N..=SHIFT_GRID_N {
        for ix in -SHIFT_GRID_N..=SHIFT_GRID_N {
            a.push(sample(before, ix, iy));
        }
    }
    let width = SHIFT_GRID_N * 2 + 1;
    let at = |v: &[f32], ix: i64, iy: i64| -> f32 {
        if ix.abs() > SHIFT_GRID_N || iy.abs() > SHIFT_GRID_N {
            return 0.0;
        }
        v[((iy + SHIFT_GRID_N) * width + ix + SHIFT_GRID_N) as usize]
    };

    let mut best = (0i64, 0i64, f64::MIN);
    for dy in -SHIFT_SEARCH..=SHIFT_SEARCH {
        for dx in -SHIFT_SEARCH..=SHIFT_SEARCH {
            let mut score = 0.0f64;
            for iy in -SHIFT_GRID_N..=SHIFT_GRID_N {
                for ix in -SHIFT_GRID_N..=SHIFT_GRID_N {
                    let b = sample(after, ix + dx, iy + dy);
                    if b > 0.0 {
                        score += (at(&a, ix, iy) * b) as f64;
                    }
                }
            }
            if score > best.2 {
                best = (dx, dy, score);
            }
        }
    }
    if best.2 <= 0.0 {
        return (0.0, 0.0);
    }
    (best.1 as f64 * SHIFT_GRID_STEP, best.0 as f64 * SHIFT_GRID_STEP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::radar::testing::DiskField;

    const CENTRE: Coords = Coords { lat: 36.0, lon: -87.0 };

    fn disk(lat: f64, lon: f64) -> Arc<dyn RadarField> {
        Arc::new(DiskField { centre: Coords { lat, lon }, radius_km: 25.0, dbz: 45.0 })
    }

    #[test]
    fn a_known_displacement_is_recovered() {
        let before = disk(36.0, -87.0);
        let after = disk(36.25, -86.75);
        let (dlat, dlon) = estimate_shift(before.as_ref(), after.as_ref(), CENTRE);
        assert!((dlat - 0.25).abs() <= SHIFT_GRID_STEP, "dlat {dlat}");
        assert!((dlon - 0.25).abs() <= SHIFT_GRID_STEP, "dlon {dlon}");
    }

    #[test]
    fn empty_fields_yield_no_motion() {
        let empty = disk(50.0, -70.0);
        assert_eq!(estimate_shift(empty.as_ref(), empty.as_ref(), CENTRE), (0.0, 0.0));
    }

    /// The storm appears midway through its hop at alpha 0.5 rather than at
    /// either endpoint.
    #[test]
    fn the_interpolated_storm_sits_between_its_endpoints() {
        let before = disk(36.0, -87.0);
        let after = disk(36.4, -87.0);
        let field = InterpolatedField::new(before, after, 0.5, (0.4, 0.0));
        let mid = Coords { lat: 36.2, lon: -87.0 };
        assert_eq!(
            field.value_at(mid, RadarProduct::Reflectivity),
            Some(45.0),
            "both advected neighbours agree at the midpoint"
        );
        let vacated = Coords { lat: 35.9, lon: -87.0 };
        assert_eq!(
            field.value_at(vacated, RadarProduct::Reflectivity),
            None,
            "well behind the moving storm there is nothing left"
        );
    }

    #[test]
    fn alpha_weights_the_neighbours() {
        let before = disk(36.0, -87.0);
        let after = disk(36.0, -87.0);
        let field = InterpolatedField::new(before, after, 0.25, (0.0, 0.0));
        assert_eq!(field.value_at(CENTRE, RadarProduct::Reflectivity), Some(45.0));
        assert!(field.supports(RadarProduct::Reflectivity));
        assert!(!field.supports(RadarProduct::Velocity), "DiskField is reflectivity-only");
    }
}
