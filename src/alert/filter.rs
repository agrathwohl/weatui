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
            lethal: alerts.tiers.lethal_codes().map(str::to_string).collect(),
            severe: alerts.tiers.severe_codes().map(str::to_string).collect(),
            watch: alerts.tiers.watch_codes().map(str::to_string).collect(),
            extra_events: alerts.extra_events.iter().cloned().collect(),
        }
    }

    fn tier_of(&self, key: &str) -> Option<ThreatTier> {
        if self.lethal.contains(key) {
            return Some(ThreatTier::Lethal);
        }
        if self.severe.contains(key) {
            return Some(ThreatTier::Severe);
        }
        if self.watch.contains(key) {
            return Some(ThreatTier::Watch);
        }
        None
    }

    /// `None` means "not worth waking someone up for". Unknown products reject
    /// by default: this is an allowlist, never a blocklist.
    ///
    /// A tier entry is either a P-VTEC `PH.S` code or a literal event name, so
    /// the same list covers both the coded products and the civil-emergency
    /// messages that carry no VTEC at all. The event-name pass also runs for
    /// VTEC-bearing alerts: previously a `return None` inside the VTEC branch
    /// made every escape hatch unreachable for exactly the coded products a
    /// user would most want to add.
    pub fn classify(&self, alert: &Alert) -> Option<ThreatTier> {
        if let Some(vtec) = alert.primary_vtec() {
            if !vtec.is_operational() {
                return None;
            }
            if let Some(tier) = self.tier_of(&vtec.phenomenon_significance()) {
                return Some(tier);
            }
        }

        if let Some(tier) = self.tier_of(&alert.properties.event) {
            return Some(tier);
        }

        if self.extra_events.contains(&alert.properties.event) {
            return Some(ThreatTier::Severe);
        }

        // Fail open, loudly. A product that carried VTEC none of which parsed
        // is a malformed or newly-introduced code, and if its event name says
        // Warning it is a real hazard the allowlist can never match. Dropping
        // it silently is the one outcome that cannot be recovered from.
        if alert.vtec_unparsed() && alert.properties.event.ends_with("Warning") {
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
    fn marine_and_advisory_products_are_rejected() {
        for (event, vtec) in [
            ("Heat Advisory", "/O.NEW.KOUN.HT.Y.0004.260727T1500Z-260728T0000Z/"),
            ("Gale Warning", "/O.NEW.KBOX.GL.W.0011.260727T0700Z-260727T1900Z/"),
            ("Rip Current Statement", "/O.NEW.KBOX.RP.S.0009.260727T0700Z-260727T1900Z/"),
            ("Beach Hazards Statement", "/O.NEW.KBOX.BH.S.0002.260727T0700Z-260727T1900Z/"),
        ] {
            let a = alert_with(event, Some(vtec));
            assert_eq!(filter().classify(&a), None, "{event} should be rejected");
        }
    }

    /// Heat kills more people in most US years than any other weather hazard,
    /// so it is alerted despite this being a radar tool. Both the current
    /// `XH.W` and the retired `EH.W` are covered because feeds carry both.
    #[test]
    fn extreme_heat_is_alerted_under_both_the_current_and_retired_code() {
        for vtec in [
            "/O.NEW.KOUN.XH.W.0002.260727T1500Z-260728T0000Z/",
            "/O.NEW.KOUN.EH.W.0002.260727T1500Z-260728T0000Z/",
        ] {
            let a = alert_with("Extreme Heat Warning", Some(vtec));
            assert_eq!(filter().classify(&a), Some(ThreatTier::Severe), "{vtec}");
        }
    }

    /// Regression, Middle Tennessee, 2026-01-26. An ice storm cut power to
    /// 230,000 homes for over a week, and the cold behind it killed 11 of the
    /// 21 Tennesseans who died, indoors, in unheated houses. NWS carried an
    /// Extreme Cold Warning through that night. The allowlist listed heat
    /// under both its current and retired codes and had no cold entry at all,
    /// so `classify` returned `None` and the warning was discarded in silence
    /// during the hours it mattered most.
    #[test]
    fn extreme_cold_is_alerted_under_both_the_current_and_retired_code() {
        for (event, vtec, tier) in [
            ("Extreme Cold Warning", "/O.NEW.KOHX.EC.W.0001.260126T2359Z-260128T0600Z/",
             ThreatTier::Severe),
            ("Wind Chill Warning", "/O.NEW.KOHX.WC.W.0001.260126T2359Z-260128T0600Z/",
             ThreatTier::Severe),
            ("Extreme Cold Watch", "/O.NEW.KOHX.EC.A.0001.260126T2359Z-260128T0600Z/",
             ThreatTier::Watch),
            ("Wind Chill Watch", "/O.NEW.KOHX.WC.A.0001.260126T2359Z-260128T0600Z/",
             ThreatTier::Watch),
        ] {
            let a = alert_with(event, Some(vtec));
            assert_eq!(filter().classify(&a), Some(tier), "{event} {vtec}");
        }
    }

    /// The pairing is the invariant: whenever a temperature extreme is
    /// alertable in one direction it must be alertable in the other, or the
    /// list silently encodes a preference about which way people die.
    #[test]
    fn heat_and_cold_are_covered_symmetrically() {
        let alerts = crate::config::Alerts::default();
        for (hot, cold) in [("XH.W", "EC.W"), ("EH.W", "WC.W")] {
            assert!(
                alerts.tiers.severe.iter().any(|s| s == hot)
                    && alerts.tiers.severe.iter().any(|s| s == cold),
                "{hot} and {cold} must both be severe"
            );
        }
        assert!(
            alerts.tiers.watch.iter().any(|s| s == "XH.A")
                && alerts.tiers.watch.iter().any(|s| s == "EC.A"),
            "XH.A and EC.A must both be watch"
        );
    }

    #[test]
    fn drowning_class_products_are_not_silently_discarded() {
        for (event, vtec, tier) in [
            ("Tsunami Warning", "/O.NEW.PAAQ.TS.W.0001.260727T0700Z-260727T1900Z/", ThreatTier::Lethal),
            ("Storm Surge Warning", "/O.NEW.KMFL.SS.W.0003.260727T0700Z-260727T1900Z/", ThreatTier::Lethal),
            ("Hurricane Warning", "/O.NEW.KMFL.HU.W.0002.260727T0700Z-260727T1900Z/", ThreatTier::Lethal),
            ("Flood Warning", "/O.NEW.KOHX.FL.W.0016.260727T0700Z-260727T1900Z/", ThreatTier::Severe),
            ("Flood Warning", "/O.NEW.KLWX.FA.W.0016.260727T0700Z-260727T1900Z/", ThreatTier::Severe),
        ] {
            let a = alert_with(event, Some(vtec));
            assert_eq!(filter().classify(&a), Some(tier), "{event} {vtec}");
        }
    }

    #[test]
    fn a_warning_whose_vtec_will_not_parse_is_not_silently_dropped() {
        let broken = alert_with("Tornado Warning", Some("/O.NEW.KTLX.NOT-A-VTEC/"));
        assert!(broken.vtec_unparsed(), "fixture must actually fail to parse");
        assert_eq!(
            filter().classify(&broken),
            Some(ThreatTier::Severe),
            "a malformed code on a real warning must fail open, not vanish"
        );
    }

    #[test]
    fn a_product_with_no_vtec_at_all_is_unaffected_by_the_fail_open_path() {
        let a = alert_with("Air Quality Alert", None);
        assert!(!a.vtec_unparsed());
        assert_eq!(filter().classify(&a), None);
    }

    #[test]
    fn a_malformed_vtec_on_a_non_warning_product_still_rejects() {
        let a = alert_with(
            "Rip Current Statement",
            Some("/O.NEW.KBOX.RP.S.NOTANETN.260727T0700Z-260727T1900Z/"),
        );
        assert!(a.vtec_unparsed());
        assert_eq!(filter().classify(&a), None, "fail-open is scoped to Warning products");
    }

    #[test]
    fn a_civil_emergency_without_any_vtec_still_classifies() {
        let a = alert_with("Evacuation Immediate", None);
        assert_eq!(filter().classify(&a), Some(ThreatTier::Lethal));
    }

    #[test]
    fn a_vtec_product_in_no_tier_can_be_rescued_by_name() {
        let gale = "/O.NEW.KBOX.GL.W.0011.260727T0700Z-260727T1900Z/";
        assert_eq!(filter().classify(&alert_with("Gale Warning", Some(gale))), None);

        let mut alerts = Alerts::default();
        alerts.extra_events.push("Gale Warning".to_string());
        let rescued = Filter::from_config(&alerts);
        assert_eq!(
            rescued.classify(&alert_with("Gale Warning", Some(gale))),
            Some(ThreatTier::Severe),
            "extra_events must reach a product that carries a VTEC"
        );
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
