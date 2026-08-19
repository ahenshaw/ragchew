//! How fast each mode is, and how deep it hears — measured, not quoted.
//!
//! Every mode in this crate trades speed against sensitivity, and the two
//! numbers are the whole reason there is more than one mode. This measures both
//! against the same yardstick so they can be put side by side:
//!
//! - **Speed** in words per minute, five characters to a word including its
//!   space, which is how every other keyboard mode is quoted. Derived from what
//!   the modem actually sends, not from a published table — for JS8 that means
//!   the characters a real frame swallows, which depends on the text, so it is
//!   measured over the same sentence as everything else.
//! - **Sensitivity** as the lowest SNR at which a whole over comes back
//!   character for character, in the 2500 Hz reference bandwidth WSJT-X and
//!   JS8Call report against. Not a detection: exact copy. A mode that finds a
//!   station whose text is in pieces has not heard it.
//!
//! Copy is asked of `protocol::decode_all`, which is what the application runs
//! — gate, arbitration and all — so the answer is the reach of this program
//! rather than of a mode in the abstract.
//!
//! The strict figure is a measure of *reliability*, and for JS8 that is mostly
//! what it measures. An over there is several frames and every one of them has
//! to land, so the column punishes a mode with a marginal frame far more than
//! the half column does — which is why Turbo, whose over is two frames, shows a
//! twelve-decibel gap between the two columns where every other mode shows two
//! or three, and why Slow appears to reach *less* far than Normal in the strict
//! column while reaching three decibels further in the half column. Compare
//! submodes by the half column; place a station in a demo band by the strict
//! one.
//!
//! Two of the three protocols carry a variable number of characters per second,
//! so a speed figure is a claim about the text as much as about the mode, and
//! published tables differ from these for that reason before any other. A JS8
//! frame here swallows 13 characters of English and 14 of a callsign line, but
//! 27 of a string built to suit its varicode; PSK31
//! types 47 words a minute in lower-case English and 37 sending callsigns. What
//! is quoted below is ordinary prose in the case that mode's operators type it
//! in — see [`PROSE`] — which is the only comparison between three unlike
//! protocols that means anything. Olivia alone is a fixed rate whatever is
//! sent.
//!
//! Run: `cargo run --release --example sensitivity [mode ...]`
//!
//! With no arguments it walks every mode. Name modes (`psk31`, `js8:Normal`,
//! `16/500`) to measure a few, which is what makes it usable while calibrating
//! a demo band.

use ragchew::protocol::{self, ModeId};
use ragchew::{js8, olivia, psk, SAMPLE_RATE};

/// Noise RMS everything is measured against. Any value works — only the ratio
/// is ever used — and this is the one `gui/src/demo.rs` builds its bands with.
const NOISE_RMS: f64 = 0.08;

/// The bandwidth SNR is quoted in, as amateur weak-signal work always quotes
/// it.
const SNR_REF_BW: f64 = 2500.0;

/// Trials per SNR point. Each gets its own noise, and a mode has to copy all of
/// them to own the level: a threshold that holds seven times in eight is a
/// threshold a demo band would fail on intermittently.
const TRIALS: usize = 8;

/// The sentence every mode sends. Ordinary QSO text — the varicodes in JS8 and
/// PSK31 are tuned for it, so anything more exotic would measure the alphabet
/// rather than the mode.
const TEXT: &str = "CQ CQ DE W1AW W1AW K ";

/// The same sentence as a PSK31 operator would actually type it.
///
/// Varicode gives its shortest codes to lower case, so case is worth about a
/// third of the mode's speed, and PSK31 operators type accordingly. Measuring
/// PSK31 on upper case would be measuring a habit nobody has. JS8 and Olivia
/// have no such choice to make — JS8's varicode is upper case and Olivia's
/// 7-bit characters cost the same either way — so they send [`TEXT`].
const TEXT_LC: &str = "cq cq de w1aw w1aw k ";

/// What a station sends in this mode, and how many times over.
///
/// PSK31 repeats because there is no other way to measure it. It has no frame
/// to synchronize on, so the detector will not believe in a station until it
/// has been on the air for most of an 8.2-second analysis block — the opening
/// of every over is lost at any SNR whatever, exactly as it is when you tune
/// across a real one. Asked for a whole over it would report "never" at +6 dB,
/// which would be a measurement of the gate's latency and not of the mode's
/// reach. So it sends its call three times, as a station calling CQ does, and
/// owes two clean copies.
fn over_of(mode: ModeId) -> (&'static str, usize, usize) {
    match mode {
        ModeId::Psk(_) => (TEXT_LC, 3, 2),
        _ => (TEXT, 1, 1),
    }
}

/// Ordinary prose, which is what a words-per-minute figure has to be quoted
/// over.
///
/// The threshold above is measured on QSO text because that is what is on the
/// air when it matters; speed cannot be, because callsigns are the most
/// expensive characters either varicode has. The same PSK31 station types at
/// 37 wpm sending "cq cq de w1aw w1aw k" and 47 wpm sending English, and the
/// second is the number every published table means.
const PROSE: &str = "THE QUICK BROWN FOX JUMPS OVER THE LAZY DOG ";
const PROSE_LC: &str = "the quick brown fox jumps over the lazy dog ";

/// The text a speed figure is measured over, in the case this mode's operators
/// would really type.
fn prose_for(mode: ModeId) -> &'static str {
    match mode {
        ModeId::Psk(_) => PROSE_LC,
        _ => PROSE,
    }
}

fn noise(n: usize, seed: u64) -> Vec<f32> {
    let mut s = seed;
    let raw: Vec<f32> = (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            (s >> 40) as f32 / (1u32 << 24) as f32 - 0.5
        })
        .collect();
    let rms = (raw.iter().map(|v| v * v).sum::<f32>() / raw.len() as f32).sqrt();
    raw.iter().map(|v| v * (NOISE_RMS as f32) / rms).collect()
}

fn power(sig: &[f32]) -> f64 {
    sig.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / sig.len().max(1) as f64
}

/// `sig` over fresh noise, scaled to sit `snr_db` above it in [`SNR_REF_BW`],
/// with a second of quiet either side so the scanner has somewhere to measure a
/// floor and somewhere to stop.
fn over_noise(sig: &[f32], snr_db: f64, seed: u64) -> Vec<f32> {
    let lead = SAMPLE_RATE as usize;
    let total = sig.len() + 2 * lead;
    let mut buf = noise(total, seed);
    let n_per_hz = NOISE_RMS * NOISE_RMS / (SAMPLE_RATE as f64 / 2.0);
    let want = n_per_hz * SNR_REF_BW * 10f64.powf(snr_db / 10.0);
    let amp = (want / power(sig).max(f64::MIN_POSITIVE)).sqrt() as f32;
    for (i, s) in sig.iter().enumerate() {
        buf[lead + i] += *s * amp;
    }
    buf
}

/// What one over of `text` looks like on the air in `mode`.
///
/// JS8 is the odd one: it sends whole frames on a cycle, so an over is however
/// many frames the text needs, each in its own 0.5-s-lead cycle slot, exactly
/// as `gui/src/demo.rs` lays them out.
fn encode(text: &str, hz: f64, mode: ModeId) -> Vec<f32> {
    match mode {
        ModeId::Olivia(m) => olivia::encode(text, hz, m),
        ModeId::Psk(m) => psk::encode(text, hz, m),
        ModeId::Js8(m) => {
            let sm = js8::submode::of(m);
            let rate = SAMPLE_RATE as usize;
            let mut out = vec![0.0f32; 0];
            let mut rest = text;
            let mut cyc = 0;
            while !rest.is_empty() {
                let (a87, used) = js8::message::freetext(rest, js8::IType::None);
                let burst = js8::modem::encode_audio_sm(&a87, hz, &sm);
                let start = cyc * sm.period_s as usize * rate + rate / 2;
                out.resize((start + burst.len()).max(out.len()), 0.0);
                for (i, s) in burst.iter().enumerate() {
                    out[start + i] += *s;
                }
                rest = &rest[used.min(rest.len())..];
                cyc += 1;
            }
            // Out to the end of the last cycle, so the over occupies the air
            // for as long as it really would.
            out.resize(cyc * sm.period_s as usize * rate, 0.0);
            out
        }
    }
}

/// Everything the application reads at `hz`, in the order it was said.
fn copy(samples: &[f32], hz: f64, mode: ModeId) -> String {
    let half = mode.bandwidth_hz().max(100.0);
    let mut d = protocol::decode_all(samples, hz - half, hz + half, &[mode]);
    d.retain(|d| (d.hz - hz).abs() < mode.assoc_tol_hz());
    d.sort_by(|a, b| a.time_s.partial_cmp(&b.time_s).unwrap());
    d.iter().map(|d| d.text.as_str()).collect()
}

/// Characters of `TEXT` a single over carries, and how long it is on the air.
///
/// For JS8 the first is what one frame's varicode swallowed and the second is
/// the mode's cycle: a station that keys for 12.6 seconds of a 15-second cycle
/// still only gets to say the next thing 15 seconds later, so the cycle is the
/// honest denominator.
fn chars_and_secs(mode: ModeId) -> (f64, f64) {
    let text = prose_for(mode);
    match mode {
        ModeId::Olivia(m) => (m.chars_per_sec(), 1.0),
        ModeId::Psk(m) => {
            let audio = psk::encode(text, 1500.0, m);
            (text.chars().count() as f64, audio.len() as f64 / SAMPLE_RATE as f64)
        }
        ModeId::Js8(m) => {
            let (_, used) = js8::message::freetext(text, js8::IType::None);
            (used as f64, js8::submode::of(m).period_s as f64)
        }
    }
}

/// Words per minute: characters per second, five characters to a word.
fn wpm(mode: ModeId) -> f64 {
    let (chars, secs) = chars_and_secs(mode);
    chars / secs * 60.0 / 5.0
}

/// The two levels a mode can be said to reach, in whole decibels.
///
/// `.0` is where **every** over of [`TRIALS`] copies exactly, and `.1` is where
/// **half** of them do. The gap between them is a couple of decibels and it
/// matters which one is being quoted: published tables for these modes state
/// the second kind — WSJT-X's figures are the level at which about half of
/// single transmissions decode — while anything built to be *demonstrated*, a
/// band in `gui/src/demo.rs` above all, has to stand on the first.
///
/// Walks down from a level nothing should struggle at, and stops two decibels
/// after the last trial that copied anything: modes do not recover once they
/// start losing characters, and the second decibel is there to say so rather
/// than to be believed.
fn thresholds(mode: ModeId) -> (Option<f64>, Option<f64>) {
    let (text, reps, owed) = over_of(mode);
    let sent = text.repeat(reps);
    let (mut solid, mut half) = (None, None);
    // Once a level has failed, a lower one that happens to pass is a fluke of
    // eight noise seeds and not a reach. Both figures are the bottom of an
    // unbroken run down from the top, so the first failure closes that figure.
    let (mut solid_run, mut half_run) = (true, true);
    let mut dead = 0;
    for step in 0..40 {
        let snr = 6.0 - step as f64;
        let hits = (0..TRIALS)
            .filter(|t| {
                // A different carrier each trial, and never on a round number:
                // a mode that only works when the station is tuned to the
                // middle of an FFT bin is not a mode that works.
                let hz = 800.0 + (*t as f64 * 173.0) % 900.0 + 3.7;
                let sig = encode(&sent, hz, mode);
                let got = copy(&over_noise(&sig, snr, 0x5E01_0000 + *t as u64), hz, mode);
                got.matches(text.trim_end()).count() >= owed
            })
            .count();
        if hits == TRIALS && solid_run {
            solid = Some(snr);
        } else {
            solid_run = false;
        }
        if hits * 2 >= TRIALS && half_run {
            half = Some(snr);
        } else {
            half_run = false;
        }
        if hits == 0 {
            dead += 1;
            if dead >= 2 {
                break;
            }
        } else {
            dead = 0;
        }
    }
    (solid, half)
}

fn main() {
    let want: Vec<String> = std::env::args().skip(1).collect();
    let all: Vec<ModeId> = js8::submode::ALL
        .iter()
        .map(|sm| ModeId::Js8(sm.mode))
        .chain(olivia::COMMON.iter().map(|m| ModeId::Olivia(*m)))
        .chain(psk::COMMON.iter().map(|m| ModeId::Psk(*m)))
        .collect();
    let modes: Vec<ModeId> = if want.is_empty() {
        all
    } else {
        want.iter()
            .map(|s| protocol::parse_mode(s).unwrap_or_else(|| panic!("unknown mode {s:?}")))
            .collect()
    };

    println!("{TRIALS} trials per level, over of {TEXT:?}");
    println!(
        "\n  {:<16} {:>7} {:>11} {:>11} {:>12}",
        "mode", "wpm", "every over", "half", "over takes"
    );
    for mode in modes {
        let (solid, half) = thresholds(mode);
        let (text, reps, _) = over_of(mode);
        let secs = encode(&text.repeat(reps), 1500.0, mode).len() as f64 / SAMPLE_RATE as f64;
        let db = |t: Option<f64>| t.map_or("never".to_string(), |t| format!("{t:+.0} dB"));
        println!(
            "  {:<16} {:>7.1} {:>11} {:>11} {:>10.1} s",
            mode.name(),
            wpm(mode),
            db(solid),
            db(half),
            secs,
        );
    }
}
