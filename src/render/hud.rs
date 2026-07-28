use crate::alert::filter::ThreatTier;
use crate::alert::state::ActiveAlert;
use crate::render::glyph;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

pub fn tier_glyph(event: &str, tier: ThreatTier) -> char {
    let e = event.to_ascii_lowercase();
    if e.contains("tornado") {
        glyph::TORNADO
    } else if e.contains("flood") {
        glyph::FLOOD
    } else if e.contains("thunderstorm") {
        glyph::THUNDERSTORM
    } else if e.contains("wind") || e.contains("statement") {
        glyph::STRONG_WIND
    } else {
        match tier {
            ThreatTier::Lethal => glyph::TORNADO,
            ThreatTier::Severe => glyph::THUNDERSTORM,
            ThreatTier::Watch => glyph::STORM_WARNING,
        }
    }
}

pub fn tier_color(tier: ThreatTier) -> Color {
    match tier {
        ThreatTier::Lethal => Color::Rgb(255, 64, 64),
        ThreatTier::Severe => Color::Rgb(255, 176, 32),
        ThreatTier::Watch => Color::Rgb(120, 200, 255),
    }
}

pub struct Hud<'a> {
    pub active: &'a [ActiveAlert],
    pub stale: bool,
    pub stale_secs: u64,
    pub site: &'a str,
    pub home: crate::geo::Coords,
    pub peak_dbz: Option<f32>,
    pub eta_for: &'a dyn Fn(&crate::alert::Alert) -> Option<i64>,
}

impl Hud<'_> {
    fn lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();

        if self.stale {
            lines.push(Line::from(Span::styled(
                format!(
                    "{} FEED STALE {}s - YOU ARE NOT BEING WARNED",
                    glyph::NO_DATA,
                    self.stale_secs
                ),
                Style::default()
                    .fg(Color::Rgb(255, 255, 255))
                    .bg(Color::Rgb(180, 0, 0))
                    .add_modifier(Modifier::BOLD),
            )));
        }

        lines.push(Line::from(Span::styled(
            match self.peak_dbz {
                Some(peak) => format!("{} {}  peak {peak:.0} dBZ", glyph::REFRESH, self.site),
                None => format!("{} {}", glyph::REFRESH, self.site),
            },
            Style::default().fg(Color::Rgb(140, 140, 150)),
        )));

        let mut active: Vec<&ActiveAlert> = self.active.iter().collect();
        active.sort_by_key(|a| std::cmp::Reverse(a.tier));

        if active.is_empty() && !self.stale {
            lines.push(Line::from(Span::styled(
                "no active warnings",
                Style::default().fg(Color::Rgb(110, 140, 110)),
            )));
            return lines;
        }

        for entry in active {
            let g = tier_glyph(&entry.alert.properties.event, entry.tier);
            let style = Style::default()
                .fg(tier_color(entry.tier))
                .add_modifier(Modifier::BOLD);
            let inside = entry.alert.contains(self.home.lat, self.home.lon);
            lines.push(Line::from(Span::styled(
                format!(
                    "{g} {}{}",
                    entry.alert.properties.event,
                    if inside { "  [YOU]" } else { "" }
                ),
                style,
            )));

            if let Some(detection) = entry.alert.tornado_detection() {
                lines.push(Line::from(Span::styled(
                    format!("  tornado {}", detection.to_lowercase()),
                    Style::default()
                        .fg(Color::Rgb(255, 255, 255))
                        .bg(Color::Rgb(180, 0, 0))
                        .add_modifier(Modifier::BOLD),
                )));
            }

            if let Some(minutes) = (self.eta_for)(&entry.alert) {
                let age = entry.alert.motion().map(|m| {
                    (chrono::Utc::now() - m.observed_at.to_utc()).num_minutes().max(0)
                });
                lines.push(Line::from(Span::styled(
                    match age {
                        Some(a) => format!("  {} impact in {minutes} min (vector {a}m old)", glyph::CLOCK),
                        None => format!("  {} impact in {minutes} min", glyph::CLOCK),
                    },
                    Style::default().fg(tier_color(entry.tier)),
                )));
            }

            if let Some(gust) = entry.alert.max_wind_gust() {
                lines.push(Line::from(Span::styled(
                    format!("  {} {gust}", glyph::STRONG_WIND),
                    Style::default().fg(Color::Rgb(200, 200, 210)),
                )));
            }
            if let Some(hail) = entry.alert.max_hail_size() {
                lines.push(Line::from(Span::styled(
                    format!("  hail {hail} in"),
                    Style::default().fg(Color::Rgb(200, 200, 210)),
                )));
            }
            if let Some(threat) = entry.alert.damage_threat() {
                lines.push(Line::from(Span::styled(
                    format!("  {threat}"),
                    Style::default()
                        .fg(Color::Rgb(255, 255, 255))
                        .bg(Color::Rgb(180, 0, 0))
                        .add_modifier(Modifier::BOLD),
                )));
            }
            let certainty = entry.alert.properties.certainty.as_deref().unwrap_or("unknown");
            let until = entry
                .alert
                .properties
                .expires
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.format("%H:%M").to_string());
            lines.push(Line::from(Span::styled(
                match until {
                    Some(t) => format!("  {} until {t}", certainty.to_lowercase()),
                    None => format!("  {}", certainty.to_lowercase()),
                },
                Style::default().fg(Color::Rgb(200, 200, 210)),
            )));
            if let Some(area) = &entry.alert.properties.area_desc {
                lines.push(Line::from(Span::styled(
                    format!("  {area}"),
                    Style::default().fg(Color::Rgb(140, 140, 150)),
                )));
            }
            if let Some(instruction) = &entry.alert.properties.instruction {
                let action = instruction.split_whitespace().collect::<Vec<_>>().join(" ");
                lines.push(Line::from(Span::styled(
                    action,
                    Style::default()
                        .fg(Color::Rgb(255, 255, 255))
                        .add_modifier(Modifier::BOLD),
                )));
            }
        }
        lines
    }
}

impl Widget for Hud<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let border = match self.active.iter().map(|a| a.tier).max() {
            _ if self.stale => Color::Rgb(255, 64, 64),
            Some(t) => tier_color(t),
            None => Color::Rgb(70, 70, 80),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .title(" weatui ");
        Paragraph::new(self.lines())
            .block(block)
            .wrap(Wrap { trim: true })
            .render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tornado_products_get_the_rotation_glyph() {
        assert_eq!(tier_glyph("Tornado Warning", ThreatTier::Lethal), glyph::TORNADO);
    }

    #[test]
    fn flood_and_thunderstorm_products_are_distinguishable() {
        let flood = tier_glyph("Flash Flood Warning", ThreatTier::Lethal);
        let storm = tier_glyph("Severe Thunderstorm Warning", ThreatTier::Severe);
        assert_eq!(flood, glyph::FLOOD);
        assert_eq!(storm, glyph::THUNDERSTORM);
        assert_ne!(flood, storm);
    }

    #[test]
    fn special_weather_statement_gets_the_wind_glyph() {
        assert_eq!(
            tier_glyph("Special Weather Statement", ThreatTier::Severe),
            glyph::STRONG_WIND
        );
    }

    #[test]
    fn unknown_event_falls_back_to_a_tier_appropriate_glyph() {
        assert_eq!(tier_glyph("Something Novel", ThreatTier::Watch), glyph::STORM_WARNING);
        assert_eq!(tier_glyph("Something Novel", ThreatTier::Lethal), glyph::TORNADO);
    }

    #[test]
    fn every_glyph_used_sits_in_a_range_the_configured_font_covers() {
        for g in [
            glyph::TORNADO,
            glyph::THUNDERSTORM,
            glyph::FLOOD,
            glyph::STRONG_WIND,
            glyph::STORM_WARNING,
            glyph::NO_DATA,
            glyph::REFRESH,
        ] {
            let c = g as u32;
            assert!((0xe300..=0xe3e3).contains(&c), "{c:x} outside the weather range");
        }
        for g in [glyph::PLAY, glyph::PAUSE, glyph::CLOCK] {
            let c = g as u32;
            assert!((0xf000..=0xf381).contains(&c), "{c:x} outside the covered range");
        }
    }

    #[test]
    fn tier_colours_are_distinct() {
        assert_ne!(tier_color(ThreatTier::Lethal), tier_color(ThreatTier::Severe));
        assert_ne!(tier_color(ThreatTier::Severe), tier_color(ThreatTier::Watch));
    }

    #[test]
    fn quiet_hud_says_so_explicitly_rather_than_rendering_blank() {
        let active: Vec<ActiveAlert> = Vec::new();
        let eta = |_: &crate::alert::Alert| None;
        let hud = Hud { active: &active, stale: false, stale_secs: 0, site: "KOHX", home: crate::geo::Coords { lat: 36.0, lon: -87.0 }, peak_dbz: None, eta_for: &eta };
        let text: String = hud
            .lines()
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("no active warnings"), "got: {text}");
    }

    #[test]
    fn stale_feed_states_plainly_that_warnings_are_not_arriving() {
        let active: Vec<ActiveAlert> = Vec::new();
        let eta = |_: &crate::alert::Alert| None;
        let hud = Hud { active: &active, stale: true, stale_secs: 420, site: "KOHX", home: crate::geo::Coords { lat: 36.0, lon: -87.0 }, peak_dbz: None, eta_for: &eta };
        let text: String = hud
            .lines()
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("NOT BEING WARNED"), "got: {text}");
        assert!(text.contains("420"));
    }

    #[test]
    fn widget_renders_in_a_small_area_without_panicking() {
        let active: Vec<ActiveAlert> = Vec::new();
        let eta = |_: &crate::alert::Alert| None;
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        Hud { active: &active, stale: false, stale_secs: 0, site: "KOHX", home: crate::geo::Coords { lat: 36.0, lon: -87.0 }, peak_dbz: None, eta_for: &eta }
            .render(area, &mut buf);
    }
}
