mod alert;
mod conditions;
mod counties;
mod config;
mod daemon;
mod geo;
mod notify;
mod radar;
mod render;
mod tui;

use anyhow::Result;

const HELP: &str = "\
weatui - NWS severe weather alerting and radar

USAGE:
    weatui           interactive monitor
    weatui -d        daemon mode, notifications only
    weatui -h        this message

CONFIG:
    ~/.config/weatui/config.toml

        [location]
        zip = \"73019\"
";

#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    Interactive,
    Daemon,
    Help,
}

pub fn parse_mode<I: AsRef<str>>(args: &[I]) -> Mode {
    let has = |want: &[&str]| args.iter().any(|a| want.contains(&a.as_ref()));
    if has(&["-h", "--help"]) {
        Mode::Help
    } else if has(&["-d", "--daemon"]) {
        Mode::Daemon
    } else {
        Mode::Interactive
    }
}

fn resolve_home(cfg: &config::Config) -> Result<geo::Coords> {
    if let Some((lat, lon)) = cfg.location.explicit_coords() {
        return Ok(geo::Coords { lat, lon });
    }
    let zip = cfg
        .location
        .zip
        .as_deref()
        .expect("config validation guarantees zip or explicit coordinates");
    geo::coords_for_zip(zip)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = parse_mode(&args);

    if mode == Mode::Help {
        print!("{HELP}");
        return Ok(());
    }

    let cfg = config::Config::load()?;
    let home = resolve_home(&cfg)?;

    notify::set_hazard_link(home);

    let script_problems = notify::preflight_scripts(&cfg.alerts.scripts);
    for problem in &script_problems {
        eprintln!("weatui: {problem}");
    }
    // Checked per tier, not globally. A tier silenced to "none" has its script
    // as the ONLY channel, and a global check passes as long as some OTHER
    // tier still has the desktop daemon, which leaves the silenced tier with
    // nothing. Starting then presents as a working monitor with a hole in it.
    let dead = notify::tiers_with_no_working_channel(&cfg.alerts.notify, &cfg.alerts.scripts);
    if !dead.is_empty() {
        anyhow::bail!(
            "the {} tier(s) are silenced and their [alerts.scripts] entries cannot run, \
             so those alerts have no delivery channel at all. Refusing to start as a \
             monitor that cannot warn you",
            dead.join(", ")
        );
    }

    if cfg.alerts.notify.uses_desktop_daemon() {
        match notify::preflight() {
            Ok(b) if mode == Mode::Daemon => println!("weatui: notifying via {}", b.label()),
            Ok(_) => {}
            Err(e) => eprintln!("weatui: {e:#}"),
        }
    }

    if mode == Mode::Daemon {
        daemon::run(cfg, home, true).await
    } else {
        tui::run(cfg, home).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a11_short_and_long_daemon_flags_both_select_daemon_mode() {
        assert_eq!(parse_mode(&["-d"]), Mode::Daemon);
        assert_eq!(parse_mode(&["--daemon"]), Mode::Daemon);
    }

    #[test]
    fn no_arguments_is_interactive() {
        let empty: [&str; 0] = [];
        assert_eq!(parse_mode(&empty), Mode::Interactive);
    }

    #[test]
    fn help_wins_over_daemon() {
        assert_eq!(parse_mode(&["-d", "--help"]), Mode::Help);
    }

    #[test]
    fn unknown_arguments_do_not_change_the_mode() {
        assert_eq!(parse_mode(&["--nonsense"]), Mode::Interactive);
    }

    #[test]
    fn explicit_coordinates_bypass_zip_lookup() {
        let cfg = config::Config::parse(
            "[location]\nlat = 41.8781\nlon = -87.6298\n",
            "/tmp/c.toml",
        )
        .unwrap();
        let home = resolve_home(&cfg).unwrap();
        assert!((home.lat - 41.8781).abs() < 1e-9);
    }

    #[test]
    fn zip_config_resolves_through_the_embedded_census_table() {
        let cfg = config::Config::parse("[location]\nzip = \"73019\"\n", "/tmp/c.toml").unwrap();
        let home = resolve_home(&cfg).unwrap();
        assert!((home.lat - 35.205661).abs() < 1e-6);
    }
}
