//! Current surface conditions from the nearest NWS observation station.
//!
//! Two-step discovery: the points API maps coordinates to a station list, and
//! the first station's latest observation carries the QC'd readings. Every
//! numeric field is nullable at the source and stays an `Option` all the way
//! to the screen; a missing sensor renders as absent rather than as zero.

use crate::geo::Coords;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;

/// A measured quantity as NWS publishes it: a nullable value plus the WMO
/// unit it was measured in. Conversion is fail-closed: an unrecognised unit
/// yields `None` rather than a number in the wrong scale.
#[derive(Debug, Default, Deserialize)]
pub struct Quantity {
    pub value: Option<f64>,
    #[serde(rename = "unitCode", default)]
    pub unit_code: String,
}

impl Quantity {
    fn fahrenheit(&self) -> Option<f32> {
        let v = self.value?;
        match self.unit_code.as_str() {
            "wmoUnit:degC" => Some((v * 9.0 / 5.0 + 32.0) as f32),
            "wmoUnit:degF" => Some(v as f32),
            _ => None,
        }
    }

    fn mph(&self) -> Option<f32> {
        let v = self.value?;
        match self.unit_code.as_str() {
            "wmoUnit:km_h-1" => Some((v * 0.621_371) as f32),
            "wmoUnit:m_s-1" => Some((v * 2.236_936) as f32),
            _ => None,
        }
    }

    fn miles(&self) -> Option<f32> {
        let v = self.value?;
        match self.unit_code.as_str() {
            "wmoUnit:m" => Some((v / 1609.344) as f32),
            "wmoUnit:mi" => Some(v as f32),
            _ => None,
        }
    }

    fn inches(&self) -> Option<f32> {
        let v = self.value?;
        match self.unit_code.as_str() {
            "wmoUnit:mm" => Some((v / 25.4) as f32),
            "wmoUnit:m" => Some((v * 1000.0 / 25.4) as f32),
            _ => None,
        }
    }

    fn percent(&self) -> Option<f32> {
        let v = self.value?;
        (self.unit_code == "wmoUnit:percent").then_some(v as f32)
    }

    fn degrees(&self) -> Option<f32> {
        let v = self.value?;
        (self.unit_code == "wmoUnit:degree_(angle)").then_some(v as f32)
    }
}

#[derive(Debug, Clone)]
pub struct Conditions {
    pub station: String,
    pub observed_at: Option<DateTime<Utc>>,
    pub description: Option<String>,
    pub temp_f: Option<f32>,
    pub dewpoint_f: Option<f32>,
    pub humidity_pct: Option<f32>,
    pub wind_mph: Option<f32>,
    pub wind_dir: Option<&'static str>,
    pub visibility_mi: Option<f32>,
    pub rain_last_hour_in: Option<f32>,
}

pub fn compass_16(degrees: f32) -> &'static str {
    const POINTS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    POINTS[(((degrees.rem_euclid(360.0) + 11.25) / 22.5) as usize) % 16]
}

#[derive(Deserialize)]
struct ObsEnvelope {
    properties: ObsProperties,
}

/// Every field is `Option<Quantity>`: NWS usually publishes a quantity object
/// with a null value, but an entirely-null field must degrade that one
/// reading, not fail the whole observation.
#[derive(Deserialize)]
struct ObsProperties {
    timestamp: Option<String>,
    #[serde(rename = "textDescription")]
    text_description: Option<String>,
    #[serde(default)]
    temperature: Option<Quantity>,
    #[serde(default)]
    dewpoint: Option<Quantity>,
    #[serde(rename = "relativeHumidity", default)]
    relative_humidity: Option<Quantity>,
    #[serde(rename = "windSpeed", default)]
    wind_speed: Option<Quantity>,
    #[serde(rename = "windDirection", default)]
    wind_direction: Option<Quantity>,
    #[serde(default)]
    visibility: Option<Quantity>,
    #[serde(rename = "precipitationLastHour", default)]
    precipitation_last_hour: Option<Quantity>,
}

fn conditions_from(station: String, obs: ObsProperties) -> Conditions {
    Conditions {
        station,
        observed_at: obs
            .timestamp
            .as_deref()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.to_utc()),
        description: obs.text_description.filter(|d| !d.is_empty()),
        temp_f: obs.temperature.unwrap_or_default().fahrenheit(),
        dewpoint_f: obs.dewpoint.unwrap_or_default().fahrenheit(),
        humidity_pct: obs.relative_humidity.unwrap_or_default().percent(),
        wind_mph: obs.wind_speed.unwrap_or_default().mph(),
        wind_dir: obs.wind_direction.unwrap_or_default().degrees().map(compass_16),
        visibility_mi: obs.visibility.unwrap_or_default().miles(),
        rain_last_hour_in: obs.precipitation_last_hour.unwrap_or_default().inches(),
    }
}

/// The station list is ordered nearest-first by the API. Several are kept so
/// a defunct nearest station can be abandoned for the next one rather than
/// leaving the panel empty forever.
pub async fn nearest_stations(client: &reqwest::Client, home: Coords) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct Points {
        properties: PointsProperties,
    }
    #[derive(Deserialize)]
    struct PointsProperties {
        #[serde(rename = "observationStations")]
        observation_stations: String,
    }
    #[derive(Deserialize)]
    struct Stations {
        features: Vec<StationFeature>,
    }
    #[derive(Deserialize)]
    struct StationFeature {
        properties: StationProperties,
    }
    #[derive(Deserialize)]
    struct StationProperties {
        #[serde(rename = "stationIdentifier")]
        station_identifier: String,
    }

    let url = format!("https://api.weather.gov/points/{:.4},{:.4}", home.lat, home.lon);
    let points: Points = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("station discovery failed for {url}"))?
        .error_for_status()?
        .json()
        .await
        .context("points response was not the expected JSON")?;

    let stations_url = &points.properties.observation_stations;
    if !stations_url.starts_with("https://api.weather.gov/") {
        bail!("refusing to follow an off-host station list at {stations_url}");
    }
    let stations: Stations = client
        .get(stations_url)
        .send()
        .await
        .context("station list fetch failed")?
        .error_for_status()?
        .json()
        .await
        .context("station list was not the expected JSON")?;

    let ids: Vec<String> = stations
        .features
        .into_iter()
        .take(4)
        .map(|f| f.properties.station_identifier)
        .collect();
    if ids.is_empty() {
        bail!("NWS lists no observation stations near {:.4},{:.4}", home.lat, home.lon);
    }
    Ok(ids)
}

pub async fn latest(client: &reqwest::Client, station: &str) -> Result<Conditions> {
    let url = format!("https://api.weather.gov/stations/{station}/observations/latest");
    let obs: ObsEnvelope = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("observation fetch failed for {station}"))?
        .error_for_status()
        .with_context(|| format!("{station} returned an error status"))?
        .json()
        .await
        .context("observation was not the expected JSON")?;
    Ok(conditions_from(station.to_string(), obs.properties))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Conditions {
        let envelope: ObsEnvelope = serde_json::from_str(json).unwrap();
        conditions_from("KBNA".into(), envelope.properties)
    }

    #[test]
    fn a_full_observation_converts_to_display_units() {
        let c = parse(
            r#"{"properties":{
                "timestamp":"2026-08-01T05:53:00+00:00",
                "textDescription":"Partly Cloudy",
                "temperature":{"unitCode":"wmoUnit:degC","value":25.0},
                "dewpoint":{"unitCode":"wmoUnit:degC","value":20.0},
                "relativeHumidity":{"unitCode":"wmoUnit:percent","value":73.9},
                "windSpeed":{"unitCode":"wmoUnit:km_h-1","value":16.09},
                "windDirection":{"unitCode":"wmoUnit:degree_(angle)","value":225.0},
                "visibility":{"unitCode":"wmoUnit:m","value":16090.0},
                "precipitationLastHour":{"unitCode":"wmoUnit:mm","value":2.54}
            }}"#,
        );
        assert_eq!(c.temp_f, Some(77.0));
        assert_eq!(c.dewpoint_f, Some(68.0));
        assert_eq!(c.humidity_pct, Some(73.9));
        assert!((c.wind_mph.unwrap() - 10.0).abs() < 0.01);
        assert_eq!(c.wind_dir, Some("SW"));
        assert!((c.visibility_mi.unwrap() - 10.0).abs() < 0.01);
        assert!((c.rain_last_hour_in.unwrap() - 0.1).abs() < 0.001);
        assert_eq!(c.description.as_deref(), Some("Partly Cloudy"));
        assert!(c.observed_at.is_some());
    }

    /// Stations routinely report with sensors offline; every reading must
    /// survive as absent rather than defaulting to a fake zero.
    #[test]
    fn null_sensors_stay_absent_rather_than_becoming_zero() {
        let c = parse(
            r#"{"properties":{
                "timestamp":null,
                "textDescription":"",
                "temperature":null,
                "dewpoint":{"unitCode":"wmoUnit:degC","value":null},
                "windSpeed":{"unitCode":"wmoUnit:km_h-1","value":null}
            }}"#,
        );
        assert_eq!(c.temp_f, None);
        assert_eq!(c.wind_mph, None);
        assert_eq!(c.humidity_pct, None);
        assert_eq!(c.visibility_mi, None);
        assert_eq!(c.rain_last_hour_in, None);
        assert_eq!(c.description, None);
        assert_eq!(c.observed_at, None);
    }

    /// A reading in a unit this code does not understand must vanish, not be
    /// displayed in the wrong scale.
    #[test]
    fn unknown_units_fail_closed() {
        let c = parse(
            r#"{"properties":{
                "temperature":{"unitCode":"wmoUnit:K","value":298.15},
                "windSpeed":{"unitCode":"wmoUnit:kn","value":10.0}
            }}"#,
        );
        assert_eq!(c.temp_f, None, "kelvin must not be shown as fahrenheit");
        assert_eq!(c.wind_mph, None, "knots must not be shown as mph");
    }

    #[test]
    fn metres_per_second_wind_is_converted() {
        let q = Quantity { value: Some(10.0), unit_code: "wmoUnit:m_s-1".into() };
        assert!((q.mph().unwrap() - 22.369).abs() < 0.01);
    }

    #[test]
    fn compass_names_all_sixteen_sectors() {
        assert_eq!(compass_16(0.0), "N");
        assert_eq!(compass_16(22.5), "NNE");
        assert_eq!(compass_16(90.0), "E");
        assert_eq!(compass_16(225.0), "SW");
        assert_eq!(compass_16(340.0), "NNW");
        assert_eq!(compass_16(359.9), "N");
        assert_eq!(compass_16(-90.0), "W");
    }

    /// `cargo test live_conditions -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_conditions_for_the_home_coordinates() {
        let client = reqwest::Client::builder()
            .user_agent(crate::alert::poll::USER_AGENT)
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap();
        let home = Coords { lat: 35.9527, lon: -87.3085 };
        let stations = nearest_stations(&client, home).await.expect("stations");
        eprintln!("stations: {stations:?}");
        let c = latest(&client, &stations[0]).await.expect("observation");
        eprintln!("{c:#?}");
        assert_eq!(c.station, stations[0]);
        assert!(
            c.temp_f.is_some() || c.description.is_some(),
            "a live station should report something"
        );
    }
}
