//! A public magnitude spectrogram for display (e.g. a waterfall), sharing the
//! JS8 6.25 Hz bin grid. This is separate from the decoder's internal sync
//! spectrogram so UI code can build one without pulling in decode internals.

use std::cell::RefCell;
use std::sync::Arc;

use rustfft::{num_complex::Complex32, Fft, FftPlanner};

use crate::modem::{SAMPLES_PER_SYMBOL, SAMPLE_RATE};

thread_local! {
    static FFT: RefCell<Arc<dyn Fft<f32>>> =
        RefCell::new(FftPlanner::<f32>::new().plan_fft_forward(SAMPLES_PER_SYMBOL));
}

/// Time/frequency magnitude grid over a frequency band.
///
/// `columns[t][b]` is the magnitude of FFT bin `bin_lo + b` at time column `t`.
/// Column `t` covers samples `[t*hop, t*hop + SAMPLES_PER_SYMBOL)`, so later
/// `t` is later in time. Frequency of bin index `b` is `(bin_lo + b) * bin_hz`.
#[derive(Clone)]
pub struct Spectrogram {
    /// Source sample rate (Hz).
    pub sample_rate: u32,
    /// Samples advanced between consecutive columns.
    pub hop: usize,
    /// First FFT bin included (its frequency is `bin_lo * bin_hz`).
    pub bin_lo: usize,
    /// Number of bins per column.
    pub n_bins: usize,
    /// Hz per bin (`sample_rate / SAMPLES_PER_SYMBOL` = 6.25).
    pub bin_hz: f64,
    /// Magnitude columns, oldest first.
    pub columns: Vec<Vec<f32>>,
}

impl Spectrogram {
    /// An empty spectrogram over `hz_lo..hz_hi` with the given `hop`, ready for
    /// [`push_window`](Self::push_window). Frequency resolution is fixed at
    /// 6.25 Hz.
    pub fn empty(hz_lo: f64, hz_hi: f64, hop: usize) -> Spectrogram {
        let bin_hz = SAMPLE_RATE as f64 / SAMPLES_PER_SYMBOL as f64; // 6.25
        let bin_lo = (hz_lo / bin_hz).floor() as usize;
        let bin_hi = ((hz_hi / bin_hz).ceil() as usize).min(SAMPLES_PER_SYMBOL / 2);
        Spectrogram {
            sample_rate: SAMPLE_RATE,
            hop: hop.clamp(1, SAMPLES_PER_SYMBOL),
            bin_lo,
            n_bins: bin_hi.saturating_sub(bin_lo) + 1,
            bin_hz,
            columns: Vec::new(),
        }
    }

    /// Append one column from a [`SAMPLES_PER_SYMBOL`]-length window (shorter is
    /// zero-padded). Used to build a rolling spectrogram from live audio.
    pub fn push_window(&mut self, window: &[f32]) {
        let mut buf = vec![Complex32::new(0.0, 0.0); SAMPLES_PER_SYMBOL];
        for (i, b) in buf.iter_mut().enumerate() {
            if let Some(&s) = window.get(i) {
                b.re = s;
            }
        }
        FFT.with(|f| f.borrow().process(&mut buf));
        let mut col = vec![0.0f32; self.n_bins];
        for (b, m) in col.iter_mut().enumerate() {
            *m = buf[self.bin_lo + b].norm();
        }
        self.columns.push(col);
    }

    /// Compute a spectrogram of `samples` over `hz_lo..hz_hi`, advancing `hop`
    /// samples per column (`hop` <= [`SAMPLES_PER_SYMBOL`]; smaller = finer time
    /// resolution). Frequency resolution is fixed at 6.25 Hz.
    pub fn compute(samples: &[f32], hz_lo: f64, hz_hi: f64, hop: usize) -> Spectrogram {
        let mut spec = Spectrogram::empty(hz_lo, hz_hi, hop);
        if samples.len() >= SAMPLES_PER_SYMBOL {
            let n_cols = (samples.len() - SAMPLES_PER_SYMBOL) / spec.hop + 1;
            spec.columns.reserve(n_cols);
            for c in 0..n_cols {
                let start = c * spec.hop;
                spec.push_window(&samples[start..start + SAMPLES_PER_SYMBOL]);
            }
        }
        spec
    }

    /// Lowest frequency (Hz) represented (center of `bin_lo`).
    pub fn hz_lo(&self) -> f64 {
        self.bin_lo as f64 * self.bin_hz
    }

    /// Highest frequency (Hz) represented.
    pub fn hz_hi(&self) -> f64 {
        (self.bin_lo + self.n_bins - 1) as f64 * self.bin_hz
    }

    /// Frequency (Hz) of local bin index `b` (0..n_bins).
    pub fn hz_of_bin(&self, b: usize) -> f64 {
        (self.bin_lo + b) as f64 * self.bin_hz
    }

    /// Time (seconds from the start of `samples`) at the center of column `t`.
    pub fn secs_of_column(&self, t: usize) -> f64 {
        (t * self.hop + SAMPLES_PER_SYMBOL / 2) as f64 / self.sample_rate as f64
    }

    /// Total time span covered (seconds).
    pub fn duration_secs(&self) -> f64 {
        if self.columns.is_empty() {
            0.0
        } else {
            (self.columns.len() * self.hop + SAMPLES_PER_SYMBOL) as f64 / self.sample_rate as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_bin_matches_tone_frequency() {
        // 1500 Hz tone = bin 240 (1500 / 6.25). Should be the strongest bin.
        let hz = 1500.0;
        let n = SAMPLES_PER_SYMBOL * 4;
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                (2.0 * std::f64::consts::PI * hz * i as f64 / SAMPLE_RATE as f64).cos() as f32
            })
            .collect();
        let spec = Spectrogram::compute(&samples, 0.0, 4000.0, SAMPLES_PER_SYMBOL);
        assert!(!spec.columns.is_empty());
        let col = &spec.columns[0];
        let peak = (0..spec.n_bins).max_by(|&a, &b| col[a].partial_cmp(&col[b]).unwrap()).unwrap();
        assert_eq!(spec.hz_of_bin(peak), hz, "peak bin should sit at the tone");
    }
}
