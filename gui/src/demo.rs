//! Synthetic bands for the app's demo modes and the headless scene validator.
//!
//! Four of them, each built to show one thing, and [`Band`] is the list:
//!
//! - [`synth`] — the mixed-protocol band behind `--demo`, described below.
//! - [`synth_weak`] — `--weak`: how far under the noise a signal can be and
//!   still be read.
//! - [`synth_crowded`] — `--crowded`: how many can be in the same place at
//!   once, which is the same coding gain spent the other way.
//! - [`synth_signals`] — `--signals`: three QSOs side by side, one on each
//!   protocol, each over saying what its mode costs in speed and in watts.
//!
//! The rest of this note is about the first.
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

use crate::channels;
use ragchew::js8::message::{self, IType};
use ragchew::js8::modem;
use ragchew::js8::submode::{self, Submode};
use ragchew::olivia::{self, Mode as Olivia};
use ragchew::protocol::ModeId;
use ragchew::psk::{self, PSK31};
use ragchew::{js8, SAMPLE_RATE};

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

/// (center Hz, mode, start time in seconds, text). Olivia stations transmit
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
/// four of them now and "which band" is one question with one answer. The menu
/// is built by walking [`Band::ALL`], so a fifth is a variant and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Band {
    /// Three protocols at once, twelve stations. What the app is *for*.
    Demo,
    /// Olivia below the noise floor, plus the one station with no FEC.
    Weak,
    /// Olivia on top of itself, five modes, all read.
    Crowded,
    /// Three QSOs, one path: what each mode costs in watts to hold.
    Signals,
}

impl Band {
    /// In the order the menu offers them: the mixed band first, being the one
    /// that shows what the app does, then the three that each make one point.
    pub const ALL: [Band; 4] = [Band::Demo, Band::Weak, Band::Crowded, Band::Signals];

    pub fn label(self) -> &'static str {
        match self {
            Band::Demo => "Demo band",
            Band::Weak => "Weak-signal band",
            Band::Crowded => "Crowded band",
            Band::Signals => "Know-your-signals band",
        }
    }

    /// What this band is called on a command line, without its dashes.
    ///
    /// The application's own flags are a clap struct and cannot read this — a
    /// derive needs one field per flag — but everything else that has to name a
    /// band does, `gui/examples/scene_png.rs` above all. A headless render is
    /// how a band gets looked at without a display, and one that could only
    /// render the first of four was the reason this exists.
    pub fn flag(self) -> &'static str {
        match self {
            Band::Demo => "demo",
            Band::Weak => "weak",
            Band::Crowded => "crowded",
            Band::Signals => "signals",
        }
    }

    /// The band a [`flag`](Self::flag) names.
    pub fn from_flag(s: &str) -> Option<Band> {
        let s = s.trim().trim_start_matches('-');
        Band::ALL.into_iter().find(|b| b.flag() == s)
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
            Band::Signals => "three QSOs side by side, one on each protocol, every over \
                              saying its own speed and how deep it hears. Each is a \
                              hundred-watt station worked by a quiet one, and the quiet \
                              one runs 20 W on PSK31, 5 W on Olivia and 2 W on JS8 — same \
                              path, same margin, the price of the mode",
        }
    }

    /// What the status bar calls it once it is loaded.
    pub fn source_name(self) -> &'static str {
        match self {
            Band::Demo => "demo band",
            Band::Weak => "weak-signal demo",
            Band::Crowded => "crowded demo",
            Band::Signals => "know-your-signals demo",
        }
    }

    pub fn synth(self) -> Vec<f32> {
        match self {
            Band::Demo => synth(),
            Band::Weak => synth_weak(),
            Band::Crowded => synth_crowded(),
            Band::Signals => synth_signals(),
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
/// really does bury its neighbor and that makes for a demonstration of physics
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
/// patch of spectrum but not a center frequency, and that is a constraint of
/// the channel tracker rather than of the decoder. `ChannelSet::add` matches a
/// decode to a thread by protocol and frequency — not by mode — within the
/// mode's own association tolerance, which for a kilohertz-wide Olivia signal
/// is 250 Hz. Two Olivia stations closer than that become one thread with both
/// texts woven into it, character by character. The decoder reads them both
/// perfectly; there is simply nowhere to put the second one. An earlier
/// arrangement here had the 8/500 dead center of the 16/1000 and showed exactly
/// that: four threads, one of them gibberish.
///
/// **What makes that possible** is that Olivia spreads a character over 64 chips
/// of a Walsh function. Two signals sharing a patch of spectrum are not adding
/// their errors together so much as failing to correlate with each other: the
/// interferer's energy lands across the whole 64-chip window rather than in the
/// one place the correlator is looking. It is the same coding gain the weak band
/// spends on going deep, spent here on ignoring the neighbors.
///
/// All at one level, which is the condition to keep. A station 10 dB above its
/// neighbor buries it, and a band showing that would be a demonstration of
/// physics rather than of decoding — the weak band's note on the same point
/// applies here twice over, because there is nothing else making it hard.
///
/// Olivia 4/125 is deliberately absent, and it is the one mode that could not
/// join. Its four tones sit on the same 31.25 Hz grid as a 16/500's sixteen, so
/// a 16/500 demodulator centered nearby reads the same station through four of
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

// ---- the "know-your-signals" band ----

/// Where a 100 W station in the "know-your-signals" band arrives, in dB against
/// the noise in [`SNR_REF_BW`].
///
/// One number for the whole band, and that is the entire point of it. Three
/// conversations, one ionosphere, one receiver: a hundred watts is a hundred
/// watts whichever mode it is keyed in, so every loud station here lands at the
/// same level, and the only thing that differs between the three threads is how
/// little power their quiet partners can get away with.
const SIGNALS_REF_SNR_DB: f32 = 6.0;

/// The power [`SIGNALS_REF_SNR_DB`] is quoted for.
const SIGNALS_REF_WATTS: f32 = 100.0;

/// How far above its mode's floor each quiet station is put.
///
/// The same margin in every thread, so the three are comparable: whatever the
/// QRP stations differ by is the modes differing, not the band plan. Four
/// decibels because the floors below are whole decibels and a mode falls apart
/// within about one of its own — see [`WEAK_STATIONS`] on how sharp that edge
/// is — so three would be inside the rounding and five would waste the point.
pub const SIGNALS_MARGIN_DB: f32 = 4.0;

/// The turnaround between one over and the next, in a mode.
///
/// Long enough that the application notices it, which is the whole of the
/// requirement. [`channels::over_gap_s`] is the silence after which what a
/// station sends next is a new over — 8.2 seconds for Olivia, five for PSK31 —
/// and the first draft of this band left a second and a half between overs,
/// which is under both. The result was two threads that read as one unbroken
/// paragraph with both operators' overs run together, and a QSO log with one
/// entry where there should have been six.
///
/// That was a fault in the band and not in the tracker. A second and a half is
/// not a turnaround anybody has ever taken: the other station has to hear you
/// stop, finish decoding your tail, and key up. Olivia is the mode that makes
/// this obvious — a block is two seconds long, so a pause of two or three of
/// them is a typist thinking rather than an over ending, and the rule is right
/// to wait for four.
///
/// Derived from the rule rather than written down beside it, so the two cannot
/// drift apart. JS8 does not use it: its overs sit on the cycle grid and end
/// with the flag the station sets on its last frame, which the tracker reads
/// directly.
fn signals_gap_s(mode: ModeId) -> f64 {
    channels::over_gap_s(mode) + 1.5
}

/// Length of the band in seconds.
///
/// Nearly three times the other three, because the thing on show takes that
/// long to happen: a QSO is several overs each way, the slowest mode here
/// spends fifteen seconds of a JS8 cycle saying thirteen characters, which is
/// ten words a minute, and every turnaround has to be a real one — see
/// [`signals_gap_s`], which is where a third of this goes. Cutting it to a
/// minute would show three stations calling CQ, which is not an exchange.
pub const SIGNALS_SECS: usize = 165;

/// One of the two operators working each other in a thread.
pub struct Op {
    pub call: &'static str,
    /// Transmitter power in watts. What the band is about: it is the only thing
    /// the two operators in a thread do differently.
    pub watts: f32,
}

impl Op {
    /// Where this station's signal arrives, in dB against the noise in
    /// [`SNR_REF_BW`].
    ///
    /// Straight from the power, because that is how it works. Both stations are
    /// heard over the same path and the path does not care which direction it
    /// is worked in, so a station running a tenth of another's power arrives
    /// exactly 10 dB down — no other difference between the two is modeled
    /// here, and none is needed.
    pub fn snr_db(&self) -> f32 {
        SIGNALS_REF_SNR_DB + 10.0 * (self.watts / SIGNALS_REF_WATTS).log10()
    }
}

/// The modes this band puts on the air, with the two numbers its texts quote:
/// speed in words per minute, and the level at which it copies.
///
/// Both are measured by `examples/sensitivity.rs` and neither is taken from a
/// published table, because neither would mean the same thing if it were.
///
/// **Speed** is five characters to a word including its space, counted over
/// ordinary English prose in the case that mode's operators type it in — upper
/// for JS8 and Olivia, lower for PSK31, whose varicode charges a third of its
/// throughput for capitals. Only Olivia has a fixed rate; the other two carry
/// whatever their varicode makes of the text, so a single figure for them is a
/// claim about the prose as much as about the mode. The same PSK31 station
/// types at 47 wpm sending English and 37 sending callsigns.
///
/// **The floor** is the lowest whole decibel at which a whole over comes back
/// character for character, every time, through the same `protocol::decode_all`
/// the application runs. That is a stricter test than the published figures for
/// these modes, which state the level at which about *half* of single
/// transmissions decode — JS8 Normal is quoted at −24 dB on that convention and
/// reaches −15 on this one. A band that has to be watched rather than
/// summarized needs the strict number: a station placed at the half-copy level
/// spends the demo in pieces.
pub const SIGNALS_MODES: &[(ModeId, f64, f32)] = &[
    (ModeId::Psk(PSK31), 47.0, -5.0),
    (ModeId::Js8(js8::Mode::Normal), 10.0, -15.0),
    (ModeId::Olivia(olivia::OL_8_250), 18.0, -13.0),
    (ModeId::Olivia(olivia::OL_16_500), 23.0, -12.0),
    (ModeId::Olivia(olivia::OL_32_1000), 29.0, -11.0),
];

/// The floor [`SIGNALS_MODES`] states for a mode.
pub fn signals_floor_db(mode: ModeId) -> f32 {
    SIGNALS_MODES
        .iter()
        .find(|(m, _, _)| *m == mode)
        .map(|(_, _, floor)| *floor)
        .unwrap_or_else(|| panic!("{} is not one of this band's modes", mode.name()))
}

/// The PSK31 thread: 1000 Hz, and the most expensive contact in the band.
///
/// Twenty watts to hold a QSO that Olivia holds on five and JS8 on two, and the
/// reason is one line long: PSK31 has no error correction. Every other mode
/// here spends bandwidth or time on redundancy and gets it back as reach; this
/// one spends nothing and gets nothing, and an error is simply a wrong letter.
///
/// The overs are long, and open with the callsigns three times over, which is
/// how PSK31 is really operated and is also insurance against the one thing
/// this decoder cannot help. There is no frame to synchronize on, so the gate
/// will not believe in a station until it has been on the air for most of an
/// 8.2-second analysis block, and an over carrying its only copy of the
/// callsign in that opening could lose it — which is what the `--demo` band's
/// PSK31 station does, and what happens when you tune across a real one. It
/// does not happen at the levels here, and it should not have to not happen: an
/// over that repeats its call cannot be robbed of it.
pub const SIGNALS_PSK_HZ: f64 = 1000.0;
pub const SIGNALS_PSK_OPS: [Op; 2] =
    [Op { call: "W1PSK", watts: 100.0 }, Op { call: "K2QRP", watts: 20.0 }];

/// `(which operator, what they send)`, in order, back to back.
///
/// Lower case throughout, as PSK31 has been since the day it was written.
///
/// **Two characters of junk trail each over but the last**, and they are left
/// here rather than tuned away, because they are worth knowing about. They land
/// 0.3 to 0.5 s after the carrier drops — `"pse k  fo"`, `"btu  ne"` — from the
/// analysis block that straddles the drop: it is nearly all signal, so it
/// clears the gate, and the tail of it is demodulated as though the station
/// were still there.
///
/// The weak band's PSK station does not do this and its test says so, which is
/// why the check there did not see this: that station is at −4 dB and has one
/// over, while these are at −1 and +6 dB and turn round twice. A stronger
/// carrier carries the straddling block over the gate more easily, so the leak
/// is worst exactly where a demo is most likely to be watched. Two characters
/// per turnaround is a small fault and a real one; `--signals` is now the band
/// that shows it.
pub const SIGNALS_PSK_OVERS: &[(usize, &str)] = &[
    (0, "cq cq cq de w1psk w1psk w1psk psk31 47 wpm and copies at -5 db pse k  "),
    (1, "w1psk de k2qrp k2qrp k2qrp ur 599 om running 20 w here es no fec at all btu  "),
    (0, "k2qrp de w1psk r r fb 100 w my end so 20 w is the price of psk31 tu 73 sk  "),
];

/// The JS8 thread: 700 Hz, Normal, and the cheapest contact in the band.
///
/// Two watts, because the LDPC and the fifteen-second cycle between them buy
/// ten decibels over PSK31 — paid for in speed, at a fifth of PSK31's words a
/// minute. That trade, stated twice in one band by two stations who can hear
/// each other, is the whole of what there is to know.
///
/// Overs run to whatever number of frames the text needs, laid end to end on
/// the cycle grid with the last frame of each marked as ending the over — which
/// is why the thread reads with a `<>` where each operator stopped talking, the
/// same mark JS8Call shows.
pub const SIGNALS_JS8_HZ: f64 = 700.0;
/// W1JSK and not W1JS8, which is what this station was called until the call
/// tracker was asked what it made of it: a call sign's suffix is letters, so
/// `qso::callsign_in` reads W1JS8 as no call at all, and the thread ended up
/// named after the *quiet* operator while the other two threads were named
/// after the loud one. The mnemonic survives the K, and the packing does too —
/// the over below still breaks mid-call, which is the case
/// `a_call_broken_over_two_js8_frames_reaches_the_panel_whole` exists for.
pub const SIGNALS_JS8_OPS: [Op; 2] =
    [Op { call: "W1JSK", watts: 100.0 }, Op { call: "K2LOW", watts: 2.0 }];

/// `(which operator, what they send)`, in order, on consecutive cycles.
pub const SIGNALS_JS8_OVERS: &[(usize, &str)] = &[
    (0, "CQ DE W1JSK JS8 NORMAL"),
    (1, "W1JSK DE K2LOW 2W ONLY"),
    (0, "10 WPM SOLID -15 DB"),
    (1, "R TU 73 SK"),
];

/// The Olivia thread: 1800 Hz, and a QSO that speeds up as it goes.
///
/// Three modes in one exchange — 8/250, then 16/500, then 32/1000 — because
/// Olivia is the one protocol here whose operators change gear mid-QSO, and
/// because the three make the trade visible inside a single conversation: 18,
/// 23 and 29 words a minute against floors of −13, −12 and −11 dB. Two decibels
/// buys sixty per cent more speed, which is a better bargain than it sounds and
/// the reason 32/1000 is the classic ragchew mode.
///
/// **All three on one center frequency**, which is both what operators do — you
/// QSY the mode, not the dial — and what puts the whole exchange on one thread.
/// `ChannelSet::add` groups decodes by protocol and frequency and *not* by mode,
/// so the three modes arrive on the row they started on and the row's mode
/// label follows the station. The crowded band leans on the same rule from the
/// other side; see [`CROWDED_STATIONS`] for what it costs there.
///
/// The quiet operator runs 5 W, which is four decibels above the *fastest*
/// mode's floor and therefore above all three. That is deliberate: the point of
/// the thread is that they can speed up, and a station that fell out of the
/// conversation at the last QSY would be making the opposite point in a band
/// that already has two others for it.
pub const SIGNALS_OLIVIA_HZ: f64 = 1800.0;
pub const SIGNALS_OLIVIA_OPS: [Op; 2] =
    [Op { call: "W1SLO", watts: 100.0 }, Op { call: "K2OLV", watts: 5.0 }];

/// `(which operator, in what mode, what they send)`, in order, back to back.
pub const SIGNALS_OLIVIA_OVERS: &[(usize, Olivia, &str)] = &[
    (0, olivia::OL_8_250, "CQ DE W1SLO 8/250 18 WPM -13 DB K "),
    (1, olivia::OL_8_250, "W1SLO DE K2OLV 5W ONLY FB K "),
    (0, olivia::OL_16_500, "QSY 16/500 23 WPM -12 DB K "),
    (1, olivia::OL_16_500, "R FASTER ES STILL 5W K "),
    (0, olivia::OL_32_1000, "NOW 32/1000 29 WPM -11 DB TU 73 "),
    (1, olivia::OL_32_1000, "R 73 SK "),
];

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

/// Synthesize the "know-your-signals" band: three QSOs side by side, one on
/// each protocol, every over stating its mode's speed and reach.
///
/// The other three bands are laid out by hand, station by station, because what
/// they demonstrate *is* the layout. This one is laid out by the clock: overs
/// are stacked end to end in the order they are spoken, so the tables above say
/// who talks and what they say and nothing about when. Nothing here can be
/// mistimed into an accidental collision, which matters more than usual —
/// interference is what the other bands are for, and any of it here would be a
/// second lesson taught over the top of the first.
pub fn synth_signals() -> Vec<f32> {
    let rate = SAMPLE_RATE as usize;
    let total = SIGNALS_SECS * rate;
    // Its own seed, as each band has: the four are each other's control, and a
    // fluke shared between them would be invisible in all four.
    let mut buf = noise(total, NOISE_RMS, 0x5169_4A15_0DEF_2B77);

    let mut mix = |start: usize, sig: &[f32], snr_db: f32| {
        let amp = scale_for_snr(sig, snr_db);
        for (i, s) in sig.iter().enumerate() {
            if start + i < total {
                buf[start + i] += *s * amp;
            }
        }
    };

    // The two continuous protocols: an over is one encode, and the next one
    // starts a turnaround after this one stops.
    let gap = signals_gap_s(ModeId::Psk(PSK31));
    let mut psk_ends = gap;
    for (op, text) in SIGNALS_PSK_OVERS {
        let sig = psk::encode(text, SIGNALS_PSK_HZ, PSK31);
        mix((psk_ends * rate as f64) as usize, &sig, SIGNALS_PSK_OPS[*op].snr_db());
        psk_ends += sig.len() as f64 / rate as f64 + gap;
    }

    // The Olivia turnaround is the same in all three of its modes — they share
    // a block length, so they share an over gap — but it is asked for per over
    // rather than once, because a mode that did not would be a silent bug the
    // day this QSO gains a gear.
    let mut olivia_ends = signals_gap_s(ModeId::Olivia(SIGNALS_OLIVIA_OVERS[0].1));
    for (op, mode, text) in SIGNALS_OLIVIA_OVERS {
        let sig = olivia::encode(text, SIGNALS_OLIVIA_HZ, *mode);
        mix((olivia_ends * rate as f64) as usize, &sig, SIGNALS_OLIVIA_OPS[*op].snr_db());
        olivia_ends +=
            sig.len() as f64 / rate as f64 + signals_gap_s(ModeId::Olivia(*mode));
    }

    // JS8 keeps time by the UTC grid instead: one frame per cycle, keyed half a
    // second in, and an over is however many cycles its text needs. The last
    // frame of each over carries the flag that ends it, so the thread reads with
    // the `<>` JS8Call shows where a station stopped talking — and so that a
    // reply landing on the same frequency starts a paragraph of its own.
    let sm = submode::NORMAL;
    let mut cyc = 0usize;
    for (op, text) in SIGNALS_JS8_OVERS {
        let snr = SIGNALS_JS8_OPS[*op].snr_db();
        let mut rest: &str = text;
        while !rest.is_empty() {
            // What is left after this frame decides whether this frame is the
            // last one, so the text has to be packed before the flag is known.
            let (_, used) = message::freetext(rest, IType::None);
            let last = used >= rest.chars().count();
            let (a87, _) = message::freetext(rest, if last { IType::Last } else { IType::None });
            let sig = modem::encode_audio_sm(&a87, SIGNALS_JS8_HZ, &sm);
            mix(cyc * sm.period_s as usize * rate + rate / 2, &sig, snr);
            rest = &rest[rest.char_indices().nth(used).map_or(rest.len(), |(i, _)| i)..];
            cyc += 1;
        }
    }

    // Nothing may be cut off by the end of the band. Overs are stacked by the
    // clock, so lengthening one — a longer text, a slower mode, one more
    // exchange — pushes everything after it along, and an over that ran past
    // the end would be truncated in silence and read as a station that stopped
    // mid-word. Here is the only place the schedule exists, so here is where it
    // is checked.
    for (what, ends) in [
        ("PSK31", psk_ends),
        ("Olivia", olivia_ends),
        ("JS8", (cyc * sm.period_s as usize) as f64),
    ] {
        assert!(
            ends <= SIGNALS_SECS as f64,
            "the {what} thread runs {:.0} s past the end of a {SIGNALS_SECS} s band",
            ends - SIGNALS_SECS as f64
        );
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

        // Every pair, not just neighboring ones. A narrow station sitting
        // inside a wide one is not adjacent to it in this list — the PSK31
        // station is separated from its Olivia host by the JS8 station — and
        // comparing neighbors alone would quietly stop noticing exactly the
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
