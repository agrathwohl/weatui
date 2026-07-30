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

/// Below this, a return is not precipitation. Dual-pol correlation coefficient
/// measures how alike the horizontal and vertical returns are: rain, snow and
/// hail are near 1.0, while insects, birds, chaff and ground clutter scatter
/// irregularly and collapse toward 0. Filtering on intensity instead would
/// keep dense bug swarms and discard genuine drizzle.
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
        }
    }
}

/// Composite reflectivity: the strongest echo anywhere in the column.
///
/// Deliberately not base reflectivity from the lowest tilt. HRRR publishes
/// REFC, the column maximum, so sampling a single cut would make the observed
/// and forecast halves of the timeline show different quantities and jump at
/// the boundary.
pub struct NexradField {
    site_id: String,
    system: RadarCoordinateSystem,
    tilts: Vec<Tilt>,
    lowest_elevation: f32,
    min_correlation: f32,
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
        let mut lowest_elevation = f32::MAX;
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
            lowest_elevation = lowest_elevation.min(elevation);
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

        Ok(NexradField {
            site_id: site.identifier_string(),
            system: RadarCoordinateSystem::new(site),
            tilts,
            lowest_elevation,
            min_correlation,
        })
    }

    /// The clutter mask applies to reflectivity only. Correlation coefficient
    /// must stay unmasked or it could never show the low values that are the
    /// entire reason to look at it, and masking velocity would hide rotation
    /// inside a debris ball.
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

        if product == RadarProduct::Reflectivity
            && let Some(cc) = &tilt.correlation
            && let Some((rho, cc_status)) =
                cc.value_at_polar(polar.azimuth_degrees, polar.range_km)
            && matches!(cc_status, GateStatus::Valid)
            && rho < self.min_correlation
        {
            return None;
        }
        Some(value)
    }

    pub fn tilt_count(&self) -> usize {
        self.tilts.len()
    }

    pub fn has_dual_pol(&self) -> bool {
        self.tilts.iter().any(|t| t.correlation.is_some())
    }
}

impl RadarField for NexradField {
    fn value_at(&self, at: Coords, product: RadarProduct) -> Option<f32> {
        let mut values = self
            .tilts
            .iter()
            .filter_map(|tilt| self.tilt_value(tilt, at, product));
        match product.reduction() {
            ColumnReduction::Max => values.reduce(f32::max),
            ColumnReduction::Min => values.reduce(f32::min),
            ColumnReduction::LowestCut => values.next(),
        }
    }

    fn supports(&self, product: RadarProduct) -> bool {
        self.tilts.iter().any(|t| t.moment(product).is_some())
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
}
