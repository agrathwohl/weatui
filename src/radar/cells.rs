//! Storm cell identification.
//!
//! A poor man's SCIT: cluster the composite reflectivity field into connected
//! cores, then interrogate each core across every moment the volume carries.
//! The point is not to out-analyse the NWS — warnings remain the authority —
//! but to tell the user *which* blob on their screen deserves attention and
//! why, before a warning exists.

use crate::geo::Coords;
use crate::radar::{RadarField, RadarProduct};

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
/// Cells farther than this from home are noise for a personal alerting tool:
/// nothing 200 km away is "about to arrive", and listing it buries the storm
/// that is.
const MAX_CELL_DISTANCE_KM: f64 = 75.0;

/// Thresholds for the threat ladder, most severe first.
///
/// Rotation evidence is the largest velocity difference between two samples
/// of the same core no more than [`LOCAL_RADIUS_CELLS`] apart — local shear,
/// sign-free, so a mesocyclone embedded in strong one-signed flow still
/// registers, while a long squall line whose opposite ends merely point
/// different ways along the beam does not. Known limits: Level II velocity
/// is not dealiased here, so folding can understate a violent couplet or
/// sharpen a fold boundary into false shear; and VIL / echo top are floors,
/// not totals, because the beam only samples part of the column.
const TDS_MAX_CC: f32 = 0.85;
const TDS_MIN_ROTATION: f32 = 25.0;
const ROTATION_SPAN_MS: f32 = 40.0;
const HAIL_VIL: f32 = 45.0;
/// ~6-7 km at [`STEP_DEG`] spacing: the scale of a couplet plus grid slack.
const LOCAL_RADIUS_CELLS: i64 = 3;
/// A 1-degree beam is ~2.6 km wide at 150 km and ~4.6 km at 260 km — wider
/// than the couplet it would need to resolve, and ~3+ km above the ground.
/// Beyond this range a shear pair is an artifact, so rotation and debris
/// claims are suppressed rather than shown as "rotation 262 km away".
const ROTATION_MAX_RANGE_KM: f64 = 150.0;
const HAIL_ECHO_TOP_KM: f32 = 14.0;
const INTENSE_DBZ: f32 = 55.0;

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

#[derive(Debug, Clone)]
pub struct StormCell {
    pub centroid: Coords,
    pub max_dbz: f32,
    pub rotation_ms: Option<f32>,
    pub min_cc: Option<f32>,
    pub max_vil: Option<f32>,
    pub max_echo_top_km: Option<f32>,
    pub distance_km: f64,
    pub bearing: &'static str,
    pub threat: CellThreat,
}

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
    if s.rotation_ms.is_some_and(|r| r >= ROTATION_SPAN_MS) {
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

        let mut shear: Option<f32> = None;
        let mut shear_centre: Option<(i64, i64)> = None;
        for (&(ix, iy), &va) in &velocity {
            if crate::geo::haversine_km(site, at(ix as usize, iy as usize))
                > ROTATION_MAX_RANGE_KM
            {
                continue;
            }
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
        let centroid = Coords { lat: wlat / weight, lon: wlon / weight };
        let distance_km = crate::geo::haversine_km(home, centroid);
        if distance_km > MAX_CELL_DISTANCE_KM {
            continue;
        }
        cells.push(StormCell {
            centroid,
            max_dbz,
            rotation_ms: stats.rotation_ms,
            min_cc: stats.min_cc,
            max_vil: stats.max_vil,
            max_echo_top_km: stats.max_echo_top_km,
            distance_km,
            bearing: crate::geo::compass_bearing(home, centroid),
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
            let keep = cells.remove(nearest);
            cells.truncate(MAX_CELLS - 1);
            cells.push(keep);
        } else {
            cells.truncate(MAX_CELLS);
        }
    }
    cells
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
        assert_eq!(cells.len(), MAX_CELLS);
        let nearest = cells
            .iter()
            .min_by(|a, b| a.distance_km.total_cmp(&b.distance_km))
            .unwrap();
        assert!(
            nearest.distance_km < 20.0,
            "the weak cell beside home must not be truncated away"
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
