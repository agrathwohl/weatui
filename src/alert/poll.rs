//! Conditional polling of api.weather.gov.
//!
//! The API is poll-only, but it advertises `cache-control: max-age=4` and
//! supports ETags, so ~5s conditional polling is sanctioned and steady-state
//! requests return a bare 304. NWS requires a User-Agent carrying contact
//! information; requests without one may be refused.

use crate::alert::{Alert, AlertCollection};
use crate::geo::Coords;
use anyhow::{Context, Result};
use reqwest::{Client, StatusCode, header};
use std::time::Duration;

pub const USER_AGENT: &str =
    "weatui (https://github.com/gwohl/weatui, andrew@grathwohl.me)";

const MAX_BACKOFF_SECS: u64 = 300;

#[derive(Debug)]
pub enum PollOutcome {
    Updated { alerts: Vec<Alert>, dropped: usize },
    Unchanged,
}

pub fn parse_max_age(cache_control: &str) -> Option<u64> {
    cache_control
        .split(',')
        .filter_map(|part| part.trim().strip_prefix("max-age="))
        .find_map(|v| v.trim().parse::<u64>().ok())
}

/// Exponential with a ceiling. `attempt` 0 means no failures yet.
pub fn backoff_secs(attempt: u32, base_secs: u64) -> u64 {
    if attempt == 0 {
        return base_secs;
    }
    base_secs
        .saturating_mul(1u64 << attempt.min(16))
        .min(MAX_BACKOFF_SECS)
}

pub fn alerts_url(at: Coords) -> String {
    format!(
        "https://api.weather.gov/alerts/active?point={:.4},{:.4}",
        at.lat, at.lon
    )
}

pub struct Poller {
    client: Client,
    url: String,
    etag: Option<String>,
    base_interval_secs: u64,
    server_max_age: Option<u64>,
    consecutive_failures: u32,
    retry_after: Option<u64>,
}

/// Only the delta-seconds form. The HTTP-date form is legal but api.weather.gov
/// does not use it, and guessing wrong is worse than falling back to backoff.
pub fn parse_retry_after(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok()
}

impl Poller {
    pub fn new(at: Coords, base_interval_secs: u64) -> Result<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(20))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Poller {
            client,
            url: alerts_url(at),
            etag: None,
            base_interval_secs,
            server_max_age: None,
            consecutive_failures: 0,
            retry_after: None,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Honour the server's own cache lifetime when it is longer than the
    /// configured interval, and back off after failures.
    pub fn next_delay(&self) -> Duration {
        if self.consecutive_failures > 0 {
            let backoff = backoff_secs(self.consecutive_failures, self.base_interval_secs);
            let wait = match self.retry_after {
                Some(asked) => asked.max(backoff).min(MAX_BACKOFF_SECS),
                None => backoff,
            };
            return Duration::from_secs(wait);
        }
        let secs = self
            .server_max_age
            .unwrap_or(self.base_interval_secs)
            .max(self.base_interval_secs);
        Duration::from_secs(secs)
    }

    pub async fn poll(&mut self) -> Result<PollOutcome> {
        let mut req = self.client.get(&self.url);
        if let Some(tag) = &self.etag {
            req = req.header(header::IF_NONE_MATCH, tag);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                return Err(e).context("alert request failed");
            }
        };

        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            self.retry_after = resp
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after);
            anyhow::bail!("api.weather.gov rate limited the request");
        }

        if let Err(e) = resp.error_for_status_ref() {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            return Err(e).context("alert request returned an error status");
        }

        self.server_max_age = resp
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_max_age);

        if resp.status() == StatusCode::NOT_MODIFIED {
            self.consecutive_failures = 0;
            self.retry_after = None;
            return Ok(PollOutcome::Unchanged);
        }

        let new_etag = resp
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let body: AlertCollection = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                return Err(e).context("alert response was not the expected GeoJSON");
            }
        };

        self.consecutive_failures = 0;
        self.retry_after = None;

        self.etag = new_etag;
        Ok(PollOutcome::Updated {
            dropped: body.dropped,
            alerts: body.features.into_iter().map(Alert::from_feature).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A15: NWS asks for contact details so they can reach the operator.
    #[test]
    fn a15_user_agent_carries_contact_information() {
        assert!(USER_AGENT.contains('@'), "no contact address: {USER_AGENT}");
        assert!(USER_AGENT.starts_with("weatui"));
    }

    #[test]
    fn a9_max_age_is_read_from_the_live_header_form() {
        assert_eq!(parse_max_age("public, max-age=4, s-maxage=5"), Some(4));
        assert_eq!(parse_max_age("max-age=30"), Some(30));
        assert_eq!(parse_max_age("no-cache"), None);
        assert_eq!(parse_max_age(""), None);
        assert_eq!(parse_max_age("max-age=notanumber"), None);
    }

    #[test]
    fn url_pins_the_point_query_to_four_decimals() {
        let u = alerts_url(Coords { lat: 35.205661, lon: -97.442738 });
        assert_eq!(u, "https://api.weather.gov/alerts/active?point=35.2057,-97.4427");
    }

    #[test]
    fn backoff_grows_then_saturates() {
        assert_eq!(backoff_secs(0, 5), 5);
        assert_eq!(backoff_secs(1, 5), 10);
        assert_eq!(backoff_secs(2, 5), 20);
        assert_eq!(backoff_secs(3, 5), 40);
        assert_eq!(backoff_secs(20, 5), MAX_BACKOFF_SECS);
    }

    #[test]
    fn healthy_poller_waits_at_least_the_configured_interval() {
        let p = Poller::new(Coords { lat: 35.0, lon: -97.0 }, 5).unwrap();
        assert_eq!(p.next_delay(), Duration::from_secs(5));
    }

    #[test]
    fn server_max_age_below_configured_interval_does_not_speed_up_polling() {
        let mut p = Poller::new(Coords { lat: 35.0, lon: -97.0 }, 10).unwrap();
        p.server_max_age = Some(4);
        assert_eq!(p.next_delay(), Duration::from_secs(10));
    }

    #[test]
    fn retry_after_is_honoured_when_the_server_asks_for_longer() {
        assert_eq!(parse_retry_after("120"), Some(120));
        assert_eq!(parse_retry_after(" 30 "), Some(30));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);

        let mut p = Poller::new(Coords { lat: 35.0, lon: -97.0 }, 5).unwrap();
        p.consecutive_failures = 1;
        p.retry_after = Some(120);
        assert_eq!(p.next_delay(), Duration::from_secs(120), "server wins when it asks for longer");

        p.retry_after = Some(1);
        assert_eq!(
            p.next_delay(),
            Duration::from_secs(10),
            "backoff wins when the server asks for less than we already back off"
        );
    }

    #[test]
    fn a_body_with_no_features_key_is_an_error_not_an_empty_sky() {
        assert!(
            serde_json::from_str::<AlertCollection>(r#"{"status":502}"#).is_err(),
            "a 200 with a different shape must not read as zero active alerts"
        );
        assert!(serde_json::from_str::<AlertCollection>(r#"{"features":[]}"#).is_ok());
    }

    #[test]
    fn failures_switch_the_delay_to_backoff() {
        let mut p = Poller::new(Coords { lat: 35.0, lon: -97.0 }, 5).unwrap();
        p.consecutive_failures = 3;
        assert_eq!(p.next_delay(), Duration::from_secs(40));
    }
}
