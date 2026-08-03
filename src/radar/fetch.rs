//! The only module that names nexrad types.
//!
//! Level II is polar volume data. Rather than resample it ourselves, we keep
//! the sweep in its native geometry and let `SweepField::value_at_polar`
//! answer per-pixel queries, which preserves full resolution under zoom.

use crate::geo::Coords;
use crate::radar::{ColumnReduction, RadarField, RadarProduct};
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use nexrad::data::aws::archive::{Identifier, download_file, list_files};
use nexrad::data::aws::realtime::{
    Chunk, assemble_volume, download_chunk, get_latest_volume, list_chunks_in_volume,
};
use nexrad::model::data::{GateStatus, Product, Scan, SweepField};
use nexrad::model::geo::{GeoPoint, RadarCoordinateSystem};

const MAX_CHUNKS_PER_VOLUME: usize = 100;

/// Below this, a return is not precipitation. Correlation coefficient
/// measures how alike the horizontal and vertical returns are: rain, snow
/// and hail sit near 1.0; insects, birds, chaff and ground clutter scatter
/// irregularly and collapse toward 0.
pub const DEFAULT_MIN_CORRELATION: f32 = 0.90;

/// One elevation cut and every moment it carries.
struct Tilt {
    elevation: f32,
    reflectivity: SweepField,
    velocity: Option<SweepField>,
    correlation: Option<SweepField>,
    differential_reflectivity: Option<SweepField>,
    spectrum_width: Option<SweepField>,
}

impl Tilt {
    fn moment(&self, product: RadarProduct) -> Option<&SweepField> {
        match product {
            RadarProduct::Reflectivity => Some(&self.reflectivity),
            RadarProduct::Velocity => self.velocity.as_ref(),
            RadarProduct::CorrelationCoefficient => self.correlation.as_ref(),
            RadarProduct::DifferentialReflectivity => self.differential_reflectivity.as_ref(),
            RadarProduct::SpectrumWidth => self.spectrum_width.as_ref(),
            // Derived from the reflectivity column, not carried by a sweep.
            RadarProduct::EchoTop | RadarProduct::VerticallyIntegratedLiquid => None,
        }
    }
}

/// Composite reflectivity: the strongest echo anywhere in the column.
///
/// Not base reflectivity from the lowest tilt. HRRR publishes
/// REFC, the column maximum, so sampling a single cut would make the observed
/// and forecast halves of the timeline show different quantities and jump at
/// the boundary.
pub struct NexradField {
    site_id: String,
    system: RadarCoordinateSystem,
    tilts: Vec<Tilt>,
    lowest_elevation: f32,
    min_correlation: f32,
    dual_pol: bool,
    beam_origin_msl_km: f64,
}

impl NexradField {
    pub fn from_scan(scan: &Scan) -> Result<Self> {
        Self::from_scan_with(scan, DEFAULT_MIN_CORRELATION)
    }

    pub fn from_scan_with(scan: &Scan, min_correlation: f32) -> Result<Self> {
        let site = scan
            .site()
            .context("scan carries no site metadata, cannot georeference it")?;

        let mut tilts: Vec<Tilt> = Vec::new();
        for sweep in scan.sweeps() {
            if !sweep.radials().iter().any(|r| r.reflectivity().is_some()) {
                continue;
            }
            let Some(reflectivity) =
                SweepField::from_radials(sweep.radials(), Product::Reflectivity)
            else {
                continue;
            };
            let elevation = reflectivity.elevation_degrees();
            let build = |p| SweepField::from_radials(sweep.radials(), p);
            tilts.push(Tilt {
                elevation,
                reflectivity,
                velocity: build(Product::Velocity),
                correlation: build(Product::CorrelationCoefficient),
                differential_reflectivity: build(Product::DifferentialReflectivity),
                spectrum_width: build(Product::SpectrumWidth),
            });
        }

        if tilts.is_empty() {
            anyhow::bail!("scan contains no sweep with reflectivity data");
        }
        tilts.sort_by(|a, b| a.elevation.total_cmp(&b.elevation));

        // The reported tilt must be one the mask actually lets through, or the
        // HUD names a Doppler cut that contributes no reflectivity at all.
        let dual_pol = tilts.iter().any(|t| t.correlation.is_some());
        let lowest_elevation = tilts
            .iter()
            .find(|t| !dual_pol || t.correlation.is_some())
            .expect("dual_pol is true only when some tilt carries correlation")
            .elevation;

        let beam_origin_msl_km =
            (site.height_meters() as f64 + site.tower_height_meters() as f64) / 1000.0;

        Ok(NexradField {
            site_id: site.identifier_string(),
            system: RadarCoordinateSystem::new(site),
            dual_pol,
            tilts,
            lowest_elevation,
            min_correlation,
            beam_origin_msl_km,
        })
    }

    /// The clutter mask applies to reflectivity only; masking correlation
    /// would hide the low values it exists to show, and masking velocity
    /// would hide rotation inside a debris ball.
    ///
    /// On a dual-pol volume the mask fails closed: a reflectivity gate with
    /// no valid correlation is dropped. WSR-88D splits low elevations into a
    /// surveillance cut (dual-pol) and a Doppler cut (velocity only), so
    /// failing open would let clutter through whichever cut lacks CC.
    fn tilt_value(&self, tilt: &Tilt, at: Coords, product: RadarProduct) -> Option<f32> {
        let polar = self.system.geo_to_polar(
            GeoPoint { latitude: at.lat, longitude: at.lon },
            tilt.elevation,
        );
        let (value, status) = tilt
            .moment(product)?
            .value_at_polar(polar.azimuth_degrees, polar.range_km)?;
        if !matches!(status, GateStatus::Valid) {
            return None;
        }

        if product == RadarProduct::Reflectivity && self.dual_pol {
            let rho = tilt
                .correlation
                .as_ref()
                .and_then(|cc| cc.value_at_polar(polar.azimuth_degrees, polar.range_km))
                .and_then(|(rho, s)| matches!(s, GateStatus::Valid).then_some(rho))?;
            if rho < self.min_correlation {
                return None;
            }
        }
        Some(value)
    }

    #[cfg(test)]
    pub fn tilt_count(&self) -> usize {
        self.tilts.len()
    }

    #[cfg(test)]
    pub fn has_dual_pol(&self) -> bool {
        self.dual_pol
    }
}

/// Height of the beam centre above the radar, in km, under the standard 4/3
/// effective-earth model that accounts for atmospheric refraction.
fn beam_height_km(range_km: f64, elevation_deg: f32) -> f64 {
    const EFFECTIVE_EARTH_KM: f64 = 8495.0;
    let e = (elevation_deg as f64).to_radians();
    range_km * e.sin() + range_km * range_km / (2.0 * EFFECTIVE_EARTH_KM)
}

impl NexradField {
    /// Highest altitude carrying meaningful echo, the standard 18 dBZ contour.
    /// No echo above the threshold on any tilt means no top, not a top at
    /// beam height.
    fn echo_top_km(&self, at: Coords) -> Option<f32> {
        const ECHO_TOP_DBZ: f32 = 18.0;
        let mut top: Option<f32> = None;
        for tilt in &self.tilts {
            let Some(dbz) = self.tilt_value(tilt, at, RadarProduct::Reflectivity) else {
                continue;
            };
            if dbz < ECHO_TOP_DBZ {
                continue;
            }
            let polar = self.system.geo_to_polar(
                GeoPoint { latitude: at.lat, longitude: at.lon },
                tilt.elevation,
            );
            // HRRR RETOP is metres above sea level; reporting height above
            // the radar instead would jump the same storm's top by the site
            // elevation whenever the timeline crosses observed to forecast.
            let h = (beam_height_km(polar.range_km, tilt.elevation)
                + self.beam_origin_msl_km) as f32;
            if top.is_none_or(|t| h > t) {
                top = Some(h);
            }
        }
        top
    }

    /// Vertically integrated liquid, kg/m^2.
    ///
    /// The operational formula integrates 3.44e-6 * Z^(4/7) over the column,
    /// with Z capped at 56 dBZ so that hail, which is a far stronger scatterer
    /// than the rain the relation was derived for, cannot inflate the total.
    fn vil(&self, at: Coords) -> Option<f32> {
        const HAIL_CAP_DBZ: f32 = 56.0;
        let mut layers: Vec<(f64, f64)> = Vec::new();
        for tilt in &self.tilts {
            let Some(dbz) = self.tilt_value(tilt, at, RadarProduct::Reflectivity) else {
                continue;
            };
            let polar = self.system.geo_to_polar(
                GeoPoint { latitude: at.lat, longitude: at.lon },
                tilt.elevation,
            );
            let z = 10f64.powf((dbz.min(HAIL_CAP_DBZ) as f64) / 10.0);
            layers.push((beam_height_km(polar.range_km, tilt.elevation), z));
        }
        if layers.len() < 2 {
            return None;
        }
        layers.sort_by(|a, b| a.0.total_cmp(&b.0));
        let total: f64 = layers
            .windows(2)
            .map(|w| {
                let dh_m = (w[1].0 - w[0].0) * 1000.0;
                3.44e-6 * ((w[0].1 + w[1].1) / 2.0).powf(4.0 / 7.0) * dh_m
            })
            .sum();
        (total > 0.0).then_some(total as f32)
    }
}

impl RadarField for NexradField {
    fn value_at(&self, at: Coords, product: RadarProduct) -> Option<f32> {
        match product {
            RadarProduct::EchoTop => return self.echo_top_km(at),
            RadarProduct::VerticallyIntegratedLiquid => return self.vil(at),
            _ => {}
        }
        let mut values = self
            .tilts
            .iter()
            .filter_map(|tilt| self.tilt_value(tilt, at, product));
        match product.reduction() {
            ColumnReduction::Max => values.reduce(f32::max),
            ColumnReduction::LowestCut => values.next(),
        }
    }

    fn supports(&self, product: RadarProduct) -> bool {
        match product {
            RadarProduct::EchoTop | RadarProduct::VerticallyIntegratedLiquid => {
                self.tilts.len() >= 2
            }
            _ => self.tilts.iter().any(|t| t.moment(product).is_some()),
        }
    }

    fn source_label(&self) -> &str {
        &self.site_id
    }

    fn elevation_degrees(&self) -> f32 {
        self.lowest_elevation
    }
}

/// Assemble the in-progress volume from the realtime chunk bucket. A volume is
/// built up chunk by chunk during the scan, so the newest one is usually partial.
pub async fn latest_scan(site: &str) -> Result<Scan> {
    let latest = get_latest_volume(site)
        .await
        .map_err(|e| anyhow!("failed to find the current volume for {site}: {e}"))?;

    let volume = latest
        .volume
        .with_context(|| format!("no realtime volume is currently published for {site}"))?;

    let ids = list_chunks_in_volume(site, volume, MAX_CHUNKS_PER_VOLUME)
        .await
        .map_err(|e| anyhow!("failed to list chunks for {site}: {e}"))?;

    if ids.is_empty() {
        anyhow::bail!("volume for {site} contains no chunks yet");
    }

    let mut chunks: Vec<Chunk> = Vec::with_capacity(ids.len());
    for id in &ids {
        let (_, chunk) = download_chunk(site, id)
            .await
            .map_err(|e| anyhow!("failed to download a chunk for {site}: {e}"))?;
        chunks.push(chunk);
    }

    assemble_volume(chunks).map_err(|e| anyhow!("failed to assemble the volume for {site}: {e}"))
}

pub async fn latest_field(site: &str) -> Result<(DateTime<Utc>, NexradField)> {
    let scan = latest_scan(site).await?;
    let observed = scan.time_range().map(|(start, _)| start).unwrap_or_else(Utc::now);
    Ok((observed, NexradField::from_scan(&scan)?))
}

/// Archive identifiers for the most recent `count` volumes, oldest first.
///
/// The archive is keyed by UTC day, so a run shortly after 00Z would otherwise
/// see almost nothing. Yesterday is consulted whenever today alone is short.
pub async fn recent_archive_ids(site: &str, count: usize) -> Result<Vec<Identifier>> {
    let today = Utc::now().date_naive();
    let mut ids = list_files(site, &today)
        .await
        .map_err(|e| anyhow!("failed to list archive volumes for {site}: {e}"))?;

    if ids.len() < count {
        let yesterday = today - Duration::days(1);
        if let Ok(mut earlier) = list_files(site, &yesterday).await {
            earlier.extend(ids);
            ids = earlier;
        }
    }

    // The bucket also carries _MDM metadata objects, which are not volumes.
    ids.retain(|id| id.date_time().is_some() && !id.name().ends_with("_MDM"));
    let skip = ids.len().saturating_sub(count);
    Ok(ids.split_off(skip))
}

pub async fn archived_field(id: Identifier) -> Result<(DateTime<Utc>, NexradField)> {
    let stamped = id.date_time();
    let name = id.name().to_string();
    let scan = download_file(id)
        .await
        .map_err(|e| anyhow!("failed to download archive volume {name}: {e}"))?
        .decompress()
        .map_err(|e| anyhow!("failed to decompress {name}: {e}"))?
        .scan()
        .map_err(|e| anyhow!("failed to decode {name}: {e}"))?;

    let observed = stamped
        .or_else(|| scan.time_range().map(|(start, _)| start))
        .unwrap_or_else(Utc::now);
    Ok((observed, NexradField::from_scan(&scan)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::radar::grid::{Viewport, rasterize};

    /// Hits the live NOAA S3 bucket, so it is excluded from the default run.
    /// `cargo test -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_volume_resamples_into_a_populated_grid() {
        let (observed_at, field) = latest_field("KTLX").await.expect("fetch KTLX");
        eprintln!("volume observed at {observed_at}");
        assert_eq!(field.source_label().trim(), "KTLX");

        let site = crate::geo::radar_site_by_id("KTLX").expect("KTLX is in the embedded table");
        let grid = rasterize(&field, &Viewport::new(site.coords, 400.0, 200, 100));
        eprintln!(
            "site {} elev {:.2} deg -> {}/{} pixels populated, dbz {:?}",
            field.source_label(),
            field.elevation_degrees(),
            grid.populated_cells(),
            grid.width * grid.height,
            grid.value_range()
        );
        assert!(grid.populated_cells() > 0, "resampled grid was entirely empty");
    }

    /// The defect that made the timeline unplayable: without backfill the ring
    /// holds one volume and there is nothing to animate.
    #[tokio::test]
    #[ignore]
    async fn archive_backfill_yields_multiple_distinct_volumes() {
        let ids = recent_archive_ids("KTLX", 4).await.expect("list archive");
        assert!(ids.len() >= 2, "expected several archived volumes, got {}", ids.len());

        let mut stamps = Vec::new();
        for id in ids {
            let (at, field) = archived_field(id).await.expect("decode archived volume");
            eprintln!("{at}  {}  {:.2} deg", field.source_label(), field.elevation_degrees());
            stamps.push(at);
        }

        assert!(
            stamps.windows(2).all(|w| w[0] < w[1]),
            "backfill must be ordered oldest first: {stamps:?}"
        );
    }

    /// Every reflectivity value a dual-pol volume reports must be backed by
    /// a valid correlation gate above the threshold.
    /// `cargo test clutter_mask_fails_closed -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn clutter_mask_fails_closed_on_every_gate_it_reports() {
        let (_at, field) = latest_field("KOHX").await.expect("KOHX");
        assert!(field.dual_pol, "KOHX is a dual-pol site");

        let home = Coords { lat: 35.9527, lon: -87.3085 };
        let (mut reported, mut unvouched) = (0usize, 0usize);
        let mut lat = home.lat - 1.5;
        while lat < home.lat + 1.5 {
            let mut lon = home.lon - 1.5;
            while lon < home.lon + 1.5 {
                let at = Coords { lat, lon };
                for tilt in &field.tilts {
                    if field.tilt_value(tilt, at, RadarProduct::Reflectivity).is_none() {
                        continue;
                    }
                    reported += 1;
                    let polar = field.system.geo_to_polar(
                        GeoPoint { latitude: at.lat, longitude: at.lon },
                        tilt.elevation,
                    );
                    let vouched = tilt
                        .correlation
                        .as_ref()
                        .and_then(|cc| cc.value_at_polar(polar.azimuth_degrees, polar.range_km))
                        .is_some_and(|(rho, s)| {
                            matches!(s, GateStatus::Valid) && rho >= field.min_correlation
                        });
                    if !vouched {
                        unvouched += 1;
                    }
                }
                lon += 0.05;
            }
            lat += 0.05;
        }
        eprintln!("{reported} reflectivity gates reported, {unvouched} without a valid CC vouching");
        assert_eq!(unvouched, 0, "{unvouched} gates bypassed the clutter mask");
    }

    /// Echo top and VIL exist on both sides of the timeline, so the observed
    /// half must produce them too or the handoff still goes blank.
    /// `cargo test derived_products -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn derived_products_are_available_from_the_observed_volume() {
        let (_at, field) = latest_field("KOHX").await.expect("KOHX");
        for product in [RadarProduct::EchoTop, RadarProduct::VerticallyIntegratedLiquid] {
            assert!(field.supports(product), "{product:?} unsupported");
            let (mut n, mut peak) = (0usize, f32::MIN);
            let mut lat = 34.8;
            while lat < 37.2 {
                let mut lon = -88.5;
                while lon < -86.1 {
                    if let Some(v) = field.value_at(Coords { lat, lon }, product) {
                        n += 1;
                        peak = peak.max(v);
                    }
                    lon += 0.05;
                }
                lat += 0.05;
            }
            eprintln!("{:>10}: {n} cells, peak {peak:.1} {}", product.label(), product.units());
            assert!(n > 0, "{product:?} produced nothing from a live volume");
        }
    }
}
