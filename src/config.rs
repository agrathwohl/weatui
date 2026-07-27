//! TOML rather than YAML: `serde_yaml` is published as `0.9.34+deprecated` with
//! no canonical successor, while `toml` is maintained alongside Cargo itself.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::PathBuf;

pub fn config_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => {
            let home = std::env::var_os("HOME")
                .context("neither XDG_CONFIG_HOME nor HOME is set, cannot locate config")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("weatui").join("config.toml"))
}

pub fn cache_dir() -> Result<PathBuf> {
    Ok(config_path()?
        .parent()
        .expect("config path always has a parent")
        .to_path_buf())
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub location: Location,
    #[serde(default)]
    pub alerts: Alerts,
    #[serde(default)]
    pub radar: Radar,
    #[serde(default)]
    pub render: Render,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Location {
    pub zip: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

impl Location {
    /// Explicit coordinates win over ZIP; ZIP requires a network lookup.
    pub fn explicit_coords(&self) -> Option<(f64, f64)> {
        match (self.lat, self.lon) {
            (Some(lat), Some(lon)) => Some((lat, lon)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Alerts {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Products with no P-VTEC that should still be accepted. Special Weather
    /// Statement is the motivating case: no VTEC, but it carries
    /// `eventMotionDescription`, `maxWindGust` and `maxHailSize`.
    #[serde(default = "default_extra_events")]
    pub extra_events: Vec<String>,
    #[serde(default)]
    pub tiers: Tiers,
    #[serde(default)]
    pub notify: NotifyLevels,
    /// Emit a critical notification when no poll has succeeded for this long.
    /// Silence must never be indistinguishable from safety.
    #[serde(default = "default_stale_after")]
    pub stale_after_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tiers {
    #[serde(default = "default_lethal")]
    pub lethal: Vec<String>,
    #[serde(default = "default_severe")]
    pub severe: Vec<String>,
    #[serde(default = "default_watch")]
    pub watch: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

impl Urgency {
    pub fn as_notify_send_arg(self) -> &'static str {
        match self {
            Urgency::Low => "low",
            Urgency::Normal => "normal",
            Urgency::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NotifyLevels {
    #[serde(default = "default_critical")]
    pub lethal: Urgency,
    #[serde(default = "default_critical")]
    pub severe: Urgency,
    #[serde(default = "default_normal")]
    pub watch: Urgency,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Radar {
    /// `"auto"` selects the nearest WSR-88D, otherwise an ICAO site id.
    #[serde(default = "default_site")]
    pub site: String,
    #[serde(default = "default_frames")]
    pub frames: usize,
    #[serde(default = "default_refresh")]
    pub refresh_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Colormap {
    Threat,
    Nws,
    Mono,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Render {
    #[serde(default = "default_colormap")]
    pub colormap: Colormap,
}

fn default_poll_interval() -> u64 {
    // api.weather.gov advertises `cache-control: max-age=4`.
    5
}
fn default_stale_after() -> u64 {
    300
}
fn default_extra_events() -> Vec<String> {
    vec!["Special Weather Statement".to_string()]
}
fn default_lethal() -> Vec<String> {
    ["TO.W", "EW.W", "FF.W"].iter().map(|s| s.to_string()).collect()
}
fn default_severe() -> Vec<String> {
    ["SV.W", "SQ.W", "DS.W"].iter().map(|s| s.to_string()).collect()
}
fn default_watch() -> Vec<String> {
    ["TO.A", "SV.A"].iter().map(|s| s.to_string()).collect()
}
fn default_critical() -> Urgency {
    Urgency::Critical
}
fn default_normal() -> Urgency {
    Urgency::Normal
}
fn default_site() -> String {
    "auto".to_string()
}
fn default_frames() -> usize {
    12
}
fn default_refresh() -> u64 {
    60
}
fn default_colormap() -> Colormap {
    Colormap::Threat
}

impl Default for Alerts {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval(),
            extra_events: default_extra_events(),
            tiers: Tiers::default(),
            notify: NotifyLevels::default(),
            stale_after_secs: default_stale_after(),
        }
    }
}

impl Default for Tiers {
    fn default() -> Self {
        Self {
            lethal: default_lethal(),
            severe: default_severe(),
            watch: default_watch(),
        }
    }
}

impl Default for NotifyLevels {
    fn default() -> Self {
        Self {
            lethal: default_critical(),
            severe: default_critical(),
            watch: default_normal(),
        }
    }
}

impl Default for Radar {
    fn default() -> Self {
        Self {
            site: default_site(),
            frames: default_frames(),
            refresh_secs: default_refresh(),
        }
    }
}

impl Default for Render {
    fn default() -> Self {
        Self {
            colormap: default_colormap(),
        }
    }
}

impl Config {
    pub fn parse(text: &str, path_for_errors: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text)
            .with_context(|| format!("failed to parse config at {path_for_errors}"))?;
        cfg.validate(path_for_errors)?;
        Ok(cfg)
    }

    pub fn load() -> Result<Self> {
        let path = config_path()?;
        let shown = path.display().to_string();
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "no config file at {shown}\n\
                 create it with at least:\n\n\
                 [location]\n\
                 zip = \"73019\"\n"
            )
        })?;
        Config::parse(&text, &shown)
    }

    /// A14: an unusable location must be an actionable error, never a panic.
    fn validate(&self, path_for_errors: &str) -> Result<()> {
        let path = path_for_errors;
        if self.location.explicit_coords().is_none() && self.location.zip.is_none() {
            bail!(
                "{path}: [location] must set either `zip` or both `lat` and `lon`\n\n\
                 [location]\n\
                 zip = \"73019\"\n\n\
                 or\n\n\
                 [location]\n\
                 lat = 35.2226\n\
                 lon = -97.4395"
            );
        }
        if self.location.explicit_coords().is_none()
            && (self.location.lat.is_some() || self.location.lon.is_some())
        {
            bail!("{path}: [location] has only one of `lat`/`lon`; set both or use `zip`");
        }
        if self.alerts.poll_interval_secs == 0 {
            bail!("{path}: alerts.poll_interval_secs must be greater than zero");
        }
        if self.radar.frames == 0 {
            bail!("{path}: radar.frames must be greater than zero");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_only_is_valid_and_defaults_apply() {
        let cfg = Config::parse("[location]\nzip = \"73019\"\n", "/tmp/config.toml").unwrap();
        assert_eq!(cfg.location.zip.as_deref(), Some("73019"));
        assert_eq!(cfg.alerts.poll_interval_secs, 5);
        assert_eq!(cfg.radar.frames, 12);
        assert_eq!(cfg.radar.refresh_secs, 60);
        assert_eq!(cfg.render.colormap, Colormap::Threat);
        assert!(cfg.alerts.tiers.lethal.iter().any(|c| c == "TO.W"));
        assert_eq!(cfg.alerts.notify.lethal, Urgency::Critical);
        assert_eq!(cfg.alerts.notify.watch, Urgency::Normal);
    }

    #[test]
    fn explicit_coords_win_over_zip_lookup() {
        let cfg = Config::parse(
            "[location]\nlat = 35.2226\nlon = -97.4395\n",
            "/tmp/config.toml",
        )
        .unwrap();
        assert_eq!(cfg.location.explicit_coords(), Some((35.2226, -97.4395)));
    }

    #[test]
    fn a14_missing_location_yields_error_naming_config_path_not_panic() {
        let err = Config::parse("[alerts]\npoll_interval_secs = 5\n", "/home/x/config.toml")
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("/home/x/config.toml"), "got: {msg}");
    }

    #[test]
    fn half_specified_coords_are_rejected() {
        let err = Config::parse("[location]\nlat = 35.2226\n", "/home/x/config.toml").unwrap_err();
        assert!(format!("{err:#}").contains("lat"));
    }

    #[test]
    fn zero_poll_interval_is_rejected() {
        let err = Config::parse(
            "[location]\nzip = \"73019\"\n\n[alerts]\npoll_interval_secs = 0\n",
            "/home/x/config.toml",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("poll_interval_secs"));
    }

    #[test]
    fn urgency_maps_to_notify_send_argument() {
        assert_eq!(Urgency::Critical.as_notify_send_arg(), "critical");
        assert_eq!(Urgency::Normal.as_notify_send_arg(), "normal");
    }
}
