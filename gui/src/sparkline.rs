//! A station's signal-to-noise, drawn small.
//!
//! Two numbers — what it is now and the best it has been — cannot tell a steady
//! path from one swinging twenty dB on a ninety-second cycle. They are the same
//! two numbers either way, and the difference decides whether the report you are
//! about to send means anything, whether to ask for the repeat now or wait for
//! the peak, and whether the contact is worth staying with. Shape is the thing
//! neither scalar carries, and shape is what this draws.
//!
//! Four decisions make it honest rather than decorative:
//!
//! * **Silence is not zero.** SNR exists only where a decode was measured. A
//!   line drawn across the gap between two overs is a fade that never happened,
//!   so the series is cut into runs at any gap longer than the mode's own
//!   idea of a pause and each run is drawn on its own.
//! * **The scale does not chase the data.** Free autoscaling turns a two dB
//!   wiggle into a collapse. The range never narrows below [`MIN_SPAN_DB`], so
//!   a flat path looks flat.
//! * **The line that matters is the mode's, not zero.** A dashed line at
//!   roughly where this mode stops copying turns the picture from "how loud" to
//!   "how much margin is left" — and it appears only when the margin is small
//!   enough to be worth looking at. See [`copy_floor_db`].
//! * **Dense series are drawn as an envelope, not a decimated line.** PSK
//!   measures every character: thousands of samples into seventy pixels. Taking
//!   one point per column would alias the fade; the extremes in each column are
//!   drawn as a bar, so the depth of the flutter survives the resolution.
//!
//! Nothing here is smoothed. `measure_snr` works off a short window, so a
//! couple of dB of the scatter is the estimator rather than the path — but
//! flutter is real, diagnostic, and lives in exactly the band a filter would
//! take out.

use eframe::egui::{self, Color32, Pos2, Rect, Stroke};

use ragchew::protocol::ModeId;
use ragchew::{js8, psk};

use crate::channels::over_gap_s;

/// The most of the past the plot will show, in seconds.
///
/// Wall time rather than a sample count: ten minutes is about as far back as a
/// fade is still telling you something about the path you are on now, whether
/// that is forty JS8 frames or four thousand PSK characters. It is a ceiling,
/// not the axis — see [`time_axis`].
pub const WINDOW_S: f64 = 600.0;

/// Fewer samples than this in the window and nothing is drawn.
///
/// Three points is a corner and four is barely a trend. Below that the numbers
/// beside the plot already say everything there is to know, and an almost-empty
/// box beside them says less than no box at all.
pub const MIN_POINTS: usize = 5;

/// The narrowest range of dB the plot will scale to, and the room left above
/// and below the data inside it.
pub const MIN_SPAN_DB: f32 = 12.0;
const PAD_DB: f32 = 2.0;

/// How close the weakest sample has to come to [`copy_floor_db`] before the
/// floor is drawn.
///
/// Ten dB of margin is a comfortable path; drawing the line there would only
/// squash the trace to make room for news nobody needs. Inside ten dB the
/// question changes to how much fade is left, and the line is the answer.
const FLOOR_NEAR_DB: f32 = 10.0;

/// Roughly the SNR at which this mode stops copying, in the same
/// [`ragchew::dsp::SNR_REF_BW_HZ`] the plotted measurements use.
///
/// A guide, not a measurement of this decoder: JS8's figures are the ones
/// JS8Call documents for its submodes, and the Olivia one is where
/// `decodes_well_below_the_noise` puts the common modes — they differ from each
/// other by less than the couple of dB this whole line is good to. PSK is
/// derived rather than tabulated: the modulation is uncoded BPSK and needs the
/// same energy per bit whatever the rate, so the threshold in a fixed reference
/// bandwidth rises straight with the symbol rate from PSK31's −10 dB.
pub fn copy_floor_db(mode: ModeId) -> f32 {
    match mode {
        ModeId::Js8(m) => match m {
            js8::Mode::Slow => -28.0,
            js8::Mode::Normal => -24.0,
            js8::Mode::Fast => -20.0,
            js8::Mode::Turbo => -18.0,
        },
        ModeId::Olivia(_) => -12.0,
        ModeId::Psk(m) => -10.0 + 10.0 * (m.baud() / psk::PSK31.baud()).log10() as f32,
    }
}

/// The longest silence that is still one continuous transmission.
///
/// A continuous mode already has an answer to this — it is what decides where
/// one over ends and the next begins, and the text panel splits on the same
/// rule. A cycle-aligned mode has none, and the figure to use is its *period*
/// rather than the length of a frame: consecutive frames start one period
/// apart, and JS8 Turbo spends only 3.95 seconds of its six-second cycle
/// transmitting, so a frame and a half breaks every over it has.
///
/// Past a period and a half is a frame that did not decode, or a station that
/// stopped. Either way there is no measurement in that window, and the hole is
/// worth seeing: patchy copy looks patchy.
pub fn gap_s(mode: ModeId) -> f64 {
    match over_gap_s(mode) {
        Some(g) => g,
        None => mode.period_s().expect("only a cycle-aligned mode has no over gap") as f64 * 1.5,
    }
}

/// The samples inside the window ending at `now_s`.
///
/// The right edge is the present, not the last decode, so a station that has
/// faded out leaves the trace short of the edge instead of stretching it to
/// fill the box. Empty when everything on record is older than the window,
/// which is the plot's own way of saying it has nothing to show.
pub fn in_window(pts: &[(f64, f32)], now_s: f64) -> &[(f64, f32)] {
    let from = now_s - WINDOW_S;
    let i = pts.partition_point(|&(t, _)| t < from);
    &pts[i..]
}

/// The stretch of time the box covers: where its left edge sits, and how wide
/// that makes it in seconds.
///
/// [`WINDOW_S`] is a limit rather than a promise. Held series are bounded by
/// *count* — see [`crate::channels::MAX_HISTORY`] — and a count is a different
/// length of time in every mode: 256 samples is an hour of JS8 Normal frames
/// and under half a minute of PSK31 characters, which measures a character at a
/// time. Pinning the left edge ten minutes back would draw PSK as an empty box
/// with a scribble in the last few pixels of it.
///
/// So the box holds what there is, and the tooltip says how long that was. The
/// cost is that two plots of the same width need not be the same span, which is
/// the price of showing the fast modes anything at all; the fix, if the fixed
/// axis is ever worth more than the resolution, is to decimate at ingest —
/// keeping both extremes per couple of seconds bounds the series by *time* and
/// loses nothing this can draw.
pub fn time_axis(pts: &[(f64, f32)], now_s: f64) -> (f64, f64) {
    let oldest = pts.first().map_or(now_s, |&(t, _)| t);
    let t0 = oldest.max(now_s - WINDOW_S);
    // A run of decodes all landing on one instant is not a time axis; give it a
    // width rather than dividing by nothing.
    (t0, (now_s - t0).max(1e-3))
}

/// Split into stretches of continuous transmission, breaking wherever the
/// station was quiet for longer than `gap`.
pub fn runs(pts: &[(f64, f32)], gap: f64) -> Vec<&[(f64, f32)]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for i in 1..pts.len() {
        if pts[i].0 - pts[i - 1].0 > gap {
            out.push(&pts[start..i]);
            start = i;
        }
    }
    if start < pts.len() {
        out.push(&pts[start..]);
    }
    out
}

/// The dB range to draw, and whether the copy floor falls inside it.
///
/// Widened to [`MIN_SPAN_DB`] about the middle of the data when the data
/// occupies less than that, so the height of the trace is always a fair reading
/// of how much the signal has actually moved.
pub fn scale(pts: &[(f64, f32)], floor: f32) -> (f32, f32) {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for &(_, db) in pts {
        lo = lo.min(db);
        hi = hi.max(db);
    }
    if !lo.is_finite() {
        return (floor, floor + MIN_SPAN_DB);
    }
    // The floor joins the picture only once the signal is near enough to it for
    // the distance to be the interesting quantity.
    if lo - floor < FLOOR_NEAR_DB {
        lo = lo.min(floor);
    }
    let (mut lo, mut hi) = (lo - PAD_DB, hi + PAD_DB);
    let short = MIN_SPAN_DB - (hi - lo);
    if short > 0.0 {
        lo -= short / 2.0;
        hi += short / 2.0;
    }
    (lo, hi)
}

/// One pixel column of a run: the extremes that landed in it, and their middle.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Column {
    pub x: f32,
    pub lo: f32,
    pub hi: f32,
}

impl Column {
    pub fn mid(&self) -> f32 {
        (self.lo + self.hi) / 2.0
    }
}

/// Reduce a run to one column per pixel of width, keeping both extremes.
///
/// `x_of` maps a sample time to a screen x. Columns come out left to right, one
/// per distinct pixel, so a sparse run (a JS8 frame every fifteen seconds) comes
/// through untouched — one sample per column, `lo == hi` — and a dense one
/// arrives as the envelope of what it did there.
pub fn columns(run: &[(f64, f32)], x_of: impl Fn(f64) -> f32) -> Vec<Column> {
    let mut out: Vec<Column> = Vec::new();
    for &(t, db) in run {
        let x = x_of(t);
        match out.last_mut() {
            Some(c) if c.x.floor() == x.floor() => {
                c.lo = c.lo.min(db);
                c.hi = c.hi.max(db);
            }
            _ => out.push(Column { x, lo: db, hi: db }),
        }
    }
    out
}

/// A signal-to-noise sparkline.
pub struct Snr<'a> {
    pts: &'a [(f64, f32)],
    mode: ModeId,
    now_s: f64,
    size: egui::Vec2,
    color: Option<Color32>,
}

impl<'a> Snr<'a> {
    pub fn new(pts: &'a [(f64, f32)], mode: ModeId, now_s: f64) -> Snr<'a> {
        // A line's height and about a fifth of a 340-point QSO panel: enough
        // for the shape, small enough to ride at the end of a row of numbers
        // without growing it.
        Snr { pts, mode, now_s, size: egui::vec2(68.0, 15.0), color: None }
    }

    pub fn size(mut self, w: f32, h: f32) -> Snr<'a> {
        self.size = egui::vec2(w, h);
        self
    }

    /// Draw the trace in this colour — the mode's, where the caller has one, so
    /// the plot is tied to the signal it describes as everything else is.
    pub fn color(mut self, c: Color32) -> Snr<'a> {
        self.color = Some(c);
        self
    }

    /// Whether there is enough here to be worth the space.
    ///
    /// Public because a caller laying out a grid has to decide whether to spend
    /// a row on this *before* it can draw one, and an empty row is worse than
    /// no row.
    pub fn would_draw(&self) -> bool {
        in_window(self.pts, self.now_s).len() >= MIN_POINTS
    }

    /// Draw it, or nothing at all if there is not enough to show. The response
    /// carries the reading of the numbers as its tooltip.
    pub fn show(self, ui: &mut egui::Ui) -> Option<egui::Response> {
        let pts = in_window(self.pts, self.now_s);
        if pts.len() < MIN_POINTS {
            return None;
        }
        let (rect, resp) = ui.allocate_exact_size(self.size, egui::Sense::hover());
        let floor = copy_floor_db(self.mode);
        let (lo, hi) = scale(pts, floor);
        if ui.is_rect_visible(rect) {
            self.paint(ui, rect, pts, lo, hi, floor);
        }
        Some(resp.on_hover_text(self.reading(pts, floor)))
    }

    fn paint(&self, ui: &egui::Ui, rect: Rect, pts: &[(f64, f32)], lo: f32, hi: f32, floor: f32) {
        let p = ui.painter();
        p.rect_filled(rect, 2, ui.visuals().extreme_bg_color);

        // Time runs to the right edge of the box whether or not a decode landed
        // there: a station that has faded out leaves the trace short of the
        // edge, which is the news. The left edge is `time_axis`'s business.
        let (t0, span) = time_axis(pts, self.now_s);
        let x_of = |t: f64| {
            rect.left() + (rect.width() as f64 * ((t - t0) / span).clamp(0.0, 1.0)) as f32
        };
        let y_of = |db: f32| {
            let f = ((db - lo) / (hi - lo)).clamp(0.0, 1.0);
            rect.bottom() - f * rect.height()
        };

        if floor > lo && floor < hi {
            let y = y_of(floor);
            p.extend(egui::Shape::dashed_line(
                &[egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                Stroke::new(1.0, ui.visuals().warn_fg_color.gamma_multiply(0.55)),
                3.0,
                3.0,
            ));
        }

        let color = self.color.unwrap_or_else(|| ui.visuals().text_color());
        let line = Stroke::new(1.0, color);
        let band = Stroke::new(1.0, color.gamma_multiply(0.45));
        for run in runs(pts, gap_s(self.mode)) {
            let cols = columns(run, x_of);
            // The envelope first, so the trace through it stays readable.
            for c in &cols {
                if c.hi - c.lo > 0.0 {
                    p.line_segment([egui::pos2(c.x, y_of(c.lo)), egui::pos2(c.x, y_of(c.hi))], band);
                }
            }
            let path: Vec<Pos2> = cols.iter().map(|c| egui::pos2(c.x, y_of(c.mid()))).collect();
            match path.len() {
                // A run of one is a single frame between two silences: a dot,
                // since a line needs somewhere to go.
                1 => p.circle_filled(path[0], 1.0, color),
                _ => p.add(egui::Shape::line(path, line)),
            };
        }

        // Where the station is now, findable at a glance in a trace that may
        // have wandered anywhere in the box.
        let &(t, db) = pts.last().expect("checked non-empty");
        p.circle_filled(egui::pos2(x_of(t), y_of(db)), 1.6, color);
    }

    /// What the picture says, in words and numbers, for the tooltip.
    fn reading(&self, pts: &[(f64, f32)], floor: f32) -> String {
        let mut sorted: Vec<f32> = pts.iter().map(|&(_, db)| db).collect();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let median = sorted[sorted.len() / 2];
        // The span the box actually covers, since it is the axis nothing else
        // labels.
        let span = time_axis(pts, self.now_s).1;
        format!(
            "signal-to-noise over the last {}:{:02} — {} decode{}\n\
             now {:+.0}, median {:+.0}, best {:+.0}, worst {:+.0} dB\n\
             {} stops copying somewhere around {:+.0} dB",
            (span as i64) / 60,
            (span as i64) % 60,
            pts.len(),
            if pts.len() == 1 { "" } else { "s" },
            pts.last().expect("checked non-empty").1,
            median,
            sorted[sorted.len() - 1],
            sorted[0],
            self.mode.name(),
            floor,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pts(times: &[f64]) -> Vec<(f64, f32)> {
        times.iter().map(|&t| (t, -10.0)).collect()
    }

    /// A silence between two overs breaks the trace rather than being drawn
    /// through — the whole reason the times are carried alongside the values.
    #[test]
    fn splits_runs_at_a_silence() {
        let p = pts(&[0.0, 15.0, 30.0, 300.0, 315.0]);
        let got = runs(&p, gap_s(ModeId::Js8(js8::Mode::Normal)));
        assert_eq!(got.len(), 2, "a four-minute silence did not break the trace");
        assert_eq!(got[0].len(), 3);
        assert_eq!(got[1].len(), 2);
    }

    /// Consecutive frames of a cycle-aligned mode are one period apart and are
    /// one transmission, not a string of one-frame overs.
    #[test]
    fn consecutive_frames_are_one_run() {
        for m in [js8::Mode::Slow, js8::Mode::Normal, js8::Mode::Fast, js8::Mode::Turbo] {
            let mode = ModeId::Js8(m);
            let period = mode.period_s().expect("JS8 is cycle-aligned") as f64;
            let p = pts(&[0.0, period, 2.0 * period, 3.0 * period]);
            assert_eq!(runs(&p, gap_s(mode)).len(), 1, "{} split its own cycle", mode.name());
        }
    }

    /// The window ends at the present, so a station that stopped ten minutes ago
    /// has nothing left to draw even though the samples are still held.
    #[test]
    fn the_window_ends_at_now() {
        let p = pts(&[0.0, 15.0, 30.0]);
        assert_eq!(in_window(&p, 30.0).len(), 3);
        assert_eq!(in_window(&p, WINDOW_S + 20.0).len(), 1, "kept the wrong end of the window");
        assert!(in_window(&p, WINDOW_S + 100.0).is_empty());
    }

    /// The axis holds what is held: a fast mode's short series fills the box
    /// rather than huddling in the last few pixels of a ten-minute one.
    #[test]
    fn the_axis_fits_what_there_is() {
        // PSK31, a character at a time: 256 samples is 25 seconds of it.
        let step = ModeId::Psk(psk::PSK31).chunk_secs();
        let dense: Vec<(f64, f32)> =
            (0..256).map(|i| (100.0 + i as f64 * step, -9.0)).collect();
        let now = dense.last().expect("built non-empty").0;
        let (t0, span) = time_axis(&dense, now);
        assert_eq!(t0, dense[0].0, "the axis started before anything was measured");
        assert!(span < 60.0, "25 s of PSK was drawn across {span:.0} s of axis");

        // JS8, where a bounded series is far longer than the window: the window
        // is what it gets.
        let sparse: Vec<(f64, f32)> = (0..256).map(|i| (i as f64 * 15.0, -9.0)).collect();
        let now = sparse.last().expect("built non-empty").0;
        let (t0, span) = time_axis(in_window(&sparse, now), now);
        assert!((span - WINDOW_S).abs() < 1e-9, "an hour of frames drew {span:.0} s");
        assert!((t0 - (now - WINDOW_S)).abs() < 1e-9);
    }

    /// A flat signal must not be magnified into a fade by a scale that fits
    /// whatever it is given.
    #[test]
    fn a_steady_signal_stays_flat() {
        let p: Vec<(f64, f32)> = vec![(0.0, -8.0), (1.0, -7.0), (2.0, -8.5)];
        let (lo, hi) = scale(&p, -24.0);
        assert!(hi - lo >= MIN_SPAN_DB, "1.5 dB of movement was scaled to {:.1} dB", hi - lo);
        assert!(lo < -8.5 && hi > -7.0, "the data fell outside its own scale");
    }

    /// The floor stays out of the way of a strong signal and joins a weak one.
    #[test]
    fn the_floor_appears_when_the_margin_is_thin() {
        let floor = -24.0;
        let strong: Vec<(f64, f32)> = vec![(0.0, -2.0), (1.0, -4.0)];
        let (lo, _) = scale(&strong, floor);
        assert!(lo > floor, "the floor squashed a signal 20 dB clear of it");

        let weak: Vec<(f64, f32)> = vec![(0.0, -20.0), (1.0, -22.0)];
        let (lo, _) = scale(&weak, floor);
        assert!(lo <= floor, "a signal 2 dB above the floor was drawn without it");
    }

    /// A dense run keeps the depth of its fades: the column carries both
    /// extremes, not whichever sample happened to land last.
    #[test]
    fn columns_keep_the_envelope() {
        // Sixteen samples across one pixel, swinging 12 dB.
        let run: Vec<(f64, f32)> = (0..16)
            .map(|i| (i as f64 * 0.05, if i % 2 == 0 { -4.0 } else { -16.0 }))
            .collect();
        let cols = columns(&run, |_| 3.0);
        assert_eq!(cols.len(), 1, "one pixel of width produced {} columns", cols.len());
        assert_eq!((cols[0].lo, cols[0].hi), (-16.0, -4.0));
    }

    /// A sparse run is left alone: one sample per column, and the column is the
    /// sample.
    #[test]
    fn a_sparse_run_is_not_reduced() {
        let run: Vec<(f64, f32)> = (0..8).map(|i| (i as f64 * 15.0, -6.0 - i as f32)).collect();
        let cols = columns(&run, |t| (t / 15.0) as f32 * 4.0);
        assert_eq!(cols.len(), run.len());
        assert!(cols.iter().all(|c| c.lo == c.hi), "a single sample was drawn as a range");
    }

    /// PSK's floor is derived from the symbol rate, and doubling the rate costs
    /// 3 dB — the property that made deriving it better than tabulating it.
    #[test]
    fn psk_floors_track_the_symbol_rate() {
        let f31 = copy_floor_db(ModeId::Psk(psk::PSK31));
        let f63 = copy_floor_db(ModeId::Psk(psk::PSK63));
        assert!((f31 - -10.0).abs() < 0.01, "PSK31 floor moved to {f31:.1}");
        assert!((f63 - f31 - 3.01).abs() < 0.1, "doubling the rate cost {:.2} dB", f63 - f31);
    }
}
