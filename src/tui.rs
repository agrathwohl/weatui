//! Interactive front end: layout, vim keymap, and the redraw loop.

use crate::alert::filter::ThreatTier;
use crate::config::{Colormap, Config};
use crate::daemon::AlertEngine;
use crate::geo::{Coords, RadarSite, nearest_radar_site, radar_site_by_id};
use crate::notify;
use crate::radar::grid::{Viewport, rasterize_product};
use crate::radar::ring::{FrameRing, RadarFrame};
use crate::radar::{DbzGrid, RadarField, RadarProduct, fetch};
use crate::render::colormap::product_rgb;
use crate::render::hud::Hud;
use crate::render::overlay::{
    HOME_MARKER, LETHAL_OUTLINE, PixelOverlay, DISTANCE_RING, SEVERE_OUTLINE, WATCH_OUTLINE,
};
use crate::render::raster::RadarRaster;
use crate::render::timeline::Timeline;
use anyhow::{Context, Result};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use std::io::{Stdout, stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MIN_SPAN_KM: f64 = 20.0;
const MAX_SPAN_KM: f64 = 900.0;
const DEFAULT_SPAN_KM: f64 = 260.0;
const HUD_WIDTH: u16 = 34;
const DISTANCE_RING_KM: &[f64] = &[25.0, 50.0, 100.0];
const MAX_FORECAST_FRAMES: usize = 18;

/// Base layers exist on both halves of the timeline, so switching bases never
/// blanks the forecast side. The observation-only moments are augmentations
/// painted over whichever base is showing.
const BASE_PRODUCTS: [RadarProduct; 3] = [
    RadarProduct::Reflectivity,
    RadarProduct::EchoTop,
    RadarProduct::VerticallyIntegratedLiquid,
];
const AUG_PRODUCTS: [RadarProduct; 4] = [
    RadarProduct::Velocity,
    RadarProduct::CorrelationCoefficient,
    RadarProduct::DifferentialReflectivity,
    RadarProduct::SpectrumWidth,
];
/// Augmentations paint only inside real echo. Below this the Doppler and
/// dual-pol moments are dominated by clear-air biologicals, and a default-on
/// velocity layer would light up every summer night with false rotation.
const AUG_MIN_REFLECTIVITY_DBZ: f32 = 30.0;

const HELP: &str = "\
 MAP
   h j k l    pan west/south/north/east
   C-d C-u    pan half a screen
   zi zo      zoom in / out
   gh         recentre on home

 TIMELINE
   [ ]        previous / next frame
   gg         oldest frame
   G          newest (live)
   space      play / pause
   < >        slower / faster

 LAYERS
   1 2 3      base: reflectivity / echo top / VIL
   4 5 6 7    toggle: velocity / debris CC / ZDR / spec width

 CELLS
   n N        cycle storm cells (map follows)

 FORECAST
   f          cycle horizon: 2h / 6h / 18h

   ?          toggle this help
   q  ZZ      quit
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForecastHorizon {
    Near,
    Mid,
    Long,
}

impl ForecastHorizon {
    /// `cycle_age` shifts the window so the horizon counts forward from now
    /// rather than from a cycle that may already be two hours old.
    pub fn steps(self, cycle_age: chrono::Duration) -> Vec<crate::radar::hrrr::ForecastStep> {
        use crate::radar::hrrr::{MAX_LEAD_MINUTES, leads_every};
        let age = cycle_age.num_minutes().clamp(0, MAX_LEAD_MINUTES as i64) as u16;
        match self {
            ForecastHorizon::Near => leads_every(15, age, age + 120),
            ForecastHorizon::Mid => {
                let mut steps = leads_every(15, age, age + 120);
                steps.extend(leads_every(60, age + 120, age + 360));
                steps
            }
            ForecastHorizon::Long => leads_every(60, age, age + 1080),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ForecastHorizon::Near => "+2h/15m",
            ForecastHorizon::Mid => "+6h",
            ForecastHorizon::Long => "+18h",
        }
    }

    pub fn next(self) -> Self {
        match self {
            ForecastHorizon::Near => ForecastHorizon::Mid,
            ForecastHorizon::Mid => ForecastHorizon::Long,
            ForecastHorizon::Long => ForecastHorizon::Near,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    CycleForecast,
    CellNext,
    CellPrev,
    SelectProduct(usize),
    ToggleProductOverlay(usize),
    PanWest,
    PanSouth,
    PanNorth,
    PanEast,
    PanHalfDown,
    PanHalfUp,
    ZoomIn,
    ZoomOut,
    RecentreHome,
    FrameBack,
    FrameForward,
    JumpOldest,
    JumpNewest,
    TogglePlay,
    Faster,
    Slower,
    ToggleHelp,
    Quit,
    Ignored,
}

/// `g`, `z` and `Z` open a pending sequence. Everything else resolves on the
/// first keypress, so single-key actions never wait for a timeout.
pub fn resolve_key(pending: &mut Option<char>, key: KeyEvent) -> Action {
    if let Some(prefix) = pending.take() {
        return match (prefix, key.code) {
            ('g', KeyCode::Char('g')) => Action::JumpOldest,
            ('g', KeyCode::Char('h')) => Action::RecentreHome,
            ('z', KeyCode::Char('i')) => Action::ZoomIn,
            ('z', KeyCode::Char('o')) => Action::ZoomOut,
            ('Z', KeyCode::Char('Z')) => Action::Quit,
            _ => Action::Ignored,
        };
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('d') => Action::PanHalfDown,
            KeyCode::Char('u') => Action::PanHalfUp,
            KeyCode::Char('c') => Action::Quit,
            _ => Action::Ignored,
        };
    }

    match key.code {
        KeyCode::Char('h') => Action::PanWest,
        KeyCode::Char('j') => Action::PanSouth,
        KeyCode::Char('k') => Action::PanNorth,
        KeyCode::Char('l') => Action::PanEast,
        KeyCode::Char('[') => Action::FrameBack,
        KeyCode::Char(']') => Action::FrameForward,
        KeyCode::Char('G') => Action::JumpNewest,
        KeyCode::Char(' ') => Action::TogglePlay,
        KeyCode::Char('>') => Action::Faster,
        KeyCode::Char('<') => Action::Slower,
        KeyCode::Char('f') => Action::CycleForecast,
        KeyCode::Char('n') => Action::CellNext,
        KeyCode::Char('N') => Action::CellPrev,
        KeyCode::Char(c @ '1'..='3') => Action::SelectProduct(c as usize - '1' as usize),
        KeyCode::Char(c @ '4'..='7') => Action::ToggleProductOverlay(c as usize - '4' as usize),
        KeyCode::Char('?') => Action::ToggleHelp,
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Char(c @ ('g' | 'z' | 'Z')) => {
            *pending = Some(c);
            Action::Ignored
        }
        _ => Action::Ignored,
    }
}

pub struct App {
    active: Vec<crate::alert::state::ActiveAlert>,
    stale: bool,
    stale_secs: u64,
    ring: FrameRing,
    viewport: Viewport,
    home: Coords,
    site: RadarSite,
    colormap: Colormap,
    temp_limits: (f32, f32),
    tz: chrono_tz::Tz,
    pending: Option<char>,
    playback: Duration,
    product: RadarProduct,
    product_overlays: Vec<RadarProduct>,
    horizon: ForecastHorizon,
    horizon_tx: Option<tokio::sync::watch::Sender<ForecastHorizon>>,
    conditions: Option<crate::conditions::Conditions>,
    obs_history: Vec<crate::conditions::Conditions>,
    hourly: Vec<crate::conditions::HourlyForecast>,
    cells: Vec<crate::radar::cells::StormCell>,
    selected_cell: Option<usize>,
    show_help: bool,
    quit: bool,
    status: String,
}

impl App {
    pub fn new(cfg: &Config, home: Coords, tz: chrono_tz::Tz) -> Result<Self> {
        let site = if cfg.radar.site.eq_ignore_ascii_case("auto") {
            nearest_radar_site(home).context("no WSR-88D site could be selected")?
        } else {
            radar_site_by_id(&cfg.radar.site)
                .with_context(|| format!("unknown radar site {:?}", cfg.radar.site))?
        };

        Ok(App {
            active: Vec::new(),
            stale: false,
            stale_secs: 0,
            // Projections are appended alongside observations, so capacity must
            // cover both. Sizing the ring to `frames` alone means enabling
            // projections silently evicts that many observed volumes.
            ring: FrameRing::new(cfg.radar.frames + MAX_FORECAST_FRAMES),
            viewport: Viewport::new(home, DEFAULT_SPAN_KM, 1, 1),
            home,
            site,
            colormap: cfg.render.colormap,
            temp_limits: (cfg.render.cold_below_f, cfg.render.hot_above_f),
            tz,
            pending: None,
            playback: Duration::from_millis(450),
            product: RadarProduct::Reflectivity,
            product_overlays: vec![RadarProduct::Velocity, RadarProduct::CorrelationCoefficient],
            horizon: ForecastHorizon::Near,
            horizon_tx: None,
            conditions: None,
            obs_history: Vec::new(),
            hourly: Vec::new(),
            cells: Vec::new(),
            selected_cell: None,
            show_help: false,
            quit: false,
            status: format!(
                "{} selected, {:.0} km away, waiting for first volume",
                site.id,
                crate::geo::haversine_km(home, site.coords)
            ),
        })
    }

    fn pan_step_km(&self) -> f64 {
        (self.viewport.span_km / 8.0).max(1.0)
    }

    fn apply(&mut self, action: Action) {
        let step = self.pan_step_km();
        match action {
            Action::PanWest => self.viewport = self.viewport.panned_km(-step, 0.0),
            Action::PanEast => self.viewport = self.viewport.panned_km(step, 0.0),
            Action::PanNorth => self.viewport = self.viewport.panned_km(0.0, step),
            Action::PanSouth => self.viewport = self.viewport.panned_km(0.0, -step),
            Action::PanHalfUp => self.viewport = self.viewport.panned_km(0.0, step * 4.0),
            Action::PanHalfDown => self.viewport = self.viewport.panned_km(0.0, -step * 4.0),
            Action::ZoomIn => self.viewport = self.viewport.zoomed(0.5, MIN_SPAN_KM, MAX_SPAN_KM),
            Action::ZoomOut => self.viewport = self.viewport.zoomed(2.0, MIN_SPAN_KM, MAX_SPAN_KM),
            Action::RecentreHome => self.viewport.centre = self.home,
            Action::FrameBack => self.ring.step_back(),
            Action::FrameForward => self.ring.step_forward(),
            Action::JumpOldest => self.ring.jump_oldest(),
            Action::JumpNewest => self.ring.jump_newest(),
            Action::TogglePlay => self.ring.toggle_play(),
            Action::Faster => {
                self.playback = self.playback.mul_f64(0.7).max(Duration::from_millis(60))
            }
            Action::Slower => {
                self.playback = self.playback.mul_f64(1.4).min(Duration::from_millis(4000))
            }
            Action::CellNext | Action::CellPrev if self.cells.is_empty() => {
                self.status = "no storm cells detected".to_string();
            }
            Action::CellNext => {
                let next = self.selected_cell.map_or(0, |i| (i + 1) % self.cells.len());
                self.select_cell(next);
            }
            Action::CellPrev => {
                let len = self.cells.len();
                let prev = self.selected_cell.map_or(len - 1, |i| (i + len - 1) % len);
                self.select_cell(prev);
            }
            Action::SelectProduct(i) => {
                if let Some(p) = BASE_PRODUCTS.get(i).copied() {
                    self.product = p;
                }
            }
            Action::ToggleProductOverlay(i) => {
                if let Some(p) = AUG_PRODUCTS.get(i).copied() {
                    match self.product_overlays.iter().position(|q| *q == p) {
                        Some(at) => drop(self.product_overlays.remove(at)),
                        None => self.product_overlays.push(p),
                    }
                }
            }
            Action::CycleForecast => {
                self.horizon = self.horizon.next();
                if let Some(tx) = &self.horizon_tx {
                    let _ = tx.send(self.horizon);
                }
                self.status = format!("forecast horizon {}", self.horizon.label());
            }
            Action::ToggleHelp => self.show_help = !self.show_help,
            Action::Quit => self.quit = true,
            Action::Ignored => {}
        }
    }

}

/// A frame with nothing to draw and a frame that failed to load are both solid
/// black. Only this number tells them apart, which is why it is on screen
/// rather than left for the user to infer.
fn drawn_cells(grid: &DbzGrid, product: RadarProduct, map: Colormap) -> usize {
    (0..grid.height)
        .flat_map(|y| (0..grid.width).map(move |x| (x, y)))
        .filter(|(x, y)| {
            grid.get(*x, *y)
                .and_then(|v| product_rgb(v, product, map))
                .is_some()
        })
        .count()
}

fn echo_summary(grid: &DbzGrid, product: RadarProduct, map: Colormap) -> String {
    match drawn_cells(grid, product, map) {
        0 if grid.value_range().is_some() => "no echo in view".to_string(),
        0 => "no data in view".to_string(),
        n => format!("{n} cells"),
    }
}

/// Coarse sweep for the closest drawable echo outside the viewport.
///
/// Only runs on an otherwise blank frame, so the cost never lands on a frame
/// that has something to draw. The step is deliberately far coarser than the
/// display: this answers "is there weather anywhere near me", not "where
/// exactly", and a fine sweep here would stall the redraw loop.
fn nearest_echo_km(
    field: &dyn RadarField,
    home: Coords,
    product: RadarProduct,
    map: Colormap,
) -> Option<(f64, &'static str)> {
    let mut best: Option<(f64, &'static str)> = None;
    let mut lat = home.lat - 8.0;
    while lat < home.lat + 8.0 {
        let mut lon = home.lon - 8.0;
        while lon < home.lon + 8.0 {
            let at = Coords { lat, lon };
            if field
                .value_at(at, product)
                .and_then(|v| product_rgb(v, product, map))
                .is_some()
            {
                let km = crate::geo::haversine_km(home, at);
                if best.is_none_or(|(d, _)| km < d) {
                    best = Some((km, crate::geo::compass_bearing(home, at)));
                }
            }
            lon += 0.25;
        }
        lat += 0.25;
    }
    best
}

impl App {
    /// The layer set is standing state, so it is rendered every frame rather
    /// than announced once into `status`, which the next poll would overwrite.
    fn layer_summary(&self) -> String {
        let mut out = match self.product.units() {
            "" => self.product.label().to_string(),
            units => format!("{} {units}", self.product.label()),
        };
        for extra in &self.product_overlays {
            out.push_str(" + ");
            out.push_str(extra.label());
        }
        out
    }

    /// Overlay products are painted before the warning geometry, so a polygon
    /// outline is never buried under the field it is warning about.
    fn paint_product_overlays(&self, overlay: &mut PixelOverlay) {
        let Some(frame) = self.ring.current() else { return };
        let field = frame.field.as_ref();
        let augs: Vec<RadarProduct> = self
            .product_overlays
            .iter()
            .copied()
            .filter(|p| field.supports(*p))
            .collect();
        if augs.is_empty() {
            return;
        }
        let echo = rasterize_product(field, &self.viewport, RadarProduct::Reflectivity);
        for product in augs {
            let grid = rasterize_product(field, &self.viewport, product);
            for y in 0..grid.height {
                for x in 0..grid.width {
                    if !echo.get(x, y).is_some_and(|dbz| dbz >= AUG_MIN_REFLECTIVITY_DBZ) {
                        continue;
                    }
                    let Some(value) = grid.get(x, y) else { continue };
                    if !product.is_notable(value) {
                        continue;
                    }
                    if let Some(rgb) = product_rgb(value, product, self.colormap) {
                        overlay.set(x as i64, y as i64, rgb);
                    }
                }
            }
        }
    }

    fn select_cell(&mut self, index: usize) {
        self.selected_cell = Some(index);
        self.viewport.centre = self.cells[index].centroid;
    }

    /// The current-conditions block always shows now; this picks what the
    /// displayed frame's own moment looked or will look like. The newest
    /// observed frame IS now, so it gets nothing extra.
    fn frame_conditions(
        &self,
    ) -> Option<(chrono::DateTime<chrono::Utc>, crate::conditions::FrameConditions<'_>)> {
        let newest_observed = self
            .ring
            .frames()
            .filter(|f| !f.projected)
            .last()
            .map(|f| f.captured_at);
        let f = self.ring.current()?;
        if !f.projected && Some(f.captured_at) == newest_observed {
            return None;
        }
        let fc = if f.projected {
            crate::conditions::FrameConditions::Forecast(crate::conditions::nearest_hourly(
                &self.hourly,
                f.captured_at,
            )?)
        } else {
            crate::conditions::FrameConditions::Observed(crate::conditions::nearest_observation(
                &self.obs_history,
                f.captured_at,
            )?)
        };
        Some((f.captured_at, fc))
    }

    fn build_overlay(&self, width: usize, height: usize) -> PixelOverlay {
        let mut overlay = PixelOverlay::new(width, height);
        overlay.draw_distance_rings(self.home, DISTANCE_RING_KM, &self.viewport, DISTANCE_RING);
        self.paint_product_overlays(&mut overlay);
        let mut ordered: Vec<_> = self.active.iter().collect();
        ordered.sort_by_key(|a| a.tier);
        for entry in ordered {
            let colour = match entry.tier {
                ThreatTier::Lethal => LETHAL_OUTLINE,
                ThreatTier::Severe => SEVERE_OUTLINE,
                ThreatTier::Watch => WATCH_OUTLINE,
            };
            if let Some(geom) = &entry.alert.geometry {
                for ring in geom.outer_rings() {
                    overlay.draw_ring(ring, &self.viewport, colour);
                }
            }
        }
        // Cell markers describe the newest observed volume; on history or
        // forecast frames they would sit on echo they do not belong to.
        let newest_observed = self
            .ring
            .frames()
            .filter(|f| !f.projected)
            .last()
            .map(|f| f.captured_at);
        let on_live_frame = self
            .ring
            .current()
            .is_some_and(|f| !f.projected && Some(f.captured_at) == newest_observed);
        for (i, cell) in self.cells.iter().enumerate().filter(|_| on_live_frame) {
            overlay.draw_cell_marker(
                cell.centroid,
                &self.viewport,
                if self.selected_cell == Some(i) {
                    HOME_MARKER
                } else {
                    crate::render::hud::threat_rgb(cell.threat)
                },
                self.selected_cell == Some(i),
            );
        }
        overlay.draw_home(self.home, &self.viewport, HOME_MARKER);
        overlay
    }

    fn draw(&mut self, frame: &mut ratatui::Frame) {
        let root = frame.area();
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(20), Constraint::Length(HUD_WIDTH)])
            .split(root);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1), Constraint::Length(1)])
            .split(columns[0]);

        let map = rows[0];
        self.viewport.width = map.width as usize;
        self.viewport.height = map.height as usize * 2;

        let overlay = self.build_overlay(self.viewport.width, self.viewport.height);
        let grid = match self.ring.current() {
            Some(f) => rasterize_product(f.field.as_ref(), &self.viewport, self.product),
            None => crate::radar::DbzGrid::new(self.viewport.width, self.viewport.height),
        };

        frame.render_widget(
            RadarRaster { grid: &grid, overlay: Some(&overlay), colormap: self.colormap, product: self.product },
            map,
        );

        // An empty frame and a broken one are the same black rectangle, so an
        // empty one has to say so on the map rather than only in the status.
        // Never paint the explainer over an active warning: a banner that
        // hides a tornado polygon is worse than an unexplained black frame.
        if drawn_cells(&grid, self.product, self.colormap) == 0
            && self.active.is_empty()
            && let Some(f) = self.ring.current()
        {
            let banner = if !f.field.supports(self.product) {
                format!(
                    " {} UNAVAILABLE ON THIS FRAME \n {} did not provide it ",
                    self.product.label().to_uppercase(),
                    f.field.source_label(),
                )
            } else {
                match nearest_echo_km(f.field.as_ref(), self.home, self.product, self.colormap) {
                    Some((km, dir)) => format!(
                        " NO {} IN VIEW \n {} \n nearest {:.0} km {} \n zo zooms out · hjkl pan ",
                        self.product.label().to_uppercase(),
                        f.field.source_label(),
                        km,
                        dir
                    ),
                    None => format!(
                        " NO {} IN VIEW \n {} \n none within 800 km ",
                        self.product.label().to_uppercase(),
                        f.field.source_label()
                    ),
                }
            };
            let lines = banner.lines().count() as u16;
            let width = banner.lines().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
            if map.width > width && map.height > lines {
                let area = Rect::new(
                    map.x + (map.width - width) / 2,
                    map.y + (map.height - lines) / 2,
                    width,
                    lines,
                );
                frame.render_widget(Clear, area);
                frame.render_widget(
                    Paragraph::new(banner)
                        .style(Style::default().fg(Color::Rgb(150, 155, 165)))
                        .alignment(ratatui::layout::Alignment::Center),
                    area,
                );
            }
        }
        frame.render_widget(Timeline { ring: &self.ring, tz: self.tz }, rows[1]);
        frame.render_widget(
            Paragraph::new(format!(
                "{} | {} | {}",
                self.layer_summary(),
                echo_summary(&grid, self.product, self.colormap),
                self.status
            ))
                .style(Style::default().fg(Color::Rgb(130, 130, 140))),
            rows[2],
        );

        let home = self.home;
        let eta = move |alert: &crate::alert::Alert| {
            alert.motion().and_then(|m| m.eta_to(home)).map(|d| d.num_minutes())
        };
        frame.render_widget(
            Hud {
                active: &self.active,
                stale: self.stale,
                stale_secs: self.stale_secs,
                site: self.site.id,
                home: self.home,
                peak_dbz: grid.value_range().map(|(_, hi)| hi),
                peak_units: self.product.units(),
                conditions: self.conditions.as_ref(),
                frame_conditions: self.frame_conditions(),
                temp_limits: self.temp_limits,
                cells: &self.cells,
                selected_cell: self.selected_cell,
                tz: self.tz,
                eta_for: &eta,
            },
            columns[1],
        );

        if self.show_help {
            let area = centred(58, 18, root);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(HELP)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" keys ")
                            .border_style(Style::default().fg(Color::Rgb(120, 200, 255))),
                    )
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
    }
}

fn centred(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// The alert engine lives in its own task, so its view of the world is shipped
/// across whole rather than mutated in place. `Tick` alone is not enough: the
/// HUD needs the full active set, not just the newly-fired notifications.
struct AlertSnapshot {
    tick: crate::daemon::Tick,
    active: Vec<crate::alert::state::ActiveAlert>,
    stale: bool,
    stale_secs: u64,
    /// A typo'd script path would otherwise fail on every alert, forever,
    /// with no feedback anywhere in the TUI.
    dispatch_error: Option<String>,
}

struct RadarUpdate {
    observed_at: chrono::DateTime<chrono::Utc>,
    field: Box<dyn RadarField>,
    /// `Some` while history is still loading, carrying "n/total" for the
    /// status line. Projections are suppressed until it is `None`.
    backfill: Option<String>,
    /// `Some` only for the live volume: backfill frames are history, and
    /// listing their storm cells would present stale threats as current.
    cells: Option<Vec<crate::radar::cells::StormCell>>,
}

struct ForecastUpdate {
    valid_at: chrono::DateTime<chrono::Utc>,
    field: Box<dyn RadarField>,
    lead_minutes: i64,
    /// Marks the start of a batch so the ring can retire the previous
    /// forecast before the replacement lands, rather than interleaving two.
    first_of_batch: bool,
    index: usize,
    total: usize,
}

enum Update {
    Alerts(Box<AlertSnapshot>),
    Radar(Result<RadarUpdate>),
    Forecast(Result<Box<ForecastUpdate>>),
    Conditions(Box<ConditionsUpdate>),
}

/// Everything one conditions cycle learned. Fields the cycle failed to fetch
/// stay empty and the last good values remain on screen, with the failure in
/// `error` for the status line.
struct ConditionsUpdate {
    current: Option<crate::conditions::Conditions>,
    history: Vec<crate::conditions::Conditions>,
    hourly: Vec<crate::conditions::HourlyForecast>,
    error: Option<String>,
}

pub async fn run(cfg: Config, home: Coords) -> Result<()> {
    // Falling back to the machine's own zone would be a silent lie whenever the
    // configured location is somewhere else, so a failed lookup shows UTC and
    // says so via the %Z abbreviation.
    let tz = match crate::geo::timezone_for(home).await {
        Ok(tz) => tz,
        Err(_) => chrono_tz::UTC,
    };

    let mut app = App::new(&cfg, home, tz)?;
    let site_id = app.site.id.to_string();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Update>(16);

    let conditions_tx = tx.clone();
    tokio::spawn(async move {
        let Ok(client) = reqwest::Client::builder()
            .user_agent(crate::alert::poll::USER_AGENT)
            .timeout(Duration::from_secs(15))
            .build()
        else {
            return;
        };
        // Discovery failure is retried each cycle, and a station whose
        // observations endpoint fails is rotated out for the next-nearest one
        // instead of being retried forever with an empty panel.
        let mut urls: Option<crate::conditions::PointsUrls> = None;
        let mut stations: Vec<String> = Vec::new();
        let mut which = 0usize;
        loop {
            let mut update = ConditionsUpdate {
                current: None,
                history: Vec::new(),
                hourly: Vec::new(),
                error: None,
            };
            let note = |e: anyhow::Error, update: &mut ConditionsUpdate| {
                if update.error.is_none() {
                    update.error = Some(format!("{e:#}"));
                }
            };
            if urls.is_none() {
                match crate::conditions::points_urls(&client, home).await {
                    Ok(u) => urls = Some(u),
                    Err(e) => note(e, &mut update),
                }
            }
            if let Some(u) = &urls {
                if stations.is_empty() {
                    match crate::conditions::nearest_stations(&client, &u.observation_stations)
                        .await
                    {
                        Ok(s) => stations = s,
                        Err(e) => note(e, &mut update),
                    }
                }
                if let Some(id) = stations.get(which % stations.len().max(1)) {
                    match crate::conditions::latest(&client, id).await {
                        Ok(c) => update.current = Some(c),
                        Err(e) => {
                            which += 1;
                            note(e, &mut update);
                        }
                    }
                    match crate::conditions::observation_history(&client, id).await {
                        Ok(h) => update.history = h,
                        Err(e) => note(e, &mut update),
                    }
                }
                match crate::conditions::hourly_forecast(&client, &u.forecast_hourly).await {
                    Ok(h) => update.hourly = h,
                    Err(e) => note(e, &mut update),
                }
            }
            if conditions_tx.send(Update::Conditions(Box::new(update))).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });

    let alert_tx = tx.clone();
    let mut engine = AlertEngine::new(&cfg, home)?;
    let notify_levels = cfg.alerts.notify.clone();
    let notify_scripts = cfg.alerts.scripts.clone();
    let stale_after = cfg.alerts.stale_after_secs;
    tokio::spawn(async move {
        loop {
            let tick = engine.tick().await;

            let mut dispatch_error = None;
            for n in &tick.fresh {
                let eta = engine.eta_minutes(n);
                if let Err(e) = notify::dispatch(n, &notify_levels, &notify_scripts, eta) {
                    dispatch_error = Some(format!("{e:#}"));
                }
            }
            if tick.went_stale {
                let _ = notify::send_stale_warning(engine.stale_elapsed());
            }

            let stale_secs = engine.stale_elapsed();
            let snapshot = AlertSnapshot {
                tick,
                active: engine.state.active().cloned().collect(),
                dispatch_error,
                stale: engine.state.is_stale(crate::daemon::now_epoch(), stale_after),
                stale_secs,
            };
            let delay = engine.next_delay();
            if alert_tx.send(Update::Alerts(Box::new(snapshot))).await.is_err() {
                return;
            }
            tokio::time::sleep(delay).await;
        }
    });

    let radar_tx = tx.clone();
    let refresh = Duration::from_secs(cfg.radar.refresh_secs.max(30));
    let radar_site = site_id.clone();
    let site_coords = app.site.coords;
    let history = cfg.radar.frames;
    tokio::spawn(async move {
        // Without this the ring holds a single volume and there is nothing to
        // animate until enough wall-clock time has passed to accumulate one.
        // Frames are sent as each download finishes so the loop fills visibly.
        match fetch::recent_archive_ids(&radar_site, history.saturating_sub(1)).await {
            Ok(ids) => {
                let total = ids.len();
                for (i, id) in ids.into_iter().enumerate() {
                    let loaded = fetch::archived_field(id).await.map(|(at, f)| {
                        RadarUpdate { observed_at: at, field: Box::new(f), backfill: None, cells: None }
                    });
                    let progress = format!("backfill {}/{total}", i + 1);
                    if radar_tx
                        .send(Update::Radar(loaded.map(|mut u| { u.backfill = Some(progress); u })))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = radar_tx.send(Update::Radar(Err(e))).await;
            }
        }

        loop {
            let result = fetch::latest_field(&radar_site)
                .await
                .map(|(at, f)| {
                    let cells = crate::radar::cells::scan(&f, site_coords, home);
                    RadarUpdate { observed_at: at, field: Box::new(f), backfill: None, cells: Some(cells) }
                });
            if radar_tx.send(Update::Radar(result)).await.is_err() {
                return;
            }
            tokio::time::sleep(refresh).await;
        }
    });

    let (horizon_tx, mut horizon_rx) = tokio::sync::watch::channel(ForecastHorizon::Near);
    app.horizon_tx = Some(horizon_tx);

    let forecast_tx = tx.clone();
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .user_agent(crate::alert::poll::USER_AGENT)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = forecast_tx
                    .send(Update::Forecast(Err(anyhow::anyhow!("{e}"))))
                    .await;
                return;
            }
        };

        loop {
            let horizon = *horizon_rx.borrow_and_update();
            match crate::radar::hrrr::latest_cycle(&client, chrono::Utc::now()).await {
                Ok(cycle) => {
                    // The horizon window is placed past the cycle's age, so
                    // this only guards against the clock moving between
                    // planning the batch and fetching it.
                    let now = chrono::Utc::now();
                    let steps: Vec<_> = horizon
                        .steps(now - cycle)
                        .into_iter()
                        .filter(|s| cycle + chrono::Duration::minutes(s.lead_minutes()) > now)
                        .collect();
                    let total = steps.len();
                    // The drop signal must ride on the first SUCCESS, not on
                    // step zero: if step zero errors, hanging the signal on it
                    // would leave the previous batch interleaved with this one.
                    let mut batch_started = false;
                    for (index, step) in steps.into_iter().enumerate() {
                        let update = crate::radar::hrrr::fetch_step(&client, cycle, step)
                            .await
                            .map(|f| {
                                Box::new(ForecastUpdate {
                                    valid_at: f.valid_at,
                                    lead_minutes: f.lead_minutes,
                                    field: Box::new(f),
                                    first_of_batch: !std::mem::replace(&mut batch_started, true),
                                    index,
                                    total,
                                })
                            });
                        if forecast_tx.send(Update::Forecast(update)).await.is_err() {
                            return;
                        }
                        // A horizon change mid-batch abandons the rest rather
                        // than finishing frames the user is no longer viewing.
                        if horizon_rx.has_changed().unwrap_or(false) {
                            break;
                        }
                    }
                }
                Err(e) => {
                    if forecast_tx.send(Update::Forecast(Err(e))).await.is_err() {
                        return;
                    }
                }
            }

            tokio::select! {
                _ = horizon_rx.changed() => {}
                _ = tokio::time::sleep(Duration::from_secs(900)) => {}
            }
        }
    });

    let mut term = setup()?;
    let outcome = event_loop(&mut app, &mut rx, &mut term).await;
    restore(&mut term)?;
    outcome
}

async fn event_loop(
    app: &mut App,
    rx: &mut tokio::sync::mpsc::Receiver<Update>,

    term: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<()> {
    let mut last_advance = Instant::now();

    loop {
        term.draw(|f| app.draw(f))?;

        if event::poll(Duration::from_millis(40))?
            && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press {
                    let action = resolve_key(&mut app.pending, key);
                    app.apply(action);
                }

        while let Ok(update) = rx.try_recv() {
            match update {
                Update::Alerts(snapshot) => {
                    app.active = snapshot.active;
                    app.stale = snapshot.stale;
                    app.stale_secs = snapshot.stale_secs;
                    if let Some(err) = snapshot.dispatch_error {
                        app.status = format!("alert dispatch failed: {err}");
                        continue;
                    }
                    app.status = match snapshot.tick.poll_error {
                        Some(err) => format!("alert poll failed: {err}"),
                        None => format!(
                            "{} active | radar {} | {} frames",
                            app.active.len(),
                            app.site.id,
                            app.ring.len()
                        ),
                    };
                }
                Update::Radar(Ok(RadarUpdate { observed_at, field, backfill, cells })) => {
                    if let Some(cells) = cells {
                        app.selected_cell =
                            app.selected_cell.filter(|i| *i < cells.len());
                        app.cells = cells;
                    }
                    let observed: Arc<dyn RadarField> = Arc::from(field);
                    let tilt = observed.elevation_degrees();
                    app.ring.push(RadarFrame {
                        captured_at: observed_at,
                        field: observed.clone(),
                        projected: false,
                    });

                    app.status = match backfill {
                        Some(progress) => format!(
                            "{} {progress} | {} frames | {:.1}deg tilt",
                            app.site.id,
                            app.ring.len(),
                            tilt
                        ),
                        None => format!(
                            "{} | {} frames | {:.1}deg tilt",
                            app.site.id,
                            app.ring.len(),
                            tilt
                        ),
                    };
                }
                Update::Forecast(Ok(update)) => {
                    if update.first_of_batch {
                        app.ring.drop_projected();
                    }
                    app.ring.push(RadarFrame {
                        captured_at: update.valid_at,
                        field: Arc::from(update.field),
                        projected: true,
                    });
                    app.status = format!(
                        "HRRR {} | +{}min | {}/{}",
                        app.horizon.label(),
                        update.lead_minutes,
                        update.index + 1,
                        update.total
                    );
                }
                Update::Forecast(Err(e)) => {
                    app.status = format!("forecast unavailable: {e:#}");
                }
                Update::Conditions(u) => {
                    if u.current.is_some() {
                        app.conditions = u.current;
                    }
                    if !u.history.is_empty() {
                        app.obs_history = u.history;
                    }
                    if !u.hourly.is_empty() {
                        app.hourly = u.hourly;
                    }
                    if let Some(e) = u.error {
                        app.status = format!("conditions unavailable: {e}");
                    }
                }
                Update::Radar(Err(e)) => {
                    app.status = format!("radar fetch failed: {e:#}");
                }
            }
        }

        if last_advance.elapsed() >= app.playback {
            app.ring.advance_playback();
            last_advance = Instant::now();
        }

        if app.quit {
            return Ok(());
        }
    }
}

fn setup() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    terminal::enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, terminal::EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn restore(term: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    terminal::disable_raw_mode()?;
    execute!(term.backend_mut(), terminal::LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn press(seq: &[KeyEvent]) -> Action {
        let mut pending = None;
        let mut last = Action::Ignored;
        for k in seq {
            last = resolve_key(&mut pending, *k);
        }
        last
    }

    #[test]
    fn a13_hjkl_pan_the_map() {
        assert_eq!(press(&[key('h')]), Action::PanWest);
        assert_eq!(press(&[key('j')]), Action::PanSouth);
        assert_eq!(press(&[key('k')]), Action::PanNorth);
        assert_eq!(press(&[key('l')]), Action::PanEast);
    }

    #[test]
    fn a13_brackets_step_frames_and_do_not_pan() {
        assert_eq!(press(&[key('[')]), Action::FrameBack);
        assert_eq!(press(&[key(']')]), Action::FrameForward);
    }

    #[test]
    fn a13_gg_jumps_oldest_and_capital_g_jumps_newest() {
        assert_eq!(press(&[key('g'), key('g')]), Action::JumpOldest);
        assert_eq!(press(&[key('G')]), Action::JumpNewest);
    }

    #[test]
    fn a13_space_toggles_playback() {
        assert_eq!(press(&[KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)]), Action::TogglePlay);
    }

    #[test]
    fn gh_recentres_on_home_without_colliding_with_the_pan_binding() {
        assert_eq!(press(&[key('g'), key('h')]), Action::RecentreHome);
    }

    #[test]
    fn zi_and_zo_zoom() {
        assert_eq!(press(&[key('z'), key('i')]), Action::ZoomIn);
        assert_eq!(press(&[key('z'), key('o')]), Action::ZoomOut);
    }

    #[test]
    fn a_lone_prefix_key_produces_no_action_and_stays_pending() {
        let mut pending = None;
        assert_eq!(resolve_key(&mut pending, key('g')), Action::Ignored);
        assert_eq!(pending, Some('g'));
    }

    #[test]
    fn an_unfinished_sequence_is_abandoned_rather_than_firing_the_single_key_action() {
        let mut pending = None;
        resolve_key(&mut pending, key('g'));
        assert_eq!(resolve_key(&mut pending, key('l')), Action::Ignored);
        assert_eq!(pending, None, "pending must clear so the next key works");
        assert_eq!(resolve_key(&mut pending, key('l')), Action::PanEast);
    }

    #[test]
    fn quit_bindings_all_work() {
        assert_eq!(press(&[key('q')]), Action::Quit);
        assert_eq!(press(&[key('Z'), key('Z')]), Action::Quit);
        assert_eq!(press(&[ctrl('c')]), Action::Quit);
    }

    #[test]
    fn control_pans_half_a_screen() {
        assert_eq!(press(&[ctrl('d')]), Action::PanHalfDown);
        assert_eq!(press(&[ctrl('u')]), Action::PanHalfUp);
    }

    #[test]
    fn speed_and_help_bindings_resolve() {
        assert_eq!(press(&[key('>')]), Action::Faster);
        assert_eq!(press(&[key('<')]), Action::Slower);
        assert_eq!(press(&[key('?')]), Action::ToggleHelp);
    }

    #[test]
    fn unbound_keys_are_ignored_rather_than_mapped_to_something_surprising() {
        assert_eq!(press(&[key('x')]), Action::Ignored);
        assert_eq!(press(&[key('~')]), Action::Ignored);
    }

    #[test]
    fn digits_one_to_three_select_the_base_product() {
        assert_eq!(press(&[key('1')]), Action::SelectProduct(0));
        assert_eq!(press(&[key('2')]), Action::SelectProduct(1));
        assert_eq!(press(&[key('3')]), Action::SelectProduct(2));
    }

    #[test]
    fn digits_four_to_seven_toggle_augmentations() {
        assert_eq!(press(&[key('4')]), Action::ToggleProductOverlay(0));
        assert_eq!(press(&[key('5')]), Action::ToggleProductOverlay(1));
        assert_eq!(press(&[key('7')]), Action::ToggleProductOverlay(3));
    }

    #[test]
    fn digits_and_shifted_digits_beyond_the_layer_set_are_unbound() {
        assert_eq!(press(&[key('8')]), Action::Ignored);
        assert_eq!(press(&[key('9')]), Action::Ignored);
        assert_eq!(press(&[key('!')]), Action::Ignored);
        assert_eq!(press(&[key('(')]), Action::Ignored);
    }



    #[test]
    fn selecting_a_product_beyond_the_list_is_a_no_op() {
        let cfg = crate::config::Config::parse("[location]\nzip = \"73019\"\n", "/tmp/c.toml").unwrap();
        let mut app =
            App::new(&cfg, Coords { lat: 35.2, lon: -97.4 }, chrono_tz::UTC).unwrap();
        app.apply(Action::SelectProduct(0));
        assert_eq!(app.product, RadarProduct::Reflectivity);
        app.apply(Action::SelectProduct(99));
        assert_eq!(app.product, RadarProduct::Reflectivity, "out of range must not change it");
    }

    /// Reports one fixed value for every product everywhere, so an overlay
    /// test can choose exactly whether the reading is diagnostic or ordinary.
    struct StormField {
        refl: f32,
        moment: f32,
    }

    impl RadarField for StormField {
        fn value_at(&self, _at: Coords, product: RadarProduct) -> Option<f32> {
            Some(match product {
                RadarProduct::Reflectivity => self.refl,
                _ => self.moment,
            })
        }
        fn supports(&self, _product: RadarProduct) -> bool {
            true
        }
        fn source_label(&self) -> &str {
            "TEST"
        }
        fn elevation_degrees(&self) -> f32 {
            0.5
        }
    }

    fn app_showing(refl: f32, moment: f32) -> App {
        let cfg =
            crate::config::Config::parse("[location]\nzip = \"73019\"\n", "/tmp/c.toml").unwrap();
        let mut app =
            App::new(&cfg, Coords { lat: 35.2, lon: -97.4 }, chrono_tz::UTC).unwrap();
        app.viewport.width = 20;
        app.viewport.height = 20;
        app.ring.push(RadarFrame {
            captured_at: chrono::Utc::now(),
            field: std::sync::Arc::new(StormField { refl, moment }),
            projected: false,
        });
        app
    }

    fn without_augs(mut app: App) -> App {
        app.product_overlays.clear();
        app
    }

    #[test]
    fn the_banner_compass_names_the_direction_of_the_nearest_echo() {
        use crate::geo::compass_bearing;
        let home = Coords { lat: 36.0, lon: -87.0 };
        assert_eq!(compass_bearing(home, Coords { lat: 38.0, lon: -87.0 }), "N");
        assert_eq!(compass_bearing(home, Coords { lat: 34.0, lon: -87.0 }), "S");
        assert_eq!(compass_bearing(home, Coords { lat: 36.0, lon: -85.0 }), "E");
        assert_eq!(compass_bearing(home, Coords { lat: 36.0, lon: -89.0 }), "W");
        assert_eq!(compass_bearing(home, Coords { lat: 34.5, lon: -88.9 }), "SW");
    }

    /// A blank frame must report where the weather actually is, so an empty
    /// view reads as "clear here" rather than "the app died".
    #[test]
    fn nearest_echo_is_found_outside_the_viewport() {
        let home = Coords { lat: 36.0, lon: -87.0 };
        let field = crate::radar::testing::DiskField {
            centre: Coords { lat: 33.5, lon: -89.6 },
            radius_km: 40.0,
            dbz: 30.0,
        };
        let (km, dir) =
            nearest_echo_km(&field, home, RadarProduct::Reflectivity, Colormap::Threat)
                .expect("echo exists within the search radius");
        assert!((250.0..400.0).contains(&km), "unexpected distance {km}");
        assert_eq!(dir, "SW");
    }

    #[test]
    fn a_field_with_no_echo_anywhere_reports_none() {
        let field = crate::radar::testing::DiskField {
            centre: Coords { lat: 0.0, lon: 0.0 },
            radius_km: 1.0,
            dbz: 30.0,
        };
        assert!(
            nearest_echo_km(
                &field,
                Coords { lat: 36.0, lon: -87.0 },
                RadarProduct::Reflectivity,
                Colormap::Threat
            )
            .is_none()
        );
    }

    fn app_with_cells(n: usize) -> App {
        let mut app = app_showing(45.0, 0.98);
        app.cells = (0..n)
            .map(|i| crate::radar::cells::StormCell {
                centroid: Coords { lat: 36.0 + i as f64 * 0.3, lon: -87.0 },
                max_dbz: 50.0,
                rotation_ms: None,
                min_cc: None,
                max_vil: None,
                max_echo_top_km: None,
                distance_km: 30.0,
                bearing: "N",
                threat: crate::radar::cells::CellThreat::Strong,
            })
            .collect();
        app
    }

    /// Cell markers describe the newest observed volume only; drawn on a
    /// forecast frame they would bracket echo they do not belong to.
    #[test]
    fn cell_markers_stay_off_forecast_frames() {
        let mut app = app_with_cells(1);
        app.cells[0].centroid = app.home;
        let on_observed = app.build_overlay(20, 20).painted();

        app.ring.push(RadarFrame {
            captured_at: chrono::Utc::now() + chrono::Duration::minutes(30),
            field: std::sync::Arc::new(StormField { refl: 45.0, moment: 0.98 }),
            projected: true,
        });
        app.ring.jump_newest();
        let on_forecast = app.build_overlay(20, 20).painted();
        assert!(
            on_forecast < on_observed,
            "markers must vanish on the forecast frame: {on_forecast} vs {on_observed}"
        );
    }

    #[test]
    fn frame_conditions_pick_forecast_for_future_and_observation_for_past() {
        let mut app = app_showing(45.0, 0.98);
        let now_frame = app.ring.current().unwrap().captured_at;

        app.hourly = vec![crate::conditions::HourlyForecast {
            valid: now_frame + chrono::Duration::minutes(60),
            temp_f: Some(78.0),
            dewpoint_f: None,
            humidity_pct: None,
            wind_mph: None,
            wind_dir: None,
            short: Some("Sunny".into()),
        }];
        let mut past_obs = crate::conditions::Conditions {
            station: "KM02".into(),
            observed_at: Some(now_frame - chrono::Duration::minutes(50)),
            description: None,
            temp_f: Some(66.0),
            dewpoint_f: None,
            humidity_pct: None,
            wind_mph: None,
            wind_dir: None,
            visibility_mi: None,
            rain_last_hour_in: None,
        };
        app.obs_history = vec![past_obs.clone()];

        assert!(
            app.frame_conditions().is_none(),
            "the newest observed frame IS now; the current block covers it"
        );

        app.ring.push(RadarFrame {
            captured_at: now_frame + chrono::Duration::minutes(45),
            field: std::sync::Arc::new(StormField { refl: 45.0, moment: 0.98 }),
            projected: true,
        });
        app.ring.jump_newest();
        match app.frame_conditions() {
            Some((_, crate::conditions::FrameConditions::Forecast(h))) => {
                assert_eq!(h.temp_f, Some(78.0));
            }
            other => panic!("expected the hourly forecast, got {:?}", other.is_some()),
        }

        past_obs.observed_at = Some(now_frame - chrono::Duration::minutes(50));
        app.ring.push(RadarFrame {
            captured_at: now_frame - chrono::Duration::minutes(45),
            field: std::sync::Arc::new(StormField { refl: 45.0, moment: 0.98 }),
            projected: false,
        });
        app.ring.jump_oldest();
        match app.frame_conditions() {
            Some((_, crate::conditions::FrameConditions::Observed(c))) => {
                assert_eq!(c.temp_f, Some(66.0));
            }
            other => panic!("expected the past observation, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn cell_keys_resolve() {
        assert_eq!(press(&[key('n')]), Action::CellNext);
        assert_eq!(press(&[key('N')]), Action::CellPrev);
    }

    #[test]
    fn cycling_wraps_and_pans_the_map_to_the_cell() {
        let mut app = app_with_cells(2);
        app.apply(Action::CellNext);
        assert_eq!(app.selected_cell, Some(0));
        assert!(
            (app.viewport.centre.lat - app.cells[0].centroid.lat).abs() < 1e-9,
            "selecting a cell must pan to it"
        );
        app.apply(Action::CellNext);
        assert_eq!(app.selected_cell, Some(1));
        app.apply(Action::CellNext);
        assert_eq!(app.selected_cell, Some(0), "cycling wraps");
        app.apply(Action::CellPrev);
        assert_eq!(app.selected_cell, Some(1), "prev wraps backwards");
    }

    #[test]
    fn cycling_with_no_cells_explains_itself_instead_of_panicking() {
        let mut app = app_with_cells(0);
        app.apply(Action::CellNext);
        assert_eq!(app.selected_cell, None);
        assert_eq!(app.status, "no storm cells detected");
        app.apply(Action::CellPrev);
        assert_eq!(app.selected_cell, None);
    }

    /// The whole point of the summary: an empty sky and a failed fetch look
    /// identical on screen, and only this line separates them.
    #[test]
    fn echo_summary_separates_an_empty_sky_from_a_missing_frame() {
        let empty = DbzGrid::new(4, 4);
        assert_eq!(
            echo_summary(&empty, RadarProduct::Reflectivity, Colormap::Threat),
            "no data in view"
        );

        let mut below_threshold = DbzGrid::new(4, 4);
        below_threshold.set(1, 1, Some(-10.0));
        assert_eq!(
            echo_summary(&below_threshold, RadarProduct::Reflectivity, Colormap::Threat),
            "no echo in view",
            "HRRR fills its grid with a no-echo floor; that is data, not absence"
        );

        let mut storm = DbzGrid::new(4, 4);
        storm.set(0, 0, Some(45.0));
        storm.set(2, 2, Some(20.0));
        assert_eq!(
            echo_summary(&storm, RadarProduct::Reflectivity, Colormap::Threat),
            "2 cells"
        );
    }

    /// The layer set must survive a background poll, which overwrites `status`.
    #[test]
    fn the_layer_summary_lists_the_base_then_each_augmentation() {
        let mut app = app_showing(30.0, 0.98);
        assert_eq!(
            app.layer_summary(),
            "reflectivity dBZ + velocity + corr coeff",
            "velocity and debris detection must be on by default"
        );

        app.apply(Action::SelectProduct(1));
        assert_eq!(app.layer_summary(), "echo top km + velocity + corr coeff");

        app.apply(Action::ToggleProductOverlay(0));
        app.apply(Action::ToggleProductOverlay(1));
        assert_eq!(app.layer_summary(), "echo top km");
    }

    #[test]
    fn augmentations_default_to_velocity_and_debris() {
        let app = app_showing(30.0, 0.98);
        assert_eq!(
            app.product_overlays,
            vec![RadarProduct::Velocity, RadarProduct::CorrelationCoefficient]
        );
    }

    #[test]
    fn toggling_an_augmentation_adds_then_removes() {
        let mut app = app_showing(30.0, 0.98);
        app.apply(Action::ToggleProductOverlay(1));
        assert_eq!(app.product_overlays, vec![RadarProduct::Velocity]);
        app.apply(Action::ToggleProductOverlay(2));
        assert_eq!(
            app.product_overlays,
            vec![RadarProduct::Velocity, RadarProduct::DifferentialReflectivity]
        );
    }

    #[test]
    fn an_augmentation_paints_its_diagnostic_tail_inside_echo() {
        let debris = app_showing(45.0, 0.45);
        let painted = debris.build_overlay(20, 20);
        let expected = product_rgb(0.45, RadarProduct::CorrelationCoefficient, debris.colormap);
        assert_eq!(painted.get(3, 3), expected, "a debris signature inside a core must be drawn");
    }

    #[test]
    fn ordinary_readings_do_not_cover_the_base_even_inside_echo() {
        let rain = app_showing(45.0, 0.98);
        let bare = without_augs(app_showing(45.0, 0.98)).build_overlay(20, 20).painted();
        assert_eq!(
            rain.build_overlay(20, 20).painted(),
            bare,
            "healthy rain has nothing diagnostic to highlight"
        );
    }

    /// The regression that would ruin every clear summer night: biologicals
    /// collapse correlation and drift at jet speed, so without the echo gate
    /// the default-on augmentations would flood an empty sky with false
    /// rotation and debris pixels.
    #[test]
    fn augmentations_stay_dark_outside_real_echo() {
        let bugs = app_showing(10.0, 0.20);
        let bare = without_augs(app_showing(10.0, 0.20)).build_overlay(20, 20).painted();
        assert_eq!(
            bugs.build_overlay(20, 20).painted(),
            bare,
            "a correlation collapse in 10 dBZ clear-air return is bugs, not debris"
        );
    }

    #[test]
    fn a_frame_that_cannot_produce_the_augmentation_keeps_its_base() {
        let mut app = without_augs(app_showing(45.0, 0.45));
        app.product_overlays = vec![RadarProduct::Velocity];
        app.ring.push(RadarFrame {
            captured_at: chrono::Utc::now() + chrono::Duration::minutes(30),
            field: std::sync::Arc::new(crate::radar::testing::DiskField {
                centre: Coords { lat: 35.2, lon: -97.4 },
                radius_km: 500.0,
                dbz: 40.0,
            }),
            projected: true,
        });
        app.ring.jump_newest();
        let bare = {
            let mut b = without_augs(app_showing(45.0, 0.45));
            b.ring.push(RadarFrame {
                captured_at: chrono::Utc::now() + chrono::Duration::minutes(30),
                field: std::sync::Arc::new(crate::radar::testing::DiskField {
                    centre: Coords { lat: 35.2, lon: -97.4 },
                    radius_km: 500.0,
                    dbz: 40.0,
                }),
                projected: true,
            });
            b.ring.jump_newest();
            b.build_overlay(20, 20).painted()
        };
        assert_eq!(
            app.build_overlay(20, 20).painted(),
            bare,
            "a forecast frame skips unsupported augmentations instead of blanking"
        );
    }


    #[test]
    fn warning_geometry_survives_an_augmentation_painted_beneath_it() {
        let app = app_showing(45.0, 0.20);
        let overlay = app.build_overlay(20, 20);
        let (cx, cy) = app.viewport.project_to_nearest_pixel(app.home);
        assert_eq!(
            overlay.get(cx as usize + 1, cy as usize),
            Some(HOME_MARKER),
            "home must stay visible through a full-coverage overlay"
        );
    }


    /// Realistic HRRR publication lags. The 0 case is the ideal that never
    /// happens in production; the rest are what the app actually sees.
    const CYCLE_AGES: [i64; 5] = [0, 35, 65, 95, 125];

    #[test]
    fn the_default_horizon_is_two_hours_at_quarter_hour_steps() {
        let steps = ForecastHorizon::Near.steps(chrono::Duration::zero());
        assert_eq!(steps.len(), 8);
        assert_eq!(steps.first().unwrap().lead_minutes(), 15);
        assert_eq!(steps.last().unwrap().lead_minutes(), 120);
    }

    /// The reported bug: forecast frames were nearly absent because the window
    /// was anchored to the cycle, so an aged cycle put it entirely in the past.
    #[test]
    fn an_aged_cycle_still_delivers_a_full_two_hour_horizon() {
        for age in CYCLE_AGES {
            let age = chrono::Duration::minutes(age);
            let steps = ForecastHorizon::Near.steps(age);
            assert_eq!(
                steps.len(),
                8,
                "a cycle {} minutes old collapsed the horizon to {} frames",
                age.num_minutes(),
                steps.len()
            );
            assert!(
                steps.iter().all(|s| s.lead_minutes() > age.num_minutes()),
                "every step must still be ahead of now for a {}-minute-old cycle",
                age.num_minutes()
            );
        }
    }

    /// Mid splices quarter-hourly and hourly lists, which is where a duplicate
    /// or out-of-order lead would hide.
    #[test]
    fn every_horizon_has_strictly_increasing_distinct_leads() {
        for age in CYCLE_AGES {
            for horizon in [ForecastHorizon::Near, ForecastHorizon::Mid, ForecastHorizon::Long] {
                let leads: Vec<i64> = horizon
                    .steps(chrono::Duration::minutes(age))
                    .iter()
                    .map(|s| s.lead_minutes())
                    .collect();
                assert!(!leads.is_empty(), "{horizon:?} produced no steps at age {age}");
                assert!(
                    leads.windows(2).all(|w| w[0] < w[1]),
                    "{horizon:?} leads not increasing at age {age}: {leads:?}"
                );
            }
        }
    }

    /// The ring is sized for observations plus forecasts, so no horizon may
    /// exceed the reserved headroom or it would evict observed volumes.
    #[test]
    fn no_horizon_exceeds_the_reserved_ring_headroom() {
        for age in CYCLE_AGES {
            for horizon in [ForecastHorizon::Near, ForecastHorizon::Mid, ForecastHorizon::Long] {
                let n = horizon.steps(chrono::Duration::minutes(age)).len();
                assert!(
                    n <= MAX_FORECAST_FRAMES,
                    "{horizon:?} needs {n} frames at age {age} but {MAX_FORECAST_FRAMES} are reserved"
                );
            }
        }
    }

    /// A clock skewed backwards must not generate leads behind the cycle.
    #[test]
    fn a_negative_cycle_age_is_treated_as_a_fresh_cycle() {
        let steps = ForecastHorizon::Near.steps(chrono::Duration::minutes(-30));
        assert_eq!(steps.first().unwrap().lead_minutes(), 15);
    }

    #[test]
    fn horizons_cycle_back_to_the_start() {
        let mut h = ForecastHorizon::Near;
        for _ in 0..3 {
            h = h.next();
        }
        assert_eq!(h, ForecastHorizon::Near);
    }

    #[test]
    fn horizon_labels_are_distinct() {
        let labels: Vec<&str> = [ForecastHorizon::Near, ForecastHorizon::Mid, ForecastHorizon::Long]
            .iter()
            .map(|h| h.label())
            .collect();
        assert_eq!(labels.len(), 3);
        assert_ne!(labels[0], labels[1]);
        assert_ne!(labels[1], labels[2]);
    }

    #[test]
    fn f_cycles_the_forecast_horizon() {
        assert_eq!(press(&[key('f')]), Action::CycleForecast);
    }

    #[test]
    fn centred_rect_fits_inside_a_small_terminal() {
        let area = Rect::new(0, 0, 20, 6);
        let c = centred(58, 18, area);
        assert!(c.width <= area.width && c.height <= area.height);
    }
}
