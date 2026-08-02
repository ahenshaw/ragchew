//! How much of a message the streaming receiver reads, against the block
//! demodulator, over SNR.
//!
//! The streaming receiver in `psk::rx` exists so live text arrives at the rate
//! it was typed instead of a block at a time. It is only worth having if it
//! reads as well as the block demodulator it bypasses, and that is not
//! self-evident: it recovers timing with a loop over a few symbols where the
//! block picks the best of sixteen phases over 256 of them.
//!
//! So this measures it. Runs on the shipping `Rx` and `modem::decode_all`
//! rather than on copies of them, so the numbers cannot drift from the code.
//!
//! Recall is the longest common subsequence as a fraction of what was sent:
//! how much of the message came through, in order, ignoring anything the
//! receiver invented around it. Insertions are the gate's problem, not the
//! demodulator's — see `examples/psk_gate.rs`.
//!
//! ```text
//! cargo run --release --example psk_stream_vs_block
//! ```

use ragchew::psk::{self, modem, rx::Rx, Mode, PSK31, PSK63};
use ragchew::{dsp, SAMPLE_RATE};

/// Longest common subsequence, as a fraction of the sent text: how much of the
/// message came through in order, ignoring inserted junk.
fn recall(sent: &str, got: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (sent.chars().collect(), got.chars().collect());
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    for i in 0..a.len() {
        for j in 0..b.len() {
            cur[j + 1] =
                if a[i] == b[j] { prev[j] + 1 } else { cur[j].max(prev[j + 1]) };
        }
        std::mem::swap(&mut prev, &mut cur);
        cur.iter_mut().for_each(|v| *v = 0);
    }
    prev[b.len()] as f64 / a.len().max(1) as f64
}

/// A band with one station at a stated SNR in the reference bandwidth.
fn band(text: &str, hz: f64, mode: Mode, snr_db: f64, seed: u64) -> (Vec<f32>, usize) {
    let sig = psk::encode(text, hz, mode);
    let lead = SAMPLE_RATE as usize; // a second of noise first
    let n = lead + sig.len() + SAMPLE_RATE as usize;

    const RMS: f64 = 0.08;
    let mut s = seed;
    let mut audio: Vec<f32> = (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            let u = (s >> 40) as f64 / (1u64 << 24) as f64 - 0.5;
            (u * RMS * 12.0f64.sqrt()) as f32
        })
        .collect();

    // Scale the signal to sit `snr_db` above the noise in the reference band.
    let noise_per_hz = RMS * RMS / (SAMPLE_RATE as f64 / 2.0);
    let want = noise_per_hz * dsp::SNR_REF_BW_HZ * 10f64.powf(snr_db / 10.0);
    let have: f64 = sig.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / sig.len() as f64;
    let g = (want / have).sqrt() as f32;
    for (i, &x) in sig.iter().enumerate() {
        audio[lead + i] += x * g;
    }
    (audio, lead)
}

fn main() {
    let text = "cq cq de w1aw w1aw k  the quick brown fox jumps over the lazy dog  \
                de w1aw ur rst 599 599 name andy qth london hw cpy? k  ";

    // One clean trial, dumped, so a systematic loss is visible as a pattern
    // rather than as a percentage.
    for mode in [PSK31, PSK63] {
        let (audio, at) = band(text, 1500.0, mode, 10.0, 0x5EED);
        let mut rx = Rx::new(1500.0, mode, at);
        let stream: String = rx.feed(&audio[at..]).iter().map(|c| c.ch).collect();
        let block: String = modem::decode_all(&audio, 300.0, 2600.0, &[mode])
            .iter().map(|d| d.text.clone()).collect();
        println!("\n{} at 10 dB", mode.name());
        println!("  sent:   {text:?}");
        println!("  stream: {stream:?}");
        println!("  block:  {block:?}");
    }

    // The gate hands over a refined carrier, not an exact one. A streaming
    // receiver has a fixed oscillator, so any error shows up as a constant
    // rotation per symbol that the phase estimate has to absorb — and that
    // estimate folds the data away by squaring, so it runs out of room at a
    // quarter turn a symbol. For PSK31 that is 7.8 Hz.
    for mode in [PSK31, PSK63] {
        println!("\n{} — tolerance to a carrier the gate placed slightly wrong", mode.name());
        print!("  offset:");
        for d in [0.0, 1.0, 2.0, 4.0, 6.0, 8.0, 12.0] {
            print!("{d:>7.0}Hz");
        }
        print!("\n  read:  ");
        for d in [0.0, 1.0, 2.0, 4.0, 6.0, 8.0, 12.0] {
            let mut acc = 0.0;
            const TRIALS: u64 = 3;
            for seed in 0..TRIALS {
                let (audio, at) = band(text, 1500.0, mode, 6.0, 0x5EED + seed * 7919);
                // Told the wrong frequency, by `d` Hz.
                let mut rx = Rx::new(1500.0 + d, mode, at);
                let got: String = rx.feed(&audio[at..]).iter().map(|c| c.ch).collect();
                acc += recall(text, &got);
            }
            print!("{:>8.0}%", 100.0 * acc / TRIALS as f64);
        }
        println!();
    }

    for mode in [PSK31, PSK63] {
        println!("\n{}  ({} chars sent)", mode.name(), text.chars().count());
        println!("  {:>6}   {:>18}   {:>18}", "SNR", "streaming", "block");
        for snr_db in [10.0, 6.0, 3.0, 0.0, -3.0, -6.0, -9.0] {
            let (mut sr, mut br) = (0.0, 0.0);
            const TRIALS: u64 = 5;
            for seed in 0..TRIALS {
                let (audio, at) = band(text, 1500.0, mode, snr_db, 0x5EED + seed * 7919);

                let mut rx = Rx::new(1500.0, mode, at);
                let stream: String = rx.feed(&audio[at..]).iter().map(|c| c.ch).collect();
                sr += recall(text, &stream);

                let block: String = modem::decode_all(&audio, 300.0, 2600.0, &[mode])
                    .iter()
                    .map(|d| d.text.clone())
                    .collect();
                br += recall(text, &block);
            }
            println!(
                "  {snr_db:>5.0}dB   {:>17.1}%   {:>17.1}%",
                100.0 * sr / TRIALS as f64,
                100.0 * br / TRIALS as f64
            );
        }
    }
}
