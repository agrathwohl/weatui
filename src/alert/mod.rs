pub mod filter;
pub mod motion;
pub mod poll;
pub mod state;
pub mod vtec;

use serde::Deserialize;
use std::collections::HashMap;

pub type Ring = Vec<[f64; 2]>;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum Geometry {
    Polygon { coordinates: Vec<Ring> },
    MultiPolygon { coordinates: Vec<Vec<Ring>> },
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
    pub severity: Option<String>,
    pub urgency: Option<String>,
    pub certainty: Option<String>,
    pub response: Option<String>,
    pub headline: Option<String>,
    pub description: Option<String>,
    pub instruction: Option<String>,
    #[serde(rename = "areaDesc")]
    pub area_desc: Option<String>,
    pub onset: Option<String>,
    pub expires: Option<String>,
    pub ends: Option<String>,
    pub sent: Option<String>,
    /// Values are arrays of mixed JSON. A non-string value must not abort the
    /// whole poll, so this is deliberately untyped and read via `param`.
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
    #[serde(default)]
    pub features: Vec<Feature>,
}

#[derive(Debug, Clone)]
pub struct Alert {
    pub properties: Properties,
    pub geometry: Option<Geometry>,
    pub vtec: Vec<vtec::VtecCode>,
}

impl Alert {
    pub fn from_feature(feature: Feature) -> Self {
        let vtec = vtec::VtecCode::parse_all(&feature.properties.param("VTEC"));
        Alert {
            properties: feature.properties,
            geometry: feature.geometry,
            vtec,
        }
    }

    pub fn primary_vtec(&self) -> Option<&vtec::VtecCode> {
        self.vtec.first()
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
    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        let Some(geom) = &self.geometry else {
            return false;
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

    #[test]
    fn alert_without_geometry_contains_nothing() {
        let json = r#"{"features":[{"geometry":null,"properties":{"event":"X","parameters":{}}}]}"#;
        let parsed: AlertCollection = serde_json::from_str(json).unwrap();
        let alert = Alert::from_feature(parsed.features.into_iter().next().unwrap());
        assert!(!alert.contains(35.5, -97.5));
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
