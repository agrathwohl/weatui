use crate::radar::ring::FrameRing;
use crate::render::glyph;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

pub const PAST: char = '\u{2501}';
pub const CURSOR: char = '\u{25c9}';
pub const PROJECTED: char = '\u{2504}';

/// Frame track as characters, oldest to newest. Projected frames use a dashed
/// rune so an extrapolated future is never mistaken for observed data.
pub fn track(ring: &FrameRing) -> String {
    ring.frames()
        .enumerate()
        .map(|(i, frame)| {
            if i == ring.cursor() {
                CURSOR
            } else if frame.projected {
                PROJECTED
            } else {
                PAST
            }
        })
        .collect()
}

pub fn status_text(ring: &FrameRing) -> String {
    if ring.is_empty() {
        return format!("{} awaiting first volume", glyph::REFRESH);
    }
    let transport = if ring.is_playing() { glyph::PLAY } else { glyph::PAUSE };
    let position = format!("{}/{}", ring.cursor() + 1, ring.len());
    // Playback legitimately parks the cursor off the newest frame, so SCRUB is
    // reserved for the user actually driving it.
    let live = match (ring.is_following_live(), ring.is_playing()) {
        (true, _) => " LIVE",
        (false, true) => " LOOP",
        (false, false) => " SCRUB",
    };
    let stamp = ring
        .current()
        .map(|f| f.captured_at.format("%H:%M:%SZ").to_string())
        .unwrap_or_default();
    format!("{transport} {position}{live}  {} {stamp}", glyph::CLOCK)
}

pub struct Timeline<'a> {
    pub ring: &'a FrameRing,
}

impl Widget for Timeline<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let colour = if self.ring.is_following_live() {
            Color::Rgb(120, 200, 255)
        } else {
            Color::Rgb(255, 176, 32)
        };
        let line = Line::from(vec![
            Span::styled(status_text(self.ring), Style::default().fg(colour)),
            Span::raw("  "),
            Span::styled(track(self.ring), Style::default().fg(Color::Rgb(200, 200, 210))),
        ]);
        Paragraph::new(line).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::Coords;
    use crate::radar::ring::RadarFrame;
    use crate::radar::testing::DiskField;
    use chrono::DateTime;
    use std::sync::Arc;

    fn frame(minute: i64, projected: bool) -> RadarFrame {
        RadarFrame {
            captured_at: DateTime::from_timestamp(1_700_000_000 + minute * 60, 0)
                .unwrap()
                .to_utc(),
            field: Arc::new(DiskField {
                centre: Coords { lat: 35.0, lon: -97.0 },
                radius_km: 50.0,
                dbz: 40.0,
            }),
            projected,
        }
    }

    fn ring_with(observed: usize, projected: usize) -> FrameRing {
        let mut r = FrameRing::new(16);
        for i in 0..observed {
            r.push(frame(i as i64, false));
        }
        for i in 0..projected {
            r.push(frame((observed + i) as i64, true));
        }
        r
    }

    #[test]
    fn empty_ring_reports_that_it_is_waiting_rather_than_showing_a_false_position() {
        let r = FrameRing::new(4);
        assert_eq!(track(&r), "");
        assert!(status_text(&r).contains("awaiting"));
    }

    #[test]
    fn track_marks_exactly_one_cursor() {
        let t = track(&ring_with(5, 0));
        assert_eq!(t.chars().filter(|c| *c == CURSOR).count(), 1);
        assert_eq!(t.chars().count(), 5);
    }

    #[test]
    fn cursor_sits_on_the_newest_frame_while_following_live() {
        let t: Vec<char> = track(&ring_with(4, 0)).chars().collect();
        assert_eq!(t[3], CURSOR);
    }

    #[test]
    fn projected_frames_use_a_distinct_rune_from_observed_ones() {
        let t: Vec<char> = track(&ring_with(3, 2)).chars().collect();
        assert_eq!(t[0], PAST);
        assert_eq!(t[3], PROJECTED, "extrapolated frames must be visually distinct");
        assert_eq!(t[4], CURSOR);
    }

    #[test]
    fn scrubbing_moves_the_cursor_and_flips_the_live_indicator() {
        let mut r = ring_with(4, 0);
        assert!(status_text(&r).contains("LIVE"));
        r.step_back();
        let t: Vec<char> = track(&r).chars().collect();
        assert_eq!(t[2], CURSOR);
        assert!(status_text(&r).contains("LOOP"), "playing off-newest is a loop, not a scrub");
        r.toggle_play();
        assert!(status_text(&r).contains("SCRUB"), "paused off-newest is a scrub");
    }

    #[test]
    fn status_reports_position_out_of_total() {
        let r = ring_with(7, 0);
        assert!(status_text(&r).contains("7/7"), "got: {}", status_text(&r));
    }

    #[test]
    fn transport_glyph_follows_play_state() {
        let mut r = ring_with(3, 0);
        assert!(status_text(&r).contains(glyph::PLAY));
        r.toggle_play();
        assert!(status_text(&r).contains(glyph::PAUSE));
    }

    #[test]
    fn widget_renders_without_panicking_in_a_one_row_area() {
        let r = ring_with(5, 1);
        let area = Rect::new(0, 0, 60, 1);
        let mut buf = Buffer::empty(area);
        Timeline { ring: &r }.render(area, &mut buf);
    }
}
