//! Shared panel geometry so the headless PNG validator and the egui app agree
//! on where things go: panel rectangles, the frequency→y mapping, and the
//! horizontal–diagonal–horizontal leader line across the connector strip.

use crate::waterfall::Viewport;

/// Pixel geometry of the three regions: text panel (left), connector strip,
/// waterfall (right). The strip's left edge is the text panel's right edge; the
/// strip's right edge is the waterfall's left edge (which is "now").
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub width: f32,
    pub height: f32,
    pub waterfall_w: f32,
    pub strip_w: f32,
}

impl Geometry {
    /// Right edge of the text panel = left edge of the strip.
    pub fn strip_x0(&self) -> f32 {
        self.width - self.waterfall_w - self.strip_w
    }
    /// Right edge of the strip = left edge of the waterfall ("now").
    pub fn strip_x1(&self) -> f32 {
        self.width - self.waterfall_w
    }
    /// Left edge of the waterfall.
    pub fn waterfall_x0(&self) -> f32 {
        self.width - self.waterfall_w
    }

    /// Map a frequency to a vertical pixel (high frequency at the top).
    pub fn hz_to_y(&self, vp: &Viewport, hz: f64) -> f32 {
        let f = (vp.f_hi - hz) / (vp.f_hi - vp.f_lo);
        (f as f32) * (self.height - 1.0)
    }

    /// True when `hz` is within the visible band.
    pub fn visible(&self, vp: &Viewport, hz: f64) -> bool {
        hz >= vp.f_lo && hz <= vp.f_hi
    }
}

/// The 4 points of a leader line across the strip: from the text row (`y_row`,
/// at the strip's left edge) to the channel's live frequency (`y_freq`, at the
/// strip's right edge / waterfall). Horizontal–diagonal–horizontal.
pub fn leader(g: &Geometry, y_row: f32, y_freq: f32) -> [(f32, f32); 4] {
    let x0 = g.strip_x0();
    let x1 = g.strip_x1();
    let k = ((x1 - x0) * 0.28).clamp(2.0, 24.0);
    [(x0, y_row), (x0 + k, y_row), (x1 - k, y_freq), (x1, y_freq)]
}
