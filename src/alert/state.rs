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
    pub instruction: Option<String>,
    pub damage_threat: Option<String>,
    pub tornado_detection: Option<String>,
}

impl Notification {
    fn from_alert(key: AlertKey, tier: ThreatTier, alert: &Alert) -> Self {
        Notification {
            key,
            tier,
            event: alert.properties.event.clone(),
            headline: alert.properties.headline.clone(),
            area: alert.properties.area_desc.clone(),
            instruction: alert.properties.instruction.clone(),
            damage_threat: alert.damage_threat(),
            tornado_detection: alert.tornado_detection(),
        }
    }

    fn severity_markers(&self) -> SeverityMarkers {
        (self.damage_threat.clone(), self.tornado_detection.clone())
    }
}

/// `damageThreat` and `tornadoDetection`. An SVS that upgrades a warning to a
/// tornado emergency reuses the ETN, so the key is unchanged and plain dedup
/// swallows the upgrade. Notification is keyed on these changing too.
type SeverityMarkers = (Option<String>, Option<String>);

pub fn key_of(alert: &Alert) -> AlertKey {
    match alert.primary_vtec() {
        Some(v) => {
            let (office, phenomenon, significance, etn) = v.event_key();
            format!("{office}.{phenomenon}.{significance}.{etn}")
        }
        None => alert.properties.id.clone().unwrap_or_else(|| {
            format!(
                "{}|{}|{}",
                alert.properties.event,
                alert.properties.area_desc.as_deref().unwrap_or(""),
                alert.properties.expires.as_deref().unwrap_or("")
            )
        }),
    }
}

#[derive(Debug, Default)]
pub struct AlertState {
    active: HashMap<AlertKey, ActiveAlert>,
    notified: HashMap<AlertKey, SeverityMarkers>,
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

            let candidate = Notification::from_alert(key.clone(), tier, &alert);
            let markers = candidate.severity_markers();
            let escalated = self.notified.get(&key).is_none_or(|prev| *prev != markers);

            if escalated {
                self.notified.insert(key.clone(), markers);
                fresh.push(candidate);
            }

            self.active.insert(key, ActiveAlert { alert, tier });
        }

        self.active.retain(|k, _| seen.contains(k));
        self.notified.retain(|k, _| seen.contains(k));
        fresh
    }

    pub fn active(&self) -> impl Iterator<Item = &ActiveAlert> {
        self.active.values()
    }

    /// Removal used to depend entirely on the alert leaving `/alerts/active`,
    /// so a feed that goes quiet mid-event left expired warnings on screen
    /// looking live. Alerts with no parseable expiry are kept: the feed is
    /// still the authority for those.
    pub fn prune_expired(&mut self, now: chrono::DateTime<chrono::Utc>) -> usize {
        let before = self.active.len();
        self.active
            .retain(|_, a| a.alert.expires_at().is_none_or(|t| t > now));
        self.notified.retain(|k, _| self.active.contains_key(k));
        before - self.active.len()
    }

    pub fn mark_poll_success(&mut self, now_epoch: u64) {
        self.last_success_epoch = Some(now_epoch);
    }

    pub fn last_success_epoch(&self) -> Option<u64> {
        self.last_success_epoch
    }

    /// A dead poller looks like calm weather; callers surface the difference.
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

    fn alert_with_params(event: &str, vtec: &str, extra: &str) -> Alert {
        let json = format!(
            r#"{{"features":[{{"geometry":null,"properties":{{"event":"{event}","parameters":{{"VTEC":["{vtec}"]{extra}}}}}}}]}}"#
        );
        let parsed: AlertCollection = serde_json::from_str(&json).unwrap();
        Alert::from_feature(parsed.features.into_iter().next().unwrap())
    }

    #[test]
    fn an_upgrade_to_a_tornado_emergency_notifies_again() {
        let mut st = AlertState::new();
        let f = filter();

        let first = st.ingest(vec![alert_with("Tornado Warning", Some(TOR_NEW))], &f);
        assert_eq!(first.len(), 1);

        let upgraded = st.ingest(
            vec![alert_with_params(
                "Tornado Warning",
                TOR_CON,
                r#","damageThreat":["CATASTROPHIC"]"#,
            )],
            &f,
        );
        assert_eq!(
            upgraded.len(),
            1,
            "an SVS upgrade reuses the ETN, so dedup must not swallow the emergency"
        );
        assert_eq!(upgraded[0].damage_threat.as_deref(), Some("CATASTROPHIC"));
    }

    #[test]
    fn an_ordinary_continuation_still_does_not_renotify() {
        let mut st = AlertState::new();
        let f = filter();
        st.ingest(vec![alert_with("Tornado Warning", Some(TOR_NEW))], &f);
        let again = st.ingest(vec![alert_with("Tornado Warning", Some(TOR_CON))], &f);
        assert!(again.is_empty(), "no severity change means no second toast");
    }

    const TOR_WATCH_SAME_ETN: &str = "/O.NEW.KTLX.TO.A.0012.260727T0600Z-260727T1200Z/";
    const TOR_WATCH_EXP_SAME_ETN: &str = "/O.EXP.KTLX.TO.A.0012.260727T0600Z-260727T1200Z/";

    #[test]
    fn a_warning_is_not_suppressed_by_a_watch_sharing_its_etn() {
        let mut st = AlertState::new();
        let f = filter();

        let first = st.ingest(vec![alert_with("Tornado Watch", Some(TOR_WATCH_SAME_ETN))], &f);
        assert_eq!(first.len(), 1, "the watch should notify");

        let second = st.ingest(
            vec![
                alert_with("Tornado Watch", Some(TOR_WATCH_SAME_ETN)),
                alert_with("Tornado Warning", Some(TOR_NEW)),
            ],
            &f,
        );
        let tiers: Vec<ThreatTier> = second.iter().map(|n| n.tier).collect();
        assert_eq!(
            tiers,
            vec![ThreatTier::Lethal],
            "the tornado warning must notify even though the watch shares its ETN"
        );
    }

    #[test]
    fn a_watch_does_not_overwrite_a_warning_sharing_its_etn() {
        let mut st = AlertState::new();
        let f = filter();
        st.ingest(
            vec![
                alert_with("Tornado Warning", Some(TOR_NEW)),
                alert_with("Tornado Watch", Some(TOR_WATCH_SAME_ETN)),
            ],
            &f,
        );
        assert_eq!(st.active().count(), 2);
        assert_eq!(st.active().map(|a| a.tier).max(), Some(ThreatTier::Lethal));
    }

    #[test]
    fn an_expiring_watch_does_not_delete_a_live_warning() {
        let mut st = AlertState::new();
        let f = filter();
        st.ingest(
            vec![
                alert_with("Tornado Warning", Some(TOR_NEW)),
                alert_with("Tornado Watch", Some(TOR_WATCH_SAME_ETN)),
            ],
            &f,
        );
        st.ingest(
            vec![
                alert_with("Tornado Warning", Some(TOR_NEW)),
                alert_with("Tornado Watch", Some(TOR_WATCH_EXP_SAME_ETN)),
            ],
            &f,
        );
        assert_eq!(
            st.active().map(|a| a.tier).max(),
            Some(ThreatTier::Lethal),
            "expiring the watch must not retire the warning that shares its ETN"
        );
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

    fn alert_expiring(vtec: &str, expires: &str) -> Alert {
        let json = format!(
            r#"{{"features":[{{"geometry":null,"properties":{{"event":"Tornado Warning","expires":"{expires}","parameters":{{"VTEC":["{vtec}"]}}}}}}]}}"#
        );
        let parsed: AlertCollection = serde_json::from_str(&json).unwrap();
        Alert::from_feature(parsed.features.into_iter().next().unwrap())
    }

    #[test]
    fn an_expired_alert_is_dropped_without_waiting_for_the_feed() {
        let mut st = AlertState::new();
        let f = filter();
        let expires = "2026-07-27T07:30:00Z";
        st.ingest(vec![alert_expiring(TOR_NEW, expires)], &f);
        assert_eq!(st.active().count(), 1);

        let before = chrono::DateTime::parse_from_rfc3339("2026-07-27T07:20:00Z").unwrap().to_utc();
        assert_eq!(st.prune_expired(before), 0, "still live");
        assert_eq!(st.active().count(), 1);

        let after = chrono::DateTime::parse_from_rfc3339("2026-07-27T07:31:00Z").unwrap().to_utc();
        assert_eq!(st.prune_expired(after), 1, "past its expiry");
        assert_eq!(st.active().count(), 0);
    }

    #[test]
    fn an_alert_with_no_expiry_is_left_to_the_feed() {
        let mut st = AlertState::new();
        let f = filter();
        st.ingest(vec![alert_with("Tornado Warning", Some(TOR_NEW))], &f);
        let far_future = chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z").unwrap().to_utc();
        assert_eq!(st.prune_expired(far_future), 0);
        assert_eq!(st.active().count(), 1);
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
