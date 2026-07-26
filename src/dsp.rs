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

/// A periodic Hann window of length `n` (`0` at both ends, area `n/2`).
pub fn hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let x = 2.0 * std::f64::consts::PI * i as f64 / n as f64;
            (0.5 - 0.5 * x.cos()) as f32
        })
        .collect()
}
