//! Storm motion vectors from the CAP `eventMotionDescription` parameter.
//!
//! ```text
//! 2026-07-27T07:00:00-00:00...storm...319DEG...30KT...46.93,-91.76
//! ```
//!
//! DO NOT "correct" `heading_deg` to return `from_bearing_deg` directly.
//! Per the NWS CAP specification the degree field is the direction the storm is
//! moving **FROM**, following meteorological convention. A storm reported at
//! 319DEG is travelling toward 139. Dropping the 180 degree inversion makes the
//! tool report an inbound storm as departing, which is the single most
//! dangerous defect this program can have.

use crate::geo::{Coords, KM_PER_KNOT_HOUR, angular_difference_deg, haversine_km,
                 initial_bearing_deg};
use anyhow::{Result, bail};
use chrono::{DateTime, Duration, FixedOffset};

#[derive(Debug, Clone)]
pub struct StormMotion {
    pub observed_at: DateTime<FixedOffset>,
    pub from_bearing_deg: f64,
    pub speed_kt: f64,
    pub track: Vec<Coords>,
}

impl StormMotion {
    pub fn parse(raw: &str) -> Result<Self> {
        let segments: Vec<&str> = raw.trim().split("...").collect();
        if segments.len() < 5 {
            bail!("malformed eventMotionDescription {raw:?}: expected 5 segments");
        }

        let observed_at = DateTime::parse_from_rfc3339(segments[0].trim())
            .map_err(|e| anyhow::anyhow!("bad timestamp in {raw:?}: {e}"))?;

        let deg_field = segments[2].trim();
        let Some(deg_str) = deg_field.strip_suffix("DEG") else {
            bail!("malformed eventMotionDescription {raw:?}: {deg_field:?} lacks DEG suffix");
        };
        let from_bearing_deg: f64 = deg_str
            .parse()
            .map_err(|_| anyhow::anyhow!("bad bearing {deg_str:?} in {raw:?}"))?;
        if !(0.0..360.0).contains(&from_bearing_deg) {
            bail!("bearing {from_bearing_deg} out of range in {raw:?}");
        }

        let kt_field = segments[3].trim();
        let Some(kt_str) = kt_field.strip_suffix("KT") else {
            bail!("malformed eventMotionDescription {raw:?}: {kt_field:?} lacks KT suffix");
        };
        let speed_kt: f64 = kt_str
            .parse()
            .map_err(|_| anyhow::anyhow!("bad speed {kt_str:?} in {raw:?}"))?;
        if speed_kt < 0.0 {
            bail!("negative speed in {raw:?}");
        }

        let nums: Vec<f64> = segments[4]
            .split(',')
            .filter_map(|s| s.trim().parse::<f64>().ok())
            .collect();
        if nums.is_empty() || !nums.len().is_multiple_of(2) {
            bail!("malformed coordinate list in {raw:?}");
        }
        let track: Vec<Coords> = nums
            .chunks_exact(2)
            .map(|p| Coords { lat: p[0], lon: p[1] })
            .collect();

        Ok(StormMotion { observed_at, from_bearing_deg, speed_kt, track })
    }

    pub fn heading_deg(&self) -> f64 {
        (self.from_bearing_deg + 180.0) % 360.0
    }

    fn closest_track_point(&self, target: Coords) -> Option<Coords> {
        self.track
            .iter()
            .copied()
            .min_by(|a, b| haversine_km(*a, target).total_cmp(&haversine_km(*b, target)))
    }

    /// Along-track time to `target`, or `None` when the storm is not closing on
    /// it. Linear extrapolation of a single vector: real storms turn, so this is
    /// an estimate rather than a promise.
    pub fn eta_to(&self, target: Coords) -> Option<Duration> {
        if self.speed_kt <= 0.0 {
            return None;
        }
        let origin = self.closest_track_point(target)?;
        let distance_km = haversine_km(origin, target);
        let offset_deg = angular_difference_deg(initial_bearing_deg(origin, target), self.heading_deg());
        if offset_deg >= 90.0 {
            return None;
        }
        let along_track_km = distance_km * offset_deg.to_radians().cos();
        let hours = along_track_km / (self.speed_kt * KM_PER_KNOT_HOUR);
        Duration::try_seconds((hours * 3600.0).round() as i64)
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE_SAMPLE: &str = "2026-07-27T07:00:00-00:00...storm...319DEG...30KT...46.93,-91.76";

    #[test]
    fn a6_parses_the_live_sample_from_api_weather_gov() {
        let m = StormMotion::parse(LIVE_SAMPLE).unwrap();
        assert!((m.from_bearing_deg - 319.0).abs() < 1e-9);
        assert!((m.speed_kt - 30.0).abs() < 1e-9);
        assert_eq!(m.track.len(), 1);
        assert!((m.track[0].lat - 46.93).abs() < 1e-9);
        assert!((m.track[0].lon - (-91.76)).abs() < 1e-9);
    }

    /// The inversion that keeps the tool from inverting inbound and outbound.
    #[test]
    fn heading_is_the_reciprocal_of_the_reported_from_bearing() {
        let m = StormMotion::parse(LIVE_SAMPLE).unwrap();
        assert!((m.heading_deg() - 139.0).abs() < 1e-9);
    }

    #[test]
    fn heading_wraps_past_north() {
        let raw = "2026-07-27T07:00:00-00:00...storm...200DEG...30KT...35.0,-97.0";
        assert!((StormMotion::parse(raw).unwrap().heading_deg() - 20.0).abs() < 1e-9);
    }

    /// A7: due-north storm 100 km south of the target at 30 kt.
    /// 30 kt = 55.56 km/h, so 100 km takes 1.80 h = 108 min.
    #[test]
    fn a7_eta_matches_hand_computed_value_within_one_minute() {
        let raw = "2026-07-27T07:00:00-00:00...storm...180DEG...30KT...34.0994,-97.0";
        let m = StormMotion::parse(raw).unwrap();
        let target = Coords { lat: 35.0, lon: -97.0 };
        let eta = m.eta_to(target).expect("storm is closing");
        let minutes = eta.num_seconds() as f64 / 60.0;
        assert!((minutes - 108.0).abs() < 1.0, "expected ~108 min, got {minutes}");
    }

    #[test]
    fn storm_moving_away_yields_no_eta_rather_than_a_negative_one() {
        let raw = "2026-07-27T07:00:00-00:00...storm...000DEG...30KT...34.0994,-97.0";
        let m = StormMotion::parse(raw).unwrap();
        assert!(m.eta_to(Coords { lat: 35.0, lon: -97.0 }).is_none());
    }

    #[test]
    fn stationary_storm_has_no_eta() {
        let raw = "2026-07-27T07:00:00-00:00...storm...180DEG...0KT...34.0,-97.0";
        assert!(
            StormMotion::parse(raw)
                .unwrap()
                .eta_to(Coords { lat: 35.0, lon: -97.0 })
                .is_none()
        );
    }

    #[test]
    fn multi_point_track_lines_are_supported() {
        let raw = "2026-07-27T07:00:00-00:00...storm...319DEG...30KT...46.93,-91.76,47.10,-91.50";
        let m = StormMotion::parse(raw).unwrap();
        assert_eq!(m.track.len(), 2);
        assert!((m.track[1].lat - 47.10).abs() < 1e-9);
    }

    #[test]
    fn malformed_input_errors_and_never_panics() {
        for bad in [
            "",
            "garbage",
            "2026-07-27T07:00:00-00:00...storm...319DEG...30KT",
            "not-a-time...storm...319DEG...30KT...46.93,-91.76",
            "2026-07-27T07:00:00-00:00...storm...319...30KT...46.93,-91.76",
            "2026-07-27T07:00:00-00:00...storm...319DEG...30...46.93,-91.76",
            "2026-07-27T07:00:00-00:00...storm...999DEG...30KT...46.93,-91.76",
            "2026-07-27T07:00:00-00:00...storm...319DEG...30KT...46.93",
            "2026-07-27T07:00:00-00:00...storm...319DEG...30KT...",
        ] {
            assert!(StormMotion::parse(bad).is_err(), "expected Err for {bad:?}");
        }
    }
}
