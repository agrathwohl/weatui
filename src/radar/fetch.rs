//! The only module that names nexrad types.
//!
//! Level II is polar volume data. Rather than resample it ourselves, we keep
//! the sweep in its native geometry and let `SweepField::value_at_polar`
//! answer per-pixel queries, which preserves full resolution under zoom.

use crate::geo::Coords;
use crate::radar::ReflectivityField;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use nexrad::data::aws::archive::{Identifier, download_file, list_files};
use nexrad::data::aws::realtime::{
    Chunk, assemble_volume, download_chunk, get_latest_volume, list_chunks_in_volume,
};
use nexrad::model::data::{GateStatus, Product, Scan, SweepField};
use nexrad::model::geo::{GeoPoint, RadarCoordinateSystem};

const MAX_CHUNKS_PER_VOLUME: usize = 100;

pub struct NexradField {
    site_id: String,
    system: RadarCoordinateSystem,
    field: SweepField,
}

impl NexradField {

    /// Base reflectivity is the lowest tilt that actually carries a
    /// reflectivity moment; higher tilts overshoot low-level storm structure.
    pub fn from_scan(scan: &Scan) -> Result<Self> {
        let site = scan
            .site()
            .context("scan carries no site metadata, cannot georeference it")?;

        let sweep = scan
            .sweeps()
            .iter()
            .filter(|s| s.radials().iter().any(|r| r.reflectivity().is_some()))
            .min_by(|a, b| {
                a.elevation_angle_degrees()
                    .unwrap_or(f32::MAX)
                    .total_cmp(&b.elevation_angle_degrees().unwrap_or(f32::MAX))
            })
            .context("scan contains no sweep with reflectivity data")?;

        let field = SweepField::from_radials(sweep.radials(), Product::Reflectivity)
            .ok_or_else(|| anyhow!("could not build a reflectivity field from the sweep"))?;

        Ok(NexradField {
            site_id: site.identifier_string(),
            system: RadarCoordinateSystem::new(site),
            field,
        })
    }
}

impl ReflectivityField for NexradField {
    fn dbz_at(&self, at: Coords) -> Option<f32> {
        let polar = self.system.geo_to_polar(
            GeoPoint { latitude: at.lat, longitude: at.lon },
            self.field.elevation_degrees(),
        );
        let (value, status) = self
            .field
            .value_at_polar(polar.azimuth_degrees, polar.range_km)?;
        matches!(status, GateStatus::Valid).then_some(value)
    }

    fn source_label(&self) -> &str {
        &self.site_id
    }

    fn elevation_degrees(&self) -> f32 {
        self.field.elevation_degrees()
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
