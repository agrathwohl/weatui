//! Headless alert pipeline shared by `weatui -d` and the interactive front end.

use crate::alert::filter::Filter;
use crate::alert::poll::{PollOutcome, Poller};
use crate::alert::state::{AlertState, Notification};
use crate::config::Config;
use crate::geo::Coords;
use crate::notify;
use anyhow::{Context, Result};
use std::time::{SystemTime, UNIX_EPOCH};

/// A clock before the epoch is absurd, but falling back to 0 made `is_stale`
/// compute a zero elapsed time and silently disable staleness detection
/// entirely. Saturating the other way fails loud instead.
pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(u64::MAX)
}

pub struct AlertEngine {
    pub poller: Poller,
    pub state: AlertState,
    pub filter: Filter,
    pub home: Coords,
    stale_after_secs: u64,
    last_stale_warning: Option<u64>,
}

pub struct Tick {
    pub fresh: Vec<Notification>,
    pub went_stale: bool,
    pub poll_error: Option<String>,
    /// Set on the first successful poll after a gap longer than the staleness
    /// threshold. Without it a laptop resume is invisible: `mark_poll_success`
    /// runs before the staleness test, so one good poll erases the whole blind
    /// window and the user is never told they were unmonitored. A warning
    /// issued and expired inside that window is unrecoverable, because the
    /// point query only ever returns currently-active alerts.
    pub recovered_after_gap_secs: Option<u64>,
}

/// Daemon mode never constructs `App`, so a misspelled site id used to go
/// unreported here while the TUI rejected it at startup.
pub fn resolve_site(cfg: &Config, home: Coords) -> Result<String> {
    let site = if cfg.radar.site.eq_ignore_ascii_case("auto") {
        crate::geo::nearest_radar_site(home)
            .context("no WSR-88D site could be selected for this location")?
    } else {
        crate::geo::radar_site_by_id(&cfg.radar.site)
            .with_context(|| format!("unknown radar site {:?}", cfg.radar.site))?
    };
    Ok(site.id.to_string())
}

/// The gap has to be measured before `mark_poll_success` overwrites it, and
/// reported even though this tick succeeded. Testing the decision separately
/// keeps it off the network path.
pub fn gap_to_report(
    gap_before_poll: Option<u64>,
    succeeded: bool,
    stale_after_secs: u64,
) -> Option<u64> {
    if !succeeded {
        return None;
    }
    gap_before_poll.filter(|gap| *gap >= stale_after_secs)
}

impl AlertEngine {
    pub fn new(cfg: &Config, home: Coords) -> Result<Self> {
        Ok(AlertEngine {
            poller: Poller::new(home, cfg.alerts.poll_interval_secs)?,
            state: AlertState::new(),
            filter: Filter::from_config(&cfg.alerts),
            home,
            stale_after_secs: cfg.alerts.stale_after_secs,
            last_stale_warning: None,
        })
    }

    pub fn eta_minutes(&self, notification: &Notification) -> Option<i64> {
        self.state
            .active()
            .find(|a| crate::alert::state::key_of(&a.alert) == notification.key)
            .and_then(|a| a.alert.motion())
            .and_then(|m| m.eta_to(self.home))
            .map(|d| d.num_minutes())
    }

    /// One poll cycle. Errors are reported rather than propagated so a transient
    /// network failure cannot terminate the monitoring loop.
    pub async fn tick(&mut self) -> Tick {
        let now = now_epoch();
        let mut out = Tick {
            fresh: Vec::new(),
            went_stale: false,
            poll_error: None,
            recovered_after_gap_secs: None,
        };

        let gap_before_poll = self
            .state
            .last_success_epoch()
            .map(|prev| now.saturating_sub(prev));

        let succeeded = match self.poller.poll().await {
            Ok(PollOutcome::Updated(alerts)) => {
                out.fresh = self.state.ingest(alerts, &self.filter);
                self.state.mark_poll_success(now);
                true
            }
            Ok(PollOutcome::Unchanged) => {
                self.state.mark_poll_success(now);
                true
            }
            Err(e) => {
                out.poll_error = Some(format!("{e:#}"));
                false
            }
        };

        self.state.prune_expired(chrono::Utc::now());

        if let Some(gap) = gap_to_report(gap_before_poll, succeeded, self.stale_after_secs) {
            out.recovered_after_gap_secs = Some(gap);
            self.last_stale_warning = None;
        }

        if self.state.is_stale(now, self.stale_after_secs) {
            let repeat_due = self
                .last_stale_warning
                .is_none_or(|t| now.saturating_sub(t) >= self.stale_after_secs);
            if repeat_due {
                self.last_stale_warning = Some(now);
                out.went_stale = true;
            }
        } else {
            self.last_stale_warning = None;
        }

        out
    }

    pub fn stale_elapsed(&self) -> u64 {
        match self.state.last_success_epoch() {
            Some(t) => now_epoch().saturating_sub(t),
            None => 0,
        }
    }

    pub fn next_delay(&self) -> std::time::Duration {
        self.poller.next_delay()
    }
}

pub async fn run(cfg: Config, home: Coords, echo_to_stdout: bool) -> Result<()> {
    let site = resolve_site(&cfg, home)?;
    let mut engine = AlertEngine::new(&cfg, home)?;
    if echo_to_stdout {
        println!(
            "weatui monitoring {:.4},{:.4} (radar {}) via {}",
            home.lat,
            home.lon,
            site,
            engine.poller.url()
        );
    }

    loop {
        let tick = engine.tick().await;

        for n in &tick.fresh {
            let eta = engine.eta_minutes(n);
            if echo_to_stdout {
                println!("{} :: {}", notify::summary_for(n), notify::body_for(n, eta));
            }
            if let Err(e) = notify::dispatch(n, &cfg.alerts.notify, &cfg.alerts.scripts, eta) {
                eprintln!("weatui: notification failed: {e:#}");
            }
        }

        if let Some(gap) = tick.recovered_after_gap_secs {
            if echo_to_stdout {
                eprintln!("weatui: polling resumed after a {}s gap", gap);
            }
            if let Err(e) = notify::send_gap_recovery(gap, &cfg.alerts.notify, &cfg.alerts.scripts) {
                eprintln!("weatui: gap recovery notice failed: {e:#}");
            }
        }

        if tick.went_stale {
            let elapsed = engine.stale_elapsed();
            if echo_to_stdout {
                eprintln!("weatui: alert feed stale for {}s", elapsed);
            }
            if let Err(e) = notify::send_stale_warning(elapsed, &cfg.alerts.notify, &cfg.alerts.scripts) {
                eprintln!("weatui: stale warning failed: {e:#}");
            }
        }

        if let Some(err) = &tick.poll_error
            && echo_to_stdout {
                eprintln!("weatui: poll failed: {err}");
            }

        tokio::time::sleep(engine.next_delay()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_successful_poll_after_a_long_gap_still_reports_the_gap() {
        assert_eq!(gap_to_report(Some(10_800), true, 300), Some(10_800));
    }

    #[test]
    fn an_ordinary_cadence_reports_no_gap() {
        assert_eq!(gap_to_report(Some(5), true, 300), None);
    }

    #[test]
    fn a_failed_poll_reports_no_recovery() {
        assert_eq!(gap_to_report(Some(10_800), false, 300), None);
    }

    #[test]
    fn startup_with_no_previous_success_is_not_a_recovered_gap() {
        assert_eq!(gap_to_report(None, true, 300), None);
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_underflow_into_a_huge_gap() {
        assert_eq!(gap_to_report(Some(0), true, 300), None);
    }
}
