//! Active alert tracking, notification dedup, and poll liveness.
//!
//! Time is passed in as epoch seconds rather than read from the clock so the
//! staleness logic is testable. `Instant` cannot be synthesised in a test.

use crate::alert::Alert;
use crate::alert::filter::{Filter, ThreatTier};
use std::collections::{HashMap, HashSet};

pub type AlertKey = String;

#[derive(Debug, Clone)]
pub struct ActiveAlert {
    pub alert: Alert,
    pub tier: ThreatTier,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub key: AlertKey,
    pub tier: ThreatTier,
    pub event: String,
    pub headline: Option<String>,
    pub area: Option<String>,
}

pub fn key_of(alert: &Alert) -> AlertKey {
    match alert.primary_vtec() {
        Some(v) => {
            let (office, phenomenon, etn) = v.event_key();
            format!("{office}.{phenomenon}.{etn}")
        }
        None => alert.properties.id.clone().unwrap_or_else(|| {
            format!(
                "{}|{}",
                alert.properties.event,
                alert.properties.area_desc.as_deref().unwrap_or("")
            )
        }),
    }
}

#[derive(Debug, Default)]
pub struct AlertState {
    active: HashMap<AlertKey, ActiveAlert>,
    notified: HashSet<AlertKey>,
    last_success_epoch: Option<u64>,
}

impl AlertState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns only the alerts that warrant a fresh desktop notification.
    pub fn ingest(&mut self, incoming: Vec<Alert>, filter: &Filter) -> Vec<Notification> {
        let mut seen: HashSet<AlertKey> = HashSet::new();
        let mut fresh = Vec::new();

        for alert in incoming {
            let Some(tier) = filter.classify(&alert) else {
                continue;
            };
            let key = key_of(&alert);

            if alert
                .primary_vtec()
                .is_some_and(|v| v.action.terminates_event())
            {
                self.active.remove(&key);
                self.notified.remove(&key);
                continue;
            }

            seen.insert(key.clone());

            if !self.notified.contains(&key) {
                self.notified.insert(key.clone());
                fresh.push(Notification {
                    key: key.clone(),
                    tier,
                    event: alert.properties.event.clone(),
                    headline: alert.properties.headline.clone(),
                    area: alert.properties.area_desc.clone(),
                });
            }

            self.active.insert(key, ActiveAlert { alert, tier });
        }

        self.active.retain(|k, _| seen.contains(k));
        self.notified.retain(|k| seen.contains(k));
        fresh
    }

    pub fn active(&self) -> impl Iterator<Item = &ActiveAlert> {
        self.active.values()
    }

    pub fn mark_poll_success(&mut self, now_epoch: u64) {
        self.last_success_epoch = Some(now_epoch);
    }

    pub fn last_success_epoch(&self) -> Option<u64> {
        self.last_success_epoch
    }

    /// A poller that has silently died looks exactly like calm weather. Callers
    /// must surface this rather than letting silence imply safety.
    pub fn is_stale(&self, now_epoch: u64, threshold_secs: u64) -> bool {
        match self.last_success_epoch {
            None => true,
            Some(t) => now_epoch.saturating_sub(t) >= threshold_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{AlertCollection, Feature};
    use crate::config::Alerts;

    fn alert_with(event: &str, vtec: Option<&str>) -> Alert {
        let params = match vtec {
            Some(v) => format!(r#"{{"VTEC":["{v}"]}}"#),
            None => "{}".to_string(),
        };
        let json = format!(
            r#"{{"features":[{{"geometry":null,"properties":{{"event":"{event}","parameters":{params}}}}}]}}"#
        );
        let parsed: AlertCollection = serde_json::from_str(&json).unwrap();
        let f: Feature = parsed.features.into_iter().next().unwrap();
        Alert::from_feature(f)
    }

    fn filter() -> Filter {
        Filter::from_config(&Alerts::default())
    }

    const TOR_NEW: &str = "/O.NEW.KTLX.TO.W.0012.260727T0700Z-260727T0730Z/";
    const TOR_CON: &str = "/O.CON.KTLX.TO.W.0012.260727T0705Z-260727T0730Z/";
    const TOR_CAN: &str = "/O.CAN.KTLX.TO.W.0012.260727T0710Z-260727T0730Z/";

    #[test]
    fn a8_same_event_seen_twice_notifies_once() {
        let mut st = AlertState::new();
        let f = filter();
        assert_eq!(st.ingest(vec![alert_with("Tornado Warning", Some(TOR_NEW))], &f).len(), 1);
        assert_eq!(st.ingest(vec![alert_with("Tornado Warning", Some(TOR_NEW))], &f).len(), 0);
        assert_eq!(st.active().count(), 1);
    }

    #[test]
    fn a8_continuation_does_not_renotify() {
        let mut st = AlertState::new();
        let f = filter();
        st.ingest(vec![alert_with("Tornado Warning", Some(TOR_NEW))], &f);
        let second = st.ingest(vec![alert_with("Tornado Warning", Some(TOR_CON))], &f);
        assert!(second.is_empty());
        assert_eq!(st.active().count(), 1);
    }

    #[test]
    fn a8_cancel_clears_the_alert() {
        let mut st = AlertState::new();
        let f = filter();
        st.ingest(vec![alert_with("Tornado Warning", Some(TOR_NEW))], &f);
        st.ingest(vec![alert_with("Tornado Warning", Some(TOR_CAN))], &f);
        assert_eq!(st.active().count(), 0);
    }

    #[test]
    fn alert_absent_from_a_later_poll_is_dropped() {
        let mut st = AlertState::new();
        let f = filter();
        st.ingest(vec![alert_with("Tornado Warning", Some(TOR_NEW))], &f);
        assert_eq!(st.active().count(), 1);
        st.ingest(Vec::new(), &f);
        assert_eq!(st.active().count(), 0);
    }

    #[test]
    fn a_cleared_alert_notifies_again_if_it_returns() {
        let mut st = AlertState::new();
        let f = filter();
        st.ingest(vec![alert_with("Tornado Warning", Some(TOR_NEW))], &f);
        st.ingest(Vec::new(), &f);
        assert_eq!(st.ingest(vec![alert_with("Tornado Warning", Some(TOR_NEW))], &f).len(), 1);
    }

    #[test]
    fn rejected_products_never_enter_state() {
        let mut st = AlertState::new();
        let f = filter();
        let out = st.ingest(
            vec![
                alert_with("Air Quality Alert", None),
                alert_with("Small Craft Advisory", Some("/O.NEW.KBOX.SC.Y.0123.260727T0700Z-260727T1900Z/")),
            ],
            &f,
        );
        assert!(out.is_empty());
        assert_eq!(st.active().count(), 0);
    }

    #[test]
    fn distinct_etns_are_tracked_separately() {
        let mut st = AlertState::new();
        let f = filter();
        let out = st.ingest(
            vec![
                alert_with("Tornado Warning", Some(TOR_NEW)),
                alert_with("Tornado Warning", Some("/O.NEW.KTLX.TO.W.0013.260727T0700Z-260727T0730Z/")),
            ],
            &f,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(st.active().count(), 2);
    }

    #[test]
    fn highest_tier_reports_the_worst_active_threat() {
        let mut st = AlertState::new();
        let f = filter();
        st.ingest(
            vec![
                alert_with("Severe Thunderstorm Watch", Some("/O.NEW.KWNS.SV.A.0455.260727T1800Z-260728T0200Z/")),
                alert_with("Tornado Warning", Some(TOR_NEW)),
            ],
            &f,
        );
        assert_eq!(st.active().map(|a| a.tier).max(), Some(ThreatTier::Lethal));
    }

    #[test]
    fn a_never_polled_state_is_stale_immediately() {
        assert!(AlertState::new().is_stale(1000, 300));
    }

    #[test]
    fn staleness_triggers_only_after_the_threshold() {
        let mut st = AlertState::new();
        st.mark_poll_success(1000);
        assert!(!st.is_stale(1100, 300));
        assert!(!st.is_stale(1299, 300));
        assert!(st.is_stale(1300, 300));
    }

    #[test]
    fn clock_going_backwards_does_not_underflow() {
        let mut st = AlertState::new();
        st.mark_poll_success(2000);
        assert!(!st.is_stale(1000, 300));
    }
}
