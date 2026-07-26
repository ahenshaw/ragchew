//! Pure waterfall rendering: a [`Spectrogram`] + a frequency [`Viewport`] → an
//! RGBA image. No windowing here, so it's testable and can be dumped to PNG.
//!
//! Orientation (per the design spec): **frequency is vertical, high at the top;
//! time is horizontal, newest at the LEFT edge and ageing to the right.**

use ragchew::Spectrogram;

use crate::colormap;

/// Visible frequency range (the shared vertical axis; zoom/pan set this).
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub f_lo: f64,
    pub f_hi: f64,
}

/// An RGBA image (row-major, 4 bytes per pixel).
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl Image {
    fn new(width: usize, height: usize) -> Image {
        Image { width, height, rgba: vec![0; width * height * 4] }
    }
    #[inline]
    fn put(&mut self, x: usize, y: usize, c: [u8; 3]) {
        if x < self.width && y < self.height {
            let i = (y * self.width + x) * 4;
            self.rgba[i] = c[0];
            self.rgba[i + 1] = c[1];
            self.rgba[i + 2] = c[2];
            self.rgba[i + 3] = 255;
        }
    }
}

/// Map a signal at `hz` and sample `offset` to a pixel in the rendered image,
/// using the same transform as [`render`]. Returns `None` if outside the view.
pub fn map_point(
    spec: &Spectrogram,
    vp: &Viewport,
    width: usize,
    height: usize,
    hz: f64,
    offset_samples: usize,
) -> Option<(usize, usize)> {
    if hz < vp.f_lo || hz > vp.f_hi || width == 0 || height == 0 {
        return None;
    }
    // frequency -> y (high at top)
    let fy = (vp.f_hi - hz) / (vp.f_hi - vp.f_lo);
    let y = (fy * (height as f64 - 1.0)).round() as usize;
    // time -> x (newest at left). column index j (0 = oldest).
    let n_cols = spec.columns.len().max(1);
    let j = (offset_samples / spec.hop).min(n_cols - 1);
    let x = (((n_cols - 1 - j) as f64) * (width as f64) / (n_cols as f64)).round() as usize;
    Some((x.min(width - 1), y.min(height - 1)))
}

/// Background color for waterfall pixels with no data (before the recording
/// starts / future / off the ends).
const BG: [u8; 3] = [8, 10, 14];

/// Global log-magnitude contrast bounds (55th, 99.5th percentiles) over the
/// whole spectrogram. Precompute once and pass to [`render_window`] for a stable
/// look while the visible window scrolls.
pub fn percentiles(spec: &Spectrogram) -> (f32, f32) {
    let mut vals: Vec<f32> = Vec::new();
    for col in &spec.columns {
        for &m in col {
            vals.push((m + 1e-9).ln());
        }
    }
    if vals.is_empty() {
        return (0.0, 1.0);
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f32| vals[((p * (vals.len() as f32 - 1.0)) as usize).min(vals.len() - 1)];
    (pct(0.55), pct(0.995))
}

/// Render the whole recording (contrast stretched to its own percentiles).
pub fn render(spec: &Spectrogram, vp: &Viewport, width: usize, height: usize) -> Image {
    let n = spec.columns.len() as i64;
    render_window(spec, vp, width, height, n, spec.columns.len().max(1), None)
}

/// Render a scrolling time window ending at column `col_hi` (exclusive, the
/// newest column = "now") and `span_cols` wide. Newest is at the **left** edge,
/// ageing right. Columns outside `[0, n_cols)` render as background, so the view
/// can sit before the start or after the end. `norm` fixes the contrast bounds
/// (see [`percentiles`]); `None` stretches to the window's own data.
pub fn render_window(
    spec: &Spectrogram,
    vp: &Viewport,
    width: usize,
    height: usize,
    col_hi: i64,
    span_cols: usize,
    norm: Option<(f32, f32)>,
) -> Image {
    let mut img = Image::new(width, height);
    if width == 0 || height == 0 || spec.columns.is_empty() || span_cols == 0 {
        for p in img.rgba.chunks_mut(4) {
            p[..3].copy_from_slice(&BG);
            p[3] = 255;
        }
        return img;
    }
    let n_cols = spec.columns.len() as i64;
    let n_bins = spec.n_bins;
    let span = span_cols as i64;

    let to_bin = |hz: f64| ((hz - spec.hz_lo()) / spec.bin_hz).round();
    let b0 = to_bin(vp.f_lo).clamp(0.0, (n_bins - 1) as f64) as usize;
    let b1 = to_bin(vp.f_hi).clamp(0.0, (n_bins - 1) as f64) as usize;
    let (b0, b1) = (b0.min(b1), b0.max(b1));
    let vb = (b1 - b0 + 1) as i64;

    // source column window per output x (newest at left)
    let col_range = |ox: usize| -> (i64, i64) {
        let hi = col_hi - (ox as i64 * span) / width as i64;
        let lo = col_hi - ((ox + 1) as i64 * span) / width as i64;
        (lo, hi.max(lo + 1))
    };
    let bin_range = |oy: usize| -> (usize, usize) {
        let top = b1 as i64 - (oy as i64 * vb) / height as i64;
        let bot = b1 as i64 - ((oy + 1) as i64 * vb) / height as i64;
        (bot.max(0) as usize, top.max(0) as usize)
    };

    let mut grid = vec![f32::NEG_INFINITY; width * height]; // -inf = no data
    let mut vals: Vec<f32> = Vec::new();
    for oy in 0..height {
        let (blo, bhi) = bin_range(oy);
        for ox in 0..width {
            let (clo, chi) = col_range(ox);
            let clo = clo.max(0);
            let chi = chi.min(n_cols);
            if clo >= chi {
                continue; // outside the recording -> stays -inf (bg)
            }
            let mut mx = 0.0f32;
            for j in clo..chi {
                let col = &spec.columns[j as usize];
                for b in blo..=bhi.min(n_bins - 1) {
                    if col[b] > mx {
                        mx = col[b];
                    }
                }
            }
            let lm = (mx + 1e-9).ln();
            grid[oy * width + ox] = lm;
            if norm.is_none() {
                vals.push(lm);
            }
        }
    }

    let (lo, hi) = norm.unwrap_or_else(|| {
        if vals.is_empty() {
            (0.0, 1.0)
        } else {
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let pct = |p: f32| vals[((p * (vals.len() as f32 - 1.0)) as usize).min(vals.len() - 1)];
            (pct(0.55), pct(0.995))
        }
    });
    let span_v = (hi - lo).max(1e-6);

    for oy in 0..height {
        for ox in 0..width {
            let lm = grid[oy * width + ox];
            let c = if lm == f32::NEG_INFINITY {
                BG
            } else {
                colormap::magma(((lm - lo) / span_v).clamp(0.0, 1.0))
            };
            img.put(ox, oy, c);
        }
    }
    img
}
