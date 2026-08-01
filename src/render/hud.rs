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

fn reading(v: Option<f32>, unit: &str) -> String {
    match v {
        Some(x) if unit == " mi" && x < 3.0 => format!("{x:.1}{unit}"),
        Some(x) => {
            let r = if x.round() == 0.0 { 0.0 } else { x.round() };
            format!("{r:.0}{unit}")
        }
        None => format!("--{unit}"),
    }
}

fn conditions_lines(c: &crate::conditions::Conditions) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::Rgb(140, 140, 150));
    let faint = Style::default().fg(Color::Rgb(100, 100, 110));
    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "{} {}  dew {}",
                glyph::THERMOMETER,
                reading(c.temp_f, "\u{b0}F"),
                reading(c.dewpoint_f, "\u{b0}F"),
            ),
            dim,
        )),
        Line::from(Span::styled(
            format!(
                "{} {}  {} {} {}",
                glyph::HUMIDITY,
                reading(c.humidity_pct, "%"),
                glyph::STRONG_WIND,
                reading(c.wind_mph, " mph"),
                c.wind_dir.unwrap_or(""),
            ),
            dim,
        )),
    ];

    let mut vis_line = format!("{} {} vis", glyph::EYE, reading(c.visibility_mi, " mi"));
    if let Some(rain) = c.rain_last_hour_in.filter(|r| *r > 0.005) {
        vis_line.push_str(&format!("  {} {rain:.2} in/h", glyph::RAINDROP));
    }
    lines.push(Line::from(Span::styled(vis_line, dim)));

    if let Some(d) = &c.description {
        lines.push(Line::from(Span::styled(d.clone(), dim)));
    }

    let age = c
        .observed_at
        .map(|t| (chrono::Utc::now() - t).num_minutes().max(0))
        .map_or(String::new(), |m| format!(" \u{b7} {m}m old"));
    lines.push(Line::from(Span::styled(format!("{}{age}", c.station), faint)));
    lines
}

pub struct Hud<'a> {
    pub active: &'a [ActiveAlert],
    pub stale: bool,
    pub stale_secs: u64,
    pub site: &'a str,
    pub home: crate::geo::Coords,
    pub peak_dbz: Option<f32>,
    pub peak_units: &'static str,
    pub conditions: Option<&'a crate::conditions::Conditions>,
    pub tz: chrono_tz::Tz,
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
                Some(peak) => format!("{} {}  peak {peak:.0} {}", glyph::REFRESH, self.site, self.peak_units),
                None => format!("{} {}", glyph::REFRESH, self.site),
            },
            Style::default().fg(Color::Rgb(140, 140, 150)),
        )));

        if self.active.is_empty()
            && let Some(c) = self.conditions
        {
            lines.extend(conditions_lines(c));
        }

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
                .map(|t| t.with_timezone(&self.tz).format("%H:%M %Z").to_string());
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

        // Below the warnings, so a long conditions block can only ever clip
        // itself off a short terminal, never the instruction to take shelter.
        if let Some(c) = self.conditions {
            lines.extend(conditions_lines(c));
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
    fn conditions_render_readings_and_mark_missing_sensors() {
        let c = crate::conditions::Conditions {
            station: "KBNA".into(),
            observed_at: Some(chrono::Utc::now() - chrono::Duration::minutes(24)),
            description: Some("Partly Cloudy".into()),
            temp_f: Some(74.0),
            dewpoint_f: None,
            humidity_pct: Some(61.0),
            wind_mph: Some(8.0),
            wind_dir: Some("SSW"),
            visibility_mi: Some(0.25),
            rain_last_hour_in: Some(0.12),
        };
        let text: Vec<String> = conditions_lines(&c)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let joined = text.join("\n");
        assert!(joined.contains("74\u{b0}F"), "{joined}");
        assert!(joined.contains("--\u{b0}F"), "a dead dewpoint sensor must show as --: {joined}");
        assert!(joined.contains("8 mph SSW"), "{joined}");
        assert!(joined.contains("0.2 mi"), "sub-3-mile visibility keeps a decimal: {joined}");
        assert!(joined.contains("0.12 in/h"), "{joined}");
        assert!(joined.contains("Partly Cloudy"), "{joined}");
        assert!(joined.contains("KBNA \u{b7} 24m old"), "{joined}");
    }

    #[test]
    fn dry_hours_hide_the_rain_reading_entirely() {
        let c = crate::conditions::Conditions {
            station: "KBNA".into(),
            observed_at: None,
            description: None,
            temp_f: None,
            dewpoint_f: None,
            humidity_pct: None,
            wind_mph: None,
            wind_dir: None,
            visibility_mi: None,
            rain_last_hour_in: Some(0.0),
        };
        let text: Vec<String> = conditions_lines(&c)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let joined = text.join("\n");
        assert!(!joined.contains("in/h"), "zero rain is not a reading worth a line: {joined}");
        assert!(joined.contains("KBNA"), "{joined}");
        assert!(!joined.contains("m old"), "no timestamp means no age claim: {joined}");
    }

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
            glyph::THERMOMETER,
            glyph::HUMIDITY,
            glyph::RAINDROP,
        ] {
            let c = g as u32;
            assert!((0xe300..=0xe3e3).contains(&c), "{c:x} outside the weather range");
        }
        for g in [glyph::PLAY, glyph::PAUSE, glyph::CLOCK, glyph::EYE] {
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
        let hud = Hud { active: &active, stale: false, stale_secs: 0, site: "KOHX", home: crate::geo::Coords { lat: 36.0, lon: -87.0 }, peak_dbz: None, peak_units: "dBZ", conditions: None, tz: chrono_tz::America::Chicago, eta_for: &eta };
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
        let hud = Hud { active: &active, stale: true, stale_secs: 420, site: "KOHX", home: crate::geo::Coords { lat: 36.0, lon: -87.0 }, peak_dbz: None, peak_units: "dBZ", conditions: None, tz: chrono_tz::America::Chicago, eta_for: &eta };
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
        Hud { active: &active, stale: false, stale_secs: 0, site: "KOHX", home: crate::geo::Coords { lat: 36.0, lon: -87.0 }, peak_dbz: None, peak_units: "dBZ", conditions: None, tz: chrono_tz::America::Chicago, eta_for: &eta }
            .render(area, &mut buf);
    }
}
