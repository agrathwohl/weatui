//! Headless alert pipeline shared by `weatui -d` and the interactive front end.

use crate::alert::filter::Filter;
use crate::alert::poll::{PollOutcome, Poller};
use crate::alert::state::{AlertState, Notification};
use crate::config::Config;
use crate::geo::Coords;
use crate::notify;
use anyhow::Result;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
        let mut out = Tick { fresh: Vec::new(), went_stale: false, poll_error: None };

        match self.poller.poll().await {
            Ok(PollOutcome::Updated(alerts)) => {
                out.fresh = self.state.ingest(alerts, &self.filter);
                self.state.mark_poll_success(now);
            }
            Ok(PollOutcome::Unchanged) => {
                self.state.mark_poll_success(now);
            }
            Err(e) => out.poll_error = Some(format!("{e:#}")),
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
    let mut engine = AlertEngine::new(&cfg, home)?;
    if echo_to_stdout {
        println!(
            "weatui monitoring {:.4},{:.4} via {}",
            home.lat,
            home.lon,
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
            if let Err(e) = notify::send(n, &cfg.alerts.notify, eta) {
                eprintln!("weatui: notification failed: {e:#}");
            }
        }

        if tick.went_stale {
            let elapsed = engine.stale_elapsed();
            if echo_to_stdout {
                eprintln!("weatui: alert feed stale for {}s", elapsed);
            }
            if let Err(e) = notify::send_stale_warning(elapsed) {
                eprintln!("weatui: stale warning failed: {e:#}");
            }
        }

        if let Some(err) = &tick.poll_error {
            if echo_to_stdout {
                eprintln!("weatui: poll failed: {err}");
            }
        }

        tokio::time::sleep(engine.next_delay()).await;
    }
}
