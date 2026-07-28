//! Viewport projection and resampling into a half-block pixel grid.
//!
//! Equirectangular about the viewport centre. At radar range (a few hundred km)
//! its distortion is far below one pixel, and it keeps pan and zoom to plain
//! arithmetic rather than a full projection.

use crate::geo::Coords;
use crate::radar::{DbzGrid, ReflectivityField};

const KM_PER_DEG_LAT: f64 = 111.19492664455873;

#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub centre: Coords,
    pub span_km: f64,
    pub width: usize,
    pub height: usize,
}

impl Viewport {
    pub fn new(centre: Coords, span_km: f64, width: usize, height: usize) -> Self {
        Viewport { centre, span_km, width, height }
    }

    pub fn km_per_pixel(&self) -> f64 {
        if self.width == 0 {
            return 0.0;
        }
        self.span_km / self.width as f64
    }

    fn km_per_deg_lon(&self) -> f64 {
        KM_PER_DEG_LAT * self.centre.lat.to_radians().cos()
    }

    /// Pixel centre to geographic position. Screen y grows downward, latitude
    /// grows northward, hence the inverted vertical term.
    pub fn pixel_coords(&self, x: usize, y: usize) -> Coords {
        let kpp = self.km_per_pixel();
        let dx_km = (x as f64 + 0.5 - self.width as f64 / 2.0) * kpp;
        let dy_km = (self.height as f64 / 2.0 - (y as f64 + 0.5)) * kpp;
        let lon_scale = self.km_per_deg_lon();
        Coords {
            lat: self.centre.lat + dy_km / KM_PER_DEG_LAT,
            lon: self.centre.lon
                + if lon_scale.abs() < f64::EPSILON { 0.0 } else { dx_km / lon_scale },
        }
    }

    /// Inverse of [`pixel_coords`] in continuous, unclipped pixel space.
    ///
    /// Deliberately not rounded: `pixel_coords` returns pixel centres, so a
    /// round trip lands on `x + 0.5`. Callers that want a pixel index must
    /// floor, while callers drawing vertices want nearest. Rounding here would
    /// shift every round trip by one pixel.
    pub fn project_unclipped(&self, at: Coords) -> (f64, f64) {
        let kpp = self.km_per_pixel();
        if kpp <= 0.0 {
            return (f64::NEG_INFINITY, f64::NEG_INFINITY);
        }
        let dy_km = (at.lat - self.centre.lat) * KM_PER_DEG_LAT;
        let dx_km = (at.lon - self.centre.lon) * self.km_per_deg_lon();
        (
            dx_km / kpp + self.width as f64 / 2.0,
            self.height as f64 / 2.0 - dy_km / kpp,
        )
    }

    pub fn project_to_nearest_pixel(&self, at: Coords) -> (i64, i64) {
        let (x, y) = self.project_unclipped(at);
        if !x.is_finite() || !y.is_finite() {
            return (i64::MIN / 4, i64::MIN / 4);
        }
        (x.floor() as i64, y.floor() as i64)
    }

    pub fn panned_km(&self, east_km: f64, north_km: f64) -> Viewport {
        let lon_scale = self.km_per_deg_lon();
        Viewport {
            centre: Coords {
                lat: self.centre.lat + north_km / KM_PER_DEG_LAT,
                lon: self.centre.lon
                    + if lon_scale.abs() < f64::EPSILON { 0.0 } else { east_km / lon_scale },
            },
            ..*self
        }
    }

    pub fn zoomed(&self, factor: f64, min_span_km: f64, max_span_km: f64) -> Viewport {
        Viewport {
            span_km: (self.span_km * factor).clamp(min_span_km, max_span_km),
            ..*self
        }
    }
}

pub fn rasterize(field: &dyn ReflectivityField, viewport: &Viewport) -> DbzGrid {
    let mut grid = DbzGrid::new(viewport.width, viewport.height);
    for y in 0..viewport.height {
        for x in 0..viewport.width {
            grid.set(x, y, field.dbz_at(viewport.pixel_coords(x, y)));
        }
    }
    grid
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::haversine_km;
    use crate::radar::testing::DiskField;

    const NORMAN: Coords = Coords { lat: 35.2057, lon: -97.4427 };

    fn viewport() -> Viewport {
        Viewport::new(NORMAN, 200.0, 200, 100)
    }

    #[test]
    fn centre_pixels_sit_at_the_viewport_centre() {
        let v = viewport();
        let c = v.pixel_coords(v.width / 2, v.height / 2);
        assert!(haversine_km(c, NORMAN) < 1.5, "off by {} km", haversine_km(c, NORMAN));
    }

    #[test]
    fn km_per_pixel_follows_the_span() {
        assert_eq!(viewport().km_per_pixel(), 1.0);
        assert_eq!(Viewport::new(NORMAN, 400.0, 200, 100).km_per_pixel(), 2.0);
    }

    #[test]
    fn increasing_y_moves_south_and_increasing_x_moves_east() {
        let v = viewport();
        let top = v.pixel_coords(100, 10);
        let bottom = v.pixel_coords(100, 90);
        let left = v.pixel_coords(10, 50);
        let right = v.pixel_coords(190, 50);
        assert!(top.lat > bottom.lat, "screen y must grow southward");
        assert!(right.lon > left.lon, "screen x must grow eastward");
    }

    #[test]
    fn pixels_are_square_in_ground_distance() {
        let v = viewport();
        let horizontal = haversine_km(v.pixel_coords(100, 50), v.pixel_coords(101, 50));
        let vertical = haversine_km(v.pixel_coords(100, 50), v.pixel_coords(100, 51));
        assert!(
            (horizontal - vertical).abs() / vertical < 0.02,
            "h {horizontal} vs v {vertical}"
        );
    }

    /// pixel_coords returns pixel centres, so projecting one back must land on
    /// x + 0.5. Rounding here instead of flooring shifts every vertex by one.
    #[test]
    fn projection_inverts_pixel_coords_at_the_half_pixel() {
        let v = viewport();
        for (x, y) in [(0, 0), (50, 25), (100, 50), (199, 99)] {
            let (px, py) = v.project_unclipped(v.pixel_coords(x, y));
            assert!((px - (x as f64 + 0.5)).abs() < 1e-9, "x {x}: got {px}");
            assert!((py - (y as f64 + 0.5)).abs() < 1e-9, "y {y}: got {py}");
            assert_eq!(v.project_to_nearest_pixel(v.pixel_coords(x, y)), (x as i64, y as i64));
        }
    }

    #[test]
    fn projection_of_a_point_outside_the_viewport_stays_signed_and_finite() {
        let v = viewport();
        let (x, _) = v.project_unclipped(Coords { lat: NORMAN.lat, lon: NORMAN.lon - 5.0 });
        assert!(x < 0.0, "a point far west must project to a negative x, got {x}");
        assert!(x.is_finite());
    }

    #[test]
    fn projection_of_a_degenerate_viewport_is_not_a_panic() {
        let v = Viewport::new(NORMAN, 200.0, 0, 0);
        let (x, y) = v.project_unclipped(NORMAN);
        assert!(!x.is_finite() && !y.is_finite());
        assert_eq!(v.project_to_nearest_pixel(NORMAN), (i64::MIN / 4, i64::MIN / 4));
    }

    #[test]
    fn panning_north_raises_the_centre_latitude_by_the_requested_distance() {
        let v = viewport().panned_km(0.0, 111.19492664455873);
        assert!((v.centre.lat - (NORMAN.lat + 1.0)).abs() < 1e-9);
    }

    #[test]
    fn panning_east_moves_the_centre_east() {
        let v = viewport().panned_km(50.0, 0.0);
        assert!(v.centre.lon > NORMAN.lon);
        assert!((v.centre.lat - NORMAN.lat).abs() < 1e-12);
    }

    #[test]
    fn zoom_is_clamped_at_both_ends() {
        let v = viewport();
        assert_eq!(v.zoomed(0.001, 25.0, 800.0).span_km, 25.0);
        assert_eq!(v.zoomed(1000.0, 25.0, 800.0).span_km, 800.0);
        assert_eq!(v.zoomed(2.0, 25.0, 800.0).span_km, 400.0);
    }

    #[test]
    fn rasterize_fills_a_disk_and_leaves_the_corners_empty() {
        let field = DiskField { centre: NORMAN, radius_km: 50.0, dbz: 45.0 };
        let v = viewport();
        let grid = rasterize(&field, &v);
        assert_eq!(grid.get(100, 50), Some(45.0));
        assert_eq!(grid.get(0, 0), None, "corner is 111 km out, beyond the disk");
        assert_eq!(grid.value_range(), Some((45.0, 45.0)));
        assert!(grid.populated_cells() > 0);
    }

    #[test]
    fn rasterize_of_a_zero_width_viewport_does_not_panic() {
        let field = DiskField { centre: NORMAN, radius_km: 50.0, dbz: 45.0 };
        let grid = rasterize(&field, &Viewport::new(NORMAN, 200.0, 0, 0));
        assert_eq!(grid.populated_cells(), 0);
    }
}
