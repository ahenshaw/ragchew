//! A synthetic multi-protocol band for the app's `--demo` mode and the headless
//! scene validator.
//!
//! Eleven stations: all four JS8 submodes on their own cycles, plus three
//! Olivia stations rag-chewing continuously across the same band, under a little
//! noise, inside the 300–2600 Hz the app scans.
//!
//! Carriers are spaced so no two occupied bandwidths collide — Olivia is wide,
//! so it takes most of the room — with one deliberate exception: a JS8 station
//! sits squarely inside the Olivia 16/500 signal at 2000 Hz. Real bands are not
//! tidy, and a monitor that only works when signals are politely separated is
//! not much of a monitor. It also makes the point that the two protocols are
//! tracked independently: they share a patch of spectrum without sharing a row
//! of text.
//!
//! They do interfere, and asymmetrically. The JS8 frames come through intact —
//! Olivia spreads its power over sixteen tones, so only one or two ever land in
//! JS8's 50 Hz, and the LDPC absorbs that. The Olivia station loses a couple of
//! blocks, because the JS8 carrier is narrow and strong and parks in one of its
//! tone bins for a whole frame. That asymmetry is real, not a defect in the
//! demo: it is what QRM costs each mode.

use ragchew::js8::message::{self, IType};
use ragchew::js8::modem;
use ragchew::js8::submode::{self, Submode};
use ragchew::olivia::{self, Mode as Olivia};
use ragchew::SAMPLE_RATE;

/// (carrier Hz, submode, per-cycle frame texts). Each station keeps its carrier
/// and transmits one frame per cycle of its mode. Carriers are chosen so the
/// per-mode bandwidths (Slow 25, Normal 50, Fast 80, Turbo 160 Hz) don't collide.
pub const JS8_STATIONS: &[(f64, Submode, &[&str])] = &[
    (500.0, submode::SLOW, &["CQ DE W1SLW ", "EM73 PSE K "]),
    (700.0, submode::NORMAL, &["CQ CQ DE K2N ", "FN20 5W ", "73 GL "]),
    (850.0, submode::NORMAL, &["DE VE3XYZ ", "RRR TU ", "SK E E "]),
    (1480.0, submode::FAST, &["CQ DE N0FST ", "DN70 ", "QRZ? "]),
    (1620.0, submode::TURBO, &["TEST DE W9T ", "FAST QSO ", "FB 73 GL "]),
    (2320.0, submode::NORMAL, &["DE G4ABC ", "IO91 QRP ", "TU 72 "]),
    (2460.0, submode::TURBO, &["DE ZL2T ", "QRO 400W ", "GL SK "]),
    // Deliberately co-channel with the Olivia 16/500 station below: its 50 Hz
    // lands in the middle of that signal's 500, and its first two frames run
    // while the Olivia station is transmitting.
    (2000.0, submode::NORMAL, &["DE W2QRM ", "IN THE MUD ", "TU 73 "]),
];

/// (centre Hz, mode, start time in seconds, text). Olivia stations transmit
/// continuously from `start` at whatever rate their mode manages, so the text
/// simply runs on until it is finished.
pub const OLIVIA_STATIONS: &[(f64, Olivia, f64, &str)] = &[
    (1100.0, olivia::OL_8_250, 1.0, "CQ CQ DE KN4RAG KN4RAG K "),
    (1330.0, olivia::OL_4_125, 3.0, "DE VE7OLV QRP 5W "),
    (2000.0, olivia::OL_16_500, 6.0, "GM OM UR RST 579 IN EM73 BTU "),
];

/// Synthesize the demo band (12 kHz mono, one minute).
pub fn synth() -> Vec<f32> {
    let rate = SAMPLE_RATE as usize;
    let total = 60 * rate;
    let mut buf = vec![0.0f32; total];

    let mix = |buf: &mut Vec<f32>, start: usize, samples: &[f32], amp: f32| {
        for (i, s) in samples.iter().enumerate() {
            if start + i < total {
                buf[start + i] += *s * amp;
            }
        }
    };

    for (hz, sm, msgs) in JS8_STATIONS {
        for (cyc, m) in msgs.iter().enumerate() {
            let (a87, _) = message::freetext(m, IType::None);
            let burst = modem::encode_audio_sm(&a87, *hz, sm);
            // frame starts 0.5 s into cycle `cyc` of this mode
            let start = (cyc * sm.period_s as usize) * rate + rate / 2;
            mix(&mut buf, start, &burst, 0.45);
        }
    }

    for (hz, mode, start_s, text) in OLIVIA_STATIONS {
        let audio = olivia::encode(text, *hz, *mode);
        mix(&mut buf, (start_s * rate as f64) as usize, &audio, 0.35);
    }

    // light deterministic noise
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    for s in buf.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let n = ((seed >> 40) as f32 / (1u32 << 24) as f32) - 0.5;
        *s += n * 0.05;
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one place two stations share spectrum is the deliberate one.
    ///
    /// Everything else is spaced clear, so that a station failing to decode in
    /// the demo means something is wrong with the decoder rather than with the
    /// band plan.
    #[test]
    fn only_the_deliberate_collision_overlaps() {
        let mut bands: Vec<(f64, f64, String)> = Vec::new();
        for (hz, sm, _) in JS8_STATIONS {
            let bw = 8.0 * sm.spacing();
            bands.push((hz - bw / 2.0, hz + bw / 2.0, format!("JS8 {} @ {hz}", sm.mode.name())));
        }
        for (hz, m, _, _) in OLIVIA_STATIONS {
            let bw = m.bandwidth as f64;
            bands.push((hz - bw / 2.0, hz + bw / 2.0, format!("{} @ {hz}", m.name())));
        }
        bands.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        let collisions: Vec<String> = bands
            .windows(2)
            .filter(|w| w[0].1 > w[1].0)
            .map(|w| format!("{} / {}", w[0].2, w[1].2))
            .collect();
        assert_eq!(
            collisions,
            vec!["Olivia 16/500 @ 2000 / JS8 Normal @ 2000"],
            "unintended overlap in the demo band"
        );
    }
}
