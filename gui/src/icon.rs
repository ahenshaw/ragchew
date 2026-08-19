//! The application icon, drawn rather than stored.
//!
//! Generated so that it comes from the app's own palette — [`crate::colormap`],
//! the same magma the waterfall is painted in — and cannot drift away from it.
//! It costs a few milliseconds once at startup, against carrying a binary in
//! the repository that nothing can check is still the right color.
//!
//! What it shows is what the app shows: four signal traces on a noise floor,
//! frequency down and time across, so a station transmitting is a horizontal
//! streak. They alternate left and right, which is both what two stations
//! working each other look like on the waterfall and what makes the icon read
//! as a conversation — which is the whole of what the app is for.

use crate::colormap::magma;

/// Supersampling factor: everything is drawn at this multiple and box-filtered
/// down. Cheaper to write than analytic coverage, and at 3x past the point
/// where the difference shows at any size an icon is seen at.
const SS: usize = 3;

/// Corner radius of the tile, as a fraction of its side.
const RADIUS: f32 = 0.215;

/// Half-thickness of a trace.
const HALF: f32 = 0.0525;

/// One signal trace: where it sits, how far it runs, and where in the colormap
/// it is drawn from — so the icon carries the palette's range rather than one
/// color out of it.
struct Trace {
    y: f32,
    x0: f32,
    x1: f32,
    heat: f32,
}

const TRACES: [Trace; 4] = [
    Trace { y: 0.235, x0: 0.150, x1: 0.610, heat: 0.62 },
    Trace { y: 0.412, x0: 0.390, x1: 0.850, heat: 0.88 },
    Trace { y: 0.588, x0: 0.150, x1: 0.700, heat: 1.00 },
    Trace { y: 0.765, x0: 0.330, x1: 0.850, heat: 0.72 },
];

/// The icon at `size` square, as RGBA8 rows top to bottom.
pub fn rgba(size: usize) -> Vec<u8> {
    let n = size * SS;
    let mut acc = vec![[0f32; 4]; size * size];
    // Grain is detail, and detail below about 64 pixels is dirt: a tab-strip
    // icon wants the shapes and nothing else. Faded rather than switched off,
    // so there is no size at which the icon changes character.
    let grain = 0.058 * (size as f32 / 128.0).clamp(0.25, 1.0);

    for sy in 0..n {
        for sx in 0..n {
            let (px, py) = ((sx as f32 + 0.5) / n as f32, (sy as f32 + 0.5) / n as f32);

            // The ground: a noise floor in the dark end of the colormap,
            // lifting very slightly down the tile so the square has depth
            // without becoming a gradient anybody would notice.
            let g = noise(px, py);
            let base = magma(0.012 + 0.040 * py + grain * g * g);
            let mut rgb = [base[0] as f32, base[1] as f32, base[2] as f32];

            for t in &TRACES {
                // A little of the floor's grain rides on the trace too, so it
                // belongs to the same picture.
                let w = (streak(px, py, t) * (0.88 + 0.12 * g)).clamp(0.0, 1.0);
                let c = magma(t.heat);
                for k in 0..3 {
                    rgb[k] = rgb[k] * (1.0 - w) + c[k] as f32 * w;
                }
            }

            // The tile's own edge, cut last so no glow can bleed past it.
            let p = &mut acc[(sy / SS) * size + sx / SS];
            for k in 0..3 {
                p[k] += rgb[k];
            }
            p[3] += if inside_tile(px, py) { 255.0 } else { 0.0 };
        }
    }

    let d = (SS * SS) as f32;
    let mut out = Vec::with_capacity(size * size * 4);
    for p in &acc {
        out.extend(p.iter().map(|c| (c / d).round().clamp(0.0, 255.0) as u8));
    }
    out
}

/// How strongly a trace paints a point.
///
/// A signal on a waterfall has no ends — it fades in when the station keys up
/// and out when it stops — and no edge across frequency either, only skirts.
/// Drawn as a hard capsule the icon read as a chat app; drawn as this it reads
/// as the thing the app actually shows. The profile across is a super-Gaussian:
/// a flat top with soft shoulders, because a plain Gaussian left the traces
/// soft the whole way through and they stopped carrying the picture.
fn streak(px: f32, py: f32, t: &Trace) -> f32 {
    let across = (-(((py - t.y) / (HALF * 0.92)).powi(2)).powf(1.7)).exp();
    let fade = HALF * 1.6;
    let along = smoothstep(t.x0 - fade, t.x0 + fade, px)
        * (1.0 - smoothstep(t.x1 - fade, t.x1 + fade, px));
    across * along
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The mottled noise floor a real band always has.
///
/// The single thing that most says "spectrogram" rather than "list of
/// messages". Cells rather than per-pixel speckle, because a waterfall's grain
/// is one transform bin wide, and cells survive being scaled down where
/// speckle turns to mud.
fn noise(px: f32, py: f32) -> f32 {
    const NX: f32 = 104.0;
    const NY: f32 = 128.0;
    let (cx, cy) = ((px * NX) as u32, (py * NY) as u32);
    let mut h = cx.wrapping_mul(0x9E37_79B9) ^ cy.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 15;
    h = h.wrapping_mul(0xC2B2_AE35);
    h ^= h >> 13;
    (h & 0xFFFF) as f32 / 65535.0
}

/// Whether a point is inside the rounded square.
fn inside_tile(px: f32, py: f32) -> bool {
    let (dx, dy) = ((px - 0.5).abs() - (0.5 - RADIUS), (py - 0.5).abs() - (0.5 - RADIUS));
    let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
    outside + dx.max(dy).min(0.0) - RADIUS <= 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(px: &[u8], size: usize, x: usize, y: usize) -> [u8; 4] {
        let i = (y * size + x) * 4;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    }

    /// The icon is a rounded tile: solid in the middle, gone at the corners.
    ///
    /// The way this silently breaks is the mask being dropped or inverted, and
    /// a square icon among rounded ones looks like a mistake rather than a
    /// choice.
    #[test]
    fn the_tile_is_rounded_and_opaque() {
        let size = 128;
        let px = rgba(size);
        assert_eq!(px.len(), size * size * 4);
        assert_eq!(at(&px, size, size / 2, size / 2)[3], 255, "the middle should be solid");
        assert_eq!(at(&px, size, 1, 1)[3], 0, "the corner should be cut away");
        assert_eq!(at(&px, size, size - 2, 1)[3], 0, "every corner");
        assert_eq!(at(&px, size, 1, size - 2)[3], 0);
        assert_eq!(at(&px, size, size - 2, size - 2)[3], 0);
        assert_eq!(at(&px, size, size / 2, 1)[3], 255, "the edge midpoint is not a corner");
    }

    /// There is actually something drawn on it, and it is drawn in the app's
    /// own palette.
    ///
    /// A blank tile is what a broken generator produces, and it is not obvious
    /// from a thumbnail that anything is wrong with it.
    #[test]
    fn the_traces_are_brighter_than_the_floor_and_of_the_palette() {
        let size = 128;
        let px = rgba(size);
        let lum = |c: [u8; 4]| c[0] as u32 + c[1] as u32 + c[2] as u32;

        // The middle trace is the brightest thing in the icon; the gap above it
        // is floor. Both sampled where that trace runs.
        let x = size / 3;
        let on = lum(at(&px, size, x, (0.588 * size as f32) as usize));
        let off = lum(at(&px, size, x, (0.500 * size as f32) as usize));
        assert!(on > off * 3, "trace {on} is not brighter than the floor {off}");

        // And the brightest thing in the icon is the top of the colormap, which
        // is where the palette is most recognizably this app's. Taken as the
        // brightest pixel rather than a chosen one, because the grain rides on
        // the traces too and no single sample is fully saturated.
        let hot = magma(1.0);
        let brightest = px
            .chunks_exact(4)
            .max_by_key(|c| lum([c[0], c[1], c[2], c[3]]))
            .expect("a non-empty icon");
        for k in 0..3 {
            let (a, b) = (brightest[k] as i32, hot[k] as i32);
            assert!((a - b).abs() < 8, "channel {k}: {a} is not the palette's {b}");
        }
    }

    /// Every size is drawn, not scaled from one — an icon asked for at 32
    /// should be composed at 32 rather than be a blur of a larger one.
    #[test]
    fn any_size_comes_out_square_and_sane() {
        for size in [16, 32, 64, 256] {
            let px = rgba(size);
            assert_eq!(px.len(), size * size * 4, "{size} is the wrong length");
            assert_eq!(at(&px, size, size / 2, size / 2)[3], 255, "{size} has no middle");
        }
    }
}
