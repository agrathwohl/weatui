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

pub fn threat_rgb(threat: crate::radar::cells::CellThreat) -> crate::render::colormap::Rgb {
    use crate::radar::cells::CellThreat;
    match threat {
        CellThreat::Debris => (255, 60, 60),
        CellThreat::Rotation => (255, 130, 40),
        CellThreat::Hail => (245, 210, 70),
        CellThreat::Intense => (225, 120, 235),
        CellThreat::Strong => (150, 160, 175),
    }
}

fn cells_lines(
    cells: &[crate::radar::cells::StormCell],
    selected: Option<usize>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if cells.is_empty() {
        return lines;
    }
    lines.push(Line::from(Span::styled(
        "storm cells (live volume)",
        Style::default().fg(Color::Rgb(100, 100, 110)),
    )));
    for (i, c) in cells.iter().enumerate() {
        let (r, g, b) = threat_rgb(c.threat);
        let mut style = Style::default().fg(Color::Rgb(r, g, b));
        let marker = if selected == Some(i) {
            style = style.add_modifier(Modifier::BOLD);
            '\u{25b8}'
        } else {
            ' '
        };
        lines.push(Line::from(Span::styled(
            format!(
                "{marker}{} {:<8} {:>3.0} km {:<3} {:.0} dBZ",
                glyph::THUNDERSTORM,
                c.threat.label(),
                c.distance_km,
                c.bearing,
                c.max_dbz
            ),
            style,
        )));
        if selected == Some(i) {
            let detail_style = Style::default().fg(Color::Rgb(200, 200, 210));
            let mut motion = Vec::new();
            if let Some(v) = c.rotation_ms {
                motion.push(format!("\u{394}v {v:.0} m/s"));
            }
            if let Some(cc) = c.min_cc {
                motion.push(format!("cc {cc:.2}"));
            }
            if !motion.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", motion.join(" \u{b7} ")),
                    detail_style,
                )));
            }
            // Floors, not totals: the beam samples only part of the column.
            let mut depth = Vec::new();
            if let Some(v) = c.max_vil {
                depth.push(format!("VIL \u{2265}{v:.0}"));
            }
            if let Some(t) = c.max_echo_top_km {
                depth.push(format!("top \u{2265}{t:.1} km"));
            }
            if !depth.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", depth.join(" \u{b7} ")),
                    detail_style,
                )));
            }
        }
    }
    lines
}

/// Conditions for the displayed frame's own moment: the plain-English sky
/// first, matching the current block above it.
fn frame_conditions_lines(
    at: chrono::DateTime<chrono::Utc>,
    fc: &crate::conditions::FrameConditions,
    tz: chrono_tz::Tz,
    temp_limits: (f32, f32),
) -> Vec<Line<'static>> {
    use crate::conditions::FrameConditions;
    let dim = Style::default().fg(Color::Rgb(140, 140, 150));
    let faint = Style::default().fg(Color::Rgb(100, 100, 110));
    let (tag, short, temp, dew, hum, wind, dir) = match fc {
        FrameConditions::Forecast(h) => (
            "forecast",
            h.short.clone(),
            h.temp_f,
            h.dewpoint_f,
            h.humidity_pct,
            h.wind_mph,
            h.wind_dir.clone().unwrap_or_default(),
        ),
        FrameConditions::Observed(c) => (
            "observed",
            c.description.clone(),
            c.temp_f,
            c.dewpoint_f,
            c.humidity_pct,
            c.wind_mph,
            c.wind_dir.unwrap_or("").to_string(),
        ),
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("at {} ({tag})", at.with_timezone(&tz).format("%H:%M %Z")),
        faint,
    ))];
    if let Some(s) = short {
        lines.push(Line::from(Span::styled(s, dim)));
    }
    lines.push(Line::from(vec![
        Span::styled(format!("{} ", glyph::THERMOMETER), dim),
        temp_span(temp, temp_limits, dim),
        Span::styled(format!("  dew {}", reading(dew, "\u{b0}F")), dim),
    ]));
    lines.push(Line::from(Span::styled(
        format!(
            "{} {}  {} {} {}",
            glyph::HUMIDITY,
            reading(hum, "%"),
            glyph::STRONG_WIND,
            reading(wind, " mph"),
            dir,
        ),
        dim,
    )));
    if let FrameConditions::Observed(c) = fc
        && c.visibility_mi.is_some()
    {
        lines.push(Line::from(Span::styled(
            format!("{} {} vis", glyph::EYE, reading(c.visibility_mi, " mi")),
            dim,
        )));
    }
    lines
}

pub fn tier_color(tier: ThreatTier) -> Color {
    match tier {
        ThreatTier::Lethal => Color::Rgb(255, 64, 64),
        ThreatTier::Severe => Color::Rgb(255, 176, 32),
        ThreatTier::Watch => Color::Rgb(120, 200, 255),
    }
}

const COLD_TEMP: Color = Color::Rgb(100, 160, 255);
const HOT_TEMP: Color = Color::Rgb(255, 90, 80);

fn temp_span(t: Option<f32>, limits: (f32, f32), neutral: Style) -> Span<'static> {
    let style = match t {
        Some(v) if v <= limits.0 => Style::default().fg(COLD_TEMP),
        Some(v) if v >= limits.1 => Style::default().fg(HOT_TEMP),
        _ => neutral,
    };
    Span::styled(reading(t, "\u{b0}F"), style)
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

fn conditions_lines(c: &crate::conditions::Conditions, temp_limits: (f32, f32)) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::Rgb(140, 140, 150));
    let faint = Style::default().fg(Color::Rgb(100, 100, 110));
    let mut lines = Vec::new();
    lines.extend(vec![
        Line::from(vec![
            Span::styled(format!("{} ", glyph::THERMOMETER), dim),
            temp_span(c.temp_f, temp_limits, dim),
            Span::styled(format!("  dew {}", reading(c.dewpoint_f, "\u{b0}F")), dim),
        ]),
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
    ]);

    let mut vis_line = format!("{} {} vis", glyph::EYE, reading(c.visibility_mi, " mi"));
    if let Some(rain) = c.rain_last_hour_in.filter(|r| *r > 0.005) {
        vis_line.push_str(&format!("  {} {rain:.2} in/h", glyph::RAINDROP));
    }
    lines.push(Line::from(Span::styled(vis_line, dim)));

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
    /// `(cold_below_f, hot_above_f)` from `[render]`.
    pub temp_limits: (f32, f32),
    pub frame_conditions:
        Option<(chrono::DateTime<chrono::Utc>, crate::conditions::FrameConditions<'a>)>,
    pub cells: &'a [crate::radar::cells::StormCell],
    pub selected_cell: Option<usize>,
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

        if let Some(d) = self.conditions.and_then(|c| c.description.clone()) {
            lines.push(Line::from(Span::styled(
                d,
                Style::default().fg(Color::Rgb(140, 140, 150)),
            )));
        }
        if self.active.is_empty() && !self.stale {
            lines.push(Line::from(Span::styled(
                "no active warnings",
                Style::default().fg(Color::Rgb(110, 140, 110)),
            )));
        }

        let mut active: Vec<&ActiveAlert> = self.active.iter().collect();
        active.sort_by_key(|a| std::cmp::Reverse(a.tier));

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
            lines.extend(conditions_lines(c, self.temp_limits));
        }
        lines.extend(cells_lines(self.cells, self.selected_cell));
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
        let inner = block.inner(area);
        let top = self.lines();
        let top_len = top.len() as u16;
        Paragraph::new(top).block(block).wrap(Wrap { trim: true }).render(area, buf);

        // The frame-moment block floats at the panel's bottom, clearly apart
        // from the current conditions. It yields entirely when the top
        // content (a warning stack) needs the room.
        if let Some((at, fc)) = &self.frame_conditions {
            let fl = frame_conditions_lines(*at, fc, self.tz, self.temp_limits);
            let h = fl.len() as u16;
            if inner.height > h && top_len + h < inner.height {
                let bottom = Rect {
                    x: inner.x,
                    y: inner.y + inner.height - h,
                    width: inner.width,
                    height: h,
                };
                Paragraph::new(fl).render(bottom, buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(threat: crate::radar::cells::CellThreat) -> crate::radar::cells::StormCell {
        crate::radar::cells::StormCell {
            centroid: crate::geo::Coords { lat: 36.3, lon: -87.0 },
            max_dbz: 57.0,
            rotation_ms: Some(46.0),
            min_cc: Some(0.78),
            max_vil: Some(48.0),
            max_echo_top_km: Some(14.3),
            distance_km: 42.0,
            bearing: "NE",
            threat,
            hazards: vec![crate::radar::cells::Hazard::Rain],
        }
    }

    #[test]
    fn cells_render_ranked_with_detail_on_the_selection() {
        use crate::radar::cells::CellThreat;
        let cells = [cell(CellThreat::Rotation), cell(CellThreat::Strong)];
        let text: Vec<String> = cells_lines(&cells, Some(0))
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let joined = text.join("\n");
        assert!(joined.contains("rotation"), "{joined}");
        assert!(joined.contains("42 km NE"), "{joined}");
        assert!(joined.contains("57 dBZ"), "{joined}");
        assert!(joined.contains("\u{394}v 46 m/s \u{b7} cc 0.78"), "{joined}");
        assert!(
            joined.contains("VIL \u{2265}48 \u{b7} top \u{2265}14.3 km"),
            "beam-limited readings must display as floors: {joined}"
        );
        assert_eq!(
            joined.matches("\u{394}v").count(),
            1,
            "detail line belongs to the selected cell only: {joined}"
        );
    }

    /// Regression: stale with no alerts once rendered conditions and cells
    /// twice.
    #[test]
    fn a_stale_feed_does_not_duplicate_the_sections() {
        use crate::radar::cells::CellThreat;
        let cells = [cell(CellThreat::Strong)];
        let c = crate::conditions::Conditions {
            station: "KBNA".into(),
            observed_at: None,
            description: None,
            temp_f: Some(74.0),
            dewpoint_f: None,
            humidity_pct: None,
            wind_mph: None,
            wind_dir: None,
            visibility_mi: None,
            rain_last_hour_in: None,
        };
        let active: Vec<ActiveAlert> = Vec::new();
        let eta = |_: &crate::alert::Alert| None;
        let hud = Hud {
            active: &active,
            stale: true,
            stale_secs: 400,
            site: "KOHX",
            home: crate::geo::Coords { lat: 36.0, lon: -87.0 },
            peak_dbz: None,
            peak_units: "dBZ",
            conditions: Some(&c),
            frame_conditions: None,
            temp_limits: (32.0, 95.0),
            cells: &cells,
            selected_cell: None,
            tz: chrono_tz::America::Chicago,
            eta_for: &eta,
        };
        let text: Vec<String> = hud
            .lines()
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let joined = text.join("\n");
        assert_eq!(joined.matches("storm cells").count(), 1, "{joined}");
        assert_eq!(joined.matches("74\u{b0}F").count(), 1, "{joined}");
        assert!(joined.contains("FEED STALE"), "{joined}");
    }

    #[test]
    fn no_cells_means_no_section_rather_than_an_empty_header() {
        assert!(cells_lines(&[], None).is_empty());
    }

    /// The panel reads top-down as: what the sky is doing, whether it can
    /// kill you, then the numbers.
    #[test]
    fn the_panel_orders_sky_then_warnings_then_readings() {
        let c = crate::conditions::Conditions {
            station: "KBNA".into(),
            observed_at: None,
            description: Some("Mostly Cloudy".into()),
            temp_f: Some(74.0),
            dewpoint_f: None,
            humidity_pct: None,
            wind_mph: None,
            wind_dir: None,
            visibility_mi: Some(10.0),
            rain_last_hour_in: None,
        };
        let active: Vec<ActiveAlert> = Vec::new();
        let eta = |_: &crate::alert::Alert| None;
        let hud = Hud {
            active: &active,
            stale: false,
            stale_secs: 0,
            site: "KOHX",
            home: crate::geo::Coords { lat: 36.0, lon: -87.0 },
            peak_dbz: None,
            peak_units: "dBZ",
            conditions: Some(&c),
            frame_conditions: None,
            temp_limits: (32.0, 95.0),
            cells: &[],
            selected_cell: None,
            tz: chrono_tz::America::Chicago,
            eta_for: &eta,
        };
        let text: Vec<String> = hud
            .lines()
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let at = |needle: &str| {
            text.iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} missing from {text:?}"))
        };
        assert!(at("Mostly Cloudy") < at("no active warnings"), "{text:?}");
        assert!(at("no active warnings") < at("74\u{b0}F"), "{text:?}");
    }

    /// The frame-moment block floats at the bottom of the panel, apart from
    /// the current conditions, and vanishes when the top content needs room.
    #[test]
    fn the_frame_block_is_anchored_to_the_panel_bottom() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let at = chrono::DateTime::parse_from_rfc3339("2026-08-02T23:45:00Z")
            .unwrap()
            .to_utc();
        let h = crate::conditions::HourlyForecast {
            valid: at,
            temp_f: Some(78.0),
            dewpoint_f: None,
            humidity_pct: None,
            wind_mph: None,
            wind_dir: None,
            short: Some("Chance Showers".into()),
        };
        let active: Vec<ActiveAlert> = Vec::new();
        let eta = |_: &crate::alert::Alert| None;
        let hud = Hud {
            active: &active,
            stale: false,
            stale_secs: 0,
            site: "KOHX",
            home: crate::geo::Coords { lat: 36.0, lon: -87.0 },
            peak_dbz: None,
            peak_units: "dBZ",
            conditions: None,
            frame_conditions: Some((at, crate::conditions::FrameConditions::Forecast(&h))),
            temp_limits: (32.0, 95.0),
            cells: &[],
            selected_cell: None,
            tz: chrono_tz::America::Chicago,
            eta_for: &eta,
        };
        let area = Rect::new(0, 0, 34, 20);
        let mut buf = Buffer::empty(area);
        hud.render(area, &mut buf);
        let row = |y: u16| -> String {
            (0..area.width).map(|x| buf.cell((x, y)).unwrap().symbol().to_string()).collect()
        };
        assert!(row(15).contains("(forecast)"), "header at bottom: {:?}", row(15));
        assert!(row(16).contains("Chance Showers"), "{:?}", row(16));
        assert!(row(18).contains("%"), "{:?}", row(18));
        assert!(
            (3..14).all(|y| !row(y).contains("(forecast)")),
            "the block must not sit next to the current conditions"
        );
    }

    #[test]
    fn a_forecast_frame_shows_its_own_moments_conditions() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-08-02T23:45:00Z")
            .unwrap()
            .to_utc();
        let h = crate::conditions::HourlyForecast {
            valid: at,
            temp_f: Some(78.0),
            dewpoint_f: Some(69.0),
            humidity_pct: Some(62.0),
            wind_mph: Some(8.0),
            wind_dir: Some("WSW".into()),
            short: Some("Chance Showers".into()),
        };
        let fc = crate::conditions::FrameConditions::Forecast(&h);
        let text: Vec<String> = frame_conditions_lines(at, &fc, chrono_tz::America::Chicago, (32.0, 95.0))
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text[0].contains("18:45 CDT"), "frame time in the location's zone: {text:?}");
        assert!(text[0].contains("(forecast)"), "{text:?}");
        assert_eq!(text[1], "Chance Showers", "plain English on top here too");
        assert!(text[2].contains("78\u{b0}F"), "{text:?}");
        assert!(text[3].contains("8 mph WSW"), "{text:?}");
    }

    #[test]
    fn a_past_frame_shows_the_observation_including_visibility() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-08-02T04:00:00Z")
            .unwrap()
            .to_utc();
        let c = crate::conditions::Conditions {
            station: "KM02".into(),
            observed_at: Some(at),
            description: Some("Fog".into()),
            temp_f: Some(66.0),
            dewpoint_f: Some(65.0),
            humidity_pct: Some(97.0),
            wind_mph: Some(3.0),
            wind_dir: Some("N"),
            visibility_mi: Some(0.5),
            rain_last_hour_in: None,
        };
        let fc = crate::conditions::FrameConditions::Observed(&c);
        let text: Vec<String> = frame_conditions_lines(at, &fc, chrono_tz::America::Chicago, (32.0, 95.0))
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text[0].contains("(observed)"), "{text:?}");
        assert_eq!(text[1], "Fog");
        assert!(text.iter().any(|l| l.contains("0.5 mi vis")), "{text:?}");
    }

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
        let text: Vec<String> = conditions_lines(&c, (32.0, 95.0))
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let joined = text.join("\n");
        assert!(joined.contains("74\u{b0}F"), "{joined}");
        assert!(joined.contains("--\u{b0}F"), "a dead dewpoint sensor must show as --: {joined}");
        assert!(joined.contains("8 mph SSW"), "{joined}");
        assert!(joined.contains("0.2 mi"), "sub-3-mile visibility keeps a decimal: {joined}");
        assert!(joined.contains("0.12 in/h"), "{joined}");
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
        let text: Vec<String> = conditions_lines(&c, (32.0, 95.0))
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
        let hud = Hud { active: &active, stale: false, stale_secs: 0, site: "KOHX", home: crate::geo::Coords { lat: 36.0, lon: -87.0 }, peak_dbz: None, peak_units: "dBZ", conditions: None, frame_conditions: None, temp_limits: (32.0, 95.0), cells: &[], selected_cell: None, tz: chrono_tz::America::Chicago, eta_for: &eta };
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
        let hud = Hud { active: &active, stale: true, stale_secs: 420, site: "KOHX", home: crate::geo::Coords { lat: 36.0, lon: -87.0 }, peak_dbz: None, peak_units: "dBZ", conditions: None, frame_conditions: None, temp_limits: (32.0, 95.0), cells: &[], selected_cell: None, tz: chrono_tz::America::Chicago, eta_for: &eta };
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
        Hud { active: &active, stale: false, stale_secs: 0, site: "KOHX", home: crate::geo::Coords { lat: 36.0, lon: -87.0 }, peak_dbz: None, peak_units: "dBZ", conditions: None, frame_conditions: None, temp_limits: (32.0, 95.0), cells: &[], selected_cell: None, tz: chrono_tz::America::Chicago, eta_for: &eta }
            .render(area, &mut buf);
    }
}
