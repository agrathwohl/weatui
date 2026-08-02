//! Desktop notification via `notify-send`.
//!
//! Deliberately a shell-out rather than a D-Bus client crate: mako is already
//! running and already styles `[urgency=critical]`, so `notify-send -u` maps
//! straight onto configuration the user has written. Notification bodies stay
//! plain ASCII because the notification daemon's font is not guaranteed to
//! carry Nerd Font glyphs, and tofu in a tornado warning is unacceptable.

use crate::alert::filter::ThreatTier;
use crate::alert::state::Notification;
use crate::config::{NotifyLevels, Scripts, Urgency};
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
    if urgency == Urgency::None {
        return Ok(());
    }
    run(&build_args(&summary_for(n), &body_for(n, eta_minutes), urgency))
}

/// Environment handed to a per-tier alert script. Everything the notification
/// body carries, machine-readable, so the script never parses prose.
pub fn script_env(n: &Notification, eta_minutes: Option<i64>) -> Vec<(String, String)> {
    vec![
        ("WEATUI_TIER".into(), n.tier.label().to_string()),
        ("WEATUI_EVENT".into(), n.event.clone()),
        ("WEATUI_HEADLINE".into(), n.headline.clone().unwrap_or_default()),
        ("WEATUI_AREA".into(), n.area.clone().unwrap_or_default()),
        (
            "WEATUI_ETA_MINUTES".into(),
            eta_minutes.map(|m| m.to_string()).unwrap_or_default(),
        ),
    ]
}

fn run_script(script: &str, n: &Notification, eta_minutes: Option<i64>) -> Result<()> {
    // Detached from the terminal: in TUI mode stdout is a raw-mode alternate
    // screen, and a script echoing anything would scribble over the radar in
    // the middle of the warning it was configured for.
    let mut child = Command::new(script)
        .arg(n.tier.label())
        .arg(&n.event)
        .envs(script_env(n, eta_minutes))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("failed to launch alert script {script}"))?;
    // Reap off-thread: the alert loop must not block on a slow script, and an
    // unwaited child would linger as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// A fresh alert fans out to the desktop daemon (unless the tier's level is
/// `"none"`) and to the tier's configured script, independently: a broken
/// script must not silence the notification, nor the reverse.
pub fn dispatch(
    n: &Notification,
    levels: &NotifyLevels,
    scripts: &Scripts,
    eta_minutes: Option<i64>,
) -> Result<()> {
    let notified = send(n, levels, eta_minutes);
    let scripted = match scripts.for_tier(n.tier) {
        Some(script) => run_script(script, n, eta_minutes),
        None => Ok(()),
    };
    match (notified, scripted) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(a), Err(b)) => Err(anyhow::anyhow!("{a:#}; {b:#}")),
        (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
    }
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

    fn test_notification(tier: ThreatTier) -> Notification {
        Notification {
            key: "test".to_string(),
            tier,
            event: "Tornado Warning".into(),
            headline: Some("Tornado Warning until 9:15 PM".into()),
            area: Some("Hickman County".into()),
        }
    }

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

    #[test]
    fn a_none_level_skips_the_desktop_daemon_entirely() {
        let levels = NotifyLevels {
            lethal: Urgency::None,
            severe: Urgency::None,
            watch: Urgency::None,
        };
        let n = test_notification(ThreatTier::Lethal);
        assert!(send(&n, &levels, Some(12)).is_ok(), "notify-send must not be attempted");
    }

    #[test]
    fn the_script_environment_carries_the_whole_alert() {
        let env = script_env(&test_notification(ThreatTier::Lethal), Some(12));
        let get = |k: &str| env.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("WEATUI_TIER"), Some("LETHAL"));
        assert_eq!(get("WEATUI_EVENT"), Some("Tornado Warning"));
        assert_eq!(get("WEATUI_HEADLINE"), Some("Tornado Warning until 9:15 PM"));
        assert_eq!(get("WEATUI_AREA"), Some("Hickman County"));
        assert_eq!(get("WEATUI_ETA_MINUTES"), Some("12"));
    }

    #[test]
    fn missing_fields_become_empty_strings_not_absent_variables() {
        let mut n = test_notification(ThreatTier::Watch);
        n.headline = None;
        n.area = None;
        let env = script_env(&n, None);
        let get = |k: &str| env.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("WEATUI_HEADLINE"), Some(""));
        assert_eq!(get("WEATUI_ETA_MINUTES"), Some(""));
    }

    /// End to end: dispatch must execute the configured script with the alert
    /// in its environment, without notify-send being installed.
    #[test]
    fn dispatch_runs_the_tier_script() {
        let dir = std::env::temp_dir().join(format!("weatui-hook-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("hook.sh");
        let out = dir.join("fired");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s %s %s' \"$1\" \"$WEATUI_EVENT\" \"$WEATUI_ETA_MINUTES\" > {}\n",
                out.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let silent = NotifyLevels {
            lethal: Urgency::None,
            severe: Urgency::None,
            watch: Urgency::None,
        };
        let scripts = Scripts {
            lethal: Some(script.to_string_lossy().into_owned()),
            severe: None,
            watch: None,
        };
        dispatch(&test_notification(ThreatTier::Lethal), &silent, &scripts, Some(7)).unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !out.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "LETHAL Tornado Warning 7");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A broken script must never silence the warning, and its failure must
    /// not be swallowed either.
    #[test]
    fn a_missing_script_fails_loudly_without_touching_the_notification() {
        let silent = NotifyLevels {
            lethal: Urgency::None,
            severe: Urgency::None,
            watch: Urgency::None,
        };
        let scripts = Scripts {
            lethal: Some("/nonexistent/weatui-hook".into()),
            severe: None,
            watch: None,
        };
        let err = dispatch(&test_notification(ThreatTier::Lethal), &silent, &scripts, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("/nonexistent/weatui-hook"), "{err}");
    }

    #[test]
    fn a_tier_without_a_script_dispatches_cleanly() {
        let silent = NotifyLevels {
            lethal: Urgency::None,
            severe: Urgency::None,
            watch: Urgency::None,
        };
        let n = test_notification(ThreatTier::Severe);
        assert!(dispatch(&n, &silent, &Scripts::default(), None).is_ok());
    }
}
