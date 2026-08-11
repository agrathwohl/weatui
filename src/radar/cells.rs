//! Storm cell identification: cluster the composite reflectivity field into
//! connected cores, then interrogate each core across every moment the
//! volume carries. Warnings remain the authority; this points at the blob
//! that deserves attention before one exists.
//!
//! [`scan`] sees one volume and has no memory. [`CellTracker`] supplies the
//! memory: it associates each volume's detections with the previous one's, so
//! a core keeps an identity while it lives, carries a measured translation,
//! and can say when it reaches the user.

use crate::geo::Coords;
use crate::radar::{RadarField, RadarProduct};
use chrono::{DateTime, Utc};

/// Reflectivity that makes a sample part of a storm core. 40 dBZ is
/// convective rain; stratiform and bright-band echo stay below it.
const CELL_MIN_DBZ: f32 = 40.0;
/// Fewer connected samples than this is speckle, not a storm.
const MIN_SAMPLES: usize = 4;
/// Grid step in degrees, ~2 km. Finer would find the same cells slower.
const STEP_DEG: f64 = 0.02;
/// Half-extent of the scan around the radar site. Beyond ~240 km the beam
/// overshoots the levels these diagnostics need anyway.
const HALF_EXTENT_DEG: f64 = 2.2;
const MAX_CELLS: usize = 8;
/// Cells farther than this from home are not listed.
const MAX_CELL_DISTANCE_KM: f64 = 75.0;

/// Thresholds for the threat ladder, most severe first.
///
/// Rotation evidence is the largest velocity difference between two samples
/// of the same core within [`LOCAL_RADIUS_CELLS`]: local, sign-free shear.
/// A mesocyclone embedded in one-signed flow registers; a squall line whose
/// ends point different ways along the beam does not. Limits: velocity is
/// not dealiased, so folding can understate a couplet or fake shear at a
/// fold boundary, and VIL / echo top are floors because the beam samples
/// part of the column.
const TDS_MAX_CC: f32 = 0.85;
/// One bar for rotation, used both on its own and as the debris corroboration.
///
/// These were previously split, 25 to corroborate a CC drop but 40 to count as
/// rotation alone, which left a 25-39 m/s couplet with no debris signature
/// classified as merely Intense and given no tornado hazard letter. Trusting a
/// span as evidence of a couplet in one branch and not the other cannot both be
/// right, and the lower bar is the safe one: the sampling lattice is coarser
/// than the couplet it measures, so every span this reports is biased low.
const TDS_MIN_ROTATION: f32 = 25.0;
const HAIL_VIL: f32 = 45.0;
/// ~6-7 km at [`STEP_DEG`] spacing: the scale of a couplet plus grid slack.
const LOCAL_RADIUS_CELLS: i64 = 3;
/// A 1-degree beam is ~2.6 km wide at 150 km, wider than the couplet it
/// would need to resolve. Beyond this range shear pairs are artifacts, so
/// rotation and debris claims are suppressed.
const ROTATION_MAX_RANGE_KM: f64 = 150.0;
const HAIL_ECHO_TOP_KM: f32 = 14.0;
const INTENSE_DBZ: f32 = 55.0;
/// Tracking follows the high-reflectivity core rather than the 40 dBZ
/// envelope around it. A supercell regenerating on its upwind flank grows
/// backwards about as fast as it advances, so the envelope's centroid creeps
/// while the storm itself moves; the core inside it travels with the storm.
/// Measured on 2013-05-20 KTLX, the envelope put the Moore tornado at 5-11 kt
/// against an actual translation near 30 kt.
const CORE_DBZ: f32 = 50.0;
/// Do not narrow this to a band below each cell's own peak. Tried on the
/// same KTLX sequence, tracking the top 10 dB of a 68 dBZ supercell swung the
/// heading through 180 degrees: the peak region pulses around inside a storm
/// independently of where the storm is going, and too few samples make its
/// centre jitter. A fixed floor understates the speed; a narrow one invents
/// a direction, which is worse.
///
/// Fewer core samples than this is a peak, not a body to take a centre of.
const MIN_CORE_SAMPLES: usize = 3;

/// Hazard letters for the map, worst first. Wind is a radial-velocity proxy
/// for damaging gusts; Lightning is a deep-updraft proxy (charge separation
/// needs a mixed-phase column, so a >= 9 km echo top stands in for it).
/// Snow is decided at render time from the surface temperature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hazard {
    Tornado,
    Hail,
    Wind,
    Lightning,
    Rain,
}

impl Hazard {
    pub fn letter(self) -> char {
        match self {
            Hazard::Tornado => 'T',
            Hazard::Hail => 'H',
            Hazard::Wind => 'W',
            Hazard::Lightning => 'L',
            Hazard::Rain => 'R',
        }
    }
}

/// NWS severe gust criterion is 58 mph (~26 m/s).
const WIND_HAZARD_MS: f32 = 26.0;
const LIGHTNING_ECHO_TOP_KM: f32 = 9.0;

pub(crate) fn hazards(
    threat: CellThreat,
    max_abs_velocity: Option<f32>,
    max_echo_top_km: Option<f32>,
) -> Vec<Hazard> {
    let mut out = Vec::new();
    if matches!(threat, CellThreat::Debris | CellThreat::Rotation) {
        out.push(Hazard::Tornado);
    }
    if threat == CellThreat::Hail {
        out.push(Hazard::Hail);
    }
    if max_abs_velocity.is_some_and(|v| v >= WIND_HAZARD_MS) {
        out.push(Hazard::Wind);
    }
    if max_echo_top_km.is_some_and(|t| t >= LIGHTNING_ECHO_TOP_KM) {
        out.push(Hazard::Lightning);
    }
    out.push(Hazard::Rain);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CellThreat {
    Strong,
    Intense,
    Hail,
    Rotation,
    Debris,
}

impl CellThreat {
    pub fn label(self) -> &'static str {
        match self {
            CellThreat::Debris => "debris",
            CellThreat::Rotation => "rotation",
            CellThreat::Hail => "hail",
            CellThreat::Intense => "intense",
            CellThreat::Strong => "strong",
        }
    }
}

/// A cell's translation across volumes.
///
/// `heading_deg` is where the storm is GOING, clockwise from north. NWS storm
/// motion text states the direction a storm comes FROM; the two are 180 apart
/// and confusing them reports an inbound storm as departing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMotion {
    pub heading_deg: f64,
    pub speed_kmh: f64,
}

impl CellMotion {
    pub fn speed_kt(self) -> f64 {
        self.speed_kmh / crate::geo::KM_PER_KNOT_HOUR
    }

    pub fn compass(self) -> &'static str {
        crate::geo::compass_16(self.heading_deg as f32)
    }
}

/// Where the current track puts the cell at its nearest point to home.
///
/// A cell already past its nearest point gets none: it is leaving.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Approach {
    pub minutes: f64,
    pub distance_km: f64,
}

#[derive(Debug, Clone)]
pub struct StormCell {
    /// Stable while the track lives. Zero until [`CellTracker::track`] runs:
    /// [`scan`] sees a single volume and cannot know what came before.
    pub id: u32,
    pub centroid: Coords,
    /// The point the tracker follows: the core's centre when the cell has
    /// one, else [`Self::centroid`]. Distance and bearing stay on the
    /// centroid, which is the blob actually drawn on the map.
    pub track_point: Coords,
    pub max_dbz: f32,
    pub rotation_ms: Option<f32>,
    /// False when no velocity sample fell inside beam-resolution range, so
    /// `rotation_ms: None` means unknown rather than calm.
    pub rotation_measurable: bool,
    pub min_cc: Option<f32>,
    pub max_vil: Option<f32>,
    pub max_echo_top_km: Option<f32>,
    pub distance_km: f64,
    pub bearing: &'static str,
    pub threat: CellThreat,
    pub hazards: Vec<Hazard>,
    /// `None` until the track has two fixes far enough apart in time to
    /// measure against.
    pub motion: Option<CellMotion>,
    pub approach: Option<Approach>,
}

#[derive(Clone)]
pub(crate) struct CellStats {
    pub max_dbz: f32,
    pub rotation_ms: Option<f32>,
    pub min_cc: Option<f32>,
    pub max_vil: Option<f32>,
    pub max_echo_top_km: Option<f32>,
}

pub(crate) fn classify(s: &CellStats) -> CellThreat {
    let rotating = s.rotation_ms.is_some_and(|r| r >= TDS_MIN_ROTATION);
    if rotating && s.min_cc.is_some_and(|cc| cc < TDS_MAX_CC) {
        return CellThreat::Debris;
    }
    if rotating {
        return CellThreat::Rotation;
    }
    if s.max_vil.is_some_and(|v| v >= HAIL_VIL)
        || s.max_echo_top_km.is_some_and(|t| t >= HAIL_ECHO_TOP_KM)
    {
        return CellThreat::Hail;
    }
    if s.max_dbz >= INTENSE_DBZ {
        return CellThreat::Intense;
    }
    CellThreat::Strong
}

pub fn scan(field: &dyn RadarField, site: Coords, home: Coords) -> Vec<StormCell> {
    let n = (2.0 * HALF_EXTENT_DEG / STEP_DEG) as usize + 1;
    let at = |ix: usize, iy: usize| Coords {
        lat: site.lat - HALF_EXTENT_DEG + iy as f64 * STEP_DEG,
        lon: site.lon - HALF_EXTENT_DEG + ix as f64 * STEP_DEG,
    };

    let mut dbz = vec![None; n * n];
    for iy in 0..n {
        for ix in 0..n {
            dbz[iy * n + ix] = field
                .value_at(at(ix, iy), RadarProduct::Reflectivity)
                .filter(|v| *v >= CELL_MIN_DBZ);
        }
    }

    let mut visited = vec![false; n * n];
    let mut cells = Vec::new();
    for start in 0..n * n {
        if visited[start] || dbz[start].is_none() {
            continue;
        }
        let mut stack = vec![start];
        let mut members = Vec::new();
        visited[start] = true;
        while let Some(i) = stack.pop() {
            members.push(i);
            let (ix, iy) = (i % n, i / n);
            for (dx, dy) in [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)] {
                let (jx, jy) = (ix as i64 + dx, iy as i64 + dy);
                if jx < 0 || jy < 0 || jx >= n as i64 || jy >= n as i64 {
                    continue;
                }
                let j = jy as usize * n + jx as usize;
                if !visited[j] && dbz[j].is_some() {
                    visited[j] = true;
                    stack.push(j);
                }
            }
        }
        if members.len() < MIN_SAMPLES {
            continue;
        }

        let mut weight = 0.0f64;
        let (mut wlat, mut wlon) = (0.0f64, 0.0f64);
        let mut core_weight = 0.0f64;
        let (mut core_lat, mut core_lon) = (0.0f64, 0.0f64);
        let mut core_samples = 0usize;
        let mut max_dbz = f32::MIN;
        let mut velocity = std::collections::HashMap::new();
        let mut cc_at = std::collections::HashMap::new();
        let (mut max_vil, mut max_top) = (None::<f32>, None::<f32>);
        for &i in &members {
            let p = at(i % n, i / n);
            let z = dbz[i].unwrap() as f64;
            weight += z;
            wlat += z * p.lat;
            wlon += z * p.lon;
            max_dbz = max_dbz.max(dbz[i].unwrap());

            let key = ((i % n) as i64, (i / n) as i64);
            if let Some(v) = field.value_at(p, RadarProduct::Velocity) {
                velocity.insert(key, v);
            }
            if let Some(cc) = field.value_at(p, RadarProduct::CorrelationCoefficient) {
                cc_at.insert(key, cc);
            }
            if let Some(v) = field.value_at(p, RadarProduct::VerticallyIntegratedLiquid)
                && max_vil.is_none_or(|m| v > m)
            {
                max_vil = Some(v);
            }
            if let Some(t) = field.value_at(p, RadarProduct::EchoTop)
                && max_top.is_none_or(|m| t > m)
            {
                max_top = Some(t);
            }
        }

        let max_abs_velocity = velocity.values().fold(None::<f32>, |m, v| {
            Some(m.map_or(v.abs(), |m| m.max(v.abs())))
        });
        let mut shear: Option<f32> = None;
        let mut shear_centre: Option<(i64, i64)> = None;
        // "not rotating" and "cannot tell whether it is rotating" rendered
        // identically, because both produced rotation_ms = None. Beyond beam
        // resolution range, and with no velocity at all, the answer is unknown
        // and the display has to say so rather than imply calm.
        let mut rotation_measurable = false;
        for (&(ix, iy), &va) in &velocity {
            if crate::geo::haversine_km(site, at(ix as usize, iy as usize))
                > ROTATION_MAX_RANGE_KM
            {
                continue;
            }
            rotation_measurable = true;
            for dy in -LOCAL_RADIUS_CELLS..=LOCAL_RADIUS_CELLS {
                for dx in -LOCAL_RADIUS_CELLS..=LOCAL_RADIUS_CELLS {
                    let Some(&vb) = velocity.get(&(ix + dx, iy + dy)) else { continue };
                    let span = (va - vb).abs();
                    if shear.is_none_or(|m| span > m) {
                        shear = Some(span);
                        shear_centre = Some((ix + dx / 2, iy + dy / 2));
                    }
                }
            }
        }
        // Debris demands the correlation collapse at the rotation, not a hail
        // core with mixed-phase CC somewhere else in the same cluster.
        // Couplet-local only: with no shear pair there is no rotation to
        // qualify, and a cluster-wide minimum would pin a distant hail
        // core's CC onto this cell's readout.
        let min_cc = shear_centre.and_then(|(sx, sy)| {
            cc_at
                .iter()
                .filter(|((ix, iy), _)| {
                    (ix - sx).abs() <= LOCAL_RADIUS_CELLS && (iy - sy).abs() <= LOCAL_RADIUS_CELLS
                })
                .map(|(_, &cc)| cc)
                .min_by(f32::total_cmp)
        });

        let stats = CellStats {
            max_dbz,
            rotation_ms: shear,
            min_cc,
            max_vil,
            max_echo_top_km: max_top,
        };
        for &i in &members {
            let Some(z) = dbz[i].filter(|v| *v >= CORE_DBZ) else { continue };
            let p = at(i % n, i / n);
            core_weight += z as f64;
            core_lat += z as f64 * p.lat;
            core_lon += z as f64 * p.lon;
            core_samples += 1;
        }

        let centroid = Coords { lat: wlat / weight, lon: wlon / weight };
        let track_point = if core_samples >= MIN_CORE_SAMPLES && core_weight > 0.0 {
            Coords { lat: core_lat / core_weight, lon: core_lon / core_weight }
        } else {
            centroid
        };
        let distance_km = crate::geo::haversine_km(home, centroid);
        if distance_km > MAX_CELL_DISTANCE_KM {
            continue;
        }
        cells.push(StormCell {
            id: 0,
            motion: None,
            approach: None,
            centroid,
            track_point,
            max_dbz,
            rotation_ms: stats.rotation_ms,
            rotation_measurable,
            min_cc: stats.min_cc,
            max_vil: stats.max_vil,
            max_echo_top_km: stats.max_echo_top_km,
            distance_km,
            bearing: crate::geo::compass_bearing(home, centroid),
            hazards: hazards(classify(&stats), max_abs_velocity, stats.max_echo_top_km),
            threat: classify(&stats),
        });
    }

    cells.sort_by(|a, b| {
        b.threat.cmp(&a.threat).then(b.max_dbz.total_cmp(&a.max_dbz))
    });
    if cells.len() > MAX_CELLS {
        // In an outbreak, the cell about to arrive matters more than the
        // eighth-strongest distant one; the nearest must survive truncation.
        let nearest = cells
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.distance_km.total_cmp(&b.1.distance_km))
            .map(|(i, _)| i)
            .unwrap();
        if nearest >= MAX_CELLS {
            // Appended rather than swapped in. Dropping to MAX_CELLS - 1 to
            // make room evicted the eighth-ranked cell, and in an outbreak
            // that one can be rotating or dropping debris. One extra row costs
            // nothing next to losing a tornadic cell off the bottom.
            let keep = cells.remove(nearest);
            cells.truncate(MAX_CELLS);
            cells.push(keep);
        } else {
            cells.truncate(MAX_CELLS);
        }
    }
    cells
}

/// The fastest a storm core plausibly translates. Above ~65 kt, two cores in
/// consecutive volumes are two storms rather than one that moved.
const MAX_TRACK_SPEED_KMH: f64 = 120.0;
/// Centroid jitter budget. The lattice is ~2.2 km and a growing core's
/// reflectivity-weighted centre wanders inside the blob even when the storm
/// itself is stationary.
const TRACK_SLACK_KM: f64 = 6.0;
/// A longer gap than this (dead feed, laptop asleep) makes association
/// guesswork, so every cell starts a fresh track instead of inheriting an
/// identity it may not deserve.
const MAX_TRACK_GAP_MIN: f64 = 20.0;
/// Volumes closer together than this quantise badly: one lattice step over
/// one minute reads as 130 km/h of pure grid noise. NEXRAD VCPs run 4-6
/// minutes, so this only rejects repeats of the same volume.
const MIN_MOTION_GAP_MIN: f64 = 1.5;
/// Weight on the newest fix. Halving the correction each volume keeps a
/// turning storm current without letting one jittery centroid swing the
/// vector across the compass.
const MOTION_SMOOTHING: f64 = 0.5;
/// Storms do not hold a heading long enough for an arrival further out than
/// this to mean anything.
const MAX_ETA_MINUTES: f64 = 120.0;
/// Below this the vector is centroid noise, not translation.
const MIN_MOTION_KMH: f64 = 3.0;

/// East/north offset in km, from great-circle distance and bearing rather
/// than a projection constant so it holds at any latitude.
fn displacement_km(from: Coords, to: Coords) -> (f64, f64) {
    let d = crate::geo::haversine_km(from, to);
    let b = crate::geo::initial_bearing_deg(from, to).to_radians();
    (d * b.sin(), d * b.cos())
}

fn motion_from((east, north): (f64, f64)) -> Option<CellMotion> {
    let speed_kmh = east.hypot(north);
    if speed_kmh < MIN_MOTION_KMH {
        return None;
    }
    Some(CellMotion {
        heading_deg: east.atan2(north).to_degrees().rem_euclid(360.0),
        speed_kmh,
    })
}

/// Closest point of approach: with `r` pointing from the cell to home and
/// `v` the cell's velocity, the track is nearest at `t = (r.v) / |v|^2`. A
/// non-positive `t` puts that moment in the past, so the cell is receding.
fn approach(centroid: Coords, (vx, vy): (f64, f64), home: Coords) -> Option<Approach> {
    let speed_sq = vx * vx + vy * vy;
    if speed_sq < MIN_MOTION_KMH * MIN_MOTION_KMH {
        return None;
    }
    let (rx, ry) = displacement_km(centroid, home);
    let hours = (rx * vx + ry * vy) / speed_sq;
    let minutes = hours * 60.0;
    if minutes <= 0.0 || minutes > MAX_ETA_MINUTES {
        return None;
    }
    Some(Approach { minutes, distance_km: (rx - vx * hours).hypot(ry - vy * hours) })
}

#[derive(Debug, Clone, Copy)]
struct Fix {
    id: u32,
    centroid: Coords,
    /// East/north km/h, smoothed across volumes.
    velocity: Option<(f64, f64)>,
}

/// Carries cell identity between volumes.
///
/// One instance per radar feed, fed every volume in chronological order.
/// Feeding it out of order or skipping volumes costs motion accuracy but
/// cannot corrupt it: a gap wider than [`MAX_TRACK_GAP_MIN`] drops the old
/// tracks rather than inventing an implausible jump.
#[derive(Debug, Default)]
pub struct CellTracker {
    next_id: u32,
    previous: Vec<Fix>,
    last_at: Option<DateTime<Utc>>,
}

impl CellTracker {
    /// Assign identities to one volume's detections and measure their motion.
    ///
    /// Association is greedy nearest-first within a gate that widens with the
    /// time since the last volume. Greedy is enough at [`MAX_CELLS`] cells:
    /// the pathological case for it needs two cores closer to each other than
    /// to their own previous fixes, which is a merge, and a merge has no
    /// correct answer to lose.
    pub fn track(
        &mut self,
        mut cells: Vec<StormCell>,
        at: DateTime<Utc>,
        home: Coords,
    ) -> Vec<StormCell> {
        let gap_min = self
            .last_at
            .map_or(f64::INFINITY, |p| (at - p).num_milliseconds() as f64 / 60_000.0);
        // A first volume gives an infinite gap, which must not become an
        // infinite gate: clearing the tracks and the gate together keeps the
        // two from ever disagreeing about whether association is allowed.
        let usable = (0.0..=MAX_TRACK_GAP_MIN).contains(&gap_min);
        if !usable {
            self.previous.clear();
        }
        let hours = if usable { gap_min / 60.0 } else { 0.0 };
        let gate_km = if usable { MAX_TRACK_SPEED_KMH * hours + TRACK_SLACK_KM } else { 0.0 };

        // Measure from where the track says the storm should be, not from
        // where it last was. In a crowded field the difference decides which
        // core a match belongs to: an unaimed gate hands the identity to
        // whichever blob happens to be nearest the stale position.
        let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
        for (ci, cell) in cells.iter().enumerate() {
            for (pi, prev) in self.previous.iter().enumerate() {
                let expected = match prev.velocity {
                    Some((e, n)) => crate::geo::offset_km(prev.centroid, e * hours, n * hours),
                    None => prev.centroid,
                };
                let d = crate::geo::haversine_km(expected, cell.track_point);
                if d <= gate_km {
                    pairs.push((d, ci, pi));
                }
            }
        }
        pairs.sort_by(|a, b| a.0.total_cmp(&b.0));

        let mut matched = vec![None; cells.len()];
        let mut claimed = vec![false; self.previous.len()];
        for (_, ci, pi) in pairs {
            if matched[ci].is_none() && !claimed[pi] {
                matched[ci] = Some(self.previous[pi]);
                claimed[pi] = true;
            }
        }

        let mut fixes = Vec::with_capacity(cells.len());
        for (cell, prev) in cells.iter_mut().zip(matched) {
            let id = match prev {
                Some(p) => p.id,
                None => {
                    self.next_id += 1;
                    self.next_id
                }
            };
            let velocity = match prev {
                Some(p) if gap_min >= MIN_MOTION_GAP_MIN => {
                    let (east, north) = displacement_km(p.centroid, cell.track_point);
                    let fresh = (east / hours, north / hours);
                    // The gate stretches by TRACK_SLACK_KM to tolerate centroid
                    // jitter, and across a four-minute volume that slack alone
                    // implies 90 km/h. Association may spend it; a velocity
                    // may not, or the readout invents storms moving at 110 kt.
                    if fresh.0.hypot(fresh.1) > MAX_TRACK_SPEED_KMH {
                        p.velocity
                    } else {
                        Some(match p.velocity {
                            Some((pe, pn)) => (
                                pe + (fresh.0 - pe) * MOTION_SMOOTHING,
                                pn + (fresh.1 - pn) * MOTION_SMOOTHING,
                            ),
                            None => fresh,
                        })
                    }
                }
                Some(p) => p.velocity,
                None => None,
            };

            cell.id = id;
            cell.motion = velocity.and_then(motion_from);
            cell.approach = velocity.and_then(|v| approach(cell.track_point, v, home));
            fixes.push(Fix { id, centroid: cell.track_point, velocity });
        }

        self.previous = fixes;
        self.last_at = Some(at);
        cells
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FnField<F: Fn(Coords, RadarProduct) -> Option<f32> + Send + Sync>(F);

    impl<F: Fn(Coords, RadarProduct) -> Option<f32> + Send + Sync> RadarField for FnField<F> {
        fn value_at(&self, p: Coords, product: RadarProduct) -> Option<f32> {
            (self.0)(p, product)
        }
        fn supports(&self, _p: RadarProduct) -> bool {
            true
        }
        fn source_label(&self) -> &str {
            "TEST"
        }
        fn elevation_degrees(&self) -> f32 {
            0.5
        }
    }

    const SITE: Coords = Coords { lat: 36.0, lon: -87.0 };
    const HOME: Coords = Coords { lat: 35.75, lon: -87.0 };

    fn disk(centre: Coords, radius_km: f64, p: Coords) -> bool {
        crate::geo::haversine_km(centre, p) <= radius_km
    }

    #[test]
    fn two_separate_cores_become_two_ranked_cells() {
        let a = Coords { lat: 36.25, lon: -87.3 };
        let b = Coords { lat: 35.6, lon: -86.6 };
        let field = FnField(move |p, product| match product {
            RadarProduct::Reflectivity if disk(a, 12.0, p) => Some(48.0),
            RadarProduct::Reflectivity if disk(b, 12.0, p) => Some(58.0),
            _ => None,
        });
        let cells = scan(&field, SITE, HOME);
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].threat, CellThreat::Intense, "58 dBZ core must rank first");
        assert_eq!(cells[1].threat, CellThreat::Strong);
        assert!(
            crate::geo::haversine_km(cells[0].centroid, b) < 5.0,
            "centroid should land on the core"
        );
    }

    #[test]
    fn weak_echo_and_speckle_are_not_cells() {
        let c = Coords { lat: 36.2, lon: -87.2 };
        let weak = FnField(move |p, product| match product {
            RadarProduct::Reflectivity if disk(c, 15.0, p) => Some(35.0),
            _ => None,
        });
        assert!(scan(&weak, SITE, HOME).is_empty(), "35 dBZ is not convective");

        let speck = FnField(move |p, product| match product {
            RadarProduct::Reflectivity if disk(c, 1.5, p) => Some(60.0),
            _ => None,
        });
        assert!(scan(&speck, SITE, HOME).is_empty(), "one hot sample is speckle");
    }

    /// A couplet is inbound on one flank, outbound on the other. The scan must
    /// combine both signs into a span rather than taking a single maximum.
    #[test]
    fn a_velocity_couplet_upgrades_the_cell_to_rotation() {
        let c = Coords { lat: 36.3, lon: -87.0 };
        let field = FnField(move |p, product| match product {
            RadarProduct::Reflectivity if disk(c, 10.0, p) => Some(50.0),
            RadarProduct::Velocity if disk(c, 10.0, p) => {
                Some(if p.lon > c.lon { 24.0 } else { -24.0 })
            }
            RadarProduct::CorrelationCoefficient if disk(c, 10.0, p) => Some(0.97),
            _ => None,
        });
        let cells = scan(&field, SITE, HOME);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].threat, CellThreat::Rotation);
        assert!((cells[0].rotation_ms.unwrap() - 48.0).abs() < 0.1);
    }

    #[test]
    fn a_correlation_collapse_inside_rotation_is_debris() {
        let c = Coords { lat: 36.3, lon: -87.0 };
        let field = FnField(move |p, product| match product {
            RadarProduct::Reflectivity if disk(c, 10.0, p) => Some(52.0),
            RadarProduct::Velocity if disk(c, 10.0, p) => {
                Some(if p.lon > c.lon { 15.0 } else { -15.0 })
            }
            RadarProduct::CorrelationCoefficient if disk(c, 3.0, p) => Some(0.70),
            RadarProduct::CorrelationCoefficient if disk(c, 10.0, p) => Some(0.97),
            _ => None,
        });
        let cells = scan(&field, SITE, HOME);
        assert_eq!(cells[0].threat, CellThreat::Debris);
    }

    /// A uniform 20 m/s flow across a 200 km line reads inbound at one end
    /// and outbound at the other from pure viewing geometry. Cluster-wide
    /// extreme pairing called that rotation; local pairing must not.
    #[test]
    fn a_squall_lines_opposite_ends_are_not_rotation() {
        let field = FnField(move |p, product| {
            let in_band = (p.lat - 36.3).abs() < 0.06 && (p.lon + 87.0).abs() < 1.5;
            match product {
                RadarProduct::Reflectivity if in_band => Some(46.0),
                RadarProduct::Velocity if in_band => Some((20.0 * (p.lon + 87.0) / 1.5) as f32),
                RadarProduct::CorrelationCoefficient if in_band => Some(0.97),
                _ => None,
            }
        });
        let cells = scan(&field, SITE, HOME);
        assert_eq!(cells.len(), 1);
        assert_eq!(
            cells[0].threat,
            CellThreat::Strong,
            "a smooth cross-line gradient is geometry, not a couplet: {:?}",
            cells[0].rotation_ms
        );
    }

    /// Storm motion is not subtracted from radial velocity, so a mesocyclone
    /// in strong flow can be all-inbound. Sign-free local shear must still
    /// register it; the old opposite-signs requirement returned None here.
    #[test]
    fn an_embedded_all_inbound_mesocyclone_still_registers() {
        let c = Coords { lat: 36.3, lon: -87.0 };
        let field = FnField(move |p, product| match product {
            RadarProduct::Reflectivity if disk(c, 10.0, p) => Some(50.0),
            RadarProduct::Velocity if disk(c, 10.0, p) => {
                Some(if disk(c, 4.0, p) && p.lon > c.lon { -47.0 } else { -5.0 })
            }
            RadarProduct::CorrelationCoefficient if disk(c, 10.0, p) => Some(0.97),
            _ => None,
        });
        let cells = scan(&field, SITE, HOME);
        assert_eq!(cells[0].threat, CellThreat::Rotation);
        assert!((cells[0].rotation_ms.unwrap() - 42.0).abs() < 0.1);
    }

    /// Hail cores run CC 0.7-0.9. One of those on the far side of the same
    /// cluster must not upgrade a couplet to a debris signature.
    #[test]
    fn a_distant_correlation_dip_does_not_make_debris() {
        let couplet = Coords { lat: 36.3, lon: -87.3 };
        let hail = Coords { lat: 36.3, lon: -86.7 };
        let field = FnField(move |p, product| {
            let in_band = (p.lat - 36.3).abs() < 0.06 && (-87.35..=-86.65).contains(&p.lon);
            match product {
                RadarProduct::Reflectivity if in_band => Some(52.0),
                RadarProduct::Velocity if in_band => {
                    Some(if disk(couplet, 3.0, p) { 30.0 } else { -15.0 })
                }
                RadarProduct::CorrelationCoefficient if disk(hail, 4.0, p) => Some(0.75),
                RadarProduct::CorrelationCoefficient if in_band => Some(0.97),
                _ => None,
            }
        });
        let cells = scan(&field, SITE, HOME);
        assert_eq!(cells.len(), 1);
        assert_eq!(
            cells[0].threat,
            CellThreat::Rotation,
            "the collapse is 50 km from the couplet; min_cc={:?}",
            cells[0].min_cc
        );
    }

    #[test]
    fn a_mesocyclone_below_the_old_bar_still_counts_as_rotation() {
        let base = CellStats {
            max_dbz: 55.0,
            rotation_ms: None,
            min_cc: Some(0.97),
            max_vil: None,
            max_echo_top_km: None,
        };

        for span in [25.0_f32, 30.0, 35.0, 39.0] {
            let s = CellStats { rotation_ms: Some(span), ..base.clone() };
            assert_eq!(
                classify(&s),
                CellThreat::Rotation,
                "{span} m/s of shear must not read as a plain intense cell"
            );
            assert!(
                hazards(classify(&s), None, None).contains(&Hazard::Tornado),
                "{span} m/s must earn a tornado hazard letter"
            );
        }

        let quiet = CellStats { rotation_ms: Some(20.0), ..base.clone() };
        assert_eq!(classify(&quiet), CellThreat::Intense, "below the bar stays intense");
    }

    #[test]
    fn debris_still_outranks_bare_rotation_at_the_same_span() {
        let rotating = CellStats {
            max_dbz: 55.0,
            rotation_ms: Some(30.0),
            min_cc: Some(0.97),
            max_vil: None,
            max_echo_top_km: None,
        };
        let debris = CellStats { min_cc: Some(0.70), ..rotating.clone() };
        assert_eq!(classify(&rotating), CellThreat::Rotation);
        assert_eq!(classify(&debris), CellThreat::Debris);
        assert!(CellThreat::Debris > CellThreat::Rotation);
    }

    #[test]
    fn the_nearest_cell_survives_truncation_in_an_outbreak() {
        let field = FnField(move |p, product| match product {
            RadarProduct::Reflectivity => {
                let near_home = disk(Coords { lat: 35.85, lon: -87.0 }, 5.0, p);
                if near_home {
                    return Some(41.0);
                }
                for k in 0..9 {
                    let c = Coords { lat: 36.05, lon: -87.55 + k as f64 * 0.1375 };
                    if disk(c, 4.0, p) {
                        return Some(60.0);
                    }
                }
                None
            }
            _ => None,
        });
        let cells = scan(&field, SITE, HOME);
        let nearest = cells
            .iter()
            .min_by(|a, b| a.distance_km.total_cmp(&b.distance_km))
            .unwrap();
        assert!(
            nearest.distance_km < 20.0,
            "the weak cell beside home must not be truncated away"
        );

        assert_eq!(
            cells.len(),
            MAX_CELLS + 1,
            "rescuing the nearest cell appends it; it must not cost the ranked cell it \
             used to displace, which in an outbreak can be a rotating one"
        );
        assert_eq!(
            cells.iter().filter(|c| c.max_dbz >= 60.0).count(),
            MAX_CELLS,
            "all eight ranked cells survive alongside the rescued one"
        );
    }

    /// A personal alerting tool has no business listing a storm 200 km away.
    #[test]
    fn cells_beyond_seventy_five_km_of_home_are_not_listed() {
        let far = Coords { lat: 36.9, lon: -87.0 };
        let field = FnField(move |p, product| match product {
            RadarProduct::Reflectivity if disk(far, 12.0, p) => Some(62.0),
            _ => None,
        });
        assert!(
            scan(&field, SITE, HOME).is_empty(),
            "a 62 dBZ core 128 km away is not this user's problem yet"
        );
    }

    /// The beam cannot resolve a couplet at long range; a strong local shear
    /// pair found out there is an artifact and must not be called rotation.
    #[test]
    fn shear_beyond_beam_resolution_range_is_not_rotation() {
        let far = Coords { lat: SITE.lat + 2.0, lon: SITE.lon };
        let near = Coords { lat: SITE.lat + 0.7, lon: SITE.lon };
        let couplet = move |c: Coords| {
            FnField(move |p: Coords, product| match product {
                RadarProduct::Reflectivity if disk(c, 10.0, p) => Some(50.0),
                RadarProduct::Velocity if disk(c, 10.0, p) => {
                    Some(if p.lon > c.lon { 24.0 } else { -24.0 })
                }
                RadarProduct::CorrelationCoefficient if disk(c, 10.0, p) => Some(0.97),
                _ => None,
            })
        };

        let far_home = Coords { lat: SITE.lat + 1.6, lon: SITE.lon };
        let far_cells = scan(&couplet(far), SITE, far_home);
        assert_eq!(far_cells.len(), 1);
        assert_eq!(
            far_cells[0].threat,
            CellThreat::Strong,
            "a 220 km couplet is beam artifact, rotation_ms={:?}",
            far_cells[0].rotation_ms
        );
        assert_eq!(far_cells[0].rotation_ms, None);

        let near_cells = scan(&couplet(near), SITE, Coords { lat: SITE.lat + 0.3, lon: SITE.lon });
        assert_eq!(
            near_cells[0].threat,
            CellThreat::Rotation,
            "the same couplet at 78 km is resolvable and must still register"
        );
    }

    fn at_minute(m: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(m * 60, 0).unwrap()
    }

    fn core_at(c: Coords) -> impl RadarField {
        FnField(move |p, product| match product {
            RadarProduct::Reflectivity if disk(c, 10.0, p) => Some(50.0),
            _ => None,
        })
    }

    #[test]
    fn a_core_keeps_its_identity_and_gains_a_measured_motion() {
        let mut tracker = CellTracker::default();
        let start = Coords { lat: 36.1, lon: -87.2 };
        let first = tracker.track(scan(&core_at(start), SITE, HOME), at_minute(0), HOME);
        assert_eq!(first.len(), 1);
        assert!(first[0].motion.is_none(), "one fix cannot show a motion");

        let moved = Coords { lat: start.lat, lon: start.lon + 0.05 };
        let second = tracker.track(scan(&core_at(moved), SITE, HOME), at_minute(5), HOME);
        assert_eq!(second[0].id, first[0].id, "the same storm must keep its id");
        let m = second[0].motion.expect("two fixes measure a motion");
        assert!(
            crate::geo::angular_difference_deg(m.heading_deg, 90.0) < 15.0,
            "the core moved due east, heading reads {}",
            m.heading_deg
        );
        assert!((m.speed_kmh - 54.0).abs() < 15.0, "{}", m.speed_kmh);
    }

    /// Ranking is rebuilt every volume. Identity must survive that, or a
    /// selection silently slides onto a different storm.
    #[test]
    fn a_ranking_change_does_not_move_an_identity_to_another_storm() {
        let west = Coords { lat: 36.1, lon: -87.3 };
        let east = Coords { lat: 36.1, lon: -86.7 };
        let pair = |west_dbz: f32, east_dbz: f32| {
            FnField(move |p, product| match product {
                RadarProduct::Reflectivity if disk(west, 10.0, p) => Some(west_dbz),
                RadarProduct::Reflectivity if disk(east, 10.0, p) => Some(east_dbz),
                _ => None,
            })
        };
        let westward = |c: &&StormCell| c.centroid.lon < -87.0;
        let eastward = |c: &&StormCell| c.centroid.lon > -87.0;

        let mut tracker = CellTracker::default();
        let first = tracker.track(scan(&pair(58.0, 45.0), SITE, HOME), at_minute(0), HOME);
        assert_eq!(first.len(), 2);
        let west_id = first.iter().find(westward).unwrap().id;
        let east_id = first.iter().find(eastward).unwrap().id;
        assert_ne!(west_id, east_id);

        let second = tracker.track(scan(&pair(45.0, 58.0), SITE, HOME), at_minute(5), HOME);
        assert_eq!(second.iter().find(westward).unwrap().id, west_id);
        assert_eq!(second.iter().find(eastward).unwrap().id, east_id);
        assert_ne!(first[0].id, second[0].id, "the top row is now a different storm");
    }

    #[test]
    fn a_jump_no_storm_could_make_starts_a_new_track() {
        let mut tracker = CellTracker::default();
        let first =
            tracker.track(scan(&core_at(Coords { lat: 36.1, lon: -87.3 }), SITE, HOME),
                at_minute(0), HOME);
        // 0.5 degrees of longitude in five minutes is over 500 km/h.
        let second =
            tracker.track(scan(&core_at(Coords { lat: 36.1, lon: -86.8 }), SITE, HOME),
                at_minute(5), HOME);
        assert_ne!(second[0].id, first[0].id);
        assert!(second[0].motion.is_none(), "a fresh track has nothing to measure from");
    }

    /// Position alone does not prove identity. After a long outage the core
    /// sitting where the old one was may be an entirely different storm.
    #[test]
    fn a_long_silence_breaks_the_track_even_where_nothing_moved() {
        let c = Coords { lat: 36.1, lon: -87.2 };
        let mut tracker = CellTracker::default();
        let first = tracker.track(scan(&core_at(c), SITE, HOME), at_minute(0), HOME);
        let second = tracker.track(scan(&core_at(c), SITE, HOME), at_minute(45), HOME);
        assert_ne!(second[0].id, first[0].id);
    }

    #[test]
    fn approach_times_a_closing_storm_and_ignores_a_departing_one() {
        let far = Coords { lat: 36.30, lon: -87.0 };
        let near = Coords { lat: 36.25, lon: -87.0 };

        let mut closing = CellTracker::default();
        closing.track(scan(&core_at(far), SITE, HOME), at_minute(0), HOME);
        let inbound = closing.track(scan(&core_at(near), SITE, HOME), at_minute(5), HOME);
        let a = inbound[0].approach.expect("a storm heading at home has an arrival");
        assert!(a.distance_km < 8.0, "a direct hit reads near zero, got {}", a.distance_km);
        assert!((a.minutes - 50.0).abs() < 15.0, "{}", a.minutes);

        let mut leaving = CellTracker::default();
        leaving.track(scan(&core_at(near), SITE, HOME), at_minute(0), HOME);
        let outbound = leaving.track(scan(&core_at(far), SITE, HOME), at_minute(5), HOME);
        assert!(
            outbound[0].approach.is_none(),
            "a storm already receding has no arrival to report"
        );
    }

    /// Polling can hand back the volume it just delivered. A zero-length step
    /// must not divide the motion by it.
    #[test]
    fn a_repeated_volume_keeps_the_track_and_its_motion_intact() {
        let a = Coords { lat: 36.1, lon: -87.2 };
        let b = Coords { lat: 36.1, lon: -87.15 };
        let mut tracker = CellTracker::default();
        tracker.track(scan(&core_at(a), SITE, HOME), at_minute(0), HOME);
        let moved = tracker.track(scan(&core_at(b), SITE, HOME), at_minute(5), HOME);
        let repeat = tracker.track(scan(&core_at(b), SITE, HOME), at_minute(5), HOME);
        assert_eq!(repeat[0].id, moved[0].id);
        assert_eq!(
            repeat[0].motion, moved[0].motion,
            "a repeat teaches nothing and must change nothing"
        );
    }

    /// Replays the 2013-05-20 Moore, OK supercell out of the S3 archive.
    ///
    /// Synthetic disks translate rigidly and never change shape. Only real
    /// echo shows whether association survives a core that grows, sheds
    /// flanking cells and reforms between volumes, which is the whole claim
    /// tracking makes. Hits the network, so it stays out of the default run.
    /// `cargo test moore -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn tracking_holds_across_the_moore_2013_supercell() {
        use chrono::Timelike;
        use std::collections::HashMap;

        let date = chrono::NaiveDate::from_ymd_opt(2013, 5, 20).unwrap();
        let site = crate::geo::radar_site_by_id("KTLX").expect("KTLX is in the table");
        let home = Coords { lat: 35.3395, lon: -97.4867 };

        // The EF5 was on the ground 19:56-20:35Z.
        let ids: Vec<_> = crate::radar::fetch::archive_ids_on("KTLX", date)
            .await
            .expect("list the archive")
            .into_iter()
            .filter(|id| {
                id.date_time().is_some_and(|t| {
                    (19 * 60 + 50..=20 * 60 + 30).contains(&(t.hour() * 60 + t.minute()))
                })
            })
            .collect();
        assert!(ids.len() >= 5, "expected a run of volumes, got {}", ids.len());

        let mut tracker = CellTracker::default();
        let mut lives: HashMap<u32, Vec<CellMotion>> = HashMap::new();
        let mut volumes = 0usize;

        for id in ids {
            let (at, field) =
                crate::radar::fetch::archived_field(id).await.expect("decode volume");
            let cells = tracker.track(scan(&field, site.coords, home), at, home);
            volumes += 1;
            eprintln!("--- {at}  {} cells", cells.len());
            for c in &cells {
                eprintln!(
                    "  #{:<3} {:<9} {:>3.0} km {:<3} {:>3.0} dBZ  {:<12} {}",
                    c.id,
                    c.threat.label(),
                    c.distance_km,
                    c.bearing,
                    c.max_dbz,
                    match c.motion {
                        Some(m) => format!("{} {:.0} kt", m.compass(), m.speed_kt()),
                        None => "-".to_string(),
                    },
                    match c.approach {
                        Some(a) => format!("nearest {:.0} km in {:.0}m", a.distance_km, a.minutes),
                        None => String::new(),
                    }
                );
                lives.entry(c.id).or_default().extend(c.motion);
            }
        }

        let (id, motions) = lives
            .iter()
            .max_by_key(|(_, m)| m.len())
            .expect("the outbreak produced at least one track");
        eprintln!("longest track #{id}: {} volumes with motion, of {volumes}", motions.len());

        assert!(
            motions.len() >= 3,
            "no cell held an identity across the sequence; longest was {} of {volumes} volumes",
            motions.len()
        );

        let headings: Vec<f64> = motions.iter().map(|m| m.heading_deg).collect();
        let spread = headings
            .iter()
            .flat_map(|a| headings.iter().map(move |b| crate::geo::angular_difference_deg(*a, *b)))
            .fold(0.0f64, f64::max);
        eprintln!("headings {headings:?} spread {spread:.0} deg");
        assert!(spread < 60.0, "a tracked supercell should not swing {spread:.0} deg");

        // No floor: the centroid of a large blob genuinely creeps when the
        // storm regenerates upwind as fast as it advances. The cap is the
        // claim worth holding, because exceeding it means the track jumped
        // to a different cell.
        for (id, motions) in &lives {
            for m in motions {
                assert!(
                    m.speed_kmh <= MAX_TRACK_SPEED_KMH,
                    "track #{id} reported {:.0} kt, above the {:.0} kt cap",
                    m.speed_kt(),
                    MAX_TRACK_SPEED_KMH / crate::geo::KM_PER_KNOT_HOUR
                );
            }
        }
    }

    #[test]
    fn hazard_letters_follow_the_cell_diagnostics() {
        assert_eq!(
            hazards(CellThreat::Rotation, Some(30.0), Some(12.0)),
            vec![Hazard::Tornado, Hazard::Wind, Hazard::Lightning, Hazard::Rain]
        );
        assert_eq!(hazards(CellThreat::Hail, Some(10.0), Some(9.5)),
            vec![Hazard::Hail, Hazard::Lightning, Hazard::Rain]);
        assert_eq!(hazards(CellThreat::Strong, None, Some(5.0)), vec![Hazard::Rain]);
        assert_eq!(
            hazards(CellThreat::Debris, Some(40.0), None),
            vec![Hazard::Tornado, Hazard::Wind, Hazard::Rain]
        );
    }

    #[test]
    fn the_classification_ladder_orders_the_threats() {
        let base = || CellStats {
            max_dbz: 45.0,
            rotation_ms: None,
            min_cc: None,
            max_vil: None,
            max_echo_top_km: None,
        };
        assert_eq!(classify(&base()), CellThreat::Strong);
        assert_eq!(classify(&CellStats { max_dbz: 56.0, ..base() }), CellThreat::Intense);
        assert_eq!(classify(&CellStats { max_vil: Some(50.0), ..base() }), CellThreat::Hail);
        assert_eq!(
            classify(&CellStats { max_echo_top_km: Some(15.0), ..base() }),
            CellThreat::Hail
        );
        assert_eq!(
            classify(&CellStats { rotation_ms: Some(45.0), ..base() }),
            CellThreat::Rotation
        );
        assert_eq!(
            classify(&CellStats { rotation_ms: Some(30.0), min_cc: Some(0.7), ..base() }),
            CellThreat::Debris
        );
        assert_eq!(
            classify(&CellStats { rotation_ms: Some(10.0), min_cc: Some(0.7), ..base() }),
            CellThreat::Strong,
            "a correlation dip without rotation is rain mixture, not debris"
        );
    }

    #[test]
    fn distance_and_bearing_are_measured_from_home() {
        let c = Coords { lat: 36.3, lon: -87.0 };
        let field = FnField(move |p, product| match product {
            RadarProduct::Reflectivity if disk(c, 10.0, p) => Some(45.0),
            _ => None,
        });
        let cells = scan(&field, SITE, HOME);
        assert_eq!(cells[0].bearing, "N", "cell is due north of home");
        assert!((cells[0].distance_km - 61.0).abs() < 6.0, "{}", cells[0].distance_km);
    }
}
