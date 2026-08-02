//! Desktop notification via `notify-send`.
//!
//! Deliberately a shell-out rather than a D-Bus client crate: mako is already
//! running and already styles `[urgency=critical]`, so `notify-send -u` maps
//! straight onto configuration the user has written. Notification bodies stay
//! plain ASCII because the notification daemon's font is not guaranteed to
//! carry Nerd Font glyphs, and tofu in a tornado warning is unacceptable.

use crate::alert::filter::ThreatTier;
use crate::alert::state::Notification;
use crate::config::{NotifyLevels, Urgency};
use anyhow::{Context, Result};
use std::process::Command;

pub fn urgency_for(tier: ThreatTier, levels: &NotifyLevels) -> Urgency {
    match tier {
        ThreatTier::Lethal => levels.lethal,
        ThreatTier::Severe => levels.severe,
        ThreatTier::Watch => levels.watch,
    }
}

pub fn build_args(summary: &str, body: &str, urgency: Urgency) -> Vec<String> {
    vec![
        "-u".to_string(),
        urgency.as_notify_send_arg().to_string(),
        "-a".to_string(),
        "weatui".to_string(),
        summary.to_string(),
        body.to_string(),
    ]
}

pub fn summary_for(n: &Notification) -> String {
    format!("[{}] {}", n.tier.label(), n.event)
}

pub fn body_for(n: &Notification, eta_minutes: Option<i64>) -> String {
    let mut parts = Vec::new();
    if let Some(h) = &n.headline {
        parts.push(h.clone());
    }
    if let Some(a) = &n.area {
        parts.push(a.clone());
    }
    if let Some(m) = eta_minutes {
        parts.push(format!("Estimated arrival in {m} min"));
    }
    if parts.is_empty() {
        parts.push(n.event.clone());
    }
    parts.join("\n")
}

fn run(args: &[String]) -> Result<()> {
    let status = Command::new("notify-send")
        .args(args)
        .status()
        .context("failed to run notify-send; is libnotify installed?")?;
    if !status.success() {
        anyhow::bail!("notify-send exited with {status}");
    }
    Ok(())
}

pub fn send(n: &Notification, levels: &NotifyLevels, eta_minutes: Option<i64>) -> Result<()> {
    let urgency = urgency_for(n.tier, levels);
    run(&build_args(&summary_for(n), &body_for(n, eta_minutes), urgency))
}

/// A dead poller is indistinguishable from calm weather, so it is announced at
/// critical urgency regardless of configured tier levels.
pub fn send_stale_warning(elapsed_secs: u64) -> Result<()> {
    let minutes = elapsed_secs / 60;
    run(&build_args(
        "[weatui] ALERT FEED STALE",
        &format!(
            "No successful poll of api.weather.gov for {minutes} min.\n\
             You are NOT being warned about severe weather right now."
        ),
        Urgency::Critical,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(tier: ThreatTier, event: &str) -> Notification {
        Notification {
            key: "KTLX.TO.12".to_string(),
            tier,
            event: event.to_string(),
            headline: Some("Tornado Warning until 3:15 PM CDT".to_string()),
            area: Some("Cleveland, OK".to_string()),
        }
    }

    #[test]
    fn a12_lethal_and_severe_map_to_critical_urgency() {
        let levels = NotifyLevels::default();
        assert_eq!(urgency_for(ThreatTier::Lethal, &levels), Urgency::Critical);
        assert_eq!(urgency_for(ThreatTier::Severe, &levels), Urgency::Critical);
    }

    #[test]
    fn watch_tier_maps_to_normal_urgency() {
        assert_eq!(
            urgency_for(ThreatTier::Watch, &NotifyLevels::default()),
            Urgency::Normal
        );
    }

    #[test]
    fn a12_critical_urgency_reaches_the_notify_send_command_line() {
        let args = build_args("s", "b", Urgency::Critical);
        let joined = args.join(" ");
        assert!(joined.contains("-u critical"), "got: {joined}");
        assert!(joined.contains("-a weatui"));
    }

    #[test]
    fn summary_leads_with_the_threat_tier() {
        let s = summary_for(&notification(ThreatTier::Lethal, "Tornado Warning"));
        assert!(s.starts_with("[LETHAL]"), "got: {s}");
        assert!(s.contains("Tornado Warning"));
    }

    #[test]
    fn body_includes_eta_when_a_motion_vector_was_available() {
        let b = body_for(&notification(ThreatTier::Lethal, "Tornado Warning"), Some(11));
        assert!(b.contains("11 min"), "got: {b}");
    }

    #[test]
    fn body_falls_back_to_the_event_name_when_nothing_else_is_present() {
        let n = Notification {
            key: "k".to_string(),
            tier: ThreatTier::Severe,
            event: "Special Weather Statement".to_string(),
            headline: None,
            area: None,
        };
        assert_eq!(body_for(&n, None), "Special Weather Statement");
    }

    #[test]
    fn notification_body_is_plain_ascii_for_daemon_font_safety() {
        let b = body_for(&notification(ThreatTier::Lethal, "Tornado Warning"), Some(5));
        assert!(b.is_ascii(), "non-ascii would risk tofu in mako: {b}");
    }

    #[test]
    fn stale_warning_states_plainly_that_warnings_are_not_arriving() {
        let args = build_args(
            "[weatui] ALERT FEED STALE",
            "No successful poll of api.weather.gov for 5 min.\nYou are NOT being warned about severe weather right now.",
            Urgency::Critical,
        );
        assert!(args.join(" ").contains("NOT being warned"));
    }
}
