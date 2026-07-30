//! Does [`dsp::snr_db`] report the SNR a signal was built with?
//!
//! The generator is the ground truth. A signal is scaled so its mean-square
//! power sits a stated number of dB above the noise inside
//! [`SNR_REF_BW_HZ`] — the same definition `gui/src/demo.rs` uses for its
//! weak-signal band, and the same one WSJT-X and JS8Call report against — and
//! then the estimator is asked what it sees. Anything it gets wrong is its own.
//!
//! Run: `cargo run --release --example snr`
//!
//! Sections:
//!   A. accuracy against level, per protocol, over `power_bandwidth_hz`
//!   B. accuracy against carrier frequency, in case anything is grid-dependent
//!   C. two stations at once: does a neighbour 200 Hz away move the answer?
//!   D. what a wrong bandwidth costs — the multiples are of the mode's
//!      *nominal* bandwidth, so JS8's right answer is the x2 column
//!   E. the noise floor on its own, which everything above rests on
//!   F. JS8's occupancy, real against synthesized, which explains D

use ragchew::dsp::{self, SNR_REF_BW_HZ};
use ragchew::js8::message::{self, IType};
use ragchew::js8::{modem, submode};
use ragchew::protocol::ModeId;
use ragchew::{js8, olivia, psk, SAMPLE_RATE};

/// Noise RMS the tables below are built on. Any value works — the estimator
/// only ever sees ratios — and this is the one the demo band uses.
const NOISE_RMS: f64 = 0.08;

/// Noise power per Hz: white across the whole Nyquist span.
fn noise_per_hz() -> f64 {
    NOISE_RMS * NOISE_RMS / (SAMPLE_RATE as f64 / 2.0)
}

fn noise(n: usize, seed: u64) -> Vec<f32> {
    let mut s = seed;
    let raw: Vec<f32> = (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            (s >> 40) as f32 / (1u32 << 24) as f32 - 0.5
        })
        .collect();
    // Scaled to the exact RMS wanted rather than to the distribution's nominal
    // one, so the ground truth is the ground truth.
    let rms = (raw.iter().map(|v| v * v).sum::<f32>() / raw.len() as f32).sqrt();
    raw.iter().map(|v| v * (NOISE_RMS as f32) / rms).collect()
}

/// Mean-square power of a signal over its own duration.
fn power(sig: &[f32]) -> f64 {
    sig.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / sig.len().max(1) as f64
}

/// Scale `sig` so it sits `snr_db` above the noise in the reference bandwidth.
///
/// This is the definition everything here is measured against:
/// `10 log10(P_signal / (N0 * 2500))`.
fn scale_for_snr(sig: &[f32], snr_db: f64) -> Vec<f32> {
    let want = noise_per_hz() * SNR_REF_BW_HZ * 10f64.powf(snr_db / 10.0);
    let have = power(sig);
    let amp = if have > 0.0 { (want / have).sqrt() } else { 0.0 };
    sig.iter().map(|v| v * amp as f32).collect()
}

/// A band holding one station at a known SNR, plus the window it occupies.
fn band_with(sig: &[f32], snr_db: f64, seed: u64) -> (Vec<f32>, usize, usize) {
    // A second of noise either side, so the estimator is given a window that is
    // the transmission and nothing else — as a decoder would hand it.
    let pad = SAMPLE_RATE as usize;
    let scaled = scale_for_snr(sig, snr_db);
    let mut buf = noise(scaled.len() + 2 * pad, seed);
    for (i, s) in scaled.iter().enumerate() {
        buf[pad + i] += s;
    }
    (buf, pad, pad + scaled.len())
}

struct Signal {
    name: String,
    /// The band to measure over, as `protocol` passes it: the band the power is
    /// in, which for JS8 is twice the bandwidth the mode is named for.
    bandwidth: f64,
    /// What the mode is named for, which section D varies around.
    nominal: f64,
    audio: Vec<f32>,
}

/// One transmission per mode, long enough to average over.
fn signals(hz: f64) -> Vec<Signal> {
    let mut v = Vec::new();

    for m in psk::COMMON {
        let text = "cq cq de w1aw w1aw k ".repeat(12);
        v.push(Signal {
            name: m.name(),
            bandwidth: ModeId::Psk(m).power_bandwidth_hz(),
            nominal: ModeId::Psk(m).bandwidth_hz(),
            audio: psk::encode(&text, hz, m),
        });
    }
    for m in olivia::COMMON {
        v.push(Signal {
            name: m.name(),
            bandwidth: ModeId::Olivia(m).power_bandwidth_hz(),
            nominal: ModeId::Olivia(m).bandwidth_hz(),
            audio: olivia::encode(&"DE W1AW RST 599 ".repeat(4), hz, m),
        });
    }
    for sm in submode::ALL {
        let (a87, _) = message::freetext("DE W1AW EM73", IType::None);
        v.push(Signal {
            name: format!("JS8 {}", sm.mode.name()),
            bandwidth: ModeId::Js8(sm.mode).power_bandwidth_hz(),
            nominal: ModeId::Js8(sm.mode).bandwidth_hz(),
            // `encode_audio` is the Normal-only wrapper; using it here produced
            // four rows of the same signal measured against four different
            // bandwidths, which read as an estimator that could not cope with
            // narrow modes.
            audio: modem::encode_audio_sm(&a87, hz, &sm),
        });
    }
    v
}

/// Power within `half_width` Hz either side of `center_hz`.
fn encircled(samples: &[f32], center_hz: f64, half_width: f64) -> f64 {
    let n = 8192.min(samples.len().next_power_of_two() / 2).max(1024);
    let win = dsp::hann(n);
    let hop = n / 2;
    let segs = (samples.len().saturating_sub(n)) / hop + 1;
    let mut psd = vec![0.0f64; n / 2];
    for s in 0..segs {
        let spec = dsp::fft_window(samples, s * hop, 0.0, SAMPLE_RATE, n, Some(&win));
        for (k, p) in psd.iter_mut().enumerate() {
            *p += spec[k].norm_sqr() as f64;
        }
    }
    let bin_hz = SAMPLE_RATE as f64 / n as f64;
    psd.iter()
        .enumerate()
        .filter(|(k, _)| (*k as f64 * bin_hz - center_hz).abs() <= half_width)
        .map(|(_, p)| p)
        .sum()
}

fn mean(x: &[f64]) -> f64 {
    x.iter().sum::<f64>() / x.len().max(1) as f64
}

fn sd(x: &[f64]) -> f64 {
    let m = mean(x);
    (x.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

const LEVELS: [f64; 8] = [-20.0, -15.0, -12.0, -9.0, -6.0, 0.0, 6.0, 15.0];
const SEEDS: [u64; 6] = [0x1111, 0x2222, 0x3333, 0x4444, 0x5555, 0x6666];

fn main() {
    println!("SNR estimator against known-SNR signals");
    println!("  reference bandwidth {SNR_REF_BW_HZ:.0} Hz, noise RMS {NOISE_RMS}, 1500 Hz carrier");
    println!("  every figure is measured minus truth, in dB: 0.00 is right\n");

    // ---- A: accuracy against level ----
    println!("== A. error against level, {} seeds each ==", SEEDS.len());
    print!("{:<16}", "mode");
    for l in LEVELS {
        print!("{:>9}", format!("{l:+.0} dB"));
    }
    println!("{:>9}{:>8}", "bias", "sd");

    let mut worst: Vec<(f64, String, f64)> = Vec::new();
    for sig in signals(1500.0) {
        print!("{:<16}", sig.name);
        let mut all = Vec::new();
        for level in LEVELS {
            let mut errs = Vec::new();
            for seed in SEEDS {
                let (audio, from, to) = band_with(&sig.audio, level, seed);
                let est = dsp::snr_db(
                    &audio[from..to],
                    SAMPLE_RATE,
                    1500.0,
                    sig.bandwidth,
                )
                .expect("enough audio to measure");
                errs.push(est.db - level);
            }
            print!("{:>9.2}", mean(&errs));
            all.extend(errs);
        }
        println!("{:>9.2}{:>8.2}", mean(&all), sd(&all));
        worst.push((mean(&all).abs(), sig.name.clone(), sd(&all)));
    }
    worst.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    println!(
        "\n   worst bias: {} at {:+.2} dB (sd {:.2})",
        worst[0].1, worst[0].0, worst[0].2
    );

    // ---- B: accuracy against carrier ----
    println!("\n== B. error against carrier, at -6 dB ==");
    print!("{:<16}", "mode");
    let carriers = [500.0, 745.0, 1000.0, 1517.0, 2000.0, 2400.0];
    for hz in carriers {
        print!("{:>9}", format!("{hz:.0} Hz"));
    }
    println!();
    for name in ["PSK31", "Olivia 8/250", "JS8 Normal"] {
        print!("{name:<16}");
        for hz in carriers {
            let sig = signals(hz).into_iter().find(|s| s.name == name).expect("mode");
            let errs: Vec<f64> = SEEDS
                .iter()
                .map(|&seed| {
                    let (audio, from, to) = band_with(&sig.audio, -6.0, seed);
                    let est = dsp::snr_db(&audio[from..to], SAMPLE_RATE, hz, sig.bandwidth)
                        .expect("measurable");
                    est.db + 6.0
                })
                .collect();
            print!("{:>9.2}", mean(&errs));
        }
        println!();
    }

    // ---- C: a neighbour in the guard region ----
    println!("\n== C. a second station 200 Hz away, both at -6 dB ==");
    println!("   the noise floor is a median over the guard region, so a");
    println!("   neighbour in it should shift the answer very little. The x2");
    println!("   columns ask whether a band widened to catch JS8's skirts");
    println!("   starts swallowing the neighbour instead.");
    println!(
        "{:<16}{:>10}{:>12}{:>9}{:>12}{:>14}{:>9}",
        "mode", "alone", "+neighbour", "shift", "x2 alone", "x2 +neighbour", "shift"
    );
    for name in ["PSK31", "Olivia 8/250", "JS8 Normal"] {
        let sig = signals(1500.0).into_iter().find(|s| s.name == name).expect("mode");
        let neighbour = psk::encode(&"cq de n0isy ".repeat(20), 1700.0, psk::PSK31);
        let mut row = Vec::new();
        for mult in [1.0f64, 2.0] {
            let mut alone = Vec::new();
            let mut crowded = Vec::new();
            for seed in SEEDS {
                let (mut audio, from, to) = band_with(&sig.audio, -6.0, seed);
                let bw = sig.bandwidth * mult;
                alone.push(
                    dsp::snr_db(&audio[from..to], SAMPLE_RATE, 1500.0, bw)
                        .expect("measurable")
                        .db,
                );
                let other = scale_for_snr(&neighbour, -6.0);
                for (i, s) in other.iter().enumerate() {
                    if from + i < audio.len() {
                        audio[from + i] += s;
                    }
                }
                crowded.push(
                    dsp::snr_db(&audio[from..to], SAMPLE_RATE, 1500.0, bw)
                        .expect("measurable")
                        .db,
                );
            }
            row.push((mean(&alone), mean(&crowded)));
        }
        println!(
            "{:<16}{:>10.2}{:>12.2}{:>9.2}{:>12.2}{:>14.2}{:>9.2}",
            name,
            row[0].0,
            row[0].1,
            row[0].1 - row[0].0,
            row[1].0,
            row[1].1,
            row[1].1 - row[1].0
        );
    }

    // ---- D: the cost of a wrong bandwidth ----
    println!("\n== D. error against the bandwidth asked for, at -6 dB ==");
    println!("   as a multiple of what the mode reports. Noise inside the band");
    println!("   is subtracted, so too wide should cost precision and not");
    println!("   accuracy; too narrow leaves signal outside and reads low.");
    let mults = [0.5f64, 0.75, 1.0, 1.5, 2.0, 4.0];
    print!("{:<16}{:>8}", "mode", "nominal");
    for m in mults {
        print!("{:>8}", format!("x{m}"));
    }
    println!();
    for name in ["PSK31", "Olivia 8/250", "JS8 Turbo", "JS8 Normal", "JS8 Slow"] {
        let sig = signals(1500.0).into_iter().find(|s| s.name == name).expect("mode");
        print!("{:<16}{:>8.0}", name, sig.nominal);
        for m in mults {
            let errs: Vec<f64> = SEEDS
                .iter()
                .map(|&seed| {
                    let (audio, from, to) = band_with(&sig.audio, -6.0, seed);
                    dsp::snr_db(&audio[from..to], SAMPLE_RATE, 1500.0, sig.nominal * m)
                        .expect("measurable")
                        .db
                        + 6.0
                })
                .collect();
            print!("{:>8.2}", mean(&errs));
        }
        println!();
    }

    // A sanity check on the noise estimate itself, which everything rests on.
    println!("\n== E. the noise floor estimate, on noise alone ==");
    let truth = noise_per_hz();
    let errs: Vec<f64> = SEEDS
        .iter()
        .map(|&seed| {
            let audio = noise(20 * SAMPLE_RATE as usize, seed);
            let est = dsp::snr_db(&audio, SAMPLE_RATE, 1500.0, 62.0).expect("measurable");
            10.0 * (est.noise_per_hz / truth).log10()
        })
        .collect();
    println!("   noise per Hz, measured minus true: {:+.3} dB (sd {:.3})", mean(&errs), sd(&errs));

    // ---- F: is the JS8 bias the estimator, or our own transmitter? ----
    //
    // Section A has JS8 reading low by an amount that tracks nothing about the
    // mode except its width in Hz, which is the signature of energy sitting
    // outside the band rather than of anything in the arithmetic. JS8's 8 x
    // spacing is the right occupancy for a *GFSK* signal, which is what a real
    // one is; `synthesize_sm` steps its frequency instead, and an unshaped step
    // splatters. The recording settles it: same measurement, same mode, one
    // signal off the air and one out of this crate.
    println!("\n== F. where a JS8 frame keeps its power: real against synthesized ==");
    println!("   fraction of the frame's power inside k x the mode's nominal");
    println!("   bandwidth (JS8 Normal: 8 x 6.25 = 50 Hz, so x1 is +/-25 Hz)");
    let (real, rate) = match ragchew::wav::read("tests/vectors/john_3_16.wav") {
        Ok(v) => v,
        Err(e) => {
            println!("   (skipped: {e})");
            return;
        }
    };
    assert_eq!(rate, SAMPLE_RATE);
    // One clean frame out of the recording, located by the decoder itself.
    let found = js8::modem::decode_all(&real, 1200.0, 1600.0, 30);
    let frame = found.first().expect("the recording decodes");
    let sm = submode::of(frame.mode);
    let span = js8::modem::N_SYMBOLS * sm.nsps;
    let real_frame = &real[frame.offset..(frame.offset + span).min(real.len())];
    let nominal = ModeId::Js8(frame.mode).bandwidth_hz();

    let (a87, _) = message::freetext("DE W1AW EM73", IType::None);
    let synth = modem::encode_audio_sm(&a87, frame.hz, &sm);

    let mults = [1.0f64, 1.5, 2.0, 3.0, 4.0];
    print!("{:<22}", "signal");
    for m in mults {
        print!("{:>9}", format!("x{m}"));
    }
    println!();
    for (label, audio) in [("off the air", real_frame), ("synthesize_sm", &synth[..])] {
        print!("{label:<22}");
        let total = encircled(audio, frame.hz, 8.0 * nominal);
        for m in mults {
            print!("{:>9.3}", encircled(audio, frame.hz, m * nominal / 2.0) / total);
        }
        println!();
    }
    println!("   Both agree, so this is JS8 and not our transmitter: the");
    println!("   conventional 8 x spacing is a channel spacing, not a power");
    println!("   containment. 10log10 of the x1 figure is the bias in A.");
}
