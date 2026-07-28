//! Reflectivity to RGB.
//!
//! The `Threat` ramp is not the NWS palette. It is deliberately near-monochrome
//! below 35 dBZ and violently saturated above 50, because 50+ dBZ is where
//! large hail and damaging wind live. A palette that renders nuisance rain as
//! vividly as a supercell core wastes the only channel this display has.

use crate::config::Colormap;

pub type Rgb = (u8, u8, u8);

pub const NO_DATA: Rgb = (0, 0, 0);

/// The faintest echo must read clearly against the black background, because
/// ordinary weather lives entirely below 35 dBZ and a ramp that starts near
/// black renders most days as an empty screen.
#[cfg(test)]
const MIN_ECHO_LUMINANCE: f32 = 55.0;

const THREAT_STOPS: &[(f32, Rgb)] = &[
    (5.0, (46, 88, 140)),
    (15.0, (40, 140, 205)),
    (25.0, (32, 196, 178)),
    (35.0, (74, 222, 92)),
    (40.0, (238, 232, 66)),
    (45.0, (255, 170, 24)),
    (50.0, (255, 60, 60)),
    (55.0, (255, 80, 150)),
    (60.0, (255, 70, 235)),
    (65.0, (195, 130, 255)),
    (70.0, (255, 255, 255)),
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

/// Starts well above black for the same reason the threat ramp does. Mono is
/// the fallback for greyscale and colour-blind use, so it needs the visibility
/// floor most, not least.
const MONO_STOPS: &[(f32, Rgb)] = &[(5.0, (62, 62, 62)), (70.0, (255, 255, 255))];

/// `None` means "no echo here", which is rendered as background rather than as
/// a dark colour, so empty sky and weak returns stay distinguishable.
pub fn dbz_to_rgb(dbz: f32, map: Colormap) -> Option<Rgb> {
    match map {
        Colormap::Threat => sample(THREAT_STOPS, dbz),
        Colormap::Nws => sample(NWS_STOPS, dbz),
        Colormap::Mono => sample(MONO_STOPS, dbz),
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

    /// Ordinary weather never leaves the bottom of the scale, so the faintest
    /// echo has to be obviously brighter than empty sky. An earlier ramp put
    /// 5 dBZ at luminance 31 against a black background and rendered most days
    /// as a blank screen.
    #[test]
    fn the_faintest_echo_is_clearly_brighter_than_empty_sky() {
        for map in [Colormap::Threat, Colormap::Mono] {
            let l = luminance(dbz_to_rgb(5.0, map).unwrap());
            assert!(
                l >= MIN_ECHO_LUMINANCE,
                "{map:?} 5 dBZ luminance {l} is too close to the {} background",
                luminance(NO_DATA)
            );
        }
    }

    /// Nws reproduces the published NWS scale, whose whole value is matching
    /// what people already recognise, so it is deliberately exempt from the
    /// visibility floor the other two obey.
    #[test]
    fn the_nws_map_is_left_authentic_rather_than_brightened() {
        assert_eq!(dbz_to_rgb(5.0, Colormap::Nws), Some((4, 233, 231)));
        assert_eq!(dbz_to_rgb(75.0, Colormap::Nws), Some((248, 0, 253)));
    }

    /// Every level in the range ordinary weather actually occupies must be
    /// legible, not just technically distinct from its neighbour.
    #[test]
    fn the_sub_severe_range_stays_legible_throughout() {
        for step in 0..7 {
            let dbz = 5.0 + step as f32 * 5.0;
            let l = luminance(dbz_to_rgb(dbz, Colormap::Threat).unwrap());
            assert!(l >= MIN_ECHO_LUMINANCE, "{dbz} dBZ luminance {l} is too dark");
        }
    }

    /// Brightening the weak end once pushed the severe reds below drizzle:
    /// 55 dBZ sat at luminance 56 while 35 dBZ sat at 181, so hail cores read
    /// as fainter than light rain. Nothing dangerous may be dimmer than the
    /// faintest thing on the scale.
    #[test]
    fn no_severe_level_is_dimmer_than_the_faintest_echo() {
        let floor = luminance(dbz_to_rgb(5.0, Colormap::Threat).unwrap());
        for step in 9..=15 {
            let dbz = step as f32 * 5.0;
            let l = luminance(dbz_to_rgb(dbz, Colormap::Threat).unwrap());
            assert!(
                l >= floor,
                "{dbz} dBZ luminance {l} is below the {floor} of the weakest echo"
            );
        }
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

    /// Prints the ramps as truecolour half-blocks so they can be judged by eye.
    /// `cargo test print_the_ramps -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn print_the_ramps() {
        for (name, map) in [
            ("threat", Colormap::Threat),
            ("nws", Colormap::Nws),
            ("mono", Colormap::Mono),
        ] {
            let mut bar = String::new();
            for step in 0..=75 {
                let dbz = step as f32;
                let (r, g, b) = cell_rgb(Some(dbz), map);
                bar.push_str(&format!("\x1b[38;2;{r};{g};{b}m\u{2588}"));
            }
            println!("{name:>7} \x1b[0m{bar}\x1b[0m");
        }
        println!("{:>7} {}", "dBZ", (0..=75).map(|d| {
            if d % 10 == 0 { char::from_digit((d / 10) as u32, 10).unwrap() } else { ' ' }
        }).collect::<String>());
        println!("\n  luminance at each stop:");
        for step in 0..=14 {
            let dbz = step as f32 * 5.0;
            match dbz_to_rgb(dbz, Colormap::Threat) {
                Some(c) => println!("   {dbz:>4.0} dBZ  {c:>16?}  L={:.0}", luminance(c)),
                None => println!("   {dbz:>4.0} dBZ  {:>16}  (no echo)", "-"),
            }
        }
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
