//! Pure waterfall rendering: a [`Spectrogram`] + a frequency [`Viewport`] → an
//! RGBA image. No windowing here, so it's testable and can be dumped to PNG.
//!
//! Orientation (per the design spec): **frequency is vertical, high at the top;
//! time is horizontal, newest at the LEFT edge and aging to the right.**

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
///
/// Estimated from a sample, not from every value. Two percentiles are a summary
/// of a distribution, and a distribution of eight million values is no better
/// summarized than one of a hundred thousand. Live capture recomputes this every
/// second against a spectrogram that reaches 12,000 columns — 7.7 M values,
/// 31 MB — and copying and sorting all of it cost 229 ms in release and 3.3 s in
/// debug, on the UI thread, every time. That is the whole frame budget and more:
/// the window stopped answering. See [`column_step`].
pub fn percentiles(spec: &Spectrogram) -> (f32, f32) {
    let step = column_step(spec.columns.len(), spec.n_bins);
    let mut vals: Vec<f32> = Vec::with_capacity(SAMPLE_TARGET + spec.n_bins);
    for col in spec.columns.iter().step_by(step) {
        vals.extend(col.iter().map(|m| (m + 1e-9).ln()));
    }
    if vals.is_empty() {
        return (0.0, 1.0);
    }
    // Selection, not a sort: only two order statistics are wanted, and the
    // 99.5th can be found in what the 55th left above it.
    let cmp = |a: &f32, b: &f32| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal);
    let idx = |p: f32| ((p * (vals.len() as f32 - 1.0)) as usize).min(vals.len() - 1);
    let (lo_i, hi_i) = (idx(0.55), idx(0.995));
    let (_, lo, rest) = vals.select_nth_unstable_by(lo_i, cmp);
    let lo = *lo;
    let hi = if hi_i > lo_i {
        let (_, hi, _) = rest.select_nth_unstable_by(hi_i - lo_i - 1, cmp);
        *hi
    } else {
        lo
    };
    (lo, hi)
}

/// How many values the contrast estimate settles for. A hundred thousand
/// samples put the 99.5th percentile inside a thousandth of the value the whole
/// spectrogram gives, which is far below anything the eye can see in a color
/// ramp.
const SAMPLE_TARGET: usize = 100_000;

/// A column stride that brings `cols * bins` down to about [`SAMPLE_TARGET`].
///
/// Time is strided; frequency never is. The two axes are not interchangeable
/// here: a signal is a few bins wide and seconds long, so dropping columns
/// costs almost nothing — the same carrier is in the next one — while dropping
/// bins steps straight over the narrow peaks the 99.5th percentile is made of.
/// Measured on the demo band, striding both moved the white point by 4% of the
/// range; striding time alone holds both bounds inside 0.2%.
fn column_step(cols: usize, bins: usize) -> usize {
    let total = cols.saturating_mul(bins);
    if total <= SAMPLE_TARGET || cols == 0 || bins == 0 {
        return 1;
    }
    (total / SAMPLE_TARGET).max(1).min(cols)
}

/// Scroll a rendered waterfall right by `shift_px` pixels and draw the columns
/// that have arrived since, into the space that opens at the left edge.
///
/// Time runs newest-at-the-left, so new data enters there and everything else
/// moves right: the image is a memmove per row plus a strip of new pixels,
/// where a full render is a logarithm and a color-map lookup for every pixel
/// of it. At a 45-second span in a 700-pixel panel that is 800 pixels of work
/// instead of 560,000.
///
/// The caller must have kept everything else identical — the same viewport,
/// size, span and contrast bounds — because each of those remaps pixels the
/// scroll leaves untouched. Returns `false` without touching the image when the
/// shift is too large to have anything left to keep, and the caller should
/// render afresh.
///
/// `col_hi` is the newest column *after* the advance, as [`render_window`]
/// takes it.
pub fn scroll_window(
    img: &mut Image,
    spec: &Spectrogram,
    vp: &Viewport,
    col_hi: i64,
    span_cols: usize,
    norm: (f32, f32),
    shift_px: usize,
) -> bool {
    let (width, height) = (img.width, img.height);
    if width == 0 || height == 0 || spec.columns.is_empty() || span_cols == 0 {
        return false;
    }
    if shift_px >= width {
        return false; // nothing worth keeping
    }
    if shift_px == 0 {
        return true;
    }

    // Everything moves right by `shift_px`, oldest falling off the right edge.
    // Rows are contiguous, so each is its own memmove.
    let stride = width * 4;
    for y in 0..height {
        let row = y * stride;
        img.rgba.copy_within(row..row + (width - shift_px) * 4, row + shift_px * 4);
    }

    let n_cols = spec.columns.len() as i64;
    let n_bins = spec.n_bins;
    let span = span_cols as i64;
    let to_bin = |hz: f64| ((hz - spec.hz_lo()) / spec.bin_hz).round();
    let b0 = to_bin(vp.f_lo).clamp(0.0, (n_bins - 1) as f64) as usize;
    let b1 = to_bin(vp.f_hi).clamp(0.0, (n_bins - 1) as f64) as usize;
    let (b0, b1) = (b0.min(b1), b0.max(b1));
    let vb = (b1 - b0 + 1) as i64;

    let (lo, hi) = norm;
    let inv_span = 1.0 / (hi - lo).max(1e-6);
    for oy in 0..height {
        let top = b1 as i64 - (oy as i64 * vb) / height as i64;
        let bot = b1 as i64 - ((oy + 1) as i64 * vb) / height as i64;
        let (blo, bhi) = (bot.max(0) as usize, top.max(0) as usize);
        for ox in 0..shift_px {
            // The same mapping `render_window` uses, so a scrolled pixel and a
            // rendered one are the same pixel.
            let chi = col_hi - (ox as i64 * span) / width as i64;
            let clo = col_hi - ((ox + 1) as i64 * span) / width as i64;
            let c = match cell(spec, clo.max(0), chi.max(clo + 1).min(n_cols), blo, bhi) {
                Some(lm) => shade(lm, lo, inv_span),
                None => BG,
            };
            img.put(ox, oy, c);
        }
    }
    true
}

/// One pixel's color, from its log magnitude and the contrast bounds.
///
/// Shared by the full render and the scroll for the same reason [`cell`] is:
/// the two must agree pixel for pixel, and the only way to be sure of that is
/// for there to be one piece of code that decides.
///
/// `inv_span` is the reciprocal of the contrast range rather than the range
/// itself, so that the per-pixel work is a multiply. Worth about a millisecond
/// of a redraw at 1200x760 — small beside the table below it, and free.
#[inline]
fn shade(lm: f32, lo: f32, inv_span: f32) -> [u8; 3] {
    let t = ((lm - lo) * inv_span).clamp(0.0, 1.0);
    colormap::lut()[(t * (colormap::LEVELS - 1) as f32 + 0.5) as usize]
}

/// The log-magnitude behind one output pixel: the loudest bin in the block of
/// spectrogram it covers. `None` where that block falls outside the recording.
///
/// Shared by the full render and the scroll so the two cannot disagree about
/// what a pixel is — which is the whole basis of scrolling instead of redrawing.
#[inline]
fn cell(spec: &Spectrogram, clo: i64, chi: i64, blo: usize, bhi: usize) -> Option<f32> {
    if clo >= chi {
        return None;
    }
    let mut mx = 0.0f32;
    for j in clo..chi {
        let col = &spec.columns[j as usize];
        for b in blo..=bhi.min(spec.n_bins - 1) {
            if col[b] > mx {
                mx = col[b];
            }
        }
    }
    Some((mx + 1e-9).ln())
}

/// Render the whole recording (contrast stretched to its own percentiles).
pub fn render(spec: &Spectrogram, vp: &Viewport, width: usize, height: usize) -> Image {
    let n = spec.columns.len() as i64;
    render_window(spec, vp, width, height, n, spec.columns.len().max(1), None)
}

/// Render a scrolling time window ending at column `col_hi` (exclusive, the
/// newest column = "now") and `span_cols` wide. Newest is at the **left** edge,
/// aging right. Columns outside `[0, n_cols)` render as background, so the view
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

    // Source column window per output x (newest at left), worked out once for
    // the whole image rather than once per pixel: it does not depend on the
    // row, and it costs two integer divisions. Two milliseconds of a redraw at
    // 1200x760, which is worth having and is not where the time went — that was
    // the color map, one search of the stop list and three interpolations per
    // pixel, now a table in `colormap`.
    let cols_at: Vec<(i64, i64)> = (0..width)
        .map(|ox| {
            let hi = col_hi - (ox as i64 * span) / width as i64;
            let lo = col_hi - ((ox + 1) as i64 * span) / width as i64;
            (lo, hi.max(lo + 1))
        })
        .collect();
    let bin_range = |oy: usize| -> (usize, usize) {
        let top = b1 as i64 - (oy as i64 * vb) / height as i64;
        let bot = b1 as i64 - ((oy + 1) as i64 * vb) / height as i64;
        (bot.max(0) as usize, top.max(0) as usize)
    };

    // The logarithm, once per spectrogram cell in view instead of once per
    // pixel — worth it exactly when a cell covers more than a pixel, which is
    // what zooming in does. Zoomed out the opposite holds: several cells fall
    // under one pixel, the reduction below takes their maximum, and one
    // logarithm per pixel is the cheaper of the two. So it is measured rather
    // than assumed, and the answer is a `max` either way: taking the maximum of
    // the logarithms and taking the logarithm of the maximum are the same
    // number to the last bit, since the logarithm only ever increases.
    //
    // The cached rectangle is what the loops below can actually reach, which is
    // a little more than the viewport: a row's bin range runs one bin under
    // `b0` at the bottom edge, and a column range one past `col_hi` where a
    // pixel is narrower than a column.
    let (vis_lo, vis_hi) = ((col_hi - span).max(0), (col_hi + 1).min(n_cols));
    let (bmin, bmax) = (b0.saturating_sub(1), b1);
    let vis_cols = (vis_hi - vis_lo).max(0) as usize;
    let vis_bins = bmax - bmin + 1;
    let logs: Option<Vec<f32>> = (vis_cols > 0 && vis_cols * vis_bins <= width * height)
        .then(|| {
            let mut v = Vec::with_capacity(vis_cols * vis_bins);
            for j in vis_lo..vis_hi {
                let col = &spec.columns[j as usize];
                v.extend(col[bmin..=bmax].iter().map(|m| (m + 1e-9).ln()));
            }
            v
        });

    let mut grid = vec![f32::NEG_INFINITY; width * height]; // -inf = no data
    let mut vals: Vec<f32> = Vec::new();
    for oy in 0..height {
        let (blo, bhi) = bin_range(oy);
        for (ox, &(clo, chi)) in cols_at.iter().enumerate() {
            let (clo, chi) = (clo.max(0), chi.min(n_cols));
            let cached = logs.as_ref().map(|l| {
                block_max(clo, chi, blo, bhi.min(bmax), |j, b| {
                    l[(j - vis_lo) as usize * vis_bins + (b - bmin)]
                })
            });
            let Some(lm) = cached.unwrap_or_else(|| cell(spec, clo, chi, blo, bhi)) else {
                continue; // outside the recording -> stays -inf (bg)
            };
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
    let inv_span = 1.0 / (hi - lo).max(1e-6);

    // Written a row at a time rather than a pixel at a time: every pixel here
    // is inside the image by construction, and `Image::put` pays for a bounds
    // check to prove it half a million times.
    for (row, lms) in img.rgba.chunks_exact_mut(width * 4).zip(grid.chunks_exact(width)) {
        for (px, &lm) in row.chunks_exact_mut(4).zip(lms) {
            let c = if lm == f32::NEG_INFINITY { BG } else { shade(lm, lo, inv_span) };
            px[0] = c[0];
            px[1] = c[1];
            px[2] = c[2];
            px[3] = 255;
        }
    }
    img
}

/// The largest value in a block of spectrogram, by whatever `at` reads out of
/// it. `None` where the block is empty.
#[inline]
fn block_max<F: Fn(i64, usize) -> f32>(
    clo: i64,
    chi: i64,
    blo: usize,
    bhi: usize,
    at: F,
) -> Option<f32> {
    if clo >= chi || blo > bhi {
        return None;
    }
    let mut mx = f32::NEG_INFINITY;
    for j in clo..chi {
        for b in blo..=bhi {
            let v = at(j, b);
            if v > mx {
                mx = v;
            }
        }
    }
    Some(mx)
}

#[cfg(test)]
mod scroll_tests {
    use super::*;
    use ragchew::spectrogram::WINDOW;

    /// Scrolling must be indistinguishable from redrawing.
    ///
    /// The whole justification for the scroll is that it produces the image a
    /// full render would have produced, so this walks a rendered waterfall
    /// forward one pixel at a time and compares every step against a fresh
    /// render at the same column — across panel widths that divide the span
    /// evenly and widths that do not, which is where an off-by-one in the
    /// pixel-to-column mapping would show.
    #[test]
    fn scrolling_matches_redrawing() {
        let hop = WINDOW / 4;
        let samples = crate::demo::synth();
        let mut spec = Spectrogram::compute(&samples, 0.0, 4000.0, hop);
        // Long enough to scroll through: the band repeated, which is fine here
        // because what is being compared is two renderings of the same data.
        while spec.columns.len() < 4000 {
            let more = Spectrogram::compute(&samples, 0.0, 4000.0, hop);
            spec.columns.extend(more.columns);
        }
        let full_band = Viewport { f_lo: 0.0, f_hi: 4000.0 };
        let zoomed = Viewport { f_lo: 1200.0, f_hi: 1800.0 };
        let norm = percentiles(&spec);

        // Every width here divides its span exactly, which is the condition
        // under which a column of new data is a whole pixel of travel.
        //
        // The comparison is worth more than it looks: only `render_window`
        // caches the logarithms, so every pair of images here is also the
        // cached path checked against the uncached one. The last size is there
        // to make sure both regimes are covered — it is the only one whose
        // spectrogram cells are fewer than its pixels, which is the test the
        // cache turns on.
        for (w, h, span) in [
            (375usize, 200usize, 1125usize), // 3 columns per pixel
            (225, 120, 1125),                // 5
            (125, 100, 1125),                // 9
            (80, 120, 1120),                 // 14, the panel at its narrowest
            (256, 128, 1024),                // a power-of-two ratio
            (250, 100, 1000),                // 4
            (200, 700, 200),                 // one column per pixel: cache on
        ] {
            for vp in [&full_band, &zoomed] {
                let step = (span / w).max(1) as i64;
                let base = 2000i64;
                let mut img = render_window(&spec, vp, w, h, base, span, Some(norm));
                for k in 1..=6i64 {
                    let col = base + k * step;
                    assert!(
                        scroll_window(&mut img, &spec, vp, col, span, norm, 1),
                        "scroll refused at w={w} span={span}"
                    );
                    let fresh = render_window(&spec, vp, w, h, col, span, Some(norm));
                    let differing = img
                        .rgba
                        .chunks(4)
                        .zip(fresh.rgba.chunks(4))
                        .filter(|(a, b)| a != b)
                        .count();
                    assert_eq!(
                        differing, 0,
                        "w={w} h={h} span={span} step {k}: {differing} pixels differ from a redraw"
                    );
                }
            }
        }
    }

    /// A shift with nothing left to keep is refused rather than silently
    /// leaving a stale image behind.
    #[test]
    fn an_oversized_scroll_is_refused() {
        let spec = Spectrogram::compute(&crate::demo::synth(), 0.0, 4000.0, WINDOW / 4);
        let vp = Viewport { f_lo: 0.0, f_hi: 4000.0 };
        let norm = percentiles(&spec);
        let mut img = render_window(&spec, &vp, 64, 32, 2000, 500, Some(norm));
        let before = img.rgba.clone();
        assert!(!scroll_window(&mut img, &spec, &vp, 2600, 500, norm, 64));
        assert!(!scroll_window(&mut img, &spec, &vp, 2600, 500, norm, 999));
        assert_eq!(img.rgba, before, "a refused scroll must not touch the image");
        // and a zero-pixel scroll is a no-op that succeeds
        assert!(scroll_window(&mut img, &spec, &vp, 2000, 500, norm, 0));
        assert_eq!(img.rgba, before);
    }
}
