//! Text drawn over the radar raster: city names and storm-cell hazard
//! letters. These are terminal-cell text, not overlay pixels, so they render
//! after [`crate::render::raster::RadarRaster`].

use crate::radar::cells::{Hazard, StormCell};
use crate::radar::grid::Viewport;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;

/// Surface temperature at or below which rain labels as snow.
const SNOW_TEMP_F: f32 = 34.0;
const MAX_CITY_LABELS: usize = 12;
const MAX_HAZARD_LETTERS: usize = 3;

fn hazard_color(h: Hazard) -> Color {
    match h {
        Hazard::Tornado => Color::Rgb(255, 60, 60),
        Hazard::Hail => Color::Rgb(245, 210, 70),
        Hazard::Wind => Color::Rgb(255, 130, 40),
        Hazard::Lightning => Color::Rgb(170, 150, 255),
        Hazard::Rain => Color::Rgb(110, 170, 255),
    }
}

pub fn hazard_letter(h: Hazard, surface_temp_f: Option<f32>) -> char {
    if h == Hazard::Rain && surface_temp_f.is_some_and(|t| t <= SNOW_TEMP_F) {
        'S'
    } else {
        h.letter()
    }
}

pub struct MapText<'a> {
    pub viewport: &'a Viewport,
    pub cells: &'a [StormCell],
    pub show_cities: bool,
    pub show_hazards: bool,
    pub surface_temp_f: Option<f32>,
    pub home: crate::geo::Coords,
    pub ring_km: &'a [f64],
}

impl MapText<'_> {
    fn cell_of(&self, at: crate::geo::Coords, area: Rect) -> Option<(u16, u16)> {
        let (px, py) = self.viewport.project_to_nearest_pixel(at);
        if px < 0 || py < 0 {
            return None;
        }
        let (col, row) = (px as u16, (py / 2) as u16);
        (col < area.width && row < area.height).then_some((area.x + col, area.y + row))
    }
}

impl Widget for MapText<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let km_per_px_ring = self.viewport.km_per_pixel();
        if km_per_px_ring > 0.0
            && let Some((hx, hy)) = self.cell_of(self.home, area)
        {
            for (i, km) in self.ring_km.iter().filter(|r| **r > 0.0).enumerate() {
                let r_cols = (km / km_per_px_ring).round() as i64;
                let x = hx as i64 + r_cols;
                if x <= hx as i64 || x as u16 >= area.right() {
                    continue;
                }
                let (r, g, b) =
                    crate::render::overlay::RING_COLORS[i % crate::render::overlay::RING_COLORS.len()];
                // Ring colors are muted for the lines; the label needs a
                // brighter shade of the same hue to be readable.
                buf.set_stringn(
                    x as u16,
                    hy,
                    format!("{km:.0}km"),
                    (area.right() - x as u16) as usize,
                    Style::default().fg(Color::Rgb(
                        (r * 2).min(210),
                        (g * 2).min(210),
                        (b * 2).min(210),
                    )),
                );
            }
        }

        if self.show_cities {
            let km_per_px = self.viewport.span_km / self.viewport.width.max(1) as f64;
            let half_lat = self.viewport.height as f64 * km_per_px / 2.0 / 111.2;
            let half_lon = self.viewport.span_km / 2.0
                / (111.2 * self.viewport.centre.lat.to_radians().cos());
            let mut used_rows: Vec<u16> = Vec::new();
            for place in crate::geo::places_within(
                self.viewport.centre,
                half_lat,
                half_lon,
                MAX_CITY_LABELS,
            ) {
                let Some((x, y)) = self.cell_of(place.coords, area) else { continue };
                // One label per screen row keeps names from colliding.
                if used_rows.contains(&y) {
                    continue;
                }
                used_rows.push(y);
                let text = format!("\u{b7}{}", place.name);
                let width = (area.right().saturating_sub(x)) as usize;
                buf.set_stringn(
                    x,
                    y,
                    &text,
                    width,
                    Style::default().fg(Color::Rgb(120, 126, 138)),
                );
            }
        }

        if self.show_hazards {
            for cell in self.cells {
                let Some((x, y)) = self.cell_of(cell.centroid, area) else { continue };
                for (x, h) in (x.saturating_add(4)..area.right())
                    .zip(cell.hazards.iter().take(MAX_HAZARD_LETTERS))
                {
                    buf.set_stringn(
                        x,
                        y,
                        hazard_letter(*h, self.surface_temp_f).to_string(),
                        1,
                        Style::default().fg(hazard_color(*h)),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::Coords;
    use crate::radar::cells::CellThreat;

    fn storm(hazards: Vec<Hazard>, at: Coords) -> StormCell {
        StormCell {
            centroid: at,
            max_dbz: 50.0,
            rotation_ms: None,
            min_cc: None,
            max_vil: None,
            max_echo_top_km: None,
            distance_km: 10.0,
            bearing: "N",
            threat: CellThreat::Strong,
            hazards,
        }
    }

    #[test]
    fn ring_distance_labels_sit_on_their_rings() {
        let home = Coords { lat: 36.0, lon: -87.0 };
        let viewport = Viewport::new(home, 120.0, 60, 60);
        let area = Rect::new(0, 0, 60, 30);
        let mut buf = Buffer::empty(area);
        MapText {
            viewport: &viewport,
            cells: &[],
            show_cities: false,
            show_hazards: false,
            surface_temp_f: None,
            home,
            ring_km: &[25.0, 50.0],
        }
        .render(area, &mut buf);
        let text: String = (0..30)
            .map(|y| {
                (0..60).map(|x| buf.cell((x, y)).unwrap().symbol().to_string()).collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("25km"), "{text}");
        assert!(text.contains("50km"), "{text}");
    }

    #[test]
    fn rain_labels_as_snow_below_freezing_surface_temps() {
        assert_eq!(hazard_letter(Hazard::Rain, Some(70.0)), 'R');
        assert_eq!(hazard_letter(Hazard::Rain, Some(30.0)), 'S');
        assert_eq!(hazard_letter(Hazard::Rain, None), 'R');
        assert_eq!(hazard_letter(Hazard::Tornado, Some(30.0)), 'T', "cold never rewrites a tornado");
    }

    #[test]
    fn hazard_letters_render_beside_the_cell() {
        let centre = Coords { lat: 36.0, lon: -87.0 };
        let viewport = Viewport::new(centre, 100.0, 40, 40);
        let cells = [storm(vec![Hazard::Tornado, Hazard::Wind, Hazard::Rain], centre)];
        let area = Rect::new(0, 0, 40, 20);
        let mut buf = Buffer::empty(area);
        MapText {
            viewport: &viewport,
            cells: &cells,
            show_cities: false,
            show_hazards: true,
            surface_temp_f: Some(70.0),
            home: Coords { lat: 0.0, lon: 0.0 },
            ring_km: &[],
        }
        .render(area, &mut buf);
        let row: String =
            (0..40).map(|x| buf.cell((x, 10)).unwrap().symbol().to_string()).collect();
        assert!(row.contains("TWR"), "letters in severity order beside the marker: {row:?}");
    }

    #[test]
    fn toggling_hazards_off_renders_nothing() {
        let centre = Coords { lat: 36.0, lon: -87.0 };
        let viewport = Viewport::new(centre, 100.0, 40, 40);
        let cells = [storm(vec![Hazard::Rain], centre)];
        let area = Rect::new(0, 0, 40, 20);
        let mut buf = Buffer::empty(area);
        MapText {
            viewport: &viewport,
            cells: &cells,
            show_cities: false,
            show_hazards: false,
            surface_temp_f: None,
            home: Coords { lat: 0.0, lon: 0.0 },
            ring_km: &[],
        }
        .render(area, &mut buf);
        assert!(
            buf.content().iter().all(|c| c.symbol() == " "),
            "disabled layer must draw nothing"
        );
    }

    #[test]
    fn a_nearby_city_gets_a_label() {
        let nashville = Coords { lat: 36.1718, lon: -86.7850 };
        let viewport = Viewport::new(nashville, 120.0, 60, 60);
        let area = Rect::new(0, 0, 60, 30);
        let mut buf = Buffer::empty(area);
        MapText {
            viewport: &viewport,
            cells: &[],
            show_cities: true,
            show_hazards: false,
            surface_temp_f: None,
            home: Coords { lat: 0.0, lon: 0.0 },
            ring_km: &[],
        }
        .render(area, &mut buf);
        let text: String = (0..30)
            .map(|y| (0..60).map(|x| buf.cell((x, y)).unwrap().symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Nashville"), "{text}");
    }
}
