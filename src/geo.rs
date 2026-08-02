//! Geodesy and US location lookup.
//!
//! Both tables are embedded rather than fetched at runtime. A tool that exists
//! to warn about severe weather must resolve a location while the network is
//! degraded, which is precisely when severe weather is happening.
//!
//! - ZCTA centroids: US Census 2023 national gazetteer, GEOID/INTPTLAT/INTPTLONG.
//! - WSR-88D sites: api.weather.gov/radar/stations?stationType=WSR-88D.

use anyhow::{Context, Result, bail};
use chrono_tz::Tz;
use std::sync::OnceLock;

const ZCTA_CSV: &str = include_str!("data/zcta_centroids.csv");
const WSR88D_CSV: &str = include_str!("data/wsr88d_sites.csv");

const EARTH_RADIUS_KM: f64 = 6371.0088;
pub const KM_PER_KNOT_HOUR: f64 = 1.852;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coords {
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct RadarSite {
    pub id: &'static str,
    pub coords: Coords,
}

fn parse_table(csv: &'static str) -> Vec<(&'static str, Coords)> {
    csv.lines()
        .filter_map(|line| {
            let mut parts = line.split(',');
            let key = parts.next()?.trim();
            let lat: f64 = parts.next()?.trim().parse().ok()?;
            let lon: f64 = parts.next()?.trim().parse().ok()?;
            if key.is_empty() {
                return None;
            }
            Some((key, Coords { lat, lon }))
        })
        .collect()
}

fn zcta_table() -> &'static Vec<(&'static str, Coords)> {
    static TABLE: OnceLock<Vec<(&'static str, Coords)>> = OnceLock::new();
    TABLE.get_or_init(|| parse_table(ZCTA_CSV))
}

pub(crate) fn radar_table() -> &'static Vec<(&'static str, Coords)> {
    static TABLE: OnceLock<Vec<(&'static str, Coords)>> = OnceLock::new();
    TABLE.get_or_init(|| parse_table(WSR88D_CSV))
}

pub fn coords_for_zip(zip: &str) -> Result<Coords> {
    let zip = zip.trim();
    if zip.len() != 5 || !zip.bytes().all(|b| b.is_ascii_digit()) {
        bail!("{zip:?} is not a five digit US ZIP code");
    }
    let table = zcta_table();
    match table.binary_search_by(|(k, _)| (*k).cmp(zip)) {
        Ok(i) => Ok(table[i].1),
        Err(_) => bail!(
            "ZIP {zip} has no Census ZCTA centroid; \
             set lat and lon explicitly in the config instead"
        ),
    }
}

/// Great-circle distance. Haversine is stable for the short separations that
/// matter here, where the spherical law of cosines loses precision.
pub fn haversine_km(a: Coords, b: Coords) -> f64 {
    let (lat1, lat2) = (a.lat.to_radians(), b.lat.to_radians());
    let dlat = (b.lat - a.lat).to_radians();
    let dlon = (b.lon - a.lon).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * h.sqrt().asin()
}

/// Forward azimuth from `a` to `b`, degrees clockwise from true north.
pub fn initial_bearing_deg(a: Coords, b: Coords) -> f64 {
    let (lat1, lat2) = (a.lat.to_radians(), b.lat.to_radians());
    let dlon = (b.lon - a.lon).to_radians();
    let y = dlon.sin() * lat2.cos();
    let x = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * dlon.cos();
    (y.atan2(x).to_degrees() + 360.0) % 360.0
}

/// Smallest absolute separation between two compass bearings, 0..=180.
pub fn angular_difference_deg(a: f64, b: f64) -> f64 {
    let d = ((a - b) % 360.0 + 360.0) % 360.0;
    if d > 180.0 { 360.0 - d } else { d }
}

pub fn compass_16(degrees: f32) -> &'static str {
    const POINTS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    POINTS[(((degrees.rem_euclid(360.0) + 11.25) / 22.5) as usize) % 16]
}

/// Compass name of the direction from one point toward another. The zonal
/// term shrinks with latitude or east-west directions would overweight.
pub fn compass_bearing(from: Coords, to: Coords) -> &'static str {
    let dlon = (to.lon - from.lon) * from.lat.to_radians().cos();
    let dlat = to.lat - from.lat;
    compass_16(dlon.atan2(dlat).to_degrees() as f32)
}

pub fn nearest_radar_site(from: Coords) -> Option<RadarSite> {
    radar_table()
        .iter()
        .map(|(id, c)| (RadarSite { id, coords: *c }, haversine_km(from, *c)))
        .min_by(|x, y| x.1.total_cmp(&y.1))
        .map(|(site, _)| site)
}

/// The IANA zone NWS assigns to these coordinates.
///
/// Displayed times must follow the configured location rather than the machine
/// or the issuing office. An alert for Tennessee can be published by an office
/// on Mountain time, and its `expires` carries that office's offset, so the
/// only coherent basis is the zone of the place being watched.
pub async fn timezone_for(at: Coords) -> Result<Tz> {
    #[derive(serde::Deserialize)]
    struct Points {
        properties: Properties,
    }
    #[derive(serde::Deserialize)]
    struct Properties {
        #[serde(rename = "timeZone")]
        time_zone: String,
    }

    let url = format!(
        "https://api.weather.gov/points/{:.4},{:.4}",
        at.lat, at.lon
    );
    let points: Points = reqwest::Client::builder()
        .user_agent(crate::alert::poll::USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("failed to build HTTP client for the timezone lookup")?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("timezone lookup failed for {url}"))?
        .error_for_status()
        .context("timezone lookup returned an error status")?
        .json()
        .await
        .context("timezone lookup returned unexpected JSON")?;

    points
        .properties
        .time_zone
        .parse()
        .map_err(|_| anyhow::anyhow!("{:?} is not a known IANA zone", points.properties.time_zone))
}

pub fn radar_site_by_id(id: &str) -> Option<RadarSite> {
    let want = id.trim().to_ascii_uppercase();
    radar_table()
        .iter()
        .find(|(k, _)| *k == want)
        .map(|(k, c)| RadarSite { id: k, coords: *c })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NORMAN: Coords = Coords { lat: 35.205661, lon: -97.442738 };

    #[test]
    fn embedded_tables_parse_completely() {
        assert_eq!(zcta_table().len(), 33791);
        assert_eq!(radar_table().len(), 159);
    }

    #[test]
    fn zip_lookup_matches_census_gazetteer() {
        let c = coords_for_zip("73019").unwrap();
        assert!((c.lat - 35.205661).abs() < 1e-6);
        assert!((c.lon - (-97.442738)).abs() < 1e-6);
    }

    #[test]
    fn malformed_zip_is_an_error_not_a_panic() {
        for bad in ["", "7301", "730190", "abcde", "7301a"] {
            assert!(coords_for_zip(bad).is_err(), "expected Err for {bad:?}");
        }
    }

    #[test]
    fn unassigned_zip_suggests_explicit_coordinates() {
        let err = coords_for_zip("00000").unwrap_err().to_string();
        assert!(err.contains("lat"), "got: {err}");
    }

    /// One degree of arc on a sphere of radius R is pi*R/180. For R = 6371.0088
    /// that is 111.195 km, which is exact rather than remembered.
    #[test]
    fn haversine_matches_analytic_one_degree_arc() {
        let expected = std::f64::consts::PI * EARTH_RADIUS_KM / 180.0;
        let along_meridian = haversine_km(
            Coords { lat: 0.0, lon: 0.0 },
            Coords { lat: 1.0, lon: 0.0 },
        );
        let along_equator = haversine_km(
            Coords { lat: 0.0, lon: 0.0 },
            Coords { lat: 0.0, lon: 1.0 },
        );
        assert!((along_meridian - expected).abs() < 1e-6, "got {along_meridian}");
        assert!((along_equator - expected).abs() < 1e-6, "got {along_equator}");
    }

    #[test]
    fn haversine_of_antipodal_points_is_half_the_circumference() {
        let d = haversine_km(
            Coords { lat: 0.0, lon: 0.0 },
            Coords { lat: 0.0, lon: 180.0 },
        );
        assert!((d - std::f64::consts::PI * EARTH_RADIUS_KM).abs() < 1e-6, "got {d}");
    }

    /// Cross-check against the equirectangular approximation, which is accurate
    /// to well under a percent at continental scale.
    #[test]
    fn haversine_agrees_with_equirectangular_estimate_for_okc_to_nyc() {
        let okc = Coords { lat: 35.4676, lon: -97.5164 };
        let nyc = Coords { lat: 40.7128, lon: -74.0060 };
        let deg_km = std::f64::consts::PI * EARTH_RADIUS_KM / 180.0;
        let mean_lat = ((okc.lat + nyc.lat) / 2.0).to_radians();
        let dx = (nyc.lon - okc.lon) * deg_km * mean_lat.cos();
        let dy = (nyc.lat - okc.lat) * deg_km;
        let approx = (dx * dx + dy * dy).sqrt();
        let d = haversine_km(okc, nyc);
        assert!((d - approx).abs() / approx < 0.01, "haversine {d} vs approx {approx}");
    }

    #[test]
    fn haversine_of_identical_points_is_zero() {
        assert!(haversine_km(NORMAN, NORMAN) < 1e-9);
    }

    #[test]
    fn bearing_due_north_and_east_are_correct() {
        let origin = Coords { lat: 0.0, lon: 0.0 };
        let north = Coords { lat: 1.0, lon: 0.0 };
        let east = Coords { lat: 0.0, lon: 1.0 };
        assert!(initial_bearing_deg(origin, north).abs() < 0.01);
        assert!((initial_bearing_deg(origin, east) - 90.0).abs() < 0.01);
    }

    #[test]
    fn angular_difference_wraps_across_north() {
        assert!((angular_difference_deg(350.0, 10.0) - 20.0).abs() < 1e-9);
        assert!((angular_difference_deg(10.0, 350.0) - 20.0).abs() < 1e-9);
        assert!((angular_difference_deg(0.0, 180.0) - 180.0).abs() < 1e-9);
    }

    #[test]
    fn nearest_radar_to_norman_is_the_oklahoma_city_site() {
        assert_eq!(nearest_radar_site(NORMAN).unwrap().id, "KTLX");
    }

    #[test]
    fn radar_site_lookup_is_case_insensitive() {
        assert_eq!(radar_site_by_id("ktlx").unwrap().id, "KTLX");
        assert!(radar_site_by_id("ZZZZ").is_none());
    }
}
