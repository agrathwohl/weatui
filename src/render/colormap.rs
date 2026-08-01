//! Reflectivity to RGB.
//!
//! The `Threat` ramp is not the NWS palette. It is deliberately near-monochrome
//! below 35 dBZ and violently saturated above 50, because 50+ dBZ is where
//! large hail and damaging wind live. A palette that renders nuisance rain as
//! vividly as a supercell core wastes the only channel this display has.

use crate::config::Colormap;
use crate::radar::RadarProduct;

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

#[cfg(test)]
pub fn cell_rgb(dbz: Option<f32>, map: Colormap) -> Rgb {
    dbz.and_then(|v| dbz_to_rgb(v, map)).unwrap_or(NO_DATA)
}

/// Inbound green, outbound red, near-zero suppressed. The convention is
/// universal in radar software and inverting it would turn an inbound gust
/// front into an apparently receding one.
/// Storm depth in km. A top above roughly 12 km means a strong updraft, so the
/// ramp turns hot where the tropopause is being approached.
const ECHO_TOP_STOPS: &[(f32, Rgb)] = &[
    (2.0, (60, 90, 150)),
    (5.0, (60, 150, 200)),
    (8.0, (90, 200, 140)),
    (11.0, (230, 220, 70)),
    (14.0, (255, 130, 40)),
    (18.0, (255, 70, 70)),
];

/// Liquid water in the column, kg/m^2. High values with a cold profile are the
/// classic large-hail signature, so the top of the ramp is deliberately loud.
const VIL_STOPS: &[(f32, Rgb)] = &[
    (1.0, (55, 80, 140)),
    (10.0, (60, 160, 190)),
    (25.0, (110, 205, 120)),
    (40.0, (235, 225, 80)),
    (55.0, (255, 140, 45)),
    (70.0, (255, 65, 65)),
];

const VELOCITY_STOPS: &[(f32, Rgb)] = &[
    (-40.0, (0, 255, 120)),
    (-25.0, (0, 200, 90)),
    (-10.0, (0, 130, 60)),
    (-2.0, (40, 60, 50)),
    (2.0, (60, 45, 45)),
    (10.0, (150, 50, 40)),
    (25.0, (220, 60, 50)),
    (40.0, (255, 120, 110)),
];

/// Inverted deliberately: low correlation is the interesting value. A collapse
/// below about 0.8 under a strong echo is a tornado debris signature, so the
/// alarming colour belongs at the bottom of this scale rather than the top.
const CORRELATION_STOPS: &[(f32, Rgb)] = &[
    (0.20, (255, 40, 40)),
    (0.60, (255, 140, 40)),
    (0.80, (230, 220, 60)),
    (0.90, (90, 160, 200)),
    (0.97, (60, 110, 190)),
    (1.05, (40, 70, 150)),
];

const DIFFERENTIAL_STOPS: &[(f32, Rgb)] = &[
    (-2.0, (80, 80, 160)),
    (0.0, (70, 110, 180)),
    (1.0, (80, 200, 130)),
    (2.5, (230, 220, 70)),
    (4.0, (240, 140, 50)),
    (6.0, (240, 60, 60)),
];

const SPECTRUM_WIDTH_STOPS: &[(f32, Rgb)] = &[
    (0.0, (40, 60, 80)),
    (3.0, (60, 140, 170)),
    (6.0, (200, 200, 80)),
    (9.0, (240, 120, 50)),
    (12.0, (250, 60, 60)),
];

/// Reflectivity keeps the user's chosen ramp. The other moments have physical
/// conventions that a preference cannot override without misreading them.
pub fn product_rgb(value: f32, product: RadarProduct, map: Colormap) -> Option<Rgb> {
    if !value.is_finite() {
        return None;
    }
    match product {
        RadarProduct::Reflectivity => dbz_to_rgb(value, map),
        RadarProduct::Velocity => {
            if value.abs() < 1.0 {
                None
            } else {
                sample_clamped(VELOCITY_STOPS, value)
            }
        }
        RadarProduct::CorrelationCoefficient => sample_clamped(CORRELATION_STOPS, value),
        RadarProduct::DifferentialReflectivity => sample_clamped(DIFFERENTIAL_STOPS, value),
        RadarProduct::SpectrumWidth => sample_clamped(SPECTRUM_WIDTH_STOPS, value),
        // Below the first stop these mean "no storm here", not "clamp to the
        // dimmest colour", so they sample rather than clamp.
        RadarProduct::EchoTop => sample(ECHO_TOP_STOPS, value),
        RadarProduct::VerticallyIntegratedLiquid => sample(VIL_STOPS, value),
    }
}

/// Unlike `sample`, values below the first stop clamp to it rather than
/// vanishing. Velocity and correlation have no "no echo" floor; a reading
/// below the scale is still a real measurement.
fn sample_clamped(stops: &[(f32, Rgb)], value: f32) -> Option<Rgb> {
    if !value.is_finite() {
        return None;
    }
    let first = stops.first()?;
    if value <= first.0 {
        return Some(first.1);
    }
    for pair in stops.windows(2) {
        let (lo_v, lo_rgb) = pair[0];
        let (hi_v, hi_rgb) = pair[1];
        if value < hi_v {
            return Some(lerp(lo_rgb, hi_rgb, (value - lo_v) / (hi_v - lo_v)));
        }
    }
    stops.last().map(|s| s.1)
}

pub fn product_cell_rgb(value: Option<f32>, product: RadarProduct, map: Colormap) -> Rgb {
    value
        .and_then(|v| product_rgb(v, product, map))
        .unwrap_or(NO_DATA)
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

    /// Inbound must be green and outbound red. Inverting this reads a storm
    /// approaching as one departing, which is the whole point of the product.
    #[test]
    fn velocity_is_green_inbound_and_red_outbound() {
        let inbound = product_rgb(-30.0, RadarProduct::Velocity, Colormap::Threat).unwrap();
        let outbound = product_rgb(30.0, RadarProduct::Velocity, Colormap::Threat).unwrap();
        assert!(inbound.1 > inbound.0, "inbound should be green-dominant: {inbound:?}");
        assert!(outbound.0 > outbound.1, "outbound should be red-dominant: {outbound:?}");
    }

    #[test]
    fn velocity_near_zero_is_suppressed_so_the_couplet_stands_out() {
        assert_eq!(product_rgb(0.0, RadarProduct::Velocity, Colormap::Threat), None);
        assert_eq!(product_rgb(0.5, RadarProduct::Velocity, Colormap::Threat), None);
        assert!(product_rgb(5.0, RadarProduct::Velocity, Colormap::Threat).is_some());
    }

    #[test]
    fn velocity_beyond_the_scale_clamps_rather_than_vanishing() {
        let extreme = product_rgb(-120.0, RadarProduct::Velocity, Colormap::Threat);
        assert_eq!(extreme, product_rgb(-40.0, RadarProduct::Velocity, Colormap::Threat));
        assert!(extreme.is_some(), "a fast inbound gate is still a measurement");
    }

    /// Low correlation is the alarming value, not the high one: a collapse
    /// under strong reflectivity is lofted debris.
    #[test]
    fn correlation_puts_the_alarming_colour_at_the_bottom() {
        let debris = product_rgb(0.4, RadarProduct::CorrelationCoefficient, Colormap::Threat)
            .unwrap();
        let rain = product_rgb(0.99, RadarProduct::CorrelationCoefficient, Colormap::Threat)
            .unwrap();
        assert!(debris.0 > debris.2, "low CC should be warm: {debris:?}");
        assert!(rain.2 > rain.0, "high CC should be cool: {rain:?}");
    }

    #[test]
    fn every_product_renders_something_across_its_working_range() {
        for (product, samples) in [
            (RadarProduct::Reflectivity, vec![10.0, 35.0, 60.0]),
            (RadarProduct::Velocity, vec![-30.0, -5.0, 5.0, 30.0]),
            (RadarProduct::CorrelationCoefficient, vec![0.3, 0.85, 0.99]),
            (RadarProduct::DifferentialReflectivity, vec![-1.0, 1.0, 5.0]),
            (RadarProduct::SpectrumWidth, vec![1.0, 5.0, 11.0]),
            (RadarProduct::EchoTop, vec![3.0, 9.0, 15.0]),
            (RadarProduct::VerticallyIntegratedLiquid, vec![5.0, 30.0, 65.0]),
        ] {
            for v in samples {
                assert!(
                    product_rgb(v, product, Colormap::Threat).is_some(),
                    "{product:?} produced nothing at {v}"
                );
            }
        }
    }

    #[test]
    fn no_product_paints_a_colour_for_a_nan_reading() {
        for product in RadarProduct::ALL {
            assert_eq!(product_rgb(f32::NAN, *product, Colormap::Threat), None, "{product:?}");
            assert_eq!(
                product_cell_rgb(Some(f32::NAN), *product, Colormap::Threat),
                NO_DATA
            );
            assert_eq!(product_cell_rgb(None, *product, Colormap::Threat), NO_DATA);
        }
    }

    /// Reflectivity is the only product whose ramp is a user preference; the
    /// rest encode physical conventions that a setting must not override.
    #[test]
    fn only_reflectivity_responds_to_the_configured_colormap() {
        let a = product_rgb(30.0, RadarProduct::Reflectivity, Colormap::Threat);
        let b = product_rgb(30.0, RadarProduct::Reflectivity, Colormap::Nws);
        assert_ne!(a, b);

        for product in [RadarProduct::Velocity, RadarProduct::CorrelationCoefficient] {
            let value = if product == RadarProduct::Velocity { 20.0 } else { 0.7 };
            assert_eq!(
                product_rgb(value, product, Colormap::Threat),
                product_rgb(value, product, Colormap::Nws),
                "{product:?} must ignore the colormap setting"
            );
        }
    }

    /// A clear sky reports echo top 0 m and VIL 0 kg/m2 as real values, not
    /// missing data. Painting them would fill every cloudless cell.
    #[test]
    fn zero_echo_top_and_zero_vil_are_invisible() {
        assert_eq!(product_rgb(0.0, RadarProduct::EchoTop, Colormap::Threat), None);
        assert_eq!(
            product_rgb(-0.999, RadarProduct::EchoTop, Colormap::Threat),
            None,
            "negative RETOP is a missing-value sentinel scaled to km"
        );
        assert_eq!(
            product_rgb(0.0, RadarProduct::VerticallyIntegratedLiquid, Colormap::Threat),
            None
        );
        assert_eq!(
            product_rgb(0.4, RadarProduct::VerticallyIntegratedLiquid, Colormap::Threat),
            None
        );
    }

    #[test]
    fn severe_echo_tops_and_vil_paint_hot_colours() {
        let deep = product_rgb(15.0, RadarProduct::EchoTop, Colormap::Threat).unwrap();
        assert!(deep.0 > deep.2, "a 15 km top should be warm: {deep:?}");
        let hail = product_rgb(65.0, RadarProduct::VerticallyIntegratedLiquid, Colormap::Threat)
            .unwrap();
        assert!(hail.0 > hail.2, "65 kg/m2 is a hail signature and should be warm: {hail:?}");
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
