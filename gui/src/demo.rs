//! Synthetic bands for the app's demo modes and the headless scene validator.
//!
//! [`synth`] builds the mixed-protocol band behind `--demo`; [`synth_weak`]
//! builds the Olivia weak-signal band behind `--weak`, where the whole point is
//! how far under the noise a signal can be and still be read.
//!
//! Twelve stations across all three protocols: all four JS8 submodes on their
//! own cycles, three Olivia stations rag-chewing continuously, and a PSK31
//! station calling CQ over and over the way a real one does — under a little
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
//! The PSK31 station is in the clear, at 620 Hz, in a 62 Hz gap between the JS8
//! stations that nothing else here is narrow enough to use. That is a choice,
//! not an oversight: it was also tried at 2150 Hz, inside the Olivia 16/500
//! signal, where it copies five of its seven calls intact and costs the Olivia
//! station nothing. Move it there — one line below — for a band where three
//! protocols share one patch of spectrum.
//!
//! What kept it in the clear is that the two damaged calls are damaged by real
//! QRM, and PSK31 is the one mode here with no error correction to absorb it.
//! A demo that shows the new mode dropping characters in the one place a reader
//! cannot tell interference from a broken decoder is a poor first impression,
//! and the band already carries a deliberate collision to make the co-channel
//! point.
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
use ragchew::psk::{self, PSK31};
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

/// (carrier Hz, start time in seconds, the call, times it is repeated).
///
/// PSK31 is continuous like Olivia, but a station has to be on the air for most
/// of one 8.2-second analysis block before the detector will believe it, and
/// one CQ call is shorter than that. So a station repeats — which is what
/// calling CQ *is*, and what makes this the easiest kind of PSK31 signal to
/// find as well as the most common.
///
/// Lower case on purpose: varicode gives its shortest codes to lower-case
/// letters, and PSK31 operators type accordingly.
pub const PSK_STATIONS: &[(f64, f64, &str, usize)] = &[
    // In the clear, in the gap between two JS8 stations. Change to 2150.0 to
    // put it inside the Olivia 16/500 signal instead — see the module docs.
    (620.0, 0.2, "cq cq de kb9psk kb9psk pse k  ", 7),
];

/// What a PSK31 station puts on the air: its call, repeated.
pub fn psk_text(call: &str, repeats: usize) -> String {
    call.repeat(repeats)
}

/// One of the synthetic bands, as the command line and the Source menu offer it.
///
/// An enum rather than the pair of booleans this used to be, because there are
/// three of them now and "which band" is one question with one answer. The menu
/// is built by walking [`Band::ALL`], so a fourth is a variant and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Band {
    /// Three protocols at once, twelve stations. What the app is *for*.
    Demo,
    /// Olivia below the noise floor, plus the one station with no FEC.
    Weak,
    /// Olivia on top of itself, five modes, all read.
    Crowded,
}

impl Band {
    /// In the order the menu offers them: the mixed band first, being the one
    /// that shows what the app does, then the two that each make one point.
    pub const ALL: [Band; 3] = [Band::Demo, Band::Weak, Band::Crowded];

    pub fn label(self) -> &'static str {
        match self {
            Band::Demo => "Demo band",
            Band::Weak => "Weak-signal band",
            Band::Crowded => "Crowded band",
        }
    }

    pub fn hover(self) -> &'static str {
        match self {
            Band::Demo => "twelve stations, all three protocols, one of them deliberately \
                           sharing a frequency with another",
            Band::Weak => "five Olivia stations below the noise floor — invisible on the \
                           waterfall and read anyway — and a PSK31 station four decibels \
                           down, which is as deep as a mode with no error correction goes",
            Band::Crowded => "five Olivia modes stacked on each other: two kilohertz-wide \
                              stations meeting in the middle, one buried inside the first, \
                              and two more piled across the seam. Every character of all \
                              five is recovered, on five separate threads",
        }
    }

    /// What the status bar calls it once it is loaded.
    pub fn source_name(self) -> &'static str {
        match self {
            Band::Demo => "demo band",
            Band::Weak => "weak-signal demo",
            Band::Crowded => "crowded demo",
        }
    }

    pub fn synth(self) -> Vec<f32> {
        match self {
            Band::Demo => synth(),
            Band::Weak => synth_weak(),
            Band::Crowded => synth_crowded(),
        }
    }
}

/// Bandwidth (Hz) the SNR figures below are quoted in — the convention amateur
/// weak-signal work uses, so the numbers mean what a ham expects them to.
const SNR_REF_BW: f32 = 2500.0;

/// Noise RMS in the weak-signal demo, and the level its SNRs are measured
/// against.
const NOISE_RMS: f32 = 0.08;

/// One Olivia station in a synthetic band, at a stated level against the noise.
///
/// Shared by [`WEAK_STATIONS`] and [`CROWDED_STATIONS`], which demonstrate
/// opposite things with the same parts: how far *under* the noise a signal can
/// be and still be read, and how many signals can sit on top of each other and
/// still be read.
pub struct OliviaStation {
    pub hz: f64,
    pub mode: Olivia,
    pub start_s: f64,
    /// Level relative to the noise in [`SNR_REF_BW`]. 0 dB is the noise floor:
    /// the station is putting as much power into that bandwidth as the noise is.
    pub snr_db: f32,
    /// Whether this station is expected to copy in full — see [`WEAK_STATIONS`]
    /// for why one of them is not.
    pub full_copy: bool,
    pub text: &'static str,
}

/// The weak-signal band: four modes, several of them overlapping, each station
/// a stated number of dB above or below the noise floor.
///
/// Two things are on show. **Overlap**: a narrow station sits inside a wider
/// one's bandwidth twice over, which is an ordinary sight on the air and the
/// arrangement most likely to make a scanner drop the quieter of a pair. They
/// are kept within a few dB of each other, because a signal 10 dB stronger
/// really does bury its neighbour and that makes for a demonstration of physics
/// rather than of decoding.
///
/// **Mode against depth**: the levels are not equal, they are each mode's own
/// reach. Sensitivity tracks characters per second almost exactly — every mode
/// spreads a character over the same 64 chips, so the slower ones spend more
/// energy on each and hear further. Hence 4/125 copying at -12 dB while 8/500,
/// three times faster, is already in pieces at -15.
pub const WEAK_STATIONS: &[OliviaStation] = &[
    // A wide, slow host with a narrow station inside its 1 kHz.
    OliviaStation {
        hz: 900.0, mode: olivia::OL_32_1000, start_s: 0.4, snr_db: -9.0, full_copy: true,
        text: "DE W1WIDE 32/1000 -9 DB ",
    },
    OliviaStation {
        hz: 700.0, mode: olivia::OL_8_250, start_s: 1.1, snr_db: -9.0, full_copy: true,
        text: "DE K2IN 8/250 -9 DB INSIDE THE WIDE ONE ",
    },
    // And again, deeper: the narrowest mode inside a 500 Hz one.
    OliviaStation {
        hz: 1750.0, mode: olivia::OL_16_500, start_s: 1.8, snr_db: -12.0, full_copy: true,
        text: "DE W4HOST 16/500 -12 DB ",
    },
    OliviaStation {
        hz: 1700.0, mode: olivia::OL_4_125, start_s: 2.5, snr_db: -12.0, full_copy: true,
        text: "DE N3NARO 4/125 -12 DB INSIDE TOO ",
    },
    // Half a dB below the two above, in the clear rather than buried, and three
    // times their character rate — which is all it takes. The FEC does not
    // degrade gently: a mode copies or it does not, and the window where it
    // merely stumbles is about a decibel wide.
    OliviaStation {
        hz: 2350.0, mode: olivia::OL_8_500, start_s: 3.2, snr_db: -12.5, full_copy: false,
        text: "DE W6GONE 8/500 -12.5 DB SAME LEVEL BUT TOO FAST ",
    },
];

/// The crowded band: five Olivia modes stacked on top of each other, every one
/// of them read in full.
///
/// The weak band asks how far *under* the noise a signal can be. This one asks
/// how many can be in the same place at once, which is the other half of what
/// the mode is for and the more ordinary situation on a real band. Every
/// station here is comfortably above the noise and plainly visible on the
/// waterfall — the difficulty is entirely that they are on top of each other:
///
/// ```text
///  400 |=========== 32/1000 @900 ===========|
///  450   |==== 16/500 @700 ====|
/// 1275              |= 8/250 @1400 =|
/// 1350             |===== 8/500 @1600 =====|
/// 1400              |========== 16/1000 @1900 ==========|
/// ```
///
/// Two wide stations meeting at 1400 Hz, one narrow station buried completely
/// inside the first, and two more piled across the seam where they meet: an
/// 8/250 inside *both* wide ones, and an 8/500 lying across that 8/250 and
/// running most of the way into the second. Five stations, four overlapping
/// pairs, and every character of all five recovered.
///
/// **Why nothing sits exactly on top of anything.** Two stations may share a
/// patch of spectrum but not a centre frequency, and that is a constraint of
/// the channel tracker rather than of the decoder. `ChannelSet::add` matches a
/// decode to a thread by protocol and frequency — not by mode — within the
/// mode's own association tolerance, which for a kilohertz-wide Olivia signal
/// is 250 Hz. Two Olivia stations closer than that become one thread with both
/// texts woven into it, character by character. The decoder reads them both
/// perfectly; there is simply nowhere to put the second one. An earlier
/// arrangement here had the 8/500 dead centre of the 16/1000 and showed exactly
/// that: four threads, one of them gibberish.
///
/// **What makes that possible** is that Olivia spreads a character over 64 chips
/// of a Walsh function. Two signals sharing a patch of spectrum are not adding
/// their errors together so much as failing to correlate with each other: the
/// interferer's energy lands across the whole 64-chip window rather than in the
/// one place the correlator is looking. It is the same coding gain the weak band
/// spends on going deep, spent here on ignoring the neighbours.
///
/// All at one level, which is the condition to keep. A station 10 dB above its
/// neighbour buries it, and a band showing that would be a demonstration of
/// physics rather than of decoding — the weak band's note on the same point
/// applies here twice over, because there is nothing else making it hard.
///
/// Olivia 4/125 is deliberately absent, and it is the one mode that could not
/// join. Its four tones sit on the same 31.25 Hz grid as a 16/500's sixteen, so
/// a 16/500 demodulator centred nearby reads the same station through four of
/// its own tones and prints a second, garbled copy. `olivia::decode_all` does
/// not arbitrate between modes on purpose — the correlation threshold is what
/// stops one signal being reported twice, and it covers a narrow mode locking
/// onto a slice of a wide one. This is the reverse, and it is not caught. The
/// 4/125 stays in the weak band, where at −12 dB it is not strong enough to
/// fake a wider mode.
pub const CROWDED_STATIONS: &[OliviaStation] = &[
    // The two wide ones, meeting at 1400 Hz.
    OliviaStation {
        hz: 900.0, mode: olivia::OL_32_1000, start_s: 0.3, snr_db: 6.0, full_copy: true,
        text: "DE W1BIG 32/1000 A KILOHERTZ WIDE AND FULL OF OTHER PEOPLE ",
    },
    OliviaStation {
        hz: 1900.0, mode: olivia::OL_16_1000, start_s: 0.9, snr_db: 6.0, full_copy: true,
        text: "DE K2WIDE 16/1000 THE OTHER KILOHERTZ AND JUST AS BUSY ",
    },
    // One buried inside each of them.
    OliviaStation {
        hz: 700.0, mode: olivia::OL_16_500, start_s: 1.5, snr_db: 6.0, full_copy: true,
        text: "DE W3MID 16/500 ENTIRELY INSIDE W1BIG AND READ ANYWAY ",
    },
    OliviaStation {
        hz: 1600.0, mode: olivia::OL_8_500, start_s: 2.1, snr_db: 6.0, full_copy: true,
        text: "DE N4FAST 8/500 MOSTLY INSIDE K2WIDE AND ACROSS W5EDGE TOO ",
    },
    // And one across the seam, inside both.
    OliviaStation {
        hz: 1400.0, mode: olivia::OL_8_250, start_s: 2.7, snr_db: 6.0, full_copy: true,
        text: "DE W5EDGE 8/250 ON THE SEAM BETWEEN THE TWO WIDE ONES ",
    },
];

/// The PSK31 station in the weak band, kept apart from the Olivia ones because
/// it is a different demonstration and a different shape of station.
///
/// **What it is here to show**: coding gain, by its absence. Every Olivia
/// station above spreads each character over 64 chips and reads perfectly at
/// nine or twelve decibels *below* the noise. PSK31 has no error correction at
/// all — an error is a wrong letter, with nothing to catch it — and it needs
/// six decibels below instead of twelve. That gap is the whole argument for
/// FEC, standing in one band where it can be seen rather than argued.
///
/// It is also a different shape: continuous rather than framed, so it repeats
/// its call the way a station calling CQ does, and it has to be on the air for
/// most of an 8.2-second analysis block before the detector will believe in it
/// at all. And lower case on purpose — varicode gives its shortest codes to
/// lower-case letters, and PSK31 operators type accordingly.
pub struct WeakPsk {
    pub hz: f64,
    pub mode: psk::Mode,
    pub start_s: f64,
    pub snr_db: f32,
    pub full_copy: bool,
    /// One call, repeated [`WeakPsk::repeats`] times.
    pub call: &'static str,
    pub repeats: usize,
}

/// Measured against this band, not chosen, and it is the *exact copy* that was
/// measured rather than a detection: −4 dB is where all 222 characters come
/// through unaltered. −5 has every callsign but is a character short, −6 starts
/// swallowing the spaces, −8 loses two calls in twelve, and −12 is mush.
///
/// Section B of `examples/psk_gate.rs` puts the detector's own floor near −8 dB,
/// so the four decibels between that and this are precisely what having no FEC
/// costs: the gate goes on finding a station whose text has already begun to
/// come apart, and there is nothing underneath to put it back together. Every
/// Olivia station here reads perfectly five to eight decibels *further* down.
///
/// At 1450 Hz it is in the clear, in the hundred-hertz gap between the wide
/// 32/1000 at 900 and the 16/500 at 1750. PSK31 is some 62 Hz across and would
/// fit inside either of them, but it is the one mode in this band with no error
/// correction to absorb a collision — the overlap demonstration is the Olivia
/// stations', which can afford it.
pub const WEAK_PSK_STATIONS: &[WeakPsk] = &[WeakPsk {
    hz: 1450.0,
    mode: PSK31,
    start_s: 0.5,
    snr_db: -4.0,
    full_copy: true,
    call: "cq cq de n0fec n0fec -4 db no fec k  ",
    repeats: 6,
}];

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

/// Scale a signal so it sits `snr_db` relative to the demo's noise, measured in
/// [`SNR_REF_BW`].
///
/// The noise is white across the whole Nyquist span, so only the fraction of it
/// inside the reference bandwidth counts against the signal.
fn scale_for_snr(sig: &[f32], snr_db: f32) -> f32 {
    let noise_in_ref = NOISE_RMS * NOISE_RMS * SNR_REF_BW / (SAMPLE_RATE as f32 / 2.0);
    let want = noise_in_ref * 10f32.powf(snr_db / 10.0);
    let have = rms(sig).powi(2);
    if have > 0.0 {
        (want / have).sqrt()
    } else {
        0.0
    }
}

/// Deterministic white noise of a given RMS.
fn noise(n: usize, rms_target: f32, seed: u64) -> Vec<f32> {
    let mut s = seed;
    let mut v: Vec<f32> = (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            (s >> 40) as f32 / (1u32 << 24) as f32 - 0.5
        })
        .collect();
    let k = rms_target / rms(&v).max(f32::EPSILON);
    for x in v.iter_mut() {
        *x *= k;
    }
    v
}

/// Synthesize the weak-signal band: Olivia stations across four modes, each a
/// known number of dB below the noise floor, several of them overlapping in
/// frequency.
///
/// This is the demonstration of what the mode is actually for. Spreading every
/// character over a 64-chip Walsh function buys enough coding gain that a signal
/// putting less power into the band than the noise does still comes through —
/// so most of these stations leave no visible trace on the waterfall at all,
/// and their text arrives anyway, even where they are sitting on top of each
/// other.
pub fn synth_weak() -> Vec<f32> {
    let rate = SAMPLE_RATE as usize;
    let total = 60 * rate;
    let mut buf = noise(total, NOISE_RMS, 0x51A9_2C3D_7E10_4B6F);

    for st in WEAK_STATIONS {
        let sig = olivia::encode(st.text, st.hz, st.mode);
        let amp = scale_for_snr(&sig, st.snr_db);
        let start = (st.start_s * rate as f64) as usize;
        for (i, s) in sig.iter().enumerate() {
            if start + i < total {
                buf[start + i] += *s * amp;
            }
        }
    }

    // And the one station with no error correction, six decibels shallower
    // than the Olivia stations for exactly that reason. See [`WeakPsk`].
    for st in WEAK_PSK_STATIONS {
        let sig = psk::encode(&psk_text(st.call, st.repeats), st.hz, st.mode);
        let amp = scale_for_snr(&sig, st.snr_db);
        let start = (st.start_s * rate as f64) as usize;
        for (i, s) in sig.iter().enumerate() {
            if start + i < total {
                buf[start + i] += *s * amp;
            }
        }
    }
    buf
}

/// Synthesize the crowded band: five Olivia modes stacked on each other, all
/// of them above the noise and all of them read in full.
///
/// The counterpart to [`synth_weak`], and built from the same parts to make the
/// contrast: there the stations are invisible and readable, here they are
/// plainly visible and on top of one another. See [`CROWDED_STATIONS`].
pub fn synth_crowded() -> Vec<f32> {
    let rate = SAMPLE_RATE as usize;
    let total = 60 * rate;
    // A different seed from the weak band's, so the two are not the same noise
    // with different signals in it — a coincidence in one would otherwise be a
    // coincidence in both, and the two bands are each other's control.
    let mut buf = noise(total, NOISE_RMS, 0x0C_120D_5EED_9A31);

    for st in CROWDED_STATIONS {
        let sig = olivia::encode(st.text, st.hz, st.mode);
        let amp = scale_for_snr(&sig, st.snr_db);
        let start = (st.start_s * rate as f64) as usize;
        for (i, s) in sig.iter().enumerate() {
            if start + i < total {
                buf[start + i] += *s * amp;
            }
        }
    }
    buf
}

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

    for (hz, start_s, call, repeats) in PSK_STATIONS {
        // Narrower than anything else here, so it needs less amplitude to be
        // just as readable.
        let audio = psk::encode(&psk_text(call, *repeats), *hz, PSK31);
        mix(&mut buf, (start_s * rate as f64) as usize, &audio, 0.3);
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
        for (hz, _, _, _) in PSK_STATIONS {
            let bw = PSK31.bandwidth_hz();
            bands.push((hz - bw / 2.0, hz + bw / 2.0, format!("PSK31 @ {hz}")));
        }
        bands.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        // Every pair, not just neighbouring ones. A narrow station sitting
        // inside a wide one is not adjacent to it in this list — the PSK31
        // station is separated from its Olivia host by the JS8 station — and
        // comparing neighbours alone would quietly stop noticing exactly the
        // kind of overlap this band is built to contain.
        let collisions: Vec<String> = bands
            .iter()
            .enumerate()
            .flat_map(|(i, a)| bands[i + 1..].iter().map(move |b| (a, b)))
            .filter(|(a, b)| a.1 > b.0 && b.1 > a.0)
            .map(|(a, b)| format!("{} / {}", a.2, b.2))
            .collect();
        assert_eq!(
            collisions,
            vec!["Olivia 16/500 @ 2000 / JS8 Normal @ 2000"],
            "unintended overlap in the demo band"
        );
    }
}
