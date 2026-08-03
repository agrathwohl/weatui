//! County borders from api.weather.gov zone geometry.
//!
//! The zone list for a state carries no geometry, so each county is fetched
//! once and the rings are cached on disk. County borders do not change, so
//! the cache has no expiry.

use crate::alert::Ring;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CountyMap {
    pub state: String,
    pub rings: Vec<Ring>,
}

fn cache_path(state: &str) -> Result<std::path::PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME").context("neither XDG_CACHE_HOME nor HOME set")?;
            std::path::PathBuf::from(home).join(".cache")
        }
    };
    Ok(base.join("weatui").join(format!("counties-{state}.json")))
}

/// State code from a county zone URL like
/// `https://api.weather.gov/zones/county/TNC081`.
pub fn state_of(county_zone_url: &str) -> Result<&str> {
    let id = county_zone_url.rsplit('/').next().unwrap_or_default();
    if id.len() < 2 || !id.chars().take(2).all(|c| c.is_ascii_uppercase()) {
        bail!("{county_zone_url:?} does not end in a county zone id");
    }
    Ok(&id[..2])
}

pub async fn load(client: &reqwest::Client, county_zone_url: &str) -> Result<CountyMap> {
    let state = state_of(county_zone_url)?;
    let cache = cache_path(state)?;
    if let Ok(bytes) = std::fs::read(&cache)
        && let Ok(map) = serde_json::from_slice::<CountyMap>(&bytes)
    {
        return Ok(map);
    }

    #[derive(Deserialize)]
    struct ZoneList {
        features: Vec<ZoneRef>,
    }
    #[derive(Deserialize)]
    struct ZoneRef {
        properties: ZoneProps,
    }
    #[derive(Deserialize)]
    struct ZoneProps {
        id: String,
    }
    #[derive(Deserialize)]
    struct ZoneDetail {
        geometry: Option<crate::alert::Geometry>,
    }

    let list: ZoneList = client
        .get(format!("https://api.weather.gov/zones?type=county&area={state}"))
        .send()
        .await
        .with_context(|| format!("county list fetch failed for {state}"))?
        .error_for_status()?
        .json()
        .await
        .context("county list was not the expected JSON")?;

    let mut rings: Vec<Ring> = Vec::new();
    for zone in &list.features {
        let detail: ZoneDetail = client
            .get(format!("https://api.weather.gov/zones/county/{}", zone.properties.id))
            .send()
            .await
            .with_context(|| format!("county {} fetch failed", zone.properties.id))?
            .error_for_status()?
            .json()
            .await
            .with_context(|| format!("county {} was not the expected JSON", zone.properties.id))?;
        if let Some(geometry) = &detail.geometry {
            rings.extend(geometry.outer_rings().into_iter().cloned());
        }
    }
    if rings.is_empty() {
        bail!("{state} returned no county geometry");
    }

    let map = CountyMap { state: state.to_string(), rings };
    if let Some(dir) = cache.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec(&map) {
        let _ = std::fs::write(&cache, bytes);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_comes_from_the_zone_id_not_the_url_path() {
        assert_eq!(state_of("https://api.weather.gov/zones/county/TNC081").unwrap(), "TN");
        assert_eq!(state_of("https://api.weather.gov/zones/county/OKC027").unwrap(), "OK");
        assert!(state_of("https://api.weather.gov/zones/county/").is_err());
        assert!(state_of("https://example.com/x/9081").is_err());
    }

    /// `cargo test live_counties -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_counties_fetch_and_cache() {
        let client = reqwest::Client::builder()
            .user_agent(crate::alert::poll::USER_AGENT)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        let map = load(&client, "https://api.weather.gov/zones/county/TNC081")
            .await
            .expect("county map");
        eprintln!("{} rings for {}", map.rings.len(), map.state);
        assert_eq!(map.state, "TN");
        assert!(map.rings.len() >= 90, "TN has 95 counties, got {}", map.rings.len());
    }
}
