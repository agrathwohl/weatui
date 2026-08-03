//! NOAA HRRR forecast reflectivity.
//!
//! HRRR is a 3 km convection-allowing model that assimilates radar every 15
//! minutes. Its `REFC` field is the model's forecast of composite
//! reflectivity: it can grow, decay, and initiate storms.
//!
//! Each `wrfsubhf{HH}` file holds four REFC records at 15, 30, 45 and 60
//! minutes past forecast hour HH. The `.idx` sidecar gives byte offsets, so a
//! single forecast step is one ranged request of a couple of hundred kilobytes
//! rather than the whole multi-megabyte file.

use crate::geo::Coords;
use crate::radar::{RadarField, RadarProduct};
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, Timelike, Utc};
use grib::{GridDefinitionTemplateValues, GridPointIndex};
use std::f64::consts::PI;

const BUCKET: &str = "https://noaa-hrrr-bdp-pds.s3.amazonaws.com";

/// GRIB2 code table 3.2 shape 6: sphere of this radius. HRRR uses it, and
/// substituting a WGS84 ellipsoid here would shift the grid by kilometres.
const EARTH_RADIUS_M: f64 = 6_371_229.0;

/// Lambert Conformal Conic, secant or tangent, on a sphere.
///
/// Only the forward projection is needed: a screen pixel already has a
/// latitude and longitude, and the question is which grid cell holds it.
#[derive(Debug, Clone)]
pub struct LambertGrid {
    ni: usize,
    nj: usize,
    n: f64,
    big_f: f64,
    rho0: f64,
    lov_rad: f64,
    x0: f64,
    y0: f64,
    dx: f64,
    dy: f64,
}

fn cone_constant(latin1: f64, latin2: f64) -> f64 {
    if (latin1 - latin2).abs() < 1e-9 {
        latin1.sin()
    } else {
        let num = (latin1.cos() / latin2.cos()).ln();
        let den = ((PI / 4.0 + latin2 / 2.0).tan() / (PI / 4.0 + latin1 / 2.0).tan()).ln();
        num / den
    }
}

/// Grid definition template 3.30 in real units, named so that the six
/// same-typed angles cannot be transposed at a call site.
#[derive(Debug, Clone, Copy)]
pub struct LambertParams {
    pub ni: usize,
    pub nj: usize,
    pub first: Coords,
    pub lad_deg: f64,
    pub lov_deg: f64,
    pub latin1_deg: f64,
    pub latin2_deg: f64,
    pub dx_m: f64,
    pub dy_m: f64,
}

impl LambertGrid {
    pub fn new(p: LambertParams) -> Result<Self> {
        let LambertParams {
            ni,
            nj,
            first,
            lad_deg,
            lov_deg,
            latin1_deg,
            latin2_deg,
            dx_m,
            dy_m,
        } = p;
        if ni == 0 || nj == 0 || dx_m <= 0.0 || dy_m <= 0.0 {
            bail!("degenerate Lambert grid {ni}x{nj} at {dx_m}x{dy_m} m");
        }
        let (latin1, latin2) = (latin1_deg.to_radians(), latin2_deg.to_radians());
        let n = cone_constant(latin1, latin2);
        if n.abs() < 1e-12 {
            bail!("Lambert cone constant collapsed to zero");
        }
        let big_f = latin1.cos() * (PI / 4.0 + latin1 / 2.0).tan().powf(n) / n;
        let rho0 = EARTH_RADIUS_M * big_f
            / (PI / 4.0 + lad_deg.to_radians() / 2.0).tan().powf(n);

        let mut grid = LambertGrid {
            ni,
            nj,
            n,
            big_f,
            rho0,
            lov_rad: lov_deg.to_radians(),
            x0: 0.0,
            y0: 0.0,
            dx: dx_m,
            dy: dy_m,
        };
        let (x0, y0) = grid.project(first);
        grid.x0 = x0;
        grid.y0 = y0;
        Ok(grid)
    }

    fn project(&self, at: Coords) -> (f64, f64) {
        let phi = at.lat.to_radians();
        let rho = EARTH_RADIUS_M * self.big_f / (PI / 4.0 + phi / 2.0).tan().powf(self.n);
        let mut dlon = at.lon.to_radians() - self.lov_rad;
        while dlon > PI {
            dlon -= 2.0 * PI;
        }
        while dlon < -PI {
            dlon += 2.0 * PI;
        }
        let theta = self.n * dlon;
        (rho * theta.sin(), self.rho0 - rho * theta.cos())
    }

    /// Row-major index, `j` slowest. `None` outside the grid, which is how
    /// everywhere beyond the CONUS domain reports having no forecast.
    pub fn index_of(&self, at: Coords) -> Option<usize> {
        if !at.lat.is_finite() || !at.lon.is_finite() || at.lat.abs() >= 90.0 {
            return None;
        }
        let (x, y) = self.project(at);
        let i = ((x - self.x0) / self.dx).round();
        let j = ((y - self.y0) / self.dy).round();
        if i < 0.0 || j < 0.0 || i >= self.ni as f64 || j >= self.nj as f64 {
            return None;
        }
        Some(j as usize * self.ni + i as usize)
    }

    pub fn dimensions(&self) -> (usize, usize) {
        (self.ni, self.nj)
    }
}

/// Every HRRR field the app can render, with the `.idx` selector that locates
/// it. All three live in the same `wrfsubh` file already being range-requested,
/// so adding one costs a second ranged GET rather than another download.
pub const FORECASTABLE: &[(RadarProduct, &str)] = &[
    (RadarProduct::Reflectivity, "REFC:entire atmosphere"),
    (RadarProduct::EchoTop, "RETOP:cloud top"),
    (RadarProduct::VerticallyIntegratedLiquid, "VIL:entire atmosphere"),
];

pub struct HrrrField {
    grid: LambertGrid,
    layers: Vec<(RadarProduct, Vec<f32>)>,
    label: String,
    pub valid_at: DateTime<Utc>,
    pub lead_minutes: i64,
}

impl HrrrField {
    pub fn from_layers(
        grid: LambertGrid,
        layers: Vec<(RadarProduct, Vec<f32>)>,
        valid_at: DateTime<Utc>,
        lead_minutes: i64,
    ) -> Self {
        HrrrField {
            grid,
            layers,
            label: format!("HRRR +{lead_minutes}min"),
            valid_at,
            lead_minutes,
        }
    }

    pub fn decode_layer(bytes: &[u8]) -> Result<(LambertGrid, Vec<f32>)> {
        let reader = std::io::Cursor::new(bytes);
        let grib2 = grib::from_reader(reader).context("REFC record is not readable GRIB2")?;
        let (_index, submessage) = grib2
            .iter()
            .next()
            .context("REFC record contained no submessage")?;

        let template = GridDefinitionTemplateValues::try_from(submessage.grid_def())
            .map_err(|e| anyhow!("unreadable grid definition: {e}"))?;
        let scan = *template.scanning_mode();
        let GridDefinitionTemplateValues::Template30(t) = template else {
            bail!("expected Lambert Conformal (template 3.30) for HRRR");
        };

        // `index_of` assumes row-major storage, west-to-east then
        // south-to-north. Any other scanning mode would decode cleanly and
        // draw every echo in the wrong place.
        if !scan.scans_positively_for_i()
            || !scan.scans_positively_for_j()
            || !scan.is_consecutive_for_i()
            || scan.scans_alternating_rows()
        {
            bail!(
                "HRRR changed its GRIB scanning mode (+i={} +j={} consecutive_i={} alternating={}); \
                 the row-major indexing here would place echo in the wrong location",
                scan.scans_positively_for_i(),
                scan.scans_positively_for_j(),
                scan.is_consecutive_for_i(),
                scan.scans_alternating_rows(),
            );
        }

        let first_point = Coords {
            lat: t.first_point_lat as f64 * 1e-6,
            lon: t.first_point_lon as f64 * 1e-6,
        };
        let grid = LambertGrid::new(LambertParams {
            ni: t.ni as usize,
            nj: t.nj as usize,
            first: first_point,
            lad_deg: t.lad as f64 * 1e-6,
            lov_deg: t.lov as f64 * 1e-6,
            latin1_deg: t.latin1 as f64 * 1e-6,
            latin2_deg: t.latin2 as f64 * 1e-6,
            dx_m: t.dx as f64 * 1e-3,
            dy_m: t.dy as f64 * 1e-3,
        })?;

        let decoder = grib::Grib2SubmessageDecoder::from(submessage)
            .map_err(|e| anyhow!("could not build a REFC decoder: {e}"))?;
        let values: Vec<f32> = decoder
            .dispatch()
            .map_err(|e| anyhow!("REFC decode failed: {e}"))?
            .collect();

        let (ni, nj) = grid.dimensions();
        if values.len() != ni * nj {
            bail!("REFC has {} values for a {ni}x{nj} grid", values.len());
        }

        Ok((grid, values))
    }
}

impl RadarField for HrrrField {
    fn supports(&self, product: RadarProduct) -> bool {
        self.layers.iter().any(|(p, _)| *p == product)
    }

    fn value_at(&self, at: Coords, product: RadarProduct) -> Option<f32> {
        let values = self
            .layers
            .iter()
            .find(|(p, _)| *p == product)
            .map(|(_, v)| v)?;
        let raw = *values.get(self.grid.index_of(at)?)?;
        if !raw.is_finite() {
            return None;
        }
        Some(match product {
            // RETOP is published in metres above sea level.
            RadarProduct::EchoTop => raw / 1000.0,
            _ => raw,
        })
    }

    fn source_label(&self) -> &str {
        &self.label
    }

    fn elevation_degrees(&self) -> f32 {
        0.0
    }
}

/// A forecast step, identified solely by its lead time in minutes.
///
/// Both the containing file and the index label derive from the lead, which
/// removes the two off-by-ones this originally shipped with. `wrfsubhf00` is
/// the analysis and holds no forecast, so files are one-based: hour HH carries
/// leads (HH-1)*60 + 15, 30, 45 and 60. The index labels those by their
/// absolute lead, so hour 2 reads "75 min fcst" rather than "15 min fcst".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ForecastStep {
    lead_minutes: u16,
}

impl ForecastStep {
    pub fn new(lead_minutes: u16) -> Self {
        ForecastStep { lead_minutes }
    }

    pub fn lead_minutes(&self) -> i64 {
        self.lead_minutes as i64
    }

    pub fn file_hour(&self) -> u8 {
        self.lead_minutes.div_ceil(60).max(1) as u8
    }
}

/// HRRR's subhourly product runs to forecast hour 18 and no further.
pub const MAX_LEAD_MINUTES: u16 = 1080;

/// Steps at `interval` covering the leads in `(after, until]`.
///
/// `after` is how old the cycle already is. HRRR publishes a cycle one to two
/// hours after its valid time, so leads are anchored to the cycle while the
/// user's horizon is anchored to now; generating from lead zero yields a
/// window mostly in the past, which is then discarded and leaves a couple of
/// frames where the horizon promised hours.
pub fn leads_every(interval: u16, after: u16, until: u16) -> Vec<ForecastStep> {
    let first = (after / interval + 1) * interval;
    (first..=until.min(MAX_LEAD_MINUTES))
        .step_by(interval as usize)
        .map(ForecastStep::new)
        .collect()
}

fn subh_stem(cycle: DateTime<Utc>, step: ForecastStep) -> String {
    format!(
        "hrrr.{}/conus/hrrr.t{:02}z.wrfsubhf{:02}.grib2",
        cycle.format("%Y%m%d"),
        cycle.hour(),
        step.file_hour()
    )
}

/// Byte range of the REFC record for `step`, read from the `.idx` sidecar.
///
/// The index gives each record's start offset but not its length, so a
/// record ends where the next one begins. The final record has no successor
/// and is fetched open-ended.
async fn record_byte_range(
    client: &reqwest::Client,
    stem: &str,
    lead_minutes: i64,
    selector: &str,
) -> Result<(u64, Option<u64>)> {
    let idx = client
        .get(format!("{BUCKET}/{stem}.idx"))
        .send()
        .await
        .with_context(|| format!("could not fetch {stem}.idx"))?
        .error_for_status()
        .with_context(|| format!("{stem}.idx is not available yet"))?
        .text()
        .await
        .context("index was not text")?;

    let wanted = format!(":{selector}:{lead_minutes} min fcst:");
    let lines: Vec<&str> = idx.lines().collect();
    let position = lines
        .iter()
        .position(|l| l.contains(&wanted))
        .with_context(|| format!("{stem}.idx has no {selector} at {lead_minutes} min"))?;

    let offset_of = |line: &str| -> Option<u64> { line.split(':').nth(1)?.parse().ok() };
    let start = offset_of(lines[position])
        .with_context(|| format!("malformed index line: {}", lines[position]))?;
    let end = lines.get(position + 1).and_then(|l| offset_of(l));
    Ok((start, end))
}

pub async fn fetch_step(
    client: &reqwest::Client,
    cycle: DateTime<Utc>,
    step: ForecastStep,
) -> Result<HrrrField> {
    let stem = subh_stem(cycle, step);
    let lead = step.lead_minutes();

    let mut grid: Option<LambertGrid> = None;
    let mut layers = Vec::new();
    for (product, selector) in FORECASTABLE {
        match fetch_layer(client, &stem, lead, selector).await {
            Ok((g, values)) => {
                grid.get_or_insert(g);
                layers.push((*product, values));
            }
            // REFC is the frame; without it there is nothing to show and the
            // caller must know. A missing RETOP or VIL record only narrows the
            // product list, and killing the whole frame over it would blank
            // the timeline for a layer most frames never display.
            Err(e) if *product == RadarProduct::Reflectivity => return Err(e),
            Err(_) => {}
        }
    }

    let grid = grid.context("no HRRR layers were configured")?;
    Ok(HrrrField::from_layers(grid, layers, cycle + Duration::minutes(lead), lead))
}

async fn fetch_layer(
    client: &reqwest::Client,
    stem: &str,
    lead: i64,
    selector: &str,
) -> Result<(LambertGrid, Vec<f32>)> {
    let (start, end) = record_byte_range(client, stem, lead, selector).await?;
    let range = match end {
        Some(e) => format!("bytes={start}-{}", e.saturating_sub(1)),
        None => format!("bytes={start}-"),
    };
    let bytes = client
        .get(format!("{BUCKET}/{stem}"))
        .header(reqwest::header::RANGE, range)
        .send()
        .await
        .with_context(|| format!("could not fetch {selector} from {stem}"))?
        .error_for_status()
        .with_context(|| format!("{selector} range request failed"))?
        .bytes()
        .await
        .with_context(|| format!("{selector} body was truncated"))?;
    HrrrField::decode_layer(&bytes)
}

/// Most recent cycle whose index is published. HRRR runs hourly but lands
/// roughly 50 to 90 minutes late, so the current hour is usually absent.
pub async fn latest_cycle(client: &reqwest::Client, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let base = now
        .with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .context("could not truncate the clock to an hour")?;

    for back in 1..=6 {
        let cycle = base - Duration::hours(back);
        let stem = subh_stem(cycle, ForecastStep::new(15));
        if let Ok(resp) = client.get(format!("{BUCKET}/{stem}.idx")).send().await
            && resp.status().is_success()
        {
            return Ok(cycle);
        }
    }
    bail!("no HRRR cycle published in the last six hours")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parameters read from a live HRRR REFC record rather than assumed.
    fn hrrr_grid() -> LambertGrid {
        LambertGrid::new(hrrr_params()).unwrap()
    }

    fn hrrr_params() -> LambertParams {
        LambertParams {
            ni: 1799,
            nj: 1059,
            first: Coords { lat: 21.138123, lon: 237.280472 },
            lad_deg: 38.5,
            lov_deg: 262.5,
            latin1_deg: 38.5,
            latin2_deg: 38.5,
            dx_m: 3000.0,
            dy_m: 3000.0,
        }
    }

    #[test]
    fn the_first_grid_point_lands_on_index_zero() {
        let g = hrrr_grid();
        assert_eq!(g.index_of(Coords { lat: 21.138123, lon: 237.280472 }), Some(0));
        assert_eq!(g.index_of(Coords { lat: 21.138123, lon: -122.719528 }), Some(0));
    }

    #[test]
    fn a_known_conus_location_lands_inside_the_grid() {
        let g = hrrr_grid();
        let (ni, nj) = g.dimensions();
        for (name, at) in [
            ("Nashville", Coords { lat: 36.1627, lon: -86.7816 }),
            ("Norman", Coords { lat: 35.2226, lon: -97.4395 }),
            ("Seattle", Coords { lat: 47.6062, lon: -122.3321 }),
            ("Miami", Coords { lat: 25.7617, lon: -80.1918 }),
        ] {
            let i = g.index_of(at).unwrap_or_else(|| panic!("{name} fell outside CONUS"));
            assert!(i < ni * nj, "{name} index {i} out of range");
        }
    }

    /// The orientation meridian is where the projection has no rotation, so a
    /// point on it must sit at the horizontal centre of the domain.
    #[test]
    fn the_orientation_meridian_maps_near_the_domain_centre() {
        let g = hrrr_grid();
        let (ni, _) = g.dimensions();
        let idx = g.index_of(Coords { lat: 38.5, lon: -97.5 }).unwrap();
        let i = idx % ni;
        assert!(
            (i as i64 - ni as i64 / 2).abs() < 20,
            "expected column near {}, got {i}",
            ni / 2
        );
    }

    #[test]
    fn moving_east_increases_the_column_and_moving_north_increases_the_row() {
        let g = hrrr_grid();
        let (ni, _) = g.dimensions();
        let here = g.index_of(Coords { lat: 38.5, lon: -97.5 }).unwrap();
        let east = g.index_of(Coords { lat: 38.5, lon: -96.5 }).unwrap();
        let north = g.index_of(Coords { lat: 39.5, lon: -97.5 }).unwrap();
        assert!(east % ni > here % ni, "east must increase the column");
        assert!(north / ni > here / ni, "north must increase the row");
    }

    /// Measured between two successive column boundaries rather than from an
    /// arbitrary start, because `index_of` rounds: a point lands somewhere
    /// inside its cell, so start-to-first-boundary is any fraction of a cell.
    #[test]
    fn one_grid_column_spans_about_three_kilometres() {
        let g = hrrr_grid();
        let (ni, _) = g.dimensions();
        let lat = 38.5;
        let column = |lon: f64| g.index_of(Coords { lat, lon }).map(|i| i % ni);

        let mut lon = -97.5;
        let start_column = column(lon).unwrap();
        while column(lon) == Some(start_column) {
            lon += 0.0002;
        }
        let first_boundary = lon;
        let next_column = column(lon).unwrap();
        while column(lon) == Some(next_column) {
            lon += 0.0002;
        }

        let width_km = crate::geo::haversine_km(
            Coords { lat, lon: first_boundary },
            Coords { lat, lon },
        );
        assert!(
            (width_km - 3.0).abs() < 0.25,
            "one column spans {width_km:.2} km, expected ~3"
        );
    }

    /// The domain is 1799 columns of 3 km, so opposite edges of the middle row
    /// must be roughly 5400 km apart.
    #[test]
    fn the_domain_is_about_five_thousand_kilometres_wide() {
        let g = hrrr_grid();
        let (ni, nj) = g.dimensions();
        let lat = 38.5;
        let mut west = -130.0;
        while g.index_of(Coords { lat, lon: west }).is_none() && west < -60.0 {
            west += 0.05;
        }
        let mut east = -60.0;
        while g.index_of(Coords { lat, lon: east }).is_none() && east > -130.0 {
            east -= 0.05;
        }
        let span = crate::geo::haversine_km(
            Coords { lat, lon: west },
            Coords { lat, lon: east },
        );
        let expected = ni as f64 * 3.0;
        assert!(
            (span - expected).abs() / expected < 0.06,
            "domain spans {span:.0} km, expected about {expected:.0} for {ni}x{nj}"
        );
    }

    #[test]
    fn locations_outside_conus_have_no_forecast_cell() {
        let g = hrrr_grid();
        for at in [
            Coords { lat: 64.8, lon: -147.7 },
            Coords { lat: 21.3, lon: -157.8 },
            Coords { lat: 51.5, lon: -0.12 },
            Coords { lat: -33.9, lon: 151.2 },
        ] {
            assert_eq!(g.index_of(at), None, "{at:?} should be outside CONUS");
        }
    }

    #[test]
    fn nonsense_coordinates_are_rejected_rather_than_panicking() {
        let g = hrrr_grid();
        assert_eq!(g.index_of(Coords { lat: f64::NAN, lon: -97.0 }), None);
        assert_eq!(g.index_of(Coords { lat: 90.0, lon: -97.0 }), None);
        assert_eq!(g.index_of(Coords { lat: f64::INFINITY, lon: 0.0 }), None);
    }

    #[test]
    fn degenerate_grids_are_an_error() {
        assert!(LambertGrid::new(LambertParams { ni: 0, ..hrrr_params() }).is_err());
        assert!(LambertGrid::new(LambertParams { nj: 0, ..hrrr_params() }).is_err());
        assert!(LambertGrid::new(LambertParams { dx_m: 0.0, ..hrrr_params() }).is_err());
        assert!(LambertGrid::new(LambertParams { dy_m: -1.0, ..hrrr_params() }).is_err());
    }

    #[test]
    fn a_secant_projection_has_a_cone_constant_between_its_parallels() {
        let n = cone_constant(30.0_f64.to_radians(), 60.0_f64.to_radians());
        assert!(n > 30.0_f64.to_radians().sin());
        assert!(n < 60.0_f64.to_radians().sin());
    }

    #[test]
    fn a_tangent_projection_uses_the_sine_of_its_single_parallel() {
        let n = cone_constant(38.5_f64.to_radians(), 38.5_f64.to_radians());
        assert!((n - 38.5_f64.to_radians().sin()).abs() < 1e-12);
    }

    #[test]
    fn a_fresh_cycle_gives_two_hours_of_quarter_hourly_steps() {
        let steps = leads_every(15, 0, 120);
        assert_eq!(steps.len(), 8);
        assert_eq!(steps[0].lead_minutes(), 15);
        assert_eq!(steps[7].lead_minutes(), 120);
    }

    /// The bug this replaced: a cycle published 95 minutes late had its whole
    /// two-hour window land in the past, so the horizon delivered a couple of
    /// frames instead of eight.
    #[test]
    fn an_aged_cycle_still_yields_a_full_window_starting_after_now() {
        let age = 95;
        let steps = leads_every(15, age, age + 120);
        assert_eq!(steps.len(), 8, "an old cycle must not shrink the horizon");
        assert!(
            steps.iter().all(|s| s.lead_minutes() > age as i64),
            "every step must be in the future: {:?}",
            steps.iter().map(|s| s.lead_minutes()).collect::<Vec<_>>()
        );
        assert_eq!(steps[0].lead_minutes(), 105, "first step snaps to the next multiple");
    }

    #[test]
    fn leads_are_always_multiples_of_their_interval() {
        for age in [0u16, 1, 7, 14, 15, 16, 59, 95, 121] {
            for interval in [15u16, 60] {
                for step in leads_every(interval, age, age + 360) {
                    assert_eq!(
                        step.lead_minutes() % interval as i64,
                        0,
                        "age {age} interval {interval} produced {}",
                        step.lead_minutes()
                    );
                }
            }
        }
    }

    /// HRRR stops at forecast hour 18; asking beyond it must truncate rather
    /// than generate URLs that will 404.
    #[test]
    fn leads_never_run_past_what_hrrr_publishes() {
        let steps = leads_every(60, 900, 900 + 1080);
        assert!(!steps.is_empty());
        assert!(
            steps.iter().all(|s| s.lead_minutes() <= MAX_LEAD_MINUTES as i64),
            "generated a lead beyond HRRR's range"
        );
        assert_eq!(steps.last().unwrap().lead_minutes(), MAX_LEAD_MINUTES as i64);
    }

    #[test]
    fn a_window_entirely_beyond_the_model_range_is_empty_not_wrapped() {
        assert!(leads_every(60, 1080, 1200).is_empty());
    }

    /// The whole chain against live NOAA data: locate a published cycle, read
    /// the index, range-fetch one REFC record, decode it, and sample it.
    /// `cargo test live_hrrr -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_hrrr_forecast_decodes_and_samples() {
        let client = reqwest::Client::builder()
            .user_agent(crate::alert::poll::USER_AGENT)
            .build()
            .unwrap();
        let cycle = latest_cycle(&client, Utc::now()).await.expect("find a cycle");
        eprintln!("cycle {cycle}");

        let steps = leads_every(15, 0, 120);
        assert_eq!(steps.len(), 8);

        for step in [steps[0], steps[7]] {
            let field = fetch_step(&client, cycle, step).await.expect("fetch REFC");
            let nashville = Coords { lat: 36.1627, lon: -86.7816 };
            let sample = field.dbz_at(nashville);
            eprintln!(
                "  +{:>3}min valid {}  Nashville={:?}",
                field.lead_minutes,
                field.valid_at.format("%H:%MZ"),
                sample
            );
            assert_eq!(field.lead_minutes, step.lead_minutes());
            assert!(sample.is_some(), "Nashville is inside CONUS and must have a value");
            assert!(field.dbz_at(Coords { lat: 51.5, lon: -0.12 }).is_none());
        }
    }

    /// Verified against the live bucket: f00 is the analysis and holds no
    /// forecast, f01 carries 15 through 60, f02 carries 75 through 120.
    /// Proves the projection actually lands on the data, not merely somewhere
    /// plausible. The raw array's maximum is known; sweeping CONUS through
    /// `dbz_at` must rediscover a comparable value. If indexing were wrong the
    /// sweep would find nothing while the array is full of echo.
    /// `cargo test hrrr_sampling_finds -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn hrrr_sampling_finds_the_echo_present_in_the_raw_array() {
        let client = reqwest::Client::builder()
            .user_agent(crate::alert::poll::USER_AGENT)
            .build()
            .unwrap();
        let cycle = latest_cycle(&client, Utc::now()).await.expect("cycle");
        let field = fetch_step(&client, cycle, ForecastStep::new(60)).await.expect("fetch");

        let refc = &field
            .layers
            .iter()
            .find(|(p, _)| *p == RadarProduct::Reflectivity)
            .expect("REFC layer")
            .1;
        let raw_max = refc.iter().copied().filter(|v| v.is_finite()).fold(f32::MIN, f32::max);
        let raw_echo = refc.iter().filter(|v| **v > 5.0).count();
        eprintln!("raw: max {raw_max:.1} dBZ, {raw_echo} cells above 5 dBZ");

        let mut sampled_max = f32::MIN;
        let mut sampled_echo = 0usize;
        let mut inside = 0usize;
        let mut lat = 25.0;
        while lat < 49.0 {
            let mut lon = -124.0;
            while lon < -67.0 {
                if let Some(v) = field.dbz_at(Coords { lat, lon }) {
                    inside += 1;
                    sampled_max = sampled_max.max(v);
                    if v > 5.0 {
                        sampled_echo += 1;
                    }
                }
                lon += 0.1;
            }
            lat += 0.1;
        }
        eprintln!(
            "sampled: {inside} points inside grid, max {sampled_max:.1} dBZ, {sampled_echo} above 5"
        );

        assert!(inside > 100_000, "projection barely lands inside the grid: {inside}");
        assert!(
            sampled_max > raw_max - 10.0,
            "sweep found max {sampled_max:.1} but the array holds {raw_max:.1}; indexing is wrong"
        );
        if raw_echo > 1000 {
            assert!(
                sampled_echo > 0,
                "array has {raw_echo} echo cells but sampling found none"
            );
        }
    }

    /// Compares radar and model over the same patch, to tell a projection
    /// bug from real disagreement.
    /// `cargo test compare_observed -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn compare_observed_and_forecast_over_the_same_area() {
        let home = Coords { lat: 35.9527, lon: -87.3085 };
        let client = reqwest::Client::builder()
            .user_agent(crate::alert::poll::USER_AGENT)
            .build()
            .unwrap();

        let (observed_at, nexrad) = crate::radar::fetch::latest_field("KOHX").await.expect("KOHX");
        let cycle = latest_cycle(&client, Utc::now()).await.expect("cycle");
        let forecast = fetch_step(&client, cycle, ForecastStep::new(60)).await.expect("fetch");

        let mut n_vals = Vec::new();
        let mut h_vals = Vec::new();
        let mut lat = home.lat - 1.5;
        while lat < home.lat + 1.5 {
            let mut lon = home.lon - 1.5;
            while lon < home.lon + 1.5 {
                let at = Coords { lat, lon };
                if let Some(v) = nexrad.dbz_at(at) {
                    n_vals.push(v);
                }
                if let Some(v) = forecast.dbz_at(at) {
                    h_vals.push(v);
                }
                lon += 0.02;
            }
            lat += 0.02;
        }

        let describe = |name: &str, v: &[f32]| {
            let above = |t: f32| v.iter().filter(|x| **x > t).count();
            eprintln!(
                "{name:>8}: n={} min={:.1} max={:.1} | >5dBZ {} | >20dBZ {} | >35dBZ {}",
                v.len(),
                v.iter().copied().fold(f32::INFINITY, f32::min),
                v.iter().copied().fold(f32::NEG_INFINITY, f32::max),
                above(5.0),
                above(20.0),
                above(35.0),
            );
        };
        eprintln!(
            "observed {observed_at}, cycle {cycle}, lead 60min | tilts={} dual_pol={}",
            nexrad.tilt_count(),
            nexrad.has_dual_pol()
        );
        describe("NEXRAD", &n_vals);
        describe("HRRR", &h_vals);
    }

    #[test]
    fn leads_map_onto_the_published_file_layout() {
        for (lead, expected_file) in [
            (15, 1),
            (30, 1),
            (45, 1),
            (60, 1),
            (75, 2),
            (90, 2),
            (120, 2),
            (135, 3),
            (360, 6),
            (1080, 18),
        ] {
            assert_eq!(
                ForecastStep::new(lead).file_hour(),
                expected_file,
                "{lead} min belongs in wrfsubhf{expected_file:02}"
            );
        }
    }

    #[test]
    fn no_lead_ever_resolves_to_the_analysis_file() {
        for lead in [0u16, 1, 15, 59, 60] {
            assert!(
                ForecastStep::new(lead).file_hour() >= 1,
                "wrfsubhf00 is the analysis and carries no forecast"
            );
        }
    }

    #[test]
    fn the_object_key_matches_the_published_layout() {
        let cycle = DateTime::from_timestamp(1_769_774_400, 0).unwrap().to_utc();
        let stem = subh_stem(cycle, ForecastStep::new(90));
        assert!(stem.starts_with("hrrr."), "got {stem}");
        assert!(stem.contains("/conus/hrrr.t"), "got {stem}");
        assert!(stem.ends_with("wrfsubhf02.grib2"), "got {stem}");
        assert!(
            subh_stem(cycle, ForecastStep::new(15)).ends_with("wrfsubhf01.grib2"),
            "the first quarter-hour lives in f01, not f00"
        );
    }

    /// The complaint this exists to settle: forecast frames render as solid
    /// black. Finds where HRRR actually predicts a storm, points the viewport
    /// at it, and drives the real render path. A blank result here is a bug; a
    /// blank result over a clear sky is just the weather.
    /// `cargo test forecast_frames_render -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn forecast_frames_render_where_the_model_predicts_a_storm() {
        use crate::config::Colormap;
        use crate::radar::grid::{Viewport, rasterize_product};
        use crate::render::colormap::product_cell_rgb;

        let client = reqwest::Client::builder()
            .user_agent(crate::alert::poll::USER_AGENT)
            .build()
            .unwrap();
        let cycle = latest_cycle(&client, Utc::now()).await.expect("cycle");
        let field = fetch_step(&client, cycle, ForecastStep::new(60)).await.expect("fetch");

        let mut best = (f32::MIN, Coords { lat: 0.0, lon: 0.0 });
        let mut lat = 25.0;
        while lat < 49.0 {
            let mut lon = -124.0;
            while lon < -67.0 {
                let at = Coords { lat, lon };
                if let Some(v) = field.dbz_at(at)
                    && v > best.0
                {
                    best = (v, at);
                }
                lon += 0.1;
            }
            lat += 0.1;
        }
        let (peak, at) = best;
        eprintln!("strongest forecast echo {peak:.1} dBZ at {:.2},{:.2}", at.lat, at.lon);
        assert!(peak > 20.0, "no forecast echo anywhere in CONUS: {peak:.1} dBZ");

        let viewport = Viewport::new(at, 400.0, 200, 100);
        let grid = rasterize_product(&field, &viewport, RadarProduct::Reflectivity);
        let lit = (0..grid.height)
            .flat_map(|y| (0..grid.width).map(move |x| (x, y)))
            .filter(|(x, y)| {
                product_cell_rgb(grid.get(*x, *y), RadarProduct::Reflectivity, Colormap::Threat)
                    != crate::render::colormap::NO_DATA
            })
            .count();
        eprintln!("{lit} of {} rendered cells are lit", grid.width * grid.height);
        assert!(lit > 100, "forecast frame rendered {lit} lit cells: frames are blank");
    }


    /// Cross-correlates the forecast against a live radar over a storm and
    /// reports which spatial offset maximises agreement. A projection fault
    /// shows up as a large, consistent best-offset; forecast error alone
    /// leaves the peak near zero.
    /// `cargo test alignment -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn forecast_alignment_against_a_live_radar() {
        let client = reqwest::Client::builder()
            .user_agent(crate::alert::poll::USER_AGENT)
            .build()
            .unwrap();
        let now = Utc::now();
        let cycle = latest_cycle(&client, now).await.expect("cycle");
        let lead = (((now - cycle).num_minutes() as u16).div_ceil(15) * 15).max(15);
        let field = fetch_step(&client, cycle, ForecastStep::new(lead)).await.expect("fetch");

        // Pick the radar with the most forecast echo inside its own coverage.
        let mut best_site = None;
        let mut best_count = 0usize;
        for site in crate::geo::radar_table()
            .iter()
            .map(|(id, c)| crate::geo::RadarSite { id, coords: *c })
        {
            let mut n = 0usize;
            let mut lat = site.coords.lat - 0.8;
            while lat < site.coords.lat + 0.8 {
                let mut lon = site.coords.lon - 0.8;
                while lon < site.coords.lon + 0.8 {
                    if field.dbz_at(Coords { lat, lon }).is_some_and(|v| v > 25.0) {
                        n += 1;
                    }
                    lon += 0.05;
                }
                lat += 0.05;
            }
            if n > best_count {
                best_count = n;
                best_site = Some(site);
            }
        }
        let site = best_site.expect("some radar has forecast echo nearby");
        eprintln!("{} has {best_count} forecast echo cells; lead {lead}min", site.id);

        let (obs_at, radar) = crate::radar::fetch::latest_field(site.id).await.expect("radar");
        eprintln!("radar {obs_at} vs forecast valid {}", field.valid_at);

        let mut rows = Vec::new();
        for dlat_i in -8i32..=8 {
            let dlat = dlat_i as f64 * 0.05;
            let mut row = Vec::new();
            for dlon_i in -8i32..=8 {
                let dlon = dlon_i as f64 * 0.05;
                let mut hits = 0usize;
                let mut tries = 0usize;
                let mut lat = site.coords.lat - 0.8;
                while lat < site.coords.lat + 0.8 {
                    let mut lon = site.coords.lon - 0.8;
                    while lon < site.coords.lon + 0.8 {
                        if field.dbz_at(Coords { lat, lon }).is_some_and(|v| v > 25.0) {
                            tries += 1;
                            let shifted = Coords { lat: lat + dlat, lon: lon + dlon };
                            if radar.dbz_at(shifted).is_some_and(|v| v > 25.0) {
                                hits += 1;
                            }
                        }
                        lon += 0.05;
                    }
                    lat += 0.05;
                }
                row.push((hits as f64 / tries.max(1) as f64, dlat, dlon));
            }
            rows.push(row);
        }
        let best = rows
            .iter()
            .flatten()
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .copied()
            .unwrap();
        let zero = rows.iter().flatten().find(|r| r.1 == 0.0 && r.2 == 0.0).copied().unwrap();
        eprintln!("agreement at zero offset: {:.1}%", zero.0 * 100.0);
        eprintln!(
            "best agreement {:.1}% at dlat {:+.2} dlon {:+.2} ({:.0} km)",
            best.0 * 100.0,
            best.1,
            best.2,
            crate::geo::haversine_km(
                site.coords,
                Coords { lat: site.coords.lat + best.1, lon: site.coords.lon + best.2 }
            )
        );
    }


    /// Every forecastable product must arrive populated, or future frames go
    /// black for that layer no matter what the weather is doing.
    /// `cargo test every_forecast_layer -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn every_forecast_layer_carries_data() {
        let client = reqwest::Client::builder()
            .user_agent(crate::alert::poll::USER_AGENT)
            .build()
            .unwrap();
        let cycle = latest_cycle(&client, Utc::now()).await.expect("cycle");
        let field = fetch_step(&client, cycle, ForecastStep::new(60)).await.expect("fetch");

        for (product, _) in FORECASTABLE {
            assert!(field.supports(*product), "{product:?} missing from the field");
            let mut n = 0usize;
            let mut peak = f32::MIN;
            let mut lat = 25.0;
            while lat < 49.0 {
                let mut lon = -124.0;
                while lon < -67.0 {
                    if let Some(v) = field.value_at(Coords { lat, lon }, *product)
                        && product.is_notable(v)
                    {
                        n += 1;
                        peak = peak.max(v);
                    }
                    lon += 0.2;
                }
                lat += 0.2;
            }
            eprintln!("{:>10}: {n} notable cells, peak {peak:.1} {}", product.label(), product.units());
            assert!(n > 0, "{product:?} forecast is empty across all of CONUS");
        }
    }
}
