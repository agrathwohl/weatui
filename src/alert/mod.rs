pub mod filter;
pub mod motion;
pub mod poll;
pub mod state;
pub mod vtec;

use serde::Deserialize;
use std::collections::HashMap;

pub type Ring = Vec<[f64; 2]>;

/// GeoJSON positions are `[lon, lat]` with an OPTIONAL third elevation value.
/// `Vec<[f64; 2]>` rejected the three-element form outright, and because the
/// whole response is one deserialize, a single such coordinate anywhere lost
/// every alert in the batch including a tornado warning.
fn rings_from_positions<'de, D>(d: D) -> Result<Vec<Ring>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<Vec<Vec<f64>>> = Vec::deserialize(d)?;
    Ok(raw
        .into_iter()
        .map(|ring| {
            ring.into_iter()
                .filter(|p| p.len() >= 2)
                .map(|p| [p[0], p[1]])
                .collect()
        })
        .collect())
}

fn polygons_from_positions<'de, D>(d: D) -> Result<Vec<Vec<Ring>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<Vec<Vec<Vec<f64>>>> = Vec::deserialize(d)?;
    Ok(raw
        .into_iter()
        .map(|poly| {
            poly.into_iter()
                .map(|ring| {
                    ring.into_iter()
                        .filter(|p| p.len() >= 2)
                        .map(|p| [p[0], p[1]])
                        .collect()
                })
                .collect()
        })
        .collect())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum Geometry {
    Polygon {
        #[serde(deserialize_with = "rings_from_positions")]
        coordinates: Vec<Ring>,
    },
    MultiPolygon {
        #[serde(deserialize_with = "polygons_from_positions")]
        coordinates: Vec<Vec<Ring>>,
    },
    #[serde(other)]
    Other,
}

impl Geometry {
    pub fn outer_rings(&self) -> Vec<&Ring> {
        match self {
            Geometry::Polygon { coordinates } => coordinates.iter().take(1).collect(),
            Geometry::MultiPolygon { coordinates } => {
                coordinates.iter().filter_map(|p| p.first()).collect()
            }
            Geometry::Other => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Properties {
    pub id: Option<String>,
    pub event: String,
    pub certainty: Option<String>,
    pub headline: Option<String>,
    /// The action NWS wants taken. In a tornado warning this is the most
    /// important string in the payload, so it is surfaced rather than stored.
    pub instruction: Option<String>,
    #[serde(rename = "areaDesc")]
    pub area_desc: Option<String>,
    pub expires: Option<String>,
    /// Values are arrays of mixed JSON. A non-string value must not abort the
    /// whole poll, so this stays untyped and is read via `param`.
    #[serde(default)]
    pub parameters: HashMap<String, Vec<serde_json::Value>>,
}

impl Properties {
    pub fn param(&self, key: &str) -> Vec<String> {
        self.parameters
            .get(key)
            .map(|vals| {
                vals.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn param_first(&self, key: &str) -> Option<String> {
        self.param(key).into_iter().next()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Feature {
    pub geometry: Option<Geometry>,
    pub properties: Properties,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlertCollection {
    /// Required, deliberately. A GeoJSON FeatureCollection always carries
    /// `features`, empty when there is nothing active. Defaulting it meant a
    /// 200 with any other shape (schema change, error envelope, captive
    /// portal) parsed as zero alerts, cleared all active state and counted as
    /// a successful poll. That is the one failure the staleness backstop
    /// cannot catch, because nothing failed.
    #[serde(deserialize_with = "features_skipping_malformed")]
    pub features: Vec<Feature>,
}

/// One unparseable feature used to discard every other alert in the response.
/// Losing one alert is bad; losing a tornado warning because some unrelated
/// product had a surprising shape is worse, so bad entries are skipped
/// individually. `features` itself is still required, so a wholesale schema
/// change is still an error rather than an empty sky.
fn features_skipping_malformed<'de, D>(d: D) -> Result<Vec<Feature>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<serde_json::Value> = Vec::deserialize(d)?;
    let offered = raw.len();
    let kept: Vec<Feature> = raw
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();

    // Skipping some is a repair; skipping all is a schema change wearing the
    // costume of a calm day. An empty result from a non-empty body would clear
    // every active alert and still count as a successful poll, which is the
    // one failure staleness cannot catch.
    if offered > 0 && kept.is_empty() {
        return Err(serde::de::Error::custom(format!(
            "all {offered} alert features failed to parse; refusing to treat that as an empty sky"
        )));
    }
    Ok(kept)
}

#[derive(Debug, Clone)]
pub struct Alert {
    pub properties: Properties,
    pub geometry: Option<Geometry>,
    pub vtec: Vec<vtec::VtecCode>,
    vtec_unparsed: bool,
}

impl Alert {
    pub fn from_feature(feature: Feature) -> Self {
        let raw = feature.properties.param("VTEC");
        let vtec = vtec::VtecCode::parse_all(&raw);
        Alert {
            vtec_unparsed: !raw.is_empty() && vtec.is_empty(),
            properties: feature.properties,
            geometry: feature.geometry,
            vtec,
        }
    }

    /// The product carried VTEC strings and none of them parsed. Distinct from
    /// carrying none at all: a malformed or newly-introduced code on a real
    /// warning would otherwise take the no-VTEC path and be dropped by an
    /// allowlist it can never match.
    pub fn vtec_unparsed(&self) -> bool {
        self.vtec_unparsed
    }

    /// The operative code, which is not always the first one.
    ///
    /// An upgrade product carries the CAN for the event it replaces AND the
    /// NEW for the replacement. Taking `first()` blindly meant that when the
    /// terminating line came first, the whole alert was read as a cancellation
    /// and the replacement warning was discarded. A product is only a
    /// termination when every code in it terminates.
    pub fn primary_vtec(&self) -> Option<&vtec::VtecCode> {
        self.vtec
            .iter()
            .find(|v| !v.action.terminates_event())
            .or_else(|| self.vtec.first())
    }

    /// `properties.expires` was parsed for display only, so an alert lingered
    /// until the feed stopped returning it. If the feed goes quiet mid-event,
    /// an expired warning stays on screen looking live.
    pub fn expires_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.properties
            .expires
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.to_utc())
    }

    pub fn motion(&self) -> Option<motion::StormMotion> {
        self.properties
            .param_first("eventMotionDescription")
            .and_then(|s| motion::StormMotion::parse(&s).ok())
    }

    pub fn max_wind_gust(&self) -> Option<String> {
        self.properties.param_first("maxWindGust")
    }

    pub fn max_hail_size(&self) -> Option<String> {
        self.properties.param_first("maxHailSize")
    }

    pub fn tornado_detection(&self) -> Option<String> {
        self.properties.param_first("tornadoDetection")
    }

    /// `CONSIDERABLE` and `CATASTROPHIC` mark PDS and tornado-emergency
    /// products respectively. Absent on most warnings.
    pub fn damage_threat(&self) -> Option<String> {
        self.properties.param_first("damageThreat")
    }

    /// Ray casting against the outer ring. GeoJSON stores `[lon, lat]`.
    ///
    /// A null geometry counts as covering the point. Zone-based products carry
    /// no polygon, and every alert reaching this program came back from a
    /// `?point=` query, so it covers the queried point by construction. Around
    /// a quarter of live alerts in the watched tiers have no geometry, all
    /// severe thunderstorm watches among them; returning `false` labelled them
    /// as somewhere else, which is the reassuring direction.
    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        let Some(geom) = &self.geometry else {
            return true;
        };
        geom.outer_rings()
            .into_iter()
            .any(|ring| point_in_ring(ring, lat, lon))
    }
}

pub fn point_in_ring(ring: &Ring, lat: f64, lon: f64) -> bool {
    let mut inside = false;
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (ring[i][0], ring[i][1]);
        let (xj, yj) = (ring[j][0], ring[j][1]);
        let crosses = (yi > lat) != (yj > lat);
        if crosses {
            let x_at_lat = (xj - xi) * (lat - yi) / (yj - yi) + xi;
            if lon < x_at_lat {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square() -> Ring {
        vec![
            [-98.0, 35.0],
            [-97.0, 35.0],
            [-97.0, 36.0],
            [-98.0, 36.0],
            [-98.0, 35.0],
        ]
    }

    #[test]
    fn a_three_element_position_does_not_discard_the_batch() {
        let json = r#"{"features":[
            {"geometry":{"type":"Polygon","coordinates":[[[-98.0,35.0,120.0],[-97.0,35.0,118.0],[-97.0,36.0,130.0],[-98.0,35.0,120.0]]]},
             "properties":{"event":"Tornado Warning","parameters":{}}}
        ]}"#;
        let parsed: AlertCollection = serde_json::from_str(json).expect("elevation must not be fatal");
        assert_eq!(parsed.features.len(), 1);
        let alert = Alert::from_feature(parsed.features.into_iter().next().unwrap());
        assert!(alert.contains(35.4, -97.6), "the polygon must survive with lon/lat intact");
    }

    #[test]
    fn one_malformed_feature_does_not_lose_the_tornado_warning_beside_it() {
        let json = r#"{"features":[
            {"nonsense":true},
            {"geometry":null,"properties":{"event":"Tornado Warning","parameters":{}}}
        ]}"#;
        let parsed: AlertCollection = serde_json::from_str(json).expect("batch must survive");
        assert_eq!(parsed.features.len(), 1, "the bad entry is skipped, not the good one");
        assert_eq!(parsed.features[0].properties.event, "Tornado Warning");
    }

    #[test]
    fn a_batch_where_every_feature_fails_is_an_error_not_a_calm_day() {
        let json = r#"{"features":[{"nonsense":true},{"also":"broken"}]}"#;
        assert!(
            serde_json::from_str::<AlertCollection>(json).is_err(),
            "an all-dropped batch would clear active alerts and still count as a good poll"
        );
        assert!(
            serde_json::from_str::<AlertCollection>(r#"{"features":[]}"#).is_ok(),
            "a genuinely empty sky is still fine"
        );
    }

    #[test]
    fn a_response_with_no_features_key_is_still_an_error() {
        assert!(serde_json::from_str::<AlertCollection>(r#"{"status":502}"#).is_err());
    }

    #[test]
    fn point_inside_polygon_is_detected() {
        assert!(point_in_ring(&square(), 35.5, -97.5));
    }

    #[test]
    fn points_outside_polygon_are_rejected() {
        assert!(!point_in_ring(&square(), 34.0, -97.5));
        assert!(!point_in_ring(&square(), 37.0, -97.5));
        assert!(!point_in_ring(&square(), 35.5, -99.0));
        assert!(!point_in_ring(&square(), 35.5, -96.0));
    }

    #[test]
    fn degenerate_ring_is_not_containment() {
        assert!(!point_in_ring(&vec![[-98.0, 35.0], [-97.0, 35.0]], 35.5, -97.5));
        assert!(!point_in_ring(&Vec::new(), 35.5, -97.5));
    }

    #[test]
    fn parameters_with_non_string_values_do_not_abort_parsing() {
        let json = r#"{
            "features": [{
                "geometry": null,
                "properties": {
                    "event": "Severe Thunderstorm Warning",
                    "parameters": { "weird": [1, 2, 3], "VTEC": ["/O.NEW.KDLH.SV.W.0087.260727T0700Z-260727T0800Z/"] }
                }
            }]
        }"#;
        let parsed: AlertCollection = serde_json::from_str(json).unwrap();
        let alert = Alert::from_feature(parsed.features.into_iter().next().unwrap());
        assert!(alert.properties.param("weird").is_empty());
        assert_eq!(alert.primary_vtec().unwrap().phenomenon_significance(), "SV.W");
    }

    /// Zone-based products carry no polygon and reach this program only via a
    /// `?point=` query, so they cover the user by construction. Treating them
    /// as covering nothing denied the [YOU] marker to every severe
    /// thunderstorm watch in the live feed.
    #[test]
    fn a_zone_based_alert_with_no_polygon_still_covers_the_user() {
        let json = r#"{"features":[{"geometry":null,"properties":{"event":"X","parameters":{}}}]}"#;
        let parsed: AlertCollection = serde_json::from_str(json).unwrap();
        let alert = Alert::from_feature(parsed.features.into_iter().next().unwrap());
        assert!(alert.contains(35.5, -97.5));
    }

    #[test]
    fn polygon_geometry_drives_containment() {
        let json = r#"{
            "features": [{
                "geometry": {"type":"Polygon","coordinates":[[[-98.0,35.0],[-97.0,35.0],[-97.0,36.0],[-98.0,36.0],[-98.0,35.0]]]},
                "properties": {"event":"Severe Thunderstorm Warning","parameters":{}}
            }]
        }"#;
        let parsed: AlertCollection = serde_json::from_str(json).unwrap();
        let alert = Alert::from_feature(parsed.features.into_iter().next().unwrap());
        assert!(alert.contains(35.5, -97.5));
        assert!(!alert.contains(40.0, -97.5));
    }
}
