//! Small DSP helpers shared by the protocol decoders.
//!
//! Everything here is deliberately protocol-agnostic: a cached FFT planner and
//! a windowed, frequency-shifted forward transform. Both the JS8 and Olivia
//! demodulators are built out of "take one symbol-length window, mix it down so
//! the carrier lands on an integer bin, transform, read tone bins".

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use rustfft::{num_complex::Complex32, Fft, FftPlanner};

thread_local! {
    static PLANNER: RefCell<FftPlanner<f32>> = RefCell::new(FftPlanner::<f32>::new());
    static PLANS: RefCell<HashMap<usize, Arc<dyn Fft<f32>>>> = RefCell::new(HashMap::new());
    static INVERSE_PLANS: RefCell<HashMap<usize, Arc<dyn Fft<f32>>>> =
        RefCell::new(HashMap::new());
}

/// A cached forward FFT plan of length `n` (per thread).
pub fn plan(n: usize) -> Arc<dyn Fft<f32>> {
    PLANS.with(|plans| {
        if let Some(p) = plans.borrow().get(&n) {
            return p.clone();
        }
        let p = PLANNER.with(|pl| pl.borrow_mut().plan_fft_forward(n));
        plans.borrow_mut().insert(n, p.clone());
        p
    })
}

/// A cached inverse FFT plan of length `n` (per thread).
///
/// Unnormalised, as rustfft leaves it: everything built on this reads ratios,
/// not absolute levels. The PSK gate uses it to cut a narrow slice out of a
/// band-wide spectrum and turn it back into a decimated complex baseband.
pub fn plan_inverse(n: usize) -> Arc<dyn Fft<f32>> {
    INVERSE_PLANS.with(|plans| {
        if let Some(p) = plans.borrow().get(&n) {
            return p.clone();
        }
        let p = PLANNER.with(|pl| pl.borrow_mut().plan_fft_inverse(n));
        plans.borrow_mut().insert(n, p.clone());
        p
    })
}

/// Forward FFT of the `n`-sample window at `start`, after mixing the input down
/// by `delta_hz` so a between-bins carrier lands on an integer bin.
///
/// Samples past the end of `samples` count as zero, so a window that runs off
/// the end is zero-padded rather than rejected. `window`, when given, must have
/// length `n` and multiplies the input sample-by-sample.
pub fn fft_window(
    samples: &[f32],
    start: usize,
    delta_hz: f64,
    sample_rate: u32,
    n: usize,
    window: Option<&[f32]>,
) -> Vec<Complex32> {
    let mut buf = vec![Complex32::new(0.0, 0.0); n];
    let w = -2.0 * std::f64::consts::PI * delta_hz / sample_rate as f64;
    for (i, b) in buf.iter_mut().enumerate() {
        let Some(&s) = samples.get(start + i) else { continue };
        let s = match window {
            Some(win) => s * win[i],
            None => s,
        };
        if delta_hz == 0.0 {
            b.re = s;
        } else {
            let ang = w * i as f64;
            *b = Complex32::new(s * ang.cos() as f32, s * ang.sin() as f32);
        }
    }
    plan(n).process(&mut buf);
    buf
}

/// Reference bandwidth amateur weak-signal work quotes SNR in.
///
/// Not a property of any mode — a convention, so that "−12 dB" means the same
/// thing whether it came off a 31 Hz PSK31 carrier or a 1 kHz Olivia signal,
/// and the same thing it means in WSJT-X and JS8Call.
pub const SNR_REF_BW_HZ: f64 = 2500.0;

/// Where in the guard region's spread the noise floor is read off.
///
/// A quarter rather than a half, because a panoramic monitor's guard region is
/// usually not empty. The quantile survives having that fraction of it full of
/// other people's signals: at the median, a station whose guard is 63% occupied
/// — Olivia 8/500 in this repository's weak-signal band, which has four
/// neighbours inside its skirts — reads 1.7 dB weak, because the middle of the
/// spread is a signal and not the noise.
///
/// Lower would tolerate a busier band still, at the cost of leaning on the
/// thin end of the distribution where the correction below is least exact and
/// the variance is worst.
const NOISE_QUANTILE: f64 = 0.25;

/// Standard-normal quantile at [`NOISE_QUANTILE`], for the correction below.
/// Kept as a constant beside it rather than computed, so the pair cannot drift
/// apart: change one and change the other.
const NOISE_QUANTILE_Z: f64 = -0.674_489_75;

/// What [`snr_db`] measured, and the parts it measured it from.
#[derive(Clone, Copy, Debug)]
pub struct Snr {
    /// Signal power against the noise in [`SNR_REF_BW_HZ`], in dB.
    pub db: f64,
    /// Signal power, with the noise inside the occupied band taken back off.
    pub signal: f64,
    /// Noise power per Hz, from the spectrum either side of the signal.
    pub noise_per_hz: f64,
}

/// Estimate a signal's SNR in [`SNR_REF_BW_HZ`], from the audio alone.
///
/// `samples` should span the transmission and not much else: the signal's power
/// is averaged over whatever it is given, so silence either side reads as the
/// station being weaker than it is.
///
/// `bandwidth_hz` must be the band the signal's *power* is in, which is not
/// always the bandwidth the mode is named for. Getting it wrong is the only way
/// to be badly wrong here, and it is wrong in both directions:
///
/// - **Too narrow** leaves signal outside and reads low, by exactly the
///   fraction left out. A JS8 frame keeps only 62% of its power inside the
///   conventional 8 × spacing, so measuring a JS8 signal over its nominal
///   bandwidth reads 2.2 dB weak. That figure is the mode's, not this crate's:
///   an off-air recording and `js8::modem::synthesize_sm` agree on it to within
///   4%. Twice the nominal band holds 99% and measures true.
/// - **Too wide** reaches past the noise and into the neighbours. Widening
///   Olivia 8/250 to 500 Hz with a station 200 Hz away reads **3 dB strong**,
///   because the neighbour's power lands inside the band and counts as signal.
///
/// So there is no safe blanket margin, and callers should pass what the mode
/// actually occupies. `examples/snr.rs` measures all of the above.
///
/// The noise is taken from the spectrum on either side of the signal rather
/// than from a quiet moment, so it needs neither a squelch nor a gap in the
/// traffic — and it is a low *quantile* of that spectrum
/// ([`NOISE_QUANTILE`]), so neighbouring stations in the guard region move it
/// very little. A quantile of a periodogram sits below its mean, which is
/// corrected for below; leave the correction out and every signal reads
/// several dB strong.
///
/// Accurate to about 0.1 dB on a real mode filling its band, and to a few
/// tenths on a bare carrier, where the band is mostly noise. Both figures are
/// bias; a single reading scatters by more.
///
/// `None` if there is too little audio to transform, or if the band asked for
/// falls outside what the sample rate carries.
pub fn snr_db(
    samples: &[f32],
    sample_rate: u32,
    center_hz: f64,
    bandwidth_hz: f64,
) -> Option<Snr> {
    // 4096 at 12 kHz is a third of a second and a 2.9 Hz bin: fine enough to
    // put a dozen bins across even PSK31's carrier, short enough that a JS8
    // frame holds thirty of them.
    let n = 4096.min(samples.len().next_power_of_two() / 2).max(256);
    if samples.len() < n {
        return None;
    }
    let fs = sample_rate as f64;
    let bin_hz = fs / n as f64;
    let win = hann(n);
    let win_power: f64 = win.iter().map(|w| (*w as f64) * (*w as f64)).sum();

    // Welch: half-overlapping segments, averaged. Averaging is what makes the
    // median correction below predictable — one periodogram is far too ragged
    // to read a noise floor off.
    let hop = n / 2;
    let segments = (samples.len() - n) / hop + 1;
    let mut psd = vec![0.0f64; n / 2];
    for s in 0..segments {
        let spec = fft_window(samples, s * hop, 0.0, sample_rate, n, Some(&win));
        for (k, p) in psd.iter_mut().enumerate() {
            *p += spec[k].norm_sqr() as f64;
        }
    }
    // Power per Hz, one-sided: a real signal's negative frequencies are not
    // transformed, so every bin but DC and Nyquist carries half of what is
    // really there. Doubling cancels out of `db` — both terms of that ratio
    // come from here — but not out of the two fields beside it, and a
    // `noise_per_hz` quietly 3 dB under the convention would be a trap for
    // whatever reads it next.
    let scale = 2.0 / (segments as f64 * fs * win_power);
    for (k, p) in psd.iter_mut().enumerate() {
        *p *= if k == 0 { scale / 2.0 } else { scale };
    }

    let bin_of = |hz: f64| (hz / bin_hz).round() as isize;
    let lo = bin_of(center_hz - bandwidth_hz / 2.0).max(0) as usize;
    let hi = (bin_of(center_hz + bandwidth_hz / 2.0) as usize).min(psd.len() - 1);
    if lo >= hi {
        return None;
    }

    // Noise from a guard region either side, starting clear of the signal's
    // skirts and reaching out a few bandwidths. Clamped to what the spectrum
    // has, and widened if a wide mode near the edge of the band leaves too few
    // bins to take a median over.
    let mut guard: Vec<f64> = Vec::new();
    for (k, &p) in psd.iter().enumerate() {
        let df = (k as f64 * bin_hz - center_hz).abs();
        if df > 0.75 * bandwidth_hz && df < 4.0 * bandwidth_hz {
            guard.push(p);
        }
    }
    if guard.len() < 32 {
        guard = psd
            .iter()
            .enumerate()
            .filter(|(k, _)| (*k as f64 * bin_hz - center_hz).abs() > 0.75 * bandwidth_hz)
            .map(|(_, &p)| p)
            .collect();
    }
    if guard.len() < 8 {
        return None;
    }
    guard.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let at = ((NOISE_QUANTILE * guard.len() as f64) as usize).min(guard.len() - 1);
    let quantile = guard[at];

    // That quantile back to a mean, for a power spectrum averaged over
    // `segments` looks: Wilson–Hilferty on chi-squared with 2m degrees of
    // freedom. It is exact enough at one look — 0.702 against ln 2 = 0.693 for
    // the median — and converges to 1 as the looks pile up. Overlapping Hann
    // segments are not quite independent, which `SEGMENT_CORRELATION` allows
    // for.
    const SEGMENT_CORRELATION: f64 = 1.06;
    let looks = (segments as f64 / SEGMENT_CORRELATION).max(1.0);
    let k = 2.0 * looks;
    let quantile_to_mean =
        (1.0 - 2.0 / (9.0 * k) + NOISE_QUANTILE_Z * (2.0 / (9.0 * k)).sqrt()).powi(3);
    let noise_per_hz = quantile / quantile_to_mean;

    // The band holds signal *and* noise; only the excess is signal. Without
    // this a signal at the noise floor would read 3 dB strong, and the range
    // that matters for JS8 and Olivia is below it.
    let in_band: f64 = psd[lo..=hi].iter().sum::<f64>() * bin_hz;
    let occupied = (hi - lo + 1) as f64 * bin_hz;
    let signal = (in_band - noise_per_hz * occupied).max(f64::MIN_POSITIVE);

    Some(Snr {
        db: 10.0 * (signal / (noise_per_hz * SNR_REF_BW_HZ)).log10(),
        signal,
        noise_per_hz,
    })
}

/// A periodic Hann window of length `n` (`0` at both ends, area `n/2`).
pub fn hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let x = 2.0 * std::f64::consts::PI * i as f64 / n as f64;
            (0.5 - 0.5 * x.cos()) as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// White noise at a known level, plus a tone scaled to sit a stated number
    /// of dB above it in [`SNR_REF_BW_HZ`].
    fn band(snr_db: f64, seed: u64) -> Vec<f32> {
        const RATE: u32 = 12_000;
        const RMS: f64 = 0.08;
        let n = 4 * RATE as usize;
        let mut s = seed;
        let mut buf: Vec<f32> = (0..n)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                (s >> 40) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect();
        let rms = (buf.iter().map(|v| v * v).sum::<f32>() / n as f32).sqrt();
        for v in buf.iter_mut() {
            *v *= (RMS as f32) / rms;
        }
        // Noise is white to Nyquist, so only the part of it inside the
        // reference bandwidth counts against the signal.
        let noise_per_hz = RMS * RMS / (RATE as f64 / 2.0);
        let want = noise_per_hz * SNR_REF_BW_HZ * 10f64.powf(snr_db / 10.0);
        // A tone of amplitude a has mean-square power a^2/2.
        let amp = (2.0 * want).sqrt() as f32;
        for (i, v) in buf.iter_mut().enumerate() {
            let t = i as f64 / RATE as f64;
            *v += amp * (2.0 * std::f64::consts::PI * 1500.0 * t).sin() as f32;
        }
        buf
    }

    /// The measurement reproduces the SNR a band was built with.
    ///
    /// `examples/snr.rs` does this far more thoroughly, over every mode and a
    /// sweep of carriers and bandwidths; this is the part of it that runs in
    /// CI, so a change that puts a dB on every reading cannot pass unnoticed.
    #[test]
    fn measures_the_snr_a_band_was_built_with() {
        for target in [-15.0f64, -6.0, 0.0, 10.0, 20.0] {
            let errs: Vec<f64> = (1u64..=16)
                .map(|seed| {
                    let audio = band(target, seed * 0x9E37_79B9);
                    snr_db(&audio, 12_000, 1500.0, 20.0).expect("measurable").db - target
                })
                .collect();
            let bias = errs.iter().sum::<f64>() / errs.len() as f64;
            let worst = errs.iter().cloned().fold(0.0f64, |a, b| a.max(b.abs()));
            // Bias is the figure that matters: a systematic dB on every
            // reading is a wrong readout, where scatter is just a number that
            // moves. A bare tone is the harshest case here — its power is in
            // one bin while the band integrates seventeen, so the noise in the
            // other sixteen is all variance. Real modes fill their band and
            // come out tighter; `examples/snr.rs` has them at 0.00 to 0.31 dB.
            assert!(bias.abs() < 0.4, "{target:+.1} dB reads with {bias:+.2} dB of bias");
            assert!(worst < 2.0, "{target:+.1} dB: one reading was {worst:.2} dB out");
        }
    }

    /// The noise floor is reported in the conventional one-sided units, which
    /// is 3 dB away from the two-sided figure the arithmetic works in. The
    /// ratio hides that, so only a direct look catches it.
    #[test]
    fn reports_the_noise_floor_in_conventional_units() {
        let audio = band(-6.0, 0x1234);
        let got = snr_db(&audio, 12_000, 1500.0, 50.0).expect("measurable");
        let truth = 0.08f64 * 0.08 / (12_000.0 / 2.0);
        let err_db = 10.0 * (got.noise_per_hz / truth).log10();
        assert!(err_db.abs() < 0.5, "noise floor is {err_db:+.2} dB off");
    }

    /// Too little audio to transform is `None`, not a wrong answer.
    #[test]
    fn refuses_to_guess_from_a_sliver() {
        assert!(snr_db(&[0.0; 64], 12_000, 1500.0, 50.0).is_none());
    }
}
