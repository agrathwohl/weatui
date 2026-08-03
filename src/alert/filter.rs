//! Two-path lethality allowlist.
//!
//! CAP `response` is unusable for this: live data shows `Monitor` covering both
//! Severe Thunderstorm Watch and Air Quality Alert, and `Avoid` covering both
//! Flash Flood Warning and Small Craft Advisory. Classification is therefore an
//! explicit allowlist over P-VTEC, plus an event-name path for the products
//! that carry no VTEC at all.

use crate::alert::Alert;
use crate::config::Alerts;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThreatTier {
    Watch,
    Severe,
    Lethal,
}

impl ThreatTier {
    pub fn label(self) -> &'static str {
        match self {
            ThreatTier::Lethal => "LETHAL",
            ThreatTier::Severe => "SEVERE",
            ThreatTier::Watch => "WATCH",
        }
    }
}

pub struct Filter {
    lethal: HashSet<String>,
    severe: HashSet<String>,
    watch: HashSet<String>,
    extra_events: HashSet<String>,
}

impl Filter {
    pub fn from_config(alerts: &Alerts) -> Self {
        Filter {
            lethal: alerts.tiers.lethal.iter().cloned().collect(),
            severe: alerts.tiers.severe.iter().cloned().collect(),
            watch: alerts.tiers.watch.iter().cloned().collect(),
            extra_events: alerts.extra_events.iter().cloned().collect(),
        }
    }

    /// `None` means "not worth waking someone up for". Unknown products reject
    /// by default: this is an allowlist, never a blocklist.
    pub fn classify(&self, alert: &Alert) -> Option<ThreatTier> {
        if let Some(vtec) = alert.primary_vtec() {
            if !vtec.is_operational() {
                return None;
            }
            let key = vtec.phenomenon_significance();
            if self.lethal.contains(&key) {
                return Some(ThreatTier::Lethal);
            }
            if self.severe.contains(&key) {
                return Some(ThreatTier::Severe);
            }
            if self.watch.contains(&key) {
                return Some(ThreatTier::Watch);
            }
            return None;
        }

        if self.extra_events.contains(&alert.properties.event) {
            return Some(ThreatTier::Severe);
        }
        None
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

    #[test]
    fn a2_severe_thunderstorm_warning_is_severe() {
        let a = alert_with(
            "Severe Thunderstorm Warning",
            Some("/O.NEW.KDLH.SV.W.0087.260727T0700Z-260727T0800Z/"),
        );
        assert_eq!(filter().classify(&a), Some(ThreatTier::Severe));
    }

    #[test]
    fn a2_tornado_warning_is_lethal() {
        let a = alert_with(
            "Tornado Warning",
            Some("/O.NEW.KTLX.TO.W.0012.260727T0700Z-260727T0730Z/"),
        );
        assert_eq!(filter().classify(&a), Some(ThreatTier::Lethal));
    }

    #[test]
    fn flash_flood_warning_is_lethal() {
        let a = alert_with(
            "Flash Flood Warning",
            Some("/O.NEW.KTLX.FF.W.0003.260727T0700Z-260727T0900Z/"),
        );
        assert_eq!(filter().classify(&a), Some(ThreatTier::Lethal));
    }

    /// A3: the live snapshot showed Air Quality Alert carrying no VTEC, so
    /// the no-VTEC path is an allowlist, not a fallthrough.
    #[test]
    fn a3_air_quality_alert_is_rejected() {
        let a = alert_with("Air Quality Alert", None);
        assert_eq!(filter().classify(&a), None);
    }

    #[test]
    fn a4_special_weather_statement_is_accepted_despite_having_no_vtec() {
        let a = alert_with("Special Weather Statement", None);
        assert_eq!(filter().classify(&a), Some(ThreatTier::Severe));
    }

    #[test]
    fn small_craft_advisory_is_rejected() {
        let a = alert_with(
            "Small Craft Advisory",
            Some("/O.NEW.KBOX.SC.Y.0123.260727T0700Z-260727T1900Z/"),
        );
        assert_eq!(filter().classify(&a), None);
    }

    #[test]
    fn heat_and_marine_products_are_rejected() {
        for (event, vtec) in [
            ("Heat Advisory", "/O.NEW.KOUN.HT.Y.0004.260727T1500Z-260728T0000Z/"),
            ("Extreme Heat Warning", "/O.NEW.KOUN.EH.W.0002.260727T1500Z-260728T0000Z/"),
            ("Gale Warning", "/O.NEW.KBOX.GL.W.0011.260727T0700Z-260727T1900Z/"),
            ("Rip Current Statement", "/O.NEW.KBOX.RP.S.0009.260727T0700Z-260727T1900Z/"),
            ("Beach Hazards Statement", "/O.NEW.KBOX.BH.S.0002.260727T0700Z-260727T1900Z/"),
        ] {
            let a = alert_with(event, Some(vtec));
            assert_eq!(filter().classify(&a), None, "{event} should be rejected");
        }
    }

    #[test]
    fn watches_classify_as_watch_tier() {
        let a = alert_with(
            "Tornado Watch",
            Some("/O.NEW.KWNS.TO.A.0455.260727T1800Z-260728T0200Z/"),
        );
        assert_eq!(filter().classify(&a), Some(ThreatTier::Watch));
    }

    #[test]
    fn unknown_event_without_vtec_rejects_by_default() {
        let a = alert_with("Some Brand New NWS Product", None);
        assert_eq!(filter().classify(&a), None);
    }

    /// Test-class products raise no alarms.
    #[test]
    fn non_operational_vtec_is_rejected_even_for_tornado_warning() {
        let a = alert_with(
            "Tornado Warning",
            Some("/T.NEW.KTLX.TO.W.0012.260727T0700Z-260727T0730Z/"),
        );
        assert_eq!(filter().classify(&a), None);
    }

    #[test]
    fn tiers_order_lethal_above_severe_above_watch() {
        assert!(ThreatTier::Lethal > ThreatTier::Severe);
        assert!(ThreatTier::Severe > ThreatTier::Watch);
    }
}
