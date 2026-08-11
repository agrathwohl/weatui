//! Desktop notification via `notify-send`.
//!
//! A shell-out rather than a D-Bus client crate: mako is already
//! running and already styles `[urgency=critical]`, so `notify-send -u` maps
//! straight onto configuration the user has written. Notification bodies stay
//! plain ASCII because the daemon's font may lack Nerd Font glyphs.

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

/// A silent visual toast does not wake a sleeping person, and waking someone is
/// the entire point of the critical tier. The freedesktop sound name is a hint:
/// daemons that do not implement it ignore it rather than failing.
const CRITICAL_SOUND: &str = "string:sound-name:alarm-clock-elapsed";

pub fn build_args(summary: &str, body: &str, urgency: Urgency) -> Vec<String> {
    let mut args = vec![
        "-u".to_string(),
        urgency.as_notify_send_arg().to_string(),
        "-a".to_string(),
        "weatui".to_string(),
    ];
    if urgency == Urgency::Critical {
        args.push("-h".to_string());
        args.push(CRITICAL_SOUND.to_string());
    }
    args.push(summary.to_string());
    args.push(body.to_string());
    args
}

/// `notify-send` missing is not discoverable at alert time: the first symptom
/// is a warning that never arrives. Checked once at startup instead.
pub fn preflight() -> Result<()> {
    let found = Command::new("notify-send")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match found {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => anyhow::bail!("notify-send exited with {s}; desktop alerts will not be delivered"),
        Err(e) => Err(e).context(
            "notify-send not found; desktop alerts will not be delivered. \
             Install libnotify, or configure [alerts.scripts] instead",
        ),
    }
}

pub fn summary_for(n: &Notification) -> String {
    format!("[{}] {}", n.tier.label(), n.event)
}

/// `CATASTROPHIC` is the tornado-emergency tag and `CONSIDERABLE` marks a PDS.
/// Both must read as themselves in the body: they are the difference between a
/// routine radar-indicated warning and the worst product NWS issues.
pub fn damage_threat_banner(threat: &str) -> String {
    match threat.to_ascii_uppercase().as_str() {
        "CATASTROPHIC" => "TORNADO EMERGENCY".to_string(),
        "CONSIDERABLE" => "PARTICULARLY DANGEROUS SITUATION".to_string(),
        other => format!("Damage threat: {other}"),
    }
}

pub fn body_for(n: &Notification, eta_minutes: Option<i64>) -> String {
    let mut parts = Vec::new();
    if let Some(t) = &n.damage_threat {
        parts.push(damage_threat_banner(t));
    }
    if let Some(d) = &n.tornado_detection {
        parts.push(format!("Tornado {}", d.to_lowercase()));
    }
    if let Some(h) = &n.headline {
        parts.push(h.clone());
    }
    if let Some(a) = &n.area {
        parts.push(a.clone());
    }
    if let Some(m) = eta_minutes {
        parts.push(format!("Estimated arrival in {m} min"));
    }
    if let Some(i) = &n.instruction {
        parts.push(i.clone());
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

/// Fans out to the desktop daemon (unless the tier's level is `"none"`) and
/// to the tier's script. The two fail independently.
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
pub fn stale_notification(elapsed_secs: u64) -> Notification {
    let minutes = elapsed_secs / 60;
    Notification {
        key: "weatui.feed.stale".to_string(),
        tier: ThreatTier::Lethal,
        event: "ALERT FEED STALE".to_string(),
        headline: Some(format!(
            "No successful poll of api.weather.gov for {minutes} min."
        )),
        area: None,
        instruction: Some(
            "You are NOT being warned about severe weather right now.".to_string(),
        ),
        damage_threat: None,
        tornado_detection: None,
    }
}

/// Fans out exactly like [`dispatch`], but pinned to critical and ignoring the
/// configured levels. Routing it through the lethal script matters: the
/// documented `notify = "none"` plus `[alerts.scripts]` setup would otherwise
/// get no staleness signal at all, because the one message meaning "this system
/// is broken" would go solely to the channel the user turned off.
pub fn send_stale_warning(elapsed_secs: u64, scripts: &Scripts) -> Result<()> {
    let n = stale_notification(elapsed_secs);
    let notified = run(&build_args(
        "[weatui] ALERT FEED STALE",
        &body_for(&n, None),
        Urgency::Critical,
    ));
    let scripted = match scripts.for_tier(ThreatTier::Lethal) {
        Some(script) => run_script(script, &n, None),
        None => Ok(()),
    };
    match (notified, scripted) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(a), Err(b)) => Err(anyhow::anyhow!("{a:#}; {b:#}")),
        (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
    }
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
            instruction: None,
            damage_threat: None,
            tornado_detection: None,
        }
    }

    fn notification(tier: ThreatTier, event: &str) -> Notification {
        Notification {
            key: "KTLX.TO.W.12".to_string(),
            tier,
            event: event.to_string(),
            headline: Some("Tornado Warning until 3:15 PM CDT".to_string()),
            area: Some("Cleveland, OK".to_string()),
            instruction: None,
            damage_threat: None,
            tornado_detection: None,
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
    fn a_tornado_emergency_does_not_read_like_a_routine_warning() {
        let routine = notification(ThreatTier::Lethal, "Tornado Warning");
        let mut emergency = routine.clone();
        emergency.damage_threat = Some("CATASTROPHIC".to_string());

        let a = body_for(&routine, Some(12));
        let b = body_for(&emergency, Some(12));
        assert_ne!(a, b, "an emergency must not notify identically to a warning");
        assert!(b.contains("TORNADO EMERGENCY"), "got: {b}");
    }

    #[test]
    fn a_pds_and_an_observed_tornado_reach_the_notification_body() {
        let mut n = notification(ThreatTier::Lethal, "Tornado Warning");
        n.damage_threat = Some("CONSIDERABLE".to_string());
        n.tornado_detection = Some("OBSERVED".to_string());
        let b = body_for(&n, None);
        assert!(b.contains("PARTICULARLY DANGEROUS SITUATION"), "got: {b}");
        assert!(b.contains("Tornado observed"), "got: {b}");
    }

    #[test]
    fn the_shelter_instruction_reaches_the_notification_body() {
        let mut n = notification(ThreatTier::Lethal, "Tornado Warning");
        n.instruction = Some("TAKE COVER NOW! Move to a basement.".to_string());
        assert!(body_for(&n, None).contains("TAKE COVER NOW"));
    }

    #[test]
    fn the_critical_tier_asks_the_daemon_for_a_sound() {
        let critical = build_args("s", "b", Urgency::Critical).join(" ");
        assert!(critical.contains("sound-name"), "got: {critical}");
        let normal = build_args("s", "b", Urgency::Normal).join(" ");
        assert!(!normal.contains("sound-name"), "got: {normal}");
    }

    #[test]
    fn a_stale_feed_reaches_the_tier_script_not_just_the_desktop() {
        let dir = std::env::temp_dir().join(format!("weatui-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("hook.sh");
        let out = dir.join("fired");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s %s' \"$1\" \"$WEATUI_EVENT\" > {}\n",
                out.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let scripts = Scripts {
            lethal: Some(script.to_string_lossy().into_owned()),
            severe: None,
            watch: None,
        };
        let _ = send_stale_warning(420, &scripts);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !out.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "LETHAL ALERT FEED STALE",
            "the documented notify=none + scripts setup must still learn the feed died"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn body_falls_back_to_the_event_name_when_nothing_else_is_present() {
        let n = Notification {
            key: "k".to_string(),
            tier: ThreatTier::Severe,
            event: "Special Weather Statement".to_string(),
            headline: None,
            area: None,
            instruction: None,
            damage_threat: None,
            tornado_detection: None,
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

    /// A broken script leaves the notification intact and fails loudly.
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
