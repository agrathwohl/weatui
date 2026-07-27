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

    /// Inverse of [`pixel_coords`], for placing markers and polygon vertices.
    /// Returns `None` when the position falls outside the viewport.
    pub fn coords_to_pixel(&self, at: Coords) -> Option<(usize, usize)> {
        let kpp = self.km_per_pixel();
        if kpp <= 0.0 {
            return None;
        }
        let dy_km = (at.lat - self.centre.lat) * KM_PER_DEG_LAT;
        let dx_km = (at.lon - self.centre.lon) * self.km_per_deg_lon();
        let x = dx_km / kpp + self.width as f64 / 2.0;
        let y = self.height as f64 / 2.0 - dy_km / kpp;
        if x < 0.0 || y < 0.0 || x >= self.width as f64 || y >= self.height as f64 {
            return None;
        }
        Some((x as usize, y as usize))
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

    #[test]
    fn coords_to_pixel_inverts_pixel_coords() {
        let v = viewport();
        for (x, y) in [(0, 0), (50, 25), (100, 50), (199, 99)] {
            let round_tripped = v.coords_to_pixel(v.pixel_coords(x, y)).unwrap();
            assert_eq!(round_tripped, (x, y), "failed for ({x},{y})");
        }
    }

    #[test]
    fn coords_outside_the_viewport_have_no_pixel() {
        let v = viewport();
        assert!(v.coords_to_pixel(Coords { lat: 60.0, lon: -97.4 }).is_none());
        assert!(v.coords_to_pixel(Coords { lat: 35.2, lon: 0.0 }).is_none());
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
