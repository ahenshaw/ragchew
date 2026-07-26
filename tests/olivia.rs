//! Olivia end to end: modulate, put it in a band with other signals and some
//! noise, and get the text back.
//!
//! The decoder has to find a signal that carries no sync pattern, is spread
//! thin enough not to stand out by energy, and can start at any frequency and
//! any moment — so the properties worth pinning down are: it finds real signals
//! down to roughly the sensitivity the mode is known for, and it invents
//! nothing when there is nothing there.

use ragchew::olivia::{self, Mode};
use ragchew::SAMPLE_RATE;

const RATE: usize = SAMPLE_RATE as usize;

/// Deterministic uniform noise, so a failure is always reproducible.
fn noise(n: usize, amp: f32, seed: u64) -> Vec<f32> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            (((s >> 40) as f32 / (1u32 << 24) as f32) - 0.5) * amp
        })
        .collect()
}

/// A transmission starting `start_s` into a noisy recording.
fn band(text: &str, hz: f64, mode: Mode, start_s: f64, noise_amp: f32, seed: u64) -> Vec<f32> {
    let sig = olivia::encode(text, hz, mode);
    let start = (start_s * RATE as f64) as usize;
    let mut audio = noise(start + sig.len() + 3 * RATE, noise_amp, seed);
    for (i, s) in sig.iter().enumerate() {
        audio[start + i] += *s;
    }
    audio
}

fn decoded(res: &[olivia::DecodeResult]) -> String {
    res.iter().map(|r| r.text()).collect()
}

const TEXT: &str = "CQ CQ DE W1AW W1AW K 73 GL OM ES TU ";

/// Every common mode round-trips through a wide band search, with no idea in
/// advance where or when the signal starts.
#[test]
fn every_common_mode_round_trips() {
    for mode in olivia::COMMON {
        let audio = band(TEXT, 1500.0, mode, 1.7, 0.0, 1);
        let res = olivia::decode_all_mode(&audio, 300.0, 2600.0, mode);
        assert_eq!(decoded(&res), TEXT, "{} did not round-trip", mode.name());
        assert!(
            (res[0].hz - 1500.0).abs() < mode.spacing(),
            "{} put the signal at {:.1} Hz",
            mode.name(),
            res[0].hz
        );
    }
}

/// A signal is found wherever it starts, in frequency and in time — neither is
/// on any grid the decoder knows about.
#[test]
fn finds_signals_at_arbitrary_offsets() {
    let mode = olivia::OL_16_500;
    for (hz, start_s) in [(812.3, 0.0), (1499.7, 0.37), (2107.9, 2.13), (1000.0, 1.05)] {
        let audio = band(TEXT, hz, mode, start_s, 2.0, 7);
        let res = olivia::decode_all_mode(&audio, 600.0, 2400.0, mode);
        assert_eq!(decoded(&res), TEXT, "missed the signal at {hz} Hz / {start_s} s");
        assert!((res[0].hz - hz).abs() < mode.spacing(), "found it at {:.1} Hz", res[0].hz);
    }
}

/// Sensitivity: a full copy of a signal well under the noise.
///
/// The noise here is uniform across the whole 6 kHz Nyquist band and about 15
/// dB above the signal in RMS, which is roughly -11 dB in the 2.5 kHz
/// bandwidth Olivia's published thresholds are quoted in — within a couple of
/// dB of what these modes are documented to manage. This is the interesting
/// property of the mode, so it is worth a regression test even though the exact
/// margin depends on the noise seed.
#[test]
fn decodes_well_below_the_noise() {
    for mode in [olivia::OL_8_250, olivia::OL_16_500, olivia::OL_4_125] {
        let audio = band(TEXT, 1500.0, mode, 1.0, 14.0, 4242);
        let res = olivia::decode_all_mode(&audio, 300.0, 2600.0, mode);
        assert_eq!(decoded(&res), TEXT, "{} lost the signal in the noise", mode.name());
    }
}

/// Nothing is invented from noise alone. A band scan tests millions of
/// frequency/timing hypotheses, so this is the property the decision thresholds
/// exist to protect.
#[test]
fn invents_nothing_from_noise() {
    let audio = noise(45 * RATE, 1.0, 20260726);
    for mode in olivia::COMMON {
        let res = olivia::decode_all_mode(&audio, 300.0, 2600.0, mode);
        assert!(
            res.is_empty(),
            "{} hallucinated {} blocks from noise: {:?}",
            mode.name(),
            res.len(),
            decoded(&res)
        );
    }
}

/// Silence is not a signal either — with no energy at all there is nothing to
/// normalise, and the decoder must not divide its way into a decode.
#[test]
fn invents_nothing_from_silence() {
    let audio = vec![0.0f32; 30 * RATE];
    for mode in olivia::COMMON {
        assert!(olivia::decode_all_mode(&audio, 300.0, 2600.0, mode).is_empty(), "{}", mode.name());
    }
}

/// Two stations in the same band, in different modes, both come back — and
/// neither is reported twice under the other's mode.
#[test]
fn separates_two_stations_in_different_modes() {
    let a = olivia::encode("DE W1AW ES TU 73 ", 900.0, olivia::OL_8_250);
    let b = olivia::encode("CQ DE VK3ABC K ", 2000.0, olivia::OL_16_500);
    let mut audio = noise(a.len().max(b.len()) + 6 * RATE, 1.0, 55);
    for (i, s) in a.iter().enumerate() {
        audio[RATE + i] += *s;
    }
    for (i, s) in b.iter().enumerate() {
        audio[2 * RATE + i] += *s;
    }

    let res = olivia::decode_all(&audio, 300.0, 2600.0, &olivia::COMMON);
    let text_near = |hz: f64| -> String {
        res.iter().filter(|r| (r.hz - hz).abs() < 100.0).map(|r| r.text()).collect()
    };
    assert_eq!(text_near(900.0), "DE W1AW ES TU 73 ");
    assert_eq!(text_near(2000.0), "CQ DE VK3ABC K ");

    let modes: Vec<String> = {
        let mut m: Vec<String> = res.iter().map(|r| r.mode.name()).collect();
        m.sort();
        m.dedup();
        m
    };
    assert_eq!(modes, vec!["Olivia 16/500", "Olivia 8/250"], "extra modes claimed the same signals");
}

/// Both sides of a QSO are copied, though they share one frequency slot.
///
/// This is what a real Olivia contact looks like and it defeats the obvious
/// decoder design. The two stations sit a fraction of a tone spacing apart —
/// far closer than a bandwidth, so any "one signal per bandwidth" rule throws
/// one of them away — and each keys up on its own block boundary, so any "one
/// block timing per frequency" rule throws the other away. Only their *times*
/// separate them cleanly.
#[test]
fn copies_both_sides_of_a_qso() {
    let mode = olivia::OL_8_500;
    let mut audio = noise(70 * RATE, 1.0, 31337);

    // 15.6 Hz apart — a quarter of this mode's tone spacing.
    let overs: [(&str, f64, f64); 3] = [
        ("DE K3CC ES BTU ", 968.75, 1.0),
        ("K3CC DE KC2VUT ", 984.4, 22.3),   // ...and not on a block boundary
        ("RR TU 73 SK ", 968.75, 45.7),
    ];
    for (text, hz, start_s) in overs {
        let sig = olivia::encode(text, hz, mode);
        let start = (start_s * RATE as f64) as usize;
        for (i, s) in sig.iter().enumerate() {
            audio[start + i] += *s;
        }
    }

    let res = olivia::decode_all_mode(&audio, 300.0, 2600.0, mode);
    for (text, _, start_s) in overs {
        let over: String = res
            .iter()
            .filter(|r| {
                let t = r.offset as f64 / RATE as f64;
                t >= start_s - 1.0 && t < start_s + 1.0 + text.len() as f64 / mode.chars_per_sec()
            })
            .map(|r| r.text())
            .collect();
        assert_eq!(over, text, "lost the over starting at {start_s} s");
    }
}

/// A wide mode sitting low in the band still works: its lowest tones approach
/// 0 Hz, which is a boundary the search has to clamp rather than give up on.
#[test]
fn decodes_a_wide_mode_low_in_the_band() {
    let mode = olivia::OL_32_1000;
    let audio = band("DE W1AW ", 700.0, mode, 0.5, 1.0, 11);
    let res = olivia::decode_all_mode(&audio, 300.0, 2600.0, mode);
    assert_eq!(decoded(&res), "DE W1AW ");
}

/// The mode's own timing figures are what the modulator actually produces: a
/// whole number of blocks, plus the half symbol of raised-cosine shaping that
/// hangs off each end.
#[test]
fn transmission_length_matches_the_mode() {
    for mode in olivia::COMMON {
        let text = "0123456789";
        let audio = olivia::encode(text, 1500.0, mode);
        let blocks = text.len().div_ceil(mode.chars_per_block());
        let body = blocks * olivia::SYMBOLS_PER_BLOCK * mode.symbol_separ();
        assert_eq!(audio.len(), body + mode.symbol_len(), "{}", mode.name());
        assert!(
            (body as f64 / RATE as f64 - blocks as f64 * mode.block_secs()).abs() < 1e-9,
            "{}: block_secs disagrees with the modulator",
            mode.name()
        );
    }
}
