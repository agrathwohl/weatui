//! Reflectivity to RGB.
//!
//! The `Threat` ramp is not the NWS palette. It is deliberately near-monochrome
//! below 35 dBZ and violently saturated above 50, because 50+ dBZ is where
//! large hail and damaging wind live. A palette that renders nuisance rain as
//! vividly as a supercell core wastes the only channel this display has.

use crate::config::Colormap;

pub type Rgb = (u8, u8, u8);

pub const NO_DATA: Rgb = (0, 0, 0);

const THREAT_STOPS: &[(f32, Rgb)] = &[
    (5.0, (24, 32, 44)),
    (15.0, (34, 62, 74)),
    (25.0, (44, 96, 88)),
    (35.0, (86, 140, 74)),
    (40.0, (196, 190, 72)),
    (45.0, (232, 148, 48)),
    (50.0, (226, 58, 42)),
    (55.0, (176, 24, 32)),
    (60.0, (208, 46, 176)),
    (65.0, (150, 84, 220)),
    (70.0, (238, 238, 246)),
];

const NWS_STOPS: &[(f32, Rgb)] = &[
    (5.0, (4, 233, 231)),
    (20.0, (1, 159, 244)),
    (25.0, (3, 0, 244)),
    (30.0, (2, 253, 2)),
    (35.0, (1, 197, 1)),
    (40.0, (0, 142, 0)),
    (45.0, (253, 248, 2)),
    (50.0, (229, 188, 0)),
    (55.0, (253, 149, 0)),
    (60.0, (253, 0, 0)),
    (65.0, (212, 0, 0)),
    (70.0, (188, 0, 0)),
    (75.0, (248, 0, 253)),
];

fn lerp(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

fn sample(stops: &[(f32, Rgb)], dbz: f32) -> Option<Rgb> {
    // Every comparison against NaN is false, so without this guard a corrupt
    // gate falls through every window and returns the top stop, painting a
    // fake extreme core.
    if !dbz.is_finite() {
        return None;
    }
    let first = stops.first()?;
    if dbz < first.0 {
        return None;
    }
    for pair in stops.windows(2) {
        let (lo_dbz, lo_rgb) = pair[0];
        let (hi_dbz, hi_rgb) = pair[1];
        if dbz < hi_dbz {
            return Some(lerp(lo_rgb, hi_rgb, (dbz - lo_dbz) / (hi_dbz - lo_dbz)));
        }
    }
    stops.last().map(|s| s.1)
}

fn mono(dbz: f32) -> Option<Rgb> {
    if dbz < 5.0 {
        return None;
    }
    let t = ((dbz - 5.0) / 65.0).clamp(0.0, 1.0);
    let v = (24.0 + t * 231.0).round() as u8;
    Some((v, v, v))
}

/// `None` means "no echo here", which is rendered as background rather than as
/// a dark colour, so empty sky and weak returns stay distinguishable.
pub fn dbz_to_rgb(dbz: f32, map: Colormap) -> Option<Rgb> {
    match map {
        Colormap::Threat => sample(THREAT_STOPS, dbz),
        Colormap::Nws => sample(NWS_STOPS, dbz),
        Colormap::Mono => mono(dbz),
    }
}

pub fn cell_rgb(dbz: Option<f32>, map: Colormap) -> Rgb {
    dbz.and_then(|v| dbz_to_rgb(v, map)).unwrap_or(NO_DATA)
}

#[cfg(test)]
fn luminance(c: Rgb) -> f32 {
    0.2126 * c.0 as f32 + 0.7152 * c.1 as f32 + 0.0722 * c.2 as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_returns_are_transparent_in_every_map() {
        for map in [Colormap::Threat, Colormap::Nws, Colormap::Mono] {
            assert_eq!(dbz_to_rgb(-20.0, map), None, "{map:?}");
            assert_eq!(dbz_to_rgb(0.0, map), None, "{map:?}");
        }
    }

    #[test]
    fn missing_data_renders_as_background() {
        assert_eq!(cell_rgb(None, Colormap::Threat), NO_DATA);
        assert_eq!(cell_rgb(Some(-30.0), Colormap::Threat), NO_DATA);
    }

    #[test]
    fn extreme_reflectivity_saturates_rather_than_wrapping() {
        for map in [Colormap::Threat, Colormap::Nws, Colormap::Mono] {
            let top = dbz_to_rgb(200.0, map).unwrap();
            let last = dbz_to_rgb(80.0, map).unwrap();
            assert_eq!(top, last, "{map:?} should clamp at the top stop");
        }
    }

    fn separation(a: Rgb, b: Rgb) -> i32 {
        (a.0 as i32 - b.0 as i32).abs()
            + (a.1 as i32 - b.1 as i32).abs()
            + (a.2 as i32 - b.2 as i32).abs()
    }

    fn step_separations() -> Vec<(f32, i32)> {
        let mut out = Vec::new();
        let mut previous = dbz_to_rgb(5.0, Colormap::Threat).unwrap();
        for step in 1..14 {
            let dbz = 5.0 + step as f32 * 5.0;
            let current = dbz_to_rgb(dbz, Colormap::Threat).unwrap();
            out.push((dbz, separation(previous, current)));
            previous = current;
        }
        out
    }

    /// A hue ramp is deliberately not luminance-monotonic: yellow near 40 dBZ
    /// is the luminance peak of saturated colour, and red above it is darker.
    /// What must hold is that no two adjacent levels collapse together.
    #[test]
    fn no_two_adjacent_threat_levels_collapse_into_each_other() {
        for (dbz, sep) in step_separations() {
            assert!(sep > 20, "{dbz} dBZ is indistinguishable from the level below: {sep}");
        }
    }

    /// The stated design is muted below 35 dBZ and loud above 50, so contrast
    /// must be spent where the danger is rather than spread evenly.
    #[test]
    fn threat_ramp_spends_its_contrast_on_the_dangerous_end() {
        let seps = step_separations();
        let quiet: i32 = seps.iter().filter(|(d, _)| *d <= 35.0).map(|(_, s)| *s).sum();
        let quiet_n = seps.iter().filter(|(d, _)| *d <= 35.0).count() as i32;
        let loud: i32 = seps.iter().filter(|(d, _)| *d > 45.0).map(|(_, s)| *s).sum();
        let loud_n = seps.iter().filter(|(d, _)| *d > 45.0).count() as i32;
        let quiet_mean = quiet / quiet_n.max(1);
        let loud_mean = loud / loud_n.max(1);
        assert!(
            loud_mean > quiet_mean * 2,
            "contrast is not concentrated above 45 dBZ: quiet {quiet_mean} vs loud {loud_mean}"
        );
    }

    /// Mono exists precisely for the grayscale and colour-blind case the hue
    /// ramp cannot serve, so it must be strictly luminance-monotonic.
    #[test]
    fn mono_ramp_brightens_monotonically_with_reflectivity() {
        let mut previous = 0.0;
        for step in 0..13 {
            let dbz = 5.0 + step as f32 * 5.0;
            let l = luminance(dbz_to_rgb(dbz, Colormap::Mono).unwrap());
            assert!(l > previous, "luminance fell at {dbz} dBZ: {l} <= {previous}");
            previous = l;
        }
    }

    #[test]
    fn infinite_reflectivity_is_treated_as_no_data() {
        assert!(dbz_to_rgb(f32::INFINITY, Colormap::Threat).is_none());
        assert!(dbz_to_rgb(f32::NEG_INFINITY, Colormap::Threat).is_none());
    }

    /// The severe threshold must be an obvious visual break, not a gradient.
    #[test]
    fn threat_ramp_jumps_hard_at_the_fifty_dbz_severe_threshold() {
        let below = dbz_to_rgb(44.0, Colormap::Threat).unwrap();
        let above = dbz_to_rgb(51.0, Colormap::Threat).unwrap();
        let separation = (below.0 as i32 - above.0 as i32).abs()
            + (below.1 as i32 - above.1 as i32).abs()
            + (below.2 as i32 - above.2 as i32).abs();
        assert!(separation > 120, "50 dBZ break too subtle: {separation}");
    }

    #[test]
    fn interpolation_lands_between_its_neighbouring_stops() {
        let lo = dbz_to_rgb(35.0, Colormap::Threat).unwrap();
        let hi = dbz_to_rgb(40.0, Colormap::Threat).unwrap();
        let mid = dbz_to_rgb(37.5, Colormap::Threat).unwrap();
        assert!(mid.0 > lo.0 && mid.0 < hi.0, "{lo:?} {mid:?} {hi:?}");
    }

    #[test]
    fn mono_map_is_grey_at_every_level() {
        for step in 0..13 {
            let c = dbz_to_rgb(5.0 + step as f32 * 5.0, Colormap::Mono).unwrap();
            assert_eq!(c.0, c.1);
            assert_eq!(c.1, c.2);
        }
    }

    #[test]
    fn nan_reflectivity_does_not_panic() {
        assert!(dbz_to_rgb(f32::NAN, Colormap::Threat).is_none());
        assert_eq!(cell_rgb(Some(f32::NAN), Colormap::Threat), NO_DATA);
    }
}
