//! A small perceptual colormap (magma-like) for waterfall magnitudes.

// (t, r, g, b) control points, t in 0..1, approximating matplotlib "magma".
const STOPS: &[(f32, [u8; 3])] = &[
    (0.00, [0, 0, 4]),
    (0.15, [40, 11, 84]),
    (0.35, [101, 21, 110]),
    (0.50, [159, 42, 99]),
    (0.65, [212, 72, 66]),
    (0.80, [245, 125, 21]),
    (0.90, [250, 193, 39]),
    (1.00, [252, 255, 164]),
];

/// Entries in the sampled colormap.
///
/// 256 was the obvious size and measured three values out of 255 away from the
/// continuous ramp at its steepest. A thousand costs three kilobytes, still
/// inside L1 next to everything else a redraw touches, measures no slower, and
/// brings that to one — which is nothing an eye or a screen resolves, and now
/// nothing an assertion would either.
pub const LEVELS: usize = 1024;

/// The colormap sampled at [`LEVELS`] points, built once.
///
/// [`magma`] is a search through the stop list and three interpolations, which
/// is nothing until it runs for every pixel of a waterfall: at 1200x760 it
/// measured 34 ms of a 61 ms redraw, and a redraw happens on every pan, zoom
/// and resize. Sampling it costs 256 of those calls once and turns the
/// per-pixel work into an index.
pub fn lut() -> &'static [[u8; 3]; LEVELS] {
    static LUT: std::sync::OnceLock<[[u8; 3]; LEVELS]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [[0u8; 3]; LEVELS];
        for (i, c) in t.iter_mut().enumerate() {
            *c = magma(i as f32 / (LEVELS - 1) as f32);
        }
        t
    })
}

/// Map a normalized value `v` in `[0, 1]` to an RGB color.
///
/// The definition of the ramp, and what [`lut`] is sampled from. Drawing goes
/// through the table; this is for the few callers that want a color at a value
/// rather than a color per pixel — [`crate::icon`], and the table itself.
pub fn magma(v: f32) -> [u8; 3] {
    let v = v.clamp(0.0, 1.0);
    let mut i = 0;
    while i + 1 < STOPS.len() && v > STOPS[i + 1].0 {
        i += 1;
    }
    let (t0, c0) = STOPS[i];
    let (t1, c1) = STOPS[(i + 1).min(STOPS.len() - 1)];
    let f = if t1 > t0 { (v - t0) / (t1 - t0) } else { 0.0 };
    [
        lerp(c0[0], c1[0], f),
        lerp(c0[1], c1[1], f),
        lerp(c0[2], c1[2], f),
    ]
}

#[inline]
fn lerp(a: u8, b: u8, f: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * f).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is the ramp, to within a value out of 255.
    ///
    /// Drawing goes through [`lut`] and everything else — the icon, any future
    /// caller wanting a color at a value — goes through [`magma`], so the two
    /// have to agree. A step of the table is a step of 1/1023 in the input, and
    /// the steepest leg of the ramp moves a channel by about 0.6 of a value
    /// over that, which is what this holds it to.
    #[test]
    fn the_table_matches_the_ramp_it_samples() {
        let lut = lut();
        let mut worst = 0i32;
        for i in 0..=10_000 {
            let v = i as f32 / 10_000.0;
            let want = magma(v);
            let got = lut[(v * (LEVELS - 1) as f32 + 0.5) as usize];
            for k in 0..3 {
                worst = worst.max((want[k] as i32 - got[k] as i32).abs());
            }
        }
        assert!(worst <= 1, "the table is {worst} values off the ramp");
    }

    /// Both ends are the ends of the ramp, since those are what a saturated
    /// signal and an empty band are painted with.
    #[test]
    fn the_ends_are_exact() {
        assert_eq!(lut()[0], magma(0.0));
        assert_eq!(lut()[LEVELS - 1], magma(1.0));
    }
}
