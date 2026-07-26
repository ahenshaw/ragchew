//! A public magnitude spectrogram for display (e.g. a waterfall). It is kept
//! separate from the decoders' internal sync spectrograms so UI code can build
//! one without pulling in decode internals, and so a single waterfall can serve
//! every protocol.

use rustfft::num_complex::Complex32;

use crate::SAMPLE_RATE;

/// Analysis window length in samples: 160 ms, i.e. a 6.25 Hz bin grid.
///
/// That is exactly one JS8 Normal symbol, and fine enough for Olivia too, whose
/// tone spacings start at 15.6 Hz.
pub const WINDOW: usize = 1920;

/// Time/frequency magnitude grid over a frequency band.
///
/// `columns[t][b]` is the magnitude of FFT bin `bin_lo + b` at time column `t`.
/// Column `t` covers samples `[t*hop, t*hop + WINDOW)`, so later
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
    /// Hz per bin (`SAMPLE_RATE / WINDOW` = 6.25).
    pub bin_hz: f64,
    /// Magnitude columns, oldest first.
    pub columns: Vec<Vec<f32>>,
}

impl Spectrogram {
    /// An empty spectrogram over `hz_lo..hz_hi` with the given `hop`, ready for
    /// [`push_window`](Self::push_window). Frequency resolution is fixed at
    /// 6.25 Hz.
    pub fn empty(hz_lo: f64, hz_hi: f64, hop: usize) -> Spectrogram {
        let bin_hz = SAMPLE_RATE as f64 / WINDOW as f64; // 6.25
        let bin_lo = (hz_lo / bin_hz).floor() as usize;
        let bin_hi = ((hz_hi / bin_hz).ceil() as usize).min(WINDOW / 2);
        Spectrogram {
            sample_rate: SAMPLE_RATE,
            hop: hop.clamp(1, WINDOW),
            bin_lo,
            n_bins: bin_hi.saturating_sub(bin_lo) + 1,
            bin_hz,
            columns: Vec::new(),
        }
    }

    /// Append one column from a [`WINDOW`]-length window (shorter is
    /// zero-padded). Used to build a rolling spectrogram from live audio.
    pub fn push_window(&mut self, window: &[f32]) {
        let mut buf = vec![Complex32::new(0.0, 0.0); WINDOW];
        for (i, b) in buf.iter_mut().enumerate() {
            if let Some(&s) = window.get(i) {
                b.re = s;
            }
        }
        crate::dsp::plan(WINDOW).process(&mut buf);
        let mut col = vec![0.0f32; self.n_bins];
        for (b, m) in col.iter_mut().enumerate() {
            *m = buf[self.bin_lo + b].norm();
        }
        self.columns.push(col);
    }

    /// Compute a spectrogram of `samples` over `hz_lo..hz_hi`, advancing `hop`
    /// samples per column (`hop` <= [`WINDOW`]; smaller = finer time
    /// resolution). Frequency resolution is fixed at 6.25 Hz.
    pub fn compute(samples: &[f32], hz_lo: f64, hz_hi: f64, hop: usize) -> Spectrogram {
        let mut spec = Spectrogram::empty(hz_lo, hz_hi, hop);
        if samples.len() >= WINDOW {
            let n_cols = (samples.len() - WINDOW) / spec.hop + 1;
            spec.columns.reserve(n_cols);
            for c in 0..n_cols {
                let start = c * spec.hop;
                spec.push_window(&samples[start..start + WINDOW]);
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
        (t * self.hop + WINDOW / 2) as f64 / self.sample_rate as f64
    }

    /// Total time span covered (seconds).
    pub fn duration_secs(&self) -> f64 {
        if self.columns.is_empty() {
            0.0
        } else {
            (self.columns.len() * self.hop + WINDOW) as f64 / self.sample_rate as f64
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
        let n = WINDOW * 4;
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                (2.0 * std::f64::consts::PI * hz * i as f64 / SAMPLE_RATE as f64).cos() as f32
            })
            .collect();
        let spec = Spectrogram::compute(&samples, 0.0, 4000.0, WINDOW);
        assert!(!spec.columns.is_empty());
        let col = &spec.columns[0];
        let peak = (0..spec.n_bins).max_by(|&a, &b| col[a].partial_cmp(&col[b]).unwrap()).unwrap();
        assert_eq!(spec.hz_of_bin(peak), hz, "peak bin should sit at the tone");
    }
}
