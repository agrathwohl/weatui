//! Interactive front end: layout, vim keymap, and the redraw loop.

use crate::alert::filter::ThreatTier;
use crate::config::{Colormap, Config};
use crate::daemon::AlertEngine;
use crate::geo::{Coords, RadarSite, nearest_radar_site, radar_site_by_id};
use crate::notify;
use crate::radar::grid::{Viewport, rasterize};
use crate::radar::ring::{FrameRing, RadarFrame};
use crate::radar::{AdvectedField, ReflectivityField, fetch};
use crate::render::hud::Hud;
use crate::render::overlay::{
    HOME_MARKER, LETHAL_OUTLINE, PixelOverlay, RANGE_RING, SEVERE_OUTLINE, WATCH_OUTLINE,
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
const PROJECTION_STEPS_MIN: &[f64] = &[10.0, 20.0, 30.0];

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

   ?          toggle this help
   q  ZZ      quit
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
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
    tz: chrono_tz::Tz,
    pending: Option<char>,
    playback: Duration,
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
            ring: FrameRing::new(cfg.radar.frames + PROJECTION_STEPS_MIN.len()),
            viewport: Viewport::new(home, DEFAULT_SPAN_KM, 1, 1),
            home,
            site,
            colormap: cfg.render.colormap,
            tz,
            pending: None,
            playback: Duration::from_millis(450),
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
            Action::ToggleHelp => self.show_help = !self.show_help,
            Action::Quit => self.quit = true,
            Action::Ignored => {}
        }
    }

    fn build_overlay(&self, width: usize, height: usize) -> PixelOverlay {
        let mut overlay = PixelOverlay::new(width, height);
        if let Some(frame) = self.ring.current() {
            overlay.draw_range_ring(
                frame.field.site_coords(),
                frame.field.max_range_km(),
                &self.viewport,
                RANGE_RING,
            );
        }
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
            Some(f) => rasterize(f.field.as_ref(), &self.viewport),
            None => crate::radar::DbzGrid::new(self.viewport.width, self.viewport.height),
        };

        frame.render_widget(
            RadarRaster { grid: &grid, overlay: Some(&overlay), colormap: self.colormap },
            map,
        );
        frame.render_widget(Timeline { ring: &self.ring, tz: self.tz }, rows[1]);
        frame.render_widget(
            Paragraph::new(self.status.clone())
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

/// Vector mean of the motions NWS published for the currently active warnings.
/// Averaging headings numerically would be wrong across the 0/360 seam, so the
/// components are averaged and the heading recovered with atan2.
fn mean_storm_motion(active: &[crate::alert::state::ActiveAlert]) -> Option<(f64, f64)> {
    let mut north = 0.0;
    let mut east = 0.0;
    let mut count = 0.0;
    for entry in active {
        if let Some(m) = entry.alert.motion() {
            let theta = m.heading_deg().to_radians();
            north += m.speed_kt * theta.cos();
            east += m.speed_kt * theta.sin();
            count += 1.0;
        }
    }
    if count == 0.0 {
        return None;
    }
    north /= count;
    east /= count;
    let speed = (north * north + east * east).sqrt();
    if speed < 1.0 {
        return None;
    }
    Some(((east.atan2(north).to_degrees() + 360.0) % 360.0, speed))
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
}

struct RadarUpdate {
    observed_at: chrono::DateTime<chrono::Utc>,
    field: Box<dyn ReflectivityField>,
    /// `Some` while history is still loading, carrying "n/total" for the
    /// status line. Projections are suppressed until it is `None`.
    backfill: Option<String>,
}

enum Update {
    Alerts(Box<AlertSnapshot>),
    Radar(Result<RadarUpdate>),
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

    let alert_tx = tx.clone();
    let mut engine = AlertEngine::new(&cfg, home)?;
    let notify_levels = cfg.alerts.notify.clone();
    let stale_after = cfg.alerts.stale_after_secs;
    tokio::spawn(async move {
        loop {
            let tick = engine.tick().await;

            for n in &tick.fresh {
                let eta = engine.eta_minutes(n);
                let _ = notify::send(n, &notify_levels, eta);
            }
            if tick.went_stale {
                let _ = notify::send_stale_warning(engine.stale_elapsed());
            }

            let stale_secs = engine.stale_elapsed();
            let snapshot = AlertSnapshot {
                tick,
                active: engine.state.active().cloned().collect(),
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
                        RadarUpdate { observed_at: at, field: Box::new(f), backfill: None }
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
                .map(|(at, f)| RadarUpdate { observed_at: at, field: Box::new(f), backfill: None });
            if radar_tx.send(Update::Radar(result)).await.is_err() {
                return;
            }
            tokio::time::sleep(refresh).await;
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
                Update::Radar(Ok(RadarUpdate { observed_at, field, backfill })) => {
                    app.ring.drop_projected();
                    let observed: Arc<dyn ReflectivityField> = Arc::from(field);
                    let tilt = observed.elevation_degrees();
                    app.ring.push(RadarFrame {
                        captured_at: observed_at,
                        field: observed.clone(),
                        projected: false,
                    });

                    // Projections are only meaningful off the newest volume, so
                    // they are skipped while history is still loading.
                    let projection = match (&backfill, mean_storm_motion(&app.active)) {
                        (None, Some((heading, speed))) => {
                            for minutes in PROJECTION_STEPS_MIN {
                                app.ring.push(RadarFrame {
                                    captured_at: observed_at
                                        + chrono::Duration::minutes(*minutes as i64),
                                    field: Arc::new(AdvectedField::new(
                                        observed.clone(),
                                        heading,
                                        speed,
                                        *minutes,
                                    )),
                                    projected: true,
                                });
                            }
                            format!(
                                " | +{}min projected @ {heading:.0}deg {speed:.0}kt",
                                PROJECTION_STEPS_MIN.last().copied().unwrap_or(0.0)
                            )
                        }
                        _ => String::new(),
                    };

                    app.status = match backfill {
                        Some(progress) => format!(
                            "{} {progress} | {} frames | {:.1}deg tilt",
                            app.site.id,
                            app.ring.len(),
                            tilt
                        ),
                        None => format!(
                            "{} | {} frames | {:.1}deg tilt{projection}",
                            app.site.id,
                            app.ring.len(),
                            tilt
                        ),
                    };
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
        assert_eq!(press(&[key('5')]), Action::Ignored);
    }

    #[test]
    fn centred_rect_fits_inside_a_small_terminal() {
        let area = Rect::new(0, 0, 20, 6);
        let c = centred(58, 18, area);
        assert!(c.width <= area.width && c.height <= area.height);
    }
}
