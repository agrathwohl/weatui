//! Vector layer composited over the reflectivity raster.
//!
//! Warning polygons and the home marker are drawn into a sparse pixel overlay
//! rather than into the dBZ grid, so annotations never masquerade as radar
//! returns and the underlying data stays queryable.

use crate::alert::Ring;
use crate::geo::Coords;
use crate::radar::grid::Viewport;
use crate::render::colormap::Rgb;

pub const LETHAL_OUTLINE: Rgb = (255, 64, 64);
pub const SEVERE_OUTLINE: Rgb = (255, 176, 32);
pub const WATCH_OUTLINE: Rgb = (120, 200, 255);
pub const HOME_MARKER: Rgb = (255, 255, 255);
pub const DISTANCE_RING: Rgb = (60, 66, 78);

#[derive(Debug, Clone)]
pub struct PixelOverlay {
    pub width: usize,
    pub height: usize,
    cells: Vec<Option<Rgb>>,
}

impl PixelOverlay {
    pub fn new(width: usize, height: usize) -> Self {
        PixelOverlay { width, height, cells: vec![None; width * height] }
    }

    pub fn get(&self, x: usize, y: usize) -> Option<Rgb> {
        if x >= self.width || y >= self.height {
            return None;
        }
        self.cells[y * self.width + x]
    }

    pub fn set(&mut self, x: i64, y: i64, rgb: Rgb) {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            return;
        }
        self.cells[y as usize * self.width + x as usize] = Some(rgb);
    }

    #[cfg(test)]
    pub fn painted(&self) -> usize {
        self.cells.iter().filter(|c| c.is_some()).count()
    }

    /// Bresenham. Polygon edges are frequently longer than the viewport, so
    /// endpoints are kept in signed space and clipped per pixel by `set`.
    pub fn draw_line(&mut self, from: (i64, i64), to: (i64, i64), rgb: Rgb) {
        let (mut x0, mut y0) = from;
        let (x1, y1) = to;
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let budget = (dx - dy) + 2;
        for _ in 0..budget.max(1) {
            self.set(x0, y0, rgb);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = err * 2;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    pub fn draw_ring(&mut self, ring: &Ring, viewport: &Viewport, rgb: Rgb) {
        if ring.len() < 2 {
            return;
        }
        let points: Vec<(i64, i64)> = ring
            .iter()
            .map(|p| viewport.project_to_nearest_pixel(Coords { lat: p[1], lon: p[0] }))
            .collect();
        for pair in points.windows(2) {
            self.draw_line(pair[0], pair[1], rgb);
        }
        if let (Some(first), Some(last)) = (points.first(), points.last())
            && first != last {
                self.draw_line(*last, *first, rgb);
            }
    }

    /// A crosshair rather than a filled dot, so the reflectivity underneath the
    /// user's own position stays readable.
    pub fn draw_home(&mut self, at: Coords, viewport: &Viewport, rgb: Rgb) {
        let (cx, cy) = viewport.project_to_nearest_pixel(at);
        for d in 1..=3 {
            self.set(cx - d, cy, rgb);
            self.set(cx + d, cy, rgb);
            self.set(cx, cy - d, rgb);
            self.set(cx, cy + d, rgb);
        }
    }

    /// Corner brackets around a storm cell so the marker never obscures the
    /// echo it is pointing at.
    pub fn draw_cell_marker(&mut self, at: Coords, viewport: &Viewport, rgb: Rgb, selected: bool) {
        let (cx, cy) = viewport.project_to_nearest_pixel(at);
        let r: i64 = if selected { 6 } else { 4 };
        let arm: i64 = if selected { 3 } else { 2 };
        for (sx, sy) in [(-1i64, -1i64), (1, -1), (-1, 1), (1, 1)] {
            let (px, py) = (cx + sx * r, cy + sy * r);
            for d in 0..arm {
                self.set(px - sx * d, py, rgb);
                self.set(px, py - sy * d, rgb);
            }
        }
    }

    pub fn draw_distance_rings(
        &mut self,
        centre: Coords,
        radii_km: &[f64],
        viewport: &Viewport,
        rgb: Rgb,
    ) {
        let kpp = viewport.km_per_pixel();
        if kpp <= 0.0 {
            return;
        }
        let (cx, cy) = viewport.project_to_nearest_pixel(centre);
        for radius_km in radii_km.iter().copied().filter(|r| *r > 0.0) {
            let radius_px = radius_km / kpp;
            if radius_px < 2.0 || radius_px > self.width as f64 * 2.0 {
                continue;
            }
            let steps = ((radius_px * 6.0) as usize).clamp(180, 4000);
            for i in 0..steps {
                let theta = i as f64 / steps as f64 * std::f64::consts::TAU;
                self.set(
                    cx + (radius_px * theta.cos()).round() as i64,
                    cy + (radius_px * theta.sin()).round() as i64,
                    rgb,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NORMAN: Coords = Coords { lat: 35.2057, lon: -97.4427 };

    fn viewport() -> Viewport {
        Viewport::new(NORMAN, 200.0, 200, 100)
    }

    #[test]
    fn new_overlay_is_empty() {
        let o = PixelOverlay::new(10, 10);
        assert_eq!(o.painted(), 0);
        assert_eq!(o.get(5, 5), None);
    }

    #[test]
    fn out_of_bounds_writes_are_discarded_rather_than_panicking() {
        let mut o = PixelOverlay::new(10, 10);
        o.set(-5, 3, HOME_MARKER);
        o.set(3, -5, HOME_MARKER);
        o.set(100, 3, HOME_MARKER);
        o.set(3, 100, HOME_MARKER);
        assert_eq!(o.painted(), 0);
    }

    #[test]
    fn horizontal_and_vertical_lines_are_continuous() {
        let mut o = PixelOverlay::new(20, 20);
        o.draw_line((2, 5), (8, 5), LETHAL_OUTLINE);
        for x in 2..=8 {
            assert_eq!(o.get(x, 5), Some(LETHAL_OUTLINE), "gap at x={x}");
        }
        o.draw_line((15, 2), (15, 9), SEVERE_OUTLINE);
        for y in 2..=9 {
            assert_eq!(o.get(15, y), Some(SEVERE_OUTLINE), "gap at y={y}");
        }
    }

    #[test]
    fn diagonal_line_reaches_its_endpoint() {
        let mut o = PixelOverlay::new(20, 20);
        o.draw_line((0, 0), (9, 9), LETHAL_OUTLINE);
        assert_eq!(o.get(0, 0), Some(LETHAL_OUTLINE));
        assert_eq!(o.get(9, 9), Some(LETHAL_OUTLINE));
    }

    #[test]
    fn a_line_entirely_outside_the_overlay_paints_nothing() {
        let mut o = PixelOverlay::new(20, 20);
        o.draw_line((-100, -100), (-50, -50), LETHAL_OUTLINE);
        assert_eq!(o.painted(), 0);
    }

    #[test]
    fn an_edge_crossing_the_boundary_still_paints_the_inside_portion() {
        let mut o = PixelOverlay::new(20, 20);
        o.draw_line((-10, 10), (10, 10), LETHAL_OUTLINE);
        assert!(o.painted() > 0, "clipped edge should still draw inside pixels");
        assert_eq!(o.get(0, 10), Some(LETHAL_OUTLINE));
    }

    #[test]
    fn polygon_ring_is_closed_even_when_the_last_vertex_is_not_repeated() {
        let mut o = PixelOverlay::new(200, 100);
        let ring: Ring = vec![
            [NORMAN.lon - 0.2, NORMAN.lat - 0.2],
            [NORMAN.lon + 0.2, NORMAN.lat - 0.2],
            [NORMAN.lon + 0.2, NORMAN.lat + 0.2],
            [NORMAN.lon - 0.2, NORMAN.lat + 0.2],
        ];
        o.draw_ring(&ring, &viewport(), LETHAL_OUTLINE);
        assert!(o.painted() > 100, "only {} pixels painted", o.painted());
    }

    #[test]
    fn degenerate_rings_draw_nothing() {
        let mut o = PixelOverlay::new(200, 100);
        o.draw_ring(&Vec::new(), &viewport(), LETHAL_OUTLINE);
        o.draw_ring(&vec![[NORMAN.lon, NORMAN.lat]], &viewport(), LETHAL_OUTLINE);
        assert_eq!(o.painted(), 0);
    }

    #[test]
    fn home_crosshair_leaves_its_own_centre_pixel_unpainted() {
        let mut o = PixelOverlay::new(200, 100);
        o.draw_home(NORMAN, &viewport(), HOME_MARKER);
        assert_eq!(o.get(100, 50), None, "centre must stay readable");
        assert_eq!(o.get(97, 50), Some(HOME_MARKER));
        assert_eq!(o.get(103, 50), Some(HOME_MARKER));
        assert_eq!(o.get(100, 47), Some(HOME_MARKER));
        assert_eq!(o.get(100, 53), Some(HOME_MARKER));
    }

    #[test]
    fn home_marker_off_screen_paints_nothing() {
        let mut o = PixelOverlay::new(200, 100);
        o.draw_home(Coords { lat: 10.0, lon: 10.0 }, &viewport(), HOME_MARKER);
        assert_eq!(o.painted(), 0);
    }
}
