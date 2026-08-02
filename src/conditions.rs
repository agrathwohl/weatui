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
        wind_dir: obs.wind_direction.unwrap_or_default().degrees().map(crate::geo::compass_16),
        visibility_mi: obs.visibility.unwrap_or_default().miles(),
        rain_last_hour_in: obs.precipitation_last_hour.unwrap_or_default().inches(),
    }
}

pub struct PointsUrls {
    pub observation_stations: String,
    pub forecast_hourly: String,
}

pub async fn points_urls(client: &reqwest::Client, home: Coords) -> Result<PointsUrls> {
    #[derive(Deserialize)]
    struct Points {
        properties: PointsProperties,
    }
    #[derive(Deserialize)]
    struct PointsProperties {
        #[serde(rename = "observationStations")]
        observation_stations: String,
        #[serde(rename = "forecastHourly")]
        forecast_hourly: String,
    }

    let url = format!("https://api.weather.gov/points/{:.4},{:.4}", home.lat, home.lon);
    let points: Points = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("point metadata fetch failed for {url}"))?
        .error_for_status()?
        .json()
        .await
        .context("points response was not the expected JSON")?;
    for u in [&points.properties.observation_stations, &points.properties.forecast_hourly] {
        if !u.starts_with("https://api.weather.gov/") {
            bail!("refusing to follow an off-host URL from the points response: {u}");
        }
    }
    Ok(PointsUrls {
        observation_stations: points.properties.observation_stations,
        forecast_hourly: points.properties.forecast_hourly,
    })
}

/// The station list is ordered nearest-first by the API. Several are kept so
/// a defunct nearest station can be abandoned for the next one rather than
/// leaving the panel empty forever.
pub async fn nearest_stations(client: &reqwest::Client, stations_url: &str) -> Result<Vec<String>> {
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
        bail!("NWS lists no observation stations at {stations_url}");
    }
    Ok(ids)
}

/// One hour of the official NWS (NBM) point forecast, for labelling forecast
/// frames with the surface conditions predicted at that frame's valid time.
#[derive(Debug, Clone)]
pub struct HourlyForecast {
    pub valid: DateTime<Utc>,
    pub temp_f: Option<f32>,
    pub dewpoint_f: Option<f32>,
    pub humidity_pct: Option<f32>,
    pub wind_mph: Option<f32>,
    pub wind_dir: Option<String>,
    pub short: Option<String>,
}

#[derive(Deserialize)]
struct HourlyEnvelope {
    properties: HourlyProperties,
}
#[derive(Deserialize)]
struct HourlyProperties {
    periods: Vec<Period>,
}
#[derive(Deserialize)]
struct Period {
    #[serde(rename = "startTime")]
    start_time: String,
    temperature: Option<f64>,
    #[serde(rename = "temperatureUnit", default)]
    temperature_unit: String,
    #[serde(default)]
    dewpoint: Option<Quantity>,
    #[serde(rename = "relativeHumidity", default)]
    relative_humidity: Option<Quantity>,
    /// Prose like "5 mph" or "5 to 10 mph"; the leading number is the value.
    #[serde(rename = "windSpeed")]
    wind_speed: Option<String>,
    #[serde(rename = "windDirection")]
    wind_direction: Option<String>,
    #[serde(rename = "shortForecast")]
    short_forecast: Option<String>,
}

fn hourly_from(p: Period) -> Option<HourlyForecast> {
    let valid = DateTime::parse_from_rfc3339(&p.start_time).ok()?.to_utc();
    Some(HourlyForecast {
        valid,
        temp_f: p.temperature.map(|t| match p.temperature_unit.as_str() {
            "C" => (t * 9.0 / 5.0 + 32.0) as f32,
            _ => t as f32,
        }),
        dewpoint_f: p.dewpoint.unwrap_or_default().fahrenheit(),
        humidity_pct: p.relative_humidity.unwrap_or_default().percent(),
        wind_mph: p
            .wind_speed
            .as_deref()
            .and_then(|w| w.split_whitespace().next())
            .and_then(|n| n.parse().ok()),
        wind_dir: p.wind_direction.filter(|d| !d.is_empty()),
        short: p.short_forecast.filter(|d| !d.is_empty()),
    })
}

pub async fn hourly_forecast(
    client: &reqwest::Client,
    forecast_hourly_url: &str,
) -> Result<Vec<HourlyForecast>> {
    let envelope: HourlyEnvelope = client
        .get(forecast_hourly_url)
        .send()
        .await
        .context("hourly forecast fetch failed")?
        .error_for_status()?
        .json()
        .await
        .context("hourly forecast was not the expected JSON")?;
    Ok(envelope.properties.periods.into_iter().filter_map(hourly_from).collect())
}

/// Recent past observations from the station, so scrubbing back through
/// observed frames can show what it was actually like at that moment.
pub async fn observation_history(
    client: &reqwest::Client,
    station: &str,
) -> Result<Vec<Conditions>> {
    #[derive(Deserialize)]
    struct History {
        features: Vec<ObsEnvelopeFeature>,
    }
    #[derive(Deserialize)]
    struct ObsEnvelopeFeature {
        properties: ObsProperties,
    }
    let url = format!("https://api.weather.gov/stations/{station}/observations?limit=36");
    let history: History = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("observation history fetch failed for {station}"))?
        .error_for_status()?
        .json()
        .await
        .context("observation history was not the expected JSON")?;
    let mut out: Vec<Conditions> = history
        .features
        .into_iter()
        .map(|f| conditions_from(station.to_string(), f.properties))
        .filter(|c| c.observed_at.is_some())
        .collect();
    out.sort_by_key(|c| c.observed_at);
    Ok(out)
}

/// The forecast is hourly while frames are quarter-hourly, so the nearest
/// hour within tolerance stands in for the frame's moment.
const HOURLY_TOLERANCE_MIN: i64 = 65;
/// Observations land hourly plus specials; beyond this the reading belongs to
/// different weather than the frame shows.
const OBSERVATION_TOLERANCE_MIN: i64 = 45;

pub fn nearest_hourly(list: &[HourlyForecast], t: DateTime<Utc>) -> Option<&HourlyForecast> {
    list.iter()
        .min_by_key(|h| (h.valid - t).num_minutes().abs())
        .filter(|h| (h.valid - t).num_minutes().abs() <= HOURLY_TOLERANCE_MIN)
}

pub fn nearest_observation(list: &[Conditions], t: DateTime<Utc>) -> Option<&Conditions> {
    list.iter()
        .filter_map(|c| c.observed_at.map(|at| (c, (at - t).num_minutes().abs())))
        .min_by_key(|(_, d)| *d)
        .filter(|(_, d)| *d <= OBSERVATION_TOLERANCE_MIN)
        .map(|(c, _)| c)
}

/// What the HUD shows for a frame that is not "now": the model's prediction
/// for a future frame, or the station's actual reading for a past one.
pub enum FrameConditions<'a> {
    Forecast(&'a HourlyForecast),
    Observed(&'a Conditions),
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
        assert_eq!(crate::geo::compass_16(0.0), "N");
        assert_eq!(crate::geo::compass_16(22.5), "NNE");
        assert_eq!(crate::geo::compass_16(90.0), "E");
        assert_eq!(crate::geo::compass_16(225.0), "SW");
        assert_eq!(crate::geo::compass_16(340.0), "NNW");
        assert_eq!(crate::geo::compass_16(359.9), "N");
        assert_eq!(crate::geo::compass_16(-90.0), "W");
    }

    #[test]
    fn hourly_periods_parse_including_prose_wind_and_celsius() {
        let json = r#"{"properties":{"periods":[
            {"startTime":"2026-08-02T00:00:00-05:00","temperature":69,"temperatureUnit":"F",
             "dewpoint":{"unitCode":"wmoUnit:degC","value":20.0},
             "relativeHumidity":{"unitCode":"wmoUnit:percent","value":100},
             "windSpeed":"5 to 10 mph","windDirection":"WSW","shortForecast":"Partly Cloudy"},
            {"startTime":"2026-08-02T01:00:00-05:00","temperature":20,"temperatureUnit":"C",
             "windSpeed":null,"windDirection":"","shortForecast":""}
        ]}}"#;
        let envelope: HourlyEnvelope = serde_json::from_str(json).unwrap();
        let hours: Vec<HourlyForecast> =
            envelope.properties.periods.into_iter().filter_map(hourly_from).collect();
        assert_eq!(hours.len(), 2);
        assert_eq!(hours[0].temp_f, Some(69.0));
        assert_eq!(hours[0].dewpoint_f, Some(68.0));
        assert_eq!(hours[0].humidity_pct, Some(100.0));
        assert_eq!(hours[0].wind_mph, Some(5.0), "prose ranges take the leading number");
        assert_eq!(hours[0].wind_dir.as_deref(), Some("WSW"));
        assert_eq!(hours[0].short.as_deref(), Some("Partly Cloudy"));
        assert_eq!(hours[1].temp_f, Some(68.0), "celsius must convert");
        assert_eq!(hours[1].wind_mph, None);
        assert_eq!(hours[1].short, None, "an empty forecast string is not a forecast");
    }

    #[test]
    fn nearest_hourly_tolerates_the_quarter_hour_offset_but_not_a_gap() {
        let base = DateTime::parse_from_rfc3339("2026-08-02T06:00:00Z").unwrap().to_utc();
        let hour = |off: i64| HourlyForecast {
            valid: base + chrono::Duration::minutes(off),
            temp_f: Some(70.0),
            dewpoint_f: None,
            humidity_pct: None,
            wind_mph: None,
            wind_dir: None,
            short: None,
        };
        let list = [hour(0), hour(60)];
        let at = |off: i64| base + chrono::Duration::minutes(off);
        assert!(nearest_hourly(&list, at(45)).is_some());
        assert_eq!(
            nearest_hourly(&list, at(45)).unwrap().valid,
            list[1].valid,
            "45 past picks the closer hour"
        );
        assert!(nearest_hourly(&list, at(120)).is_some(), "60 min past the last hour is within tolerance");
        assert!(nearest_hourly(&list, at(180)).is_none(), "two hours past the data is a gap");
    }

    #[test]
    fn nearest_observation_refuses_readings_from_different_weather() {
        let base = DateTime::parse_from_rfc3339("2026-08-02T06:00:00Z").unwrap().to_utc();
        let obs = |off: i64| {
            let mut c = parse(
                r#"{"properties":{"temperature":{"unitCode":"wmoUnit:degC","value":21.0}}}"#,
            );
            c.observed_at = Some(base + chrono::Duration::minutes(off));
            c
        };
        let list = [obs(0), obs(53)];
        assert!(nearest_observation(&list, base + chrono::Duration::minutes(30)).is_some());
        assert!(
            nearest_observation(&list, base + chrono::Duration::minutes(120)).is_none(),
            "an hour-old reading does not describe this frame"
        );
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
        let urls = points_urls(&client, home).await.expect("points urls");
        let stations = nearest_stations(&client, &urls.observation_stations)
            .await
            .expect("stations");
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
