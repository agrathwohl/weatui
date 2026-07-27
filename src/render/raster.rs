//! Half-block rasteriser.
//!
//! U+2580 UPPER HALF BLOCK splits a character cell into two independently
//! coloured pixels: foreground paints the top, background the bottom. That is
//! why the pixel grid is twice as tall as the terminal region it occupies.

use crate::config::Colormap;
use crate::radar::DbzGrid;
use crate::render::colormap::{Rgb, cell_rgb};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

pub const UPPER_HALF_BLOCK: char = '\u{2580}';

/// A10: terminal row `r` draws pixel rows `2r` (foreground) and `2r+1`
/// (background) of column `c`.
pub fn cell_colors(grid: &DbzGrid, col: usize, row: usize, map: Colormap) -> (Rgb, Rgb) {
    (
        cell_rgb(grid.get(col, row * 2), map),
        cell_rgb(grid.get(col, row * 2 + 1), map),
    )
}

fn to_color((r, g, b): Rgb) -> Color {
    Color::Rgb(r, g, b)
}

pub struct RadarRaster<'a> {
    pub grid: &'a DbzGrid,
    pub colormap: Colormap,
}

impl Widget for RadarRaster<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for row in 0..area.height as usize {
            for col in 0..area.width as usize {
                let (top, bottom) = cell_colors(self.grid, col, row, self.colormap);
                let position = (area.x + col as u16, area.y + row as u16);
                if let Some(cell) = buf.cell_mut(position) {
                    cell.set_char(UPPER_HALF_BLOCK)
                        .set_fg(to_color(top))
                        .set_bg(to_color(bottom));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::colormap::{NO_DATA, dbz_to_rgb};

    fn grid() -> DbzGrid {
        let mut g = DbzGrid::new(3, 4);
        g.set(0, 0, Some(55.0));
        g.set(0, 1, Some(20.0));
        g.set(1, 2, Some(65.0));
        g
    }

    #[test]
    fn a10_foreground_is_the_even_pixel_row_and_background_the_odd_one() {
        let (fg, bg) = cell_colors(&grid(), 0, 0, Colormap::Threat);
        assert_eq!(fg, dbz_to_rgb(55.0, Colormap::Threat).unwrap());
        assert_eq!(bg, dbz_to_rgb(20.0, Colormap::Threat).unwrap());
    }

    #[test]
    fn a10_second_terminal_row_reads_pixel_rows_two_and_three() {
        let (fg, bg) = cell_colors(&grid(), 1, 1, Colormap::Threat);
        assert_eq!(fg, dbz_to_rgb(65.0, Colormap::Threat).unwrap());
        assert_eq!(bg, NO_DATA);
    }

    #[test]
    fn a10_two_pixels_in_one_cell_can_carry_different_colours() {
        let (fg, bg) = cell_colors(&grid(), 0, 0, Colormap::Threat);
        assert_ne!(fg, bg, "half-block must give two independent pixels");
    }

    #[test]
    fn empty_pixels_render_as_background_not_as_a_dark_echo() {
        let (fg, bg) = cell_colors(&grid(), 2, 0, Colormap::Threat);
        assert_eq!(fg, NO_DATA);
        assert_eq!(bg, NO_DATA);
    }

    #[test]
    fn reading_beyond_the_grid_is_background_rather_than_a_panic() {
        let (fg, bg) = cell_colors(&grid(), 99, 99, Colormap::Threat);
        assert_eq!(fg, NO_DATA);
        assert_eq!(bg, NO_DATA);
    }

    #[test]
    fn widget_writes_half_blocks_with_both_colours_set() {
        let g = grid();
        let area = Rect::new(0, 0, 3, 2);
        let mut buf = Buffer::empty(area);
        RadarRaster { grid: &g, colormap: Colormap::Threat }.render(area, &mut buf);

        let cell = buf.cell((0, 0)).unwrap();
        assert_eq!(cell.symbol(), UPPER_HALF_BLOCK.to_string());
        assert_eq!(cell.fg, to_color(dbz_to_rgb(55.0, Colormap::Threat).unwrap()));
        assert_eq!(cell.bg, to_color(dbz_to_rgb(20.0, Colormap::Threat).unwrap()));
    }

    #[test]
    fn widget_honours_a_non_zero_area_origin() {
        let g = grid();
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 10));
        let area = Rect::new(4, 3, 3, 2);
        RadarRaster { grid: &g, colormap: Colormap::Threat }.render(area, &mut buf);
        assert_eq!(buf.cell((4, 3)).unwrap().symbol(), UPPER_HALF_BLOCK.to_string());
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), " ");
    }

    #[test]
    fn widget_larger_than_the_grid_does_not_panic() {
        let g = grid();
        let area = Rect::new(0, 0, 40, 20);
        let mut buf = Buffer::empty(area);
        RadarRaster { grid: &g, colormap: Colormap::Threat }.render(area, &mut buf);
        assert_eq!(buf.cell((30, 15)).unwrap().symbol(), UPPER_HALF_BLOCK.to_string());
    }
}
