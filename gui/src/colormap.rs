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

/// Map a normalized value `v` in `[0, 1]` to an RGB color.
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
