//! Interactive panorama app (eframe).
//!
//! Everything is drawn into one canvas (not separate egui panels) so leader
//! lines can span from the text on the left to the waterfall on the right. The
//! geometry, nudge layout, and leader routing are shared with the headless PNG
//! validator (`scene`, `layout`, `waterfall`).
//!
//! Loading is off the UI thread: the window opens immediately, the waterfall
//! appears and starts playing as soon as the (fast) spectrogram is ready, and
//! decoded text streams in behind it as the (much slower) scan reports mode by
//! mode — one thread each, so the wait is the slowest mode rather than the sum.
//!
//! Playback: decodes are *revealed* as a simulated clock advances (each appears
//! when its audio would have finished), so text streams into channels in the
//! same chunks a live receiver produces — a JS8 frame every 15 s, an Olivia
//! block every couple of seconds — and the waterfall scrolls. Stand-in for the
//! live decode loop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread;

use eframe::egui::{self, Color32, FontId, Pos2, Rect, Stroke};

use ragchew::protocol::{self, ModeId, Protocol};
use ragchew::spectrogram::WINDOW;
use ragchew::{wav, Spectrogram, SAMPLE_RATE};

use crate::demo::Band;
use crate::channels::{Channel, ChannelSet};
use crate::diag::{self, Health};
use crate::layout;
use crate::record::Recorder;
use crate::rig;
use crate::traffic;
use crate::{diag_info, diag_warn};
use crate::qso::{Dir, QsoSet};
use crate::scene::{self, Geometry};
use crate::sparkline;
use crate::tx::{self, Tx};
use crate::waterfall::{self, Viewport};

/// What this build calls itself, taken from the manifest rather than written
/// out here — a version in two places is a version that disagrees with itself.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const HIGHLIGHT_SECS: f64 = 2.5;

/// How far one row still has to travel before the newest character it holds
/// sits where the newest character belongs.
///
/// A row is right-aligned on its newest character, so every character that
/// arrives pushes everything before it one character to the left. Drawing the
/// row shifted right by what it still owes is what turns that push into a
/// glide: nothing moves at the instant new text lands, and the whole row then
/// slides left as the debt is paid off.
///
/// A debt, rather than an ease from the last arrival, because text does not
/// arrive on a schedule. PSK31 types five characters a second while the slide
/// runs for one, so an ease restarted on every character never reached halfway
/// before springing back to the right — five times a second, which is what a
/// streaming row shimmering on screen actually was.
#[derive(Clone, Copy, Default)]
struct Slide {
    /// Characters the channel had said when it was last drawn.
    chars: usize,
    /// Pixels the row is still shifted right by.
    px: f32,
}

/// One row as it was drawn this frame, for [`App::watch_rows`] to compare
/// against the last time it saw it.
struct RowNow {
    id: u64,
    hz: f64,
    mode: ModeId,
    /// Characters put on the row.
    drawn: usize,
    /// Characters the channel holds, which is more than fits.
    held: usize,
    y: f32,
    /// What the row is shifted right by, if it is still sliding.
    offset: f32,
    /// The instant text is being forgotten before, and the oldest text the
    /// channel still has a time for. Together they say whether the timeout is
    /// what took the row's text.
    cut: f64,
    oldest: Option<f64>,
}

/// What a row looked like when it was last on screen.
#[derive(Clone, Copy)]
struct RowSeen {
    hz: f64,
    mode: ModeId,
    drawn: usize,
    held: usize,
    /// Wall clock when it was last drawn.
    at: f64,
    /// Wall clock when it was last written about, so a row that flaps every
    /// frame writes one line a second rather than sixty.
    said_at: f64,
}

/// How long a row is remembered after it stops being drawn, so that one which
/// goes and comes back is known to be the same row rather than a new one.
const ROW_MEMORY_S: f64 = 120.0;

/// Longest a row may be missing before its return is worth remarking on. A
/// frame or two is the interface catching up; half a second is a gap the eye
/// sees.
const ROW_BLINK_S: f64 = 0.5;

/// A frequency the hand has asked for, on its way to the rig.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Tuning {
    hz: f64,
    /// When it was last touched, on the wall clock.
    at: f64,
    /// Whether the rig has been told.
    ///
    /// A step of the wheel or an arrow is a whole intent and goes out as it is
    /// made. A typed number is not: entered a digit at a time it passes through
    /// seven other frequencies on the way — 04.074.000, 07.074.000 — and any of
    /// them is a place the radio would actually go, in another band, keyed to
    /// somebody else's QSO. So typing waits for Enter, and Escape or looking
    /// away throws it back.
    sent: bool,
}

/// How long after the last wheel click the rig's own reading takes over again.
///
/// Long enough to cover the gap between readings — the dial is asked for on
/// half the PTT's beat — and short enough that a hand let go of the wheel sees
/// the rig's answer rather than its own last guess.
const TUNING_SETTLES_S: f64 = 1.5;

/// One wheel click, in scroll units. egui reports a notch as about this much;
/// a touchpad reports a stream of smaller values, which accumulate into the
/// same thing.
const SCROLL_NOTCH: f32 = 50.0;

/// How many steps a turn of the wheel is worth.
///
/// The magnitude matters. Taking the sign alone — which is what this did at
/// first — moves one step per frame however hard the wheel is spun, so crossing
/// a band means a hundred and forty thousand clicks.
fn notches(scroll: f32) -> f64 {
    let turns = (scroll / SCROLL_NOTCH) as f64;
    // A slow touchpad still moves the dial: less than a notch is one step, in
    // whichever direction it went.
    if turns.abs() < 1.0 {
        turns.signum()
    } else {
        turns.round()
    }
}

/// `value` with the digit in the `place` column set to `digit`.
///
/// The column, not the number: setting the hundreds of a frequency leaves every
/// other digit where it was, which is what typing into a dial means and what
/// makes eight keystrokes an entry rather than eight tunes.
fn with_digit(value: f64, place: f64, digit: u32) -> f64 {
    let value = value.max(0.0);
    let had = (value / place).floor() % 10.0;
    (value - had * place + digit as f64 * place).max(0.0)
}

/// A little triangle, pointing up or down, filling `at`.
///
/// Painted rather than typed for the reason the clear mark is: the arrows a
/// keyboard offers are in none of the fonts egui bundles, and these have to
/// line up with a digit a few pixels wide.
fn arrow(painter: &egui::Painter, at: egui::Rect, up: bool, colour: Color32) {
    let (w, mid) = (at.width() * 0.34, at.center().x);
    let (near, far) = if up { (at.bottom(), at.top()) } else { (at.top(), at.bottom()) };
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(mid - w, near),
            egui::pos2(mid + w, near),
            egui::pos2(mid, far),
        ],
        colour,
        Stroke::NONE,
    ));
}

/// Where a rig's filter sits in the audio this app draws.
///
/// A receiver's filter is quoted as a width and placed about a centre, and the
/// centre is the part no rig volunteers over CAT in any way this could rely
/// on. Fifteen hundred hertz is the convention for data operating — it is what
/// a K3 and a KX3 use for their data modes out of the box — so that is what
/// this assumes, and the readout says as much rather than pretending to know.
///
/// An assumption is tolerable here in a way it was not for the transmit flag:
/// wrong by a couple of hundred hertz the shading is slightly misplaced and
/// visibly so, where a wrong transmit indicator is invisible and dangerous.
const FILTER_CENTRE_HZ: f64 = 1500.0;

/// The stretches of the band this app draws that fall outside the rig's
/// filter, as `(low, high)` in audio hertz.
///
/// Empty when the width is unknown or the mode is not one where an audio
/// passband means anything — nothing is dimmed on a guess.
fn outside_the_filter(width_hz: Option<f64>, sideband: bool) -> Vec<(f64, f64)> {
    let Some(width) = width_hz.filter(|w| *w > 0.0) else { return Vec::new() };
    if !sideband {
        return Vec::new();
    }
    let half = width / 2.0;
    vec![
        (f64::MIN, FILTER_CENTRE_HZ - half),
        (FILTER_CENTRE_HZ + half, f64::MAX),
    ]
}

/// Where a signal actually is on the band: the dial, plus or minus the audio
/// offset the app hears it at.
///
/// The sign is the sideband's, which is why this waited for the mode to be
/// readable. On upper sideband a tone at 1500 Hz of audio is 1500 Hz *above*
/// the dial; on lower it is that far below. In anything else — FM, AM, CW —
/// an audio offset is not a displacement from the dial in a way worth printing
/// as a frequency, so nothing is printed.
///
/// `None` when the dial is unknown or the mode is not one of the four, rather
/// than a number that would be wrong in a way nobody could see.
fn on_the_band(dial: Option<f64>, mode: Option<&str>, audio_hz: f64) -> Option<f64> {
    let sign = match mode? {
        "USB" | "PKTUSB" | "DATA" => 1.0,
        "LSB" | "PKTLSB" | "DATA-REV" => -1.0,
        _ => return None,
    };
    Some(dial? + sign * audio_hz)
}

/// The same, in the fixed places a dial has.
///
/// Every decade keeps its column whatever the frequency, because on this dial a
/// digit is a control: at 7.070.000 with the megahertz written as one digit
/// there is no tens column to put a cursor on, so 14 MHz cannot be typed at
/// all. A radio solves it the same way — the positions are always there and the
/// leading zeros are blanked, which here means drawn dim.
///
/// Three megahertz digits rather than two, so a rig on two metres has somewhere
/// to put its hundreds.
fn dial_digits(hz: f64) -> String {
    let total = hz.round().max(0.0) as u64;
    let (mhz, rest) = (total / 1_000_000, total % 1_000_000);
    format!("{mhz:03}.{:03}.{:03}", rest / 1000, rest % 1000)
}

/// How many leading zeros of a dial reading are blanks rather than digits.
fn leading_blanks(reading: &str) -> usize {
    reading.chars().take_while(|c| *c == '0').count().min(2)
}

/// A frequency the way a rig shows it: megahertz, kilohertz, hertz, in groups
/// a reader can say out loud — 14.074.000 rather than 14074000.
///
/// All the way down to single hertz. A digital station cares about them: the
/// modes here are tens of hertz wide, the app's own readouts are quoted to a
/// tenth, and a dial that stopped at tens would be the one number on the screen
/// that could not say where a signal was.
fn megahertz(hz: f64) -> String {
    let total = hz.round().max(0.0) as u64;
    let (mhz, rest) = (total / 1_000_000, total % 1_000_000);
    format!("{mhz}.{:03}.{:03}", rest / 1000, rest % 1000)
}

/// A frequency typed but not yet entered: the same dial, not yet the truth.
const VFO_PENDING: Color32 = Color32::from_rgb(180, 220, 255);

/// A radio's own readout: amber on black, as every rig on the desk has been
/// since they stopped being analogue. It is not the palette's to theme — the
/// point of it is that it looks like the dial it is showing.
const VFO_AMBER: Color32 = Color32::from_rgb(255, 176, 0);
const VFO_BLACK: Color32 = Color32::from_rgb(12, 10, 8);

/// How much larger than body text the frequency is drawn. Big enough to read
/// from where an operator sits, which is not where the mouse is.
const VFO_SCALE: f32 = 1.9;

/// Transmitting because this app asked for it, and transmitting for a reason it
/// does not know. Two colours because they call for two different reactions.
const TX_RED: Color32 = Color32::from_rgb(255, 60, 60);
const TX_ELSEWHERE: Color32 = Color32::from_rgb(255, 170, 40);

/// Where a USB serial cable is likeliest to turn up on this platform, which is
/// where an Elecraft KXUSB lands. Only what shows before anything is detected
/// or saved — see [`rig::serial::DEFAULT_DEVICE`].
#[cfg(feature = "serial")]
const DEFAULT_RIG_DEVICE: &str = rig::serial::DEFAULT_DEVICE;

/// A build with no directly-driven rig has no port to name. Empty rather than a
/// plausible guess, because the setting is still carried through the file for
/// the build that *can* open one, and a name invented here would overwrite
/// theirs the first time this build saved.
#[cfg(not(feature = "serial"))]
const DEFAULT_RIG_DEVICE: &str = "";

/// The rate the port setting starts at.
///
/// A KX3 leaves the factory at 38400 and every Elecraft takes it, so that is
/// the guess. Kept here rather than taken from the rig module because the
/// setting outlives the feature: a build with no direct rig still carries the
/// number through the settings file untouched.
const DEFAULT_RIG_BAUD: u32 = 38_400;

/// The transmit-power box, and the bandwidth box.
///
/// Both exist as functions for one reason: `clamp_existing_to_range(false)`.
/// egui's `DragValue` clamps the value it is *given* by default, writes the
/// clamped number back through the borrow, and reports it as a change — so a
/// reading from the rig that falls outside the range this app is using would be
/// altered on the way to the screen and then sent back to the radio as a
/// setting. A rig sitting on a 12 kHz AM filter would be narrowed to the end of
/// a range nobody asked for.
///
/// Turning it off leaves the drag and the keyboard still bounded — those are
/// clamped further down in egui, on the paths that represent the operator
/// actually asking for something. Only the rig's own reading passes through
/// untouched, which is the whole distinction: this app may refuse to *ask* for
/// a value, and may never misreport one.
fn power_box(watts: &mut f64, lo: f64, hi: f64) -> egui::DragValue<'_> {
    egui::DragValue::new(watts)
        .speed(0.5)
        .range(lo..=hi)
        .suffix(" W")
        .clamp_existing_to_range(false)
}

fn bandwidth_box(khz: &mut f64, lo: f64, hi: f64) -> egui::DragValue<'_> {
    egui::DragValue::new(khz)
        .speed(0.05)
        .range(lo..=hi)
        .fixed_decimals(2)
        .suffix(" kHz")
        .clamp_existing_to_range(false)
}

/// What to bound a setting by when the rig has not said what it will take.
///
/// Not a claim about any radio — a guard against a drag running away, which is
/// all a range can honestly be here. Wide enough that no rig's own reading is
/// anywhere near them: an amateur station does not make ten kilowatts, and a
/// receiver filter is not a megahertz wide. The earlier values were 100 Hz to
/// 4 kHz, which are real numbers for a KX3 on a data mode and quietly wrong for
/// anything on AM.
const ANY_POWER_W: (f64, f64) = (0.0, 10_000.0);
const ANY_BANDWIDTH_KHZ: (f64, f64) = (0.001, 1_000.0);

/// A slider with no number on it, beside a value box that already has one.
///
/// egui's own `Slider` carries its own value box, which is the widget that was
/// tried and rejected: in a panel this narrow the track it leaves over is a few
/// dozen pixels, and a few dozen pixels of track across a hundred watts is not
/// a control, it is a target. So the number stays in the `DragValue` and this
/// draws only the track, given every pixel left in the row.
///
/// Returns whether the operator moved it.
fn track(ui: &mut egui::Ui, v: &mut f64, lo: f64, hi: f64) -> bool {
    // Whatever the label and the box did not use. Floored, because a grid cell
    // can report almost nothing while it is being laid out for the first time
    // and a zero-width slider cannot be grabbed at all.
    ui.spacing_mut().slider_width = ui.available_width().max(MIN_TRACK_W);
    ui.add(
        egui::Slider::new(v, lo..=hi)
            .show_value(false)
            // `Edits`, not the default `Always`, for the reason given on
            // [`power_box`]: the operator may not drag past the ends, and the
            // rig's own reading is left exactly as the rig gave it. A track is
            // only ever drawn over a range the rig stated, but even then the
            // reading can sit outside it — a range read on one band against a
            // rig now on another, an amplifier in line — and the reading is
            // the fact.
            .clamping(egui::SliderClamping::Edits),
    )
    .changed()
}

/// Narrowest a track may be drawn.
///
/// The complaint that started this was a slider too small to use, so there is a
/// floor rather than only a stretch: a panel dragged narrow shrinks the text
/// before it shrinks this.
const MIN_TRACK_W: f32 = 90.0;

/// A setting's tooltip, with a word about the missing slider when one is.
///
/// Which settings get a track depends on the radio, not on the interface: a
/// `rigctld` rig states its power exactly and says nothing about its passband,
/// an Elecraft is the other way round, and a Yaesu says neither. On screen that
/// reads as one control having a slider and its neighbour not, which looks like
/// an oversight — it looked like one to the person who wrote it, with the whole
/// design in front of him. The rig is what did not say, and a tooltip is the
/// only place that can be said.
fn hint(about: &str, ranged: bool) -> String {
    match ranged {
        true => about.to_string(),
        false => format!(
            "{about}\n\nThis rig does not report the range it will accept, so there is no \
             track to drag. Type the figure, or drag the figure itself."
        ),
    }
}

/// A way of keying, as the settings offer it.
///
/// The tag is the word written to the settings file, so the menu and the file
/// cannot drift apart, and it — not a position — is what identifies a choice.
/// A build compiled without a given rig simply has no row here, and the
/// segmented control's indices then mean whatever this build has. Adding a
/// radio is a row and a feature rather than a renumbering of everything that
/// reads the control.
struct Keyer {
    tag: &'static str,
    label: &'static str,
    hover: &'static str,
}

/// Everything this build can key with, in the order the settings offer it.
///
/// `none` and `rigctld` are always here: one is no hardware at all and the
/// other is one daemon for every rig hamlib knows. What follows is one row per
/// directly-driven radio, each behind its own feature.
const KEYERS: &[Keyer] = &[
    Keyer {
        tag: "none",
        label: "None",
        hover: "nothing is keyed: the rig decides, on VOX or a straight-through interface",
    },
    Keyer {
        // Lower case on purpose, and the one segment that stays that way: this
        // is the name of a program the operator types, not a word. `Rigctld` is
        // not a thing that exists.
        tag: "rigctld",
        label: "rigctld",
        hover: "hamlib's daemon: run `rigctld -m <model> -r <device>` and give its address \
                below. Hamlib will not key a rig it has no PTT method for — add \
                `--set-conf=ptt_type=RIG` (or RTS, or DTR) if it refuses.",
    },
    #[cfg(feature = "elecraft")]
    Keyer {
        tag: "elecraft",
        label: "Elecraft",
        hover: "a KX3 or K3 on a serial cable, spoken to directly — no daemon to run. The \
                rate is whatever the radio's menu says; a KX3 leaves the factory at 38400.",
    },
    #[cfg(feature = "yaesu")]
    Keyer {
        tag: "yaesu",
        label: "Yaesu",
        hover: "an FT-991A, FT-891, FT-710 or FTDX on a serial cable, spoken to directly — \
                no daemon to run. The rate is menu 031, CAT RATE, and an FT-991A leaves the \
                factory at 4800 rather than the Elecraft's 38400. The filter is not shown \
                for these: a Yaesu reports its width as a table index rather than a number \
                of hertz. Not the older FT-817 or FT-857, which speak a different CAT.",
    },
];

/// The word for a setting, as written to the settings file.
fn keying_tag(keying: &rig::Keying) -> &'static str {
    match keying {
        rig::Keying::None => "none",
        rig::Keying::RigCtld { .. } => "rigctld",
        #[cfg(feature = "elecraft")]
        rig::Keying::Elecraft { .. } => "elecraft",
        #[cfg(feature = "yaesu")]
        rig::Keying::Yaesu { .. } => "yaesu",
    }
}

/// Where `rigctld` listens unless told otherwise — hamlib's own default.
const DEFAULT_RIG_HOST: &str = "127.0.0.1";
const DEFAULT_RIG_PORT: u16 = 4532;

/// Most text one arrival may slide in, in characters.
///
/// The slide is for text as it is *said*: a PSK character, an Olivia block of
/// five, a JS8 frame of thirteen. Larger than that and it is not an arrival at
/// all but a catch-up — a station's receiver being opened part-way through an
/// over hands over everything from where it started transmitting, which can be
/// a whole window and a hundred characters. Charging the row for all of it
/// pushed the text clean off its own panel, where it sat blank for a moment and
/// then swept back in oldest-first, looking for all the world like a row being
/// cleared and redrawn. Filled-in history should simply be there.
const SLIDE_MAX_CHARS: f32 = 16.0;

/// Longest a station's text can be asked to stay on its row, in minutes.
///
/// Two hours is a long evening on one frequency, and past it the bound on how
/// much text a channel keeps at all has taken over anyway: see
/// `channels::MAX_TEXT_CHARS`.
const MAX_FORGET_MIN: f32 = 120.0;

/// Longest text can be asked to take going.
const MAX_FADE_S: f32 = 60.0;

/// One stroke of the clear button's mark, in the asset's own units.
struct MarkStroke {
    from: Pos2,
    to: Pos2,
    width: f32,
}

/// The mark on the clear button, from `gui/assets/Red_X.svg`: the two strokes
/// that are the whole of that file, in its own 100×100 viewport.
///
/// Transcribed rather than rendered. Both strokes are straight lines —
/// `the_clear_mark_is_the_one_in_the_asset` reads the file and fails the moment
/// that stops being true — and rasterising them would mean carrying an SVG
/// stack in the binary to draw, at the eleven pixels this is seen at, what two
/// line segments draw better at every scale factor.
///
/// The digits are the file's, shortened to what an `f32` can hold: the same
/// number, written the way Rust writes it. That test is what ties the two
/// together, so it is the thing to fix if the asset ever moves.
const CLEAR_MARK: [MarkStroke; 2] = [
    MarkStroke {
        from: Pos2::new(6.3895625, 6.419563),
        to: Pos2::new(93.58044, 93.610437),
        width: 18.05196,
    },
    MarkStroke {
        from: Pos2::new(6.3894, 93.6106),
        to: Pos2::new(93.830213, 6.4194),
        width: 17.802021,
    },
];

/// The side of the viewport [`CLEAR_MARK`] is drawn in.
const CLEAR_MARK_BOX: f32 = 100.0;

/// The colour the asset draws it in, `stroke:#ff0000`.
///
/// It goes through [`legible`] like every other accent here: pure red on the
/// light palette is a stain rather than a mark.
const CLEAR_MARK_RED: Color32 = Color32::from_rgb(255, 0, 0);

/// Width of the band bar overlaid on the waterfall's right edge.
const BAND_BAR_W: f32 = 14.0;

/// Shortest the band bar's thumb is allowed to get, so a deep zoom still leaves
/// something to grab.
const MIN_THUMB_H: f32 = 10.0;

/// How far either side of the connector strip still counts as grabbing the
/// splitter.
const SPLIT_GRAB: f32 = 3.0;

/// Narrowest slice of band the waterfall will zoom into, and the floor a
/// restored view is held to. Below this the labels have nothing to attach to.
const MIN_SPAN_HZ: f64 = 50.0;

/// Neither panel may be dragged smaller than this.
const MIN_WATERFALL_W: f32 = 80.0;
const MIN_TEXT_W: f32 = 160.0;

/// Narrowest the QSO panel goes. Below this the info fields are unreadable and
/// the log wraps every other word.
const MIN_QSO_W: f32 = 240.0;

/// Clamp a wanted waterfall width so both panels stay usable in a window this
/// wide.
///
/// The minimums cannot always both be honoured — a narrow enough window has no
/// room for either — so the waterfall's wins and the text panel takes what is
/// left. Getting that order wrong is a panic, not a cosmetic bug: `clamp` needs
/// its bounds the right way round.
fn clamp_waterfall_w(window_w: f32, strip_w: f32, want: f32) -> f32 {
    let max = (window_w - strip_w - MIN_TEXT_W).max(MIN_WATERFALL_W);
    want.clamp(MIN_WATERFALL_W, max)
}

/// Panoramic monitor for HAM radio digital modes: a waterfall of a whole audio
/// band, with every conversation in it decoded and printed inline.
#[derive(clap::Parser)]
#[command(name = "ragchew", version, about, long_about = None)]
struct Args {
    /// Audio file to decode: 12 kHz mono 16-bit PCM WAV.
    ///
    /// Transcode anything else with
    /// `ffmpeg -i in.mp3 -ac 1 -ar 12000 out.wav`.
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Decode live from an audio input device. This is what happens with no
    /// arguments at all, so the flag is only worth typing for the sake of
    /// saying so.
    #[arg(long, conflicts_with_all = ["file", "demo", "weak", "crowded", "signals"])]
    live: bool,

    /// Synthetic band carrying all three protocols, twelve stations.
    #[arg(long, conflicts_with_all = ["file", "weak", "crowded", "signals"])]
    demo: bool,

    /// Synthetic Olivia band, every station below the noise floor.
    #[arg(long, alias = "olivia", conflicts_with_all = ["file", "crowded", "signals"])]
    weak: bool,

    /// Synthetic Olivia band with five modes stacked on top of each other.
    #[arg(long, conflicts_with_all = ["file", "signals"])]
    crowded: bool,

    /// Synthetic band of three QSOs, one per protocol, each over stating its
    /// mode's speed and how deep it hears.
    #[arg(long, conflicts_with = "file")]
    signals: bool,

    /// Record the live capture to a WAV file alongside the log.
    ///
    /// Writes the 12 kHz mono stream the decoders actually see, so the file
    /// plays a fault back exactly: reopen it with `ragchew <file>` and the same
    /// audio is decoded again. About 86 MB an hour. Can also be turned on and
    /// off from the Diagnostics menu while running.
    #[arg(long)]
    record: bool,

    /// Log every decode to a CSV file: `utc,t_s,mode,hz,snr_db,quality,text`.
    ///
    /// Live, this is the station's own record of what was heard — a night of
    /// traffic is a few kilobytes, against 700 MB for `--record`, so it is the
    /// one to leave on.
    ///
    /// Given a FILE as well it is a batch job instead: the recording is decoded
    /// to CSV with no window opened, and the program exits. Writes beside the
    /// input (`night.wav` -> `night.csv`) unless told otherwise with
    /// `--csv=<PATH>`. A file this app recorded is named for when it started,
    /// so its traffic comes out with real timestamps.
    // A `String` rather than a `PathBuf`: clap's path parser rejects the empty
    // string, which is exactly what a bare `--csv` has to produce for the value
    // to be optional at all. `require_equals` is what keeps `ragchew a.wav
    // --csv` from swallowing a following positional as the output name.
    #[arg(long, value_name = "PATH", num_args = 0..=1, require_equals = true,
          default_missing_value = "")]
    csv: Option<String>,

    /// Where the log, the CSV and any recordings are written.
    ///
    /// Defaults to `$XDG_DATA_HOME/ragchew`, or `~/.local/share/ragchew`.
    #[arg(long, value_name = "DIR")]
    log_dir: Option<PathBuf>,

    /// Do not write a log file.
    #[arg(long, conflicts_with = "log_dir")]
    no_log: bool,

    /// Add ragchew to the desktop's application menu, and exit.
    ///
    /// Writes a launcher entry and its icons under `$XDG_DATA_HOME`
    /// (`~/.local/share`), pointing at the binary this was run from — so run
    /// the one you mean to keep, and run it again if you move it. Needs no
    /// privileges, and is undone by deleting the files it names.
    #[arg(long)]
    install: bool,
}

/// Write the launcher entry, for this binary, where the desktop looks.
///
/// The path recorded is [`std::env::current_exe`] — the copy being run right
/// now. That is the only path that is certainly right, and it is why this is a
/// flag on the app rather than a script that would have to be told.
fn install_launcher() -> Result<Vec<std::path::PathBuf>, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot tell where this binary is: {e}"))?;
    // Resolved, so a launcher started months later does not follow a symlink
    // into a build directory that has been rebuilt under it.
    let exe = exe.canonicalize().unwrap_or(exe);
    let root = if let Some(x) = std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        std::path::PathBuf::from(x)
    } else if let Some(h) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        std::path::PathBuf::from(h).join(".local/share")
    } else {
        return Err("neither XDG_DATA_HOME nor HOME is set; nowhere to install".into());
    };
    crate::desktop::install(&root, &exe)
}

/// Side of the window icon handed to the window manager, in pixels.
///
/// Larger than anything a title bar or a task switcher asks for, since they
/// scale down from what they are given and scaling up looks like a mistake.
const ICON_PX: usize = 256;

pub fn run() -> eframe::Result<()> {
    let args = <Args as clap::Parser>::parse();

    // Installing is an errand, not a session: do it, say what was written, and
    // stop. Before the log is opened, for the same reason the batch job below
    // is — an errand has no business rotating away an overnight session's log.
    if args.install {
        match install_launcher() {
            Ok(wrote) => {
                for p in &wrote {
                    eprintln!("wrote {}", p.display());
                }
                eprintln!(
                    "\nragchew is now in the application menu. If it does not appear at \
                     once, log out and back in — some desktops only rescan then."
                );
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    // A file plus --csv is a conversion, not a session: decode it, write the
    // rows, say what happened and stop. It runs before the log is opened and
    // deliberately does not open one — a batch job reports on stderr, where a
    // batch job's output belongs, and ten conversions must not rotate away the
    // overnight session logs that the log directory exists to keep.
    if let (Some(file), Some(csv)) = (&args.file, &args.csv) {
        let out = Some(Path::new(csv)).filter(|p| !p.as_os_str().is_empty());
        match batch_to_csv(file, out) {
            Ok((rows, path)) => {
                eprintln!("{rows} decodes -> {}", path.display());
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    // First, and before anything else that could fail: a fault that takes a
    // night to appear leaves nothing behind unless the log was already open
    // when it happened. The panic hook goes in alongside, so a thread that dies
    // on the way up is recorded too.
    if !args.no_log {
        diag::install_panic_hook();
        match diag::start(args.log_dir.as_deref()) {
            Some(p) => eprintln!("logging to {}", p.display()),
            None => eprintln!("could not open a log file; continuing without one"),
        }
    }
    diag_info!(
        "start",
        "ragchew {} on {} — args: {:?}",
        VERSION,
        std::env::consts::OS,
        std::env::args().skip(1).collect::<Vec<_>>()
    );

    // A monitor with nothing on the command line should be monitoring: opening
    // deaf and waiting to be told to listen is a worse default than the one
    // thing the program is for. A file, a demo, or an explicit --live still win.
    // Which synthetic band was asked for, if any. The flags conflict with each
    // other in the parser, so at most one is set.
    let band = match (args.demo, args.weak, args.crowded, args.signals) {
        (true, _, _, _) => Some(Band::Demo),
        (_, true, _, _) => Some(Band::Weak),
        (_, _, true, _) => Some(Band::Crowded),
        (_, _, _, true) => Some(Band::Signals),
        _ => None,
    };
    let live = args.live || (args.file.is_none() && band.is_none());

    let title = match () {
        _ if live => "ragchew — live".to_string(),
        _ => match band {
            Some(b) => format!("ragchew — {}", b.source_name()),
            None => "ragchew".to_string(),
        },
    };

    let options = eframe::NativeOptions {
        // The inner size is only the first-run default; after that eframe
        // restores the window's own geometry.
        //
        // The app id has to be pinned: eframe derives the settings folder from
        // it, and it otherwise falls back to the window title — which varies
        // with the mode, so `--demo` and `--live` would each keep their own
        // separate settings and none of them would agree.
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_app_id("ragchew")
            // Drawn here rather than carried as a file, so it cannot drift away
            // from the palette it is made of. 256 is the largest a window
            // manager asks for; see `icon` for what it costs.
            .with_icon(egui::IconData {
                rgba: crate::icon::rgba(ICON_PX),
                width: ICON_PX as u32,
                height: ICON_PX as u32,
            }),
        persist_window: true,
        ..Default::default()
    };
    // Build the app inside the creator so a non-Send audio stream never crosses
    // a thread boundary.
    eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            let mut app = App::base((0.0, 4000.0));
            if let Some(saved) =
                cc.storage.and_then(|s| eframe::get_value::<Settings>(s, eframe::APP_KEY))
            {
                app.apply(saved);
            }
            // The saved palette has to reach egui before the first frame, or
            // the window flashes the system theme on the way to the wanted one.
            cc.egui_ctx.set_theme(app.theme.preference());
            // Before `go_live`, which is what reads them.
            app.record = args.record;
            // A path given here names the live log; bare `--csv` auto-names it.
            if let Some(csv) = &args.csv {
                app.set_csv(true, Some(Path::new(csv)).filter(|p| !p.as_os_str().is_empty()));
            }
            if live {
                app.go_live();
            } else if let Some(b) = band {
                app.set_source(b.synth(), b.source_name());
            } else if let Some(path) = &args.file {
                app.open_file(path);
            }
            Ok(Box::new(app))
        }),
    )
}

impl App {
    /// Start or stop the CSV traffic log.
    ///
    /// Unlike recording, this is not tied to a capture: the writer is a file
    /// and a flag, and the decode threads consult it when they have something
    /// to write. So it can be switched on before going live, and survives a
    /// change of audio device without a break in the record.
    /// `at` names the file; `None` puts an auto-named one in the data
    /// directory, which is what the menu toggle wants and what an overnight
    /// run should have.
    fn set_csv(&mut self, on: bool, at: Option<&Path>) {
        if !on {
            traffic::stop();
            return;
        }
        if traffic::is_on() {
            return;
        }
        let started = match at {
            Some(p) => traffic::start_at(p),
            None => traffic::start(&diag::data_dir()),
        };
        match started {
            Some(p) => diag_info!("traffic", "logging decodes to {}", p.display()),
            None => {
                let where_ = at.map_or_else(diag::data_dir, |p| p.to_path_buf());
                self.warn(format!("could not write {}", where_.display()));
                diag_warn!("traffic", "could not open a CSV at {}", where_.display());
            }
        }
    }
}

/// A duration as `h:mm:ss`, for a recording that may well run all night.
fn hms(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// Decode a WAV file straight to CSV and return how many rows were written.
///
/// The batch counterpart of the live traffic log, and the other half of
/// `--record`: a night's recording decoded offline, into the same shape of file
/// the live run would have produced, so the two can be compared row for row
/// when the question is why the live one stopped.
///
/// Headless — no window is opened — and it prints progress to stderr, because
/// an eight-hour recording is minutes of decoding and silence would be
/// indistinguishable from a hang.
///
/// One thread per mode, then [`protocol::arbitrate`], exactly as the file view
/// does it: a mode's own threshold is what keeps a signal out of its results,
/// so the scans do not need each other to *run* — but they do need reconciling,
/// since neither a PSK thread nor an Olivia one can tell on its own that they
/// have both claimed the same signal.
pub fn batch_to_csv(input: &Path, out: Option<&Path>) -> Result<(usize, PathBuf), String> {
    let name = input.file_name().unwrap_or(input.as_os_str()).to_string_lossy().to_string();
    let p = input.to_str().ok_or_else(|| format!("{name}: path is not valid UTF-8"))?;
    let (samples, rate) = wav::read(p).map_err(|e| format!("{name}: {e}"))?;
    if rate != SAMPLE_RATE {
        return Err(format!(
            "{name} is {rate} Hz; ragchew needs {SAMPLE_RATE} Hz mono \
             (ffmpeg -i in -ac 1 -ar {SAMPLE_RATE} out.wav)"
        ));
    }

    // Default beside the input and named after it, so decoding a directory of
    // recordings does not need a path invented for each one.
    let out = out.map(|o| o.to_path_buf()).unwrap_or_else(|| input.with_extension("csv"));
    // A recording this app made is named for when it started, which is the only
    // surviving record of when its audio was. Anything else keeps bare offsets.
    let origin = diag::stamp_in_name(&name);

    let secs = samples.len() as f64 / SAMPLE_RATE as f64;
    eprintln!(
        "{name}: {:.0} s of audio, decoding{}...",
        secs,
        match origin {
            Some(t) => format!(" from {}", diag::stamp_iso(t)),
            None => String::new(),
        }
    );

    let samples = Arc::new(samples);
    let modes = protocol::default_modes();
    let mut handles = Vec::new();
    for m in modes {
        let samples = samples.clone();
        handles.push(thread::spawn(move || {
            let found = protocol::decode_all(&samples, 300.0, 2600.0, &[m]);
            eprintln!("  {:>14}  {} decodes", m.name(), found.len());
            found
        }));
    }
    let mut found = Vec::new();
    for h in handles {
        found.extend(h.join().map_err(|_| "a decode thread panicked".to_string())?);
    }
    let kept = protocol::arbitrate(found);

    traffic::start_at(&out).ok_or_else(|| format!("could not write {}", out.display()))?;
    for d in &kept {
        traffic::log(origin.map(|t| t + d.time_s), d.time_s, d);
    }
    let rows = traffic::rows() as usize;
    traffic::stop();
    Ok((rows, out))
}

/// What a QSO opened by hand starts in until the operator says otherwise.
/// JS8 Normal is the mode most likely to raise someone.
const DEFAULT_QSO_MODE: ModeId = ModeId::Js8(ragchew::js8::Mode::Normal);

/// Which palette the window uses.
///
/// Stored and restored by name for the same reason the mode list is: a name
/// from a build that knew one more theme than this one is skipped, where an
/// enum that failed to deserialise would take every other setting down with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Theme {
    /// Whatever the desktop asks for, and it follows changes to that.
    #[default]
    Auto,
    Dark,
    Light,
}

impl Theme {
    const ALL: [Theme; 3] = [Theme::Auto, Theme::Dark, Theme::Light];

    fn name(self) -> &'static str {
        match self {
            Theme::Auto => "auto",
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }

    fn parse(name: &str) -> Option<Theme> {
        Theme::ALL.into_iter().find(|t| t.name() == name)
    }

    fn label(self) -> &'static str {
        match self {
            Theme::Auto => "Auto",
            Theme::Dark => "Dark",
            Theme::Light => "Light",
        }
    }

    fn hover(self) -> &'static str {
        match self {
            Theme::Auto => "follow the desktop",
            Theme::Dark => "always dark",
            Theme::Light => "always light",
        }
    }

    fn preference(self) -> egui::ThemePreference {
        match self {
            Theme::Auto => egui::ThemePreference::System,
            Theme::Dark => egui::ThemePreference::Dark,
            Theme::Light => egui::ThemePreference::Light,
        }
    }
}

/// Per-mode colour, so a channel's colour tells you what it is.
///
/// Each protocol gets its own end of the spectrum — JS8 cool through warm,
/// Olivia violet through cyan, PSK a yellow of its own — so you can tell the
/// families apart at a glance and still read the individual mode off the shade.
fn mode_color(m: ModeId) -> Color32 {
    use ragchew::js8::Mode as J;
    match m {
        ModeId::Js8(J::Slow) => Color32::from_rgb(130, 200, 255),   // blue
        ModeId::Js8(J::Normal) => Color32::from_rgb(150, 255, 150), // green
        ModeId::Js8(J::Fast) => Color32::from_rgb(255, 195, 120),   // orange
        ModeId::Js8(J::Turbo) => Color32::from_rgb(255, 140, 200),  // pink
        // Narrow and slow at the violet end, wide and fast towards cyan.
        ModeId::Olivia(o) => match (o.tones, o.bandwidth) {
            (4, 125) => Color32::from_rgb(190, 150, 255),
            (8, 250) => Color32::from_rgb(215, 155, 235),
            (16, 500) => Color32::from_rgb(235, 165, 205),
            (32, 1000) => Color32::from_rgb(170, 180, 255),
            (8, 500) => Color32::from_rgb(140, 220, 235),
            (16, 1000) => Color32::from_rgb(130, 235, 200),
            _ => Color32::from_rgb(200, 190, 230),
        },
        // Clear of both families: PSK31 is narrow enough to sit inside an
        // Olivia signal on the waterfall, so it must not shade like one.
        ModeId::Psk(_) => Color32::from_rgb(245, 220, 130), // yellow
    }
}

/// The mode's colour, legible as text on the panel background.
///
/// The palette above is pitched to sit on a dark panel, and on a light one
/// those pastels are all but white. Scaling them towards black keeps the hue —
/// which is the part that identifies the mode — while restoring the contrast.
fn mode_text_color(dark: bool, m: ModeId) -> Color32 {
    legible(dark, mode_color(m))
}

/// A colour picked for a dark panel, made to hold up on a light one.
///
/// Every accent here is a light tint, which is right on the dark background
/// this started life with and nearly invisible on a white one. Scaling towards
/// black preserves the hue, and the hue is what carries the meaning.
fn legible(dark: bool, c: Color32) -> Color32 {
    if dark {
        return c;
    }
    // Not `gamma_multiply`: that scales alpha too, and a translucent tint on a
    // white panel is exactly the problem being fixed.
    const K: f32 = 0.45;
    Color32::from_rgb(
        (c.r() as f32 * K) as u8,
        (c.g() as f32 * K) as u8,
        (c.b() as f32 * K) as u8,
    )
}

/// What survives a restart.
///
/// How the window is arranged, what it is listening for, and where in the band
/// it is pointed. Not the loaded file, since starting up decoding something the
/// operator has forgotten about is worse than starting up empty.
///
/// The view is kept because an operator who works one corner of the band wants
/// to come back to it, not to the whole 4 kHz every time. What it must not do
/// is reopen somewhere unusable, so a restored view is fitted to the band on
/// the way in — see [`App::set_view`].
///
/// Modes are stored by name rather than by deriving `serde` down through the
/// codec crate. Names are stable, they are already round-tripped by
/// [`protocol::parse_mode`], and an unrecognised one — from an older build, or a
/// newer one — is simply skipped instead of failing the whole file.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Settings {
    text_px: f32,
    waterfall_w: f32,
    span_s: f32,
    scroll_secs: f32,
    speed: f32,
    modes: Vec<String>,
    input_device: Option<String>,
    output_device: Option<String>,
    show_all_inputs: bool,
    /// Width of the QSO panel. The conversations themselves are not kept: a log
    /// is a record, and writing one belongs to a logging program, not to a
    /// monitor that would be silently reopening half-finished contacts.
    qso_w: f32,
    /// Frequency bounds of the waterfall view, low then high. `None` — which is
    /// also what a settings file written before this existed deserialises to —
    /// means the whole band.
    view_hz: Option<(f64, f64)>,
    /// Palette, by [`Theme::name`].
    theme: String,
    /// Mode a QSO opened by hand starts in, by [`ModeId::name`].
    default_mode: String,
    /// How long a station's text stays on its row before it starts fading off,
    /// in minutes. Zero keeps everything.
    ///
    /// A settings file written before this existed has no opinion about it and
    /// takes the default below, which is the whole reason the default is worth
    /// choosing carefully: it is what every existing installation will get.
    forget_min: f32,
    /// How long text takes to go once it has passed `forget_min`, in seconds.
    fade_secs: f32,
    /// How the transmitter is keyed: `"none"` or `"rigctld"`, with the daemon's
    /// address beside it.
    ///
    /// Three plain fields rather than the enum itself, for the reason the modes
    /// are names: a settings file outlives the shape of the type that wrote it,
    /// and a word this build does not recognise should cost the rig setting
    /// rather than the whole file.
    rig_keying: String,
    rig_host: String,
    rig_port: u16,
    /// The serial port an Elecraft is on, and the rate its menu is set to.
    rig_device: String,
    rig_baud: u32,
    /// Whether the rig's settings are expanded under the dial.
    rig_more: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            text_px: 14.0,
            waterfall_w: 300.0,
            span_s: 45.0,
            scroll_secs: 1.0,
            speed: 8.0,
            modes: protocol::default_modes().iter().map(|m| m.name()).collect(),
            input_device: None,
            output_device: None,
            show_all_inputs: false,
            qso_w: 340.0,
            view_hz: None,
            theme: Theme::default().name().to_string(),
            default_mode: DEFAULT_QSO_MODE.name(),
            // A quarter of an hour is longer than any over and longer than the
            // pause in the middle of a ragchew, and shorter than the gap that
            // means the QSO ended: a station not heard from in fifteen minutes
            // has gone, and its row is holding space against the ones that
            // have not.
            forget_min: 15.0,
            fade_secs: 8.0,
            rig_keying: "none".to_string(),
            rig_host: DEFAULT_RIG_HOST.to_string(),
            rig_port: DEFAULT_RIG_PORT,
            rig_device: DEFAULT_RIG_DEVICE.to_string(),
            rig_baud: DEFAULT_RIG_BAUD,
            rig_more: false,
        }
    }
}

/// A round tick spacing for a `span` Hz view, aiming for roughly six labels.
///
/// The old scale was fixed at whole kilohertz, which is fine for the whole band
/// and useless the moment you zoom into one conversation.
fn tick_step(span: f64) -> f64 {
    const STEPS: [f64; 9] = [5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0];
    let target = span / 6.0;
    STEPS.iter().copied().find(|&s| s >= target).unwrap_or(*STEPS.last().unwrap())
}

/// Audio-clock seconds as `m:ss`.
///
/// Minutes are not wrapped at an hour: this counts from the start of a file or
/// of a capture, and "83:20" is a clearer answer to "how far in?" than "1:23:20"
/// is, at the lengths anyone actually monitors for.
fn clock(s: f64) -> String {
    let s = s.max(0.0);
    format!("{}:{:02}", (s / 60.0) as u64, (s % 60.0) as u64)
}

/// Now, in UTC seconds — the clock the transmit queue schedules against.
fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// A wall-clock instant as `YYYY-MM-DD hh:mm:ssZ`.
///
/// UTC, because that is what a contact is logged in: a QSO spans operators in
/// as many time zones as it has stations, and only one clock is common to them.
/// Nothing here needs a time zone database, so nothing here pulls one in.
fn utc_stamp(t: std::time::SystemTime) -> String {
    let secs = match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // A clock set before 1970 is a broken clock, not a time to print.
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z", tod / 3600, (tod / 60) % 60, tod % 60)
}

/// Civil date from a count of days since 1970-01-01, by Howard Hinnant's
/// `civil_from_days`: shift the era to start on 1 March so the leap day falls
/// at the end of a year, then it is all exact integer arithmetic.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468; // days since 0000-03-01
    let era = z.div_euclid(146_097); // 400 years, the leap-rule period
    let doe = z.rem_euclid(146_097); // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of March-based year
    let mp = (5 * doy + 2) / 153; // March-based month, [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The two words on the play/pause button, in the order it shows them.
const TRANSPORT: [&str; 2] = ["pause", "play"];

/// The play/pause button: one fixed slot, an icon painted rather than typed,
/// and the word beside it.
///
/// The icon is painted because ⏸ and ▶ are not in the same bundled font — the
/// first exists only in the emoji font at 0.75 em, the second comes from Noto
/// at 0.87 em — so clicking swapped one typeface for another and the icon
/// visibly changed size, even though the button box never did. Two rectangles
/// and a triangle are the same size by construction.
///
/// The slot is sized for the longer word and the icon is pinned to the left of
/// it, so nothing moves when the state flips except the tail of the word.
fn transport_button(ui: &mut egui::Ui, playing: bool) -> egui::Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let pad = ui.spacing().button_padding;
    let icon = ui.text_style_height(&egui::TextStyle::Button) * 0.55;
    let gap = 6.0;
    let lay = |w: &str| {
        ui.painter().layout_no_wrap(w.to_owned(), font.clone(), Color32::PLACEHOLDER)
    };
    let widest = TRANSPORT.iter().map(|w| lay(w).size().x).fold(0.0_f32, f32::max);
    let galley = lay(if playing { TRANSPORT[0] } else { TRANSPORT[1] });

    let size = egui::vec2(
        pad.x * 2.0 + icon + gap + widest,
        ui.spacing().interact_size.y.max(galley.size().y + pad.y * 2.0),
    );
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let v = *ui.style().interact(&resp);
        let p = ui.painter();
        p.rect(rect, v.corner_radius, v.weak_bg_fill, v.bg_stroke, egui::StrokeKind::Inside);

        let color = v.fg_stroke.color;
        let box_ = Rect::from_center_size(
            egui::pos2(rect.left() + pad.x + icon / 2.0, rect.center().y),
            egui::Vec2::splat(icon),
        );
        if playing {
            // Pause: two bars, with the gap between them a third of the width.
            let bar = icon / 3.0;
            for x in [box_.left(), box_.right() - bar] {
                p.rect_filled(
                    Rect::from_min_size(egui::pos2(x, box_.top()), egui::vec2(bar, icon)),
                    1,
                    color,
                );
            }
        } else {
            // Play: a triangle inscribed in the same square.
            p.add(egui::Shape::convex_polygon(
                vec![box_.left_top(), box_.left_bottom(), egui::pos2(box_.right(), box_.center().y)],
                color,
                Stroke::NONE,
            ));
        }
        p.galley(
            egui::pos2(rect.left() + pad.x + icon + gap, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
    }
    resp
}

/// The clear button: the mark from the asset, painted, and the word beside it.
///
/// Painted for the reason [`transport_button`] paints its icon, and for one
/// more. The crosses a keyboard offers for this — ✖, ✕, ❌ — are in none of the
/// three fonts egui bundles and draw as an empty box; `×` is in the text face,
/// but that is the trouble with it, since a multiplication sign at button size
/// is a thin dash of a thing where a mark is wanted. Two strokes are the same
/// two strokes at any scale factor.
fn clear_button(ui: &mut egui::Ui) -> egui::Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let pad = ui.spacing().button_padding;
    let icon = ui.text_style_height(&egui::TextStyle::Button) * 0.55;
    let gap = 6.0;
    let galley = ui.painter().layout_no_wrap("clear".to_owned(), font, Color32::PLACEHOLDER);

    let size = egui::vec2(
        pad.x * 2.0 + icon + gap + galley.size().x,
        ui.spacing().interact_size.y.max(galley.size().y + pad.y * 2.0),
    );
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let v = *ui.style().interact(&resp);
        let red = legible(ui.visuals().dark_mode, CLEAR_MARK_RED);
        let p = ui.painter();
        p.rect(rect, v.corner_radius, v.weak_bg_fill, v.bg_stroke, egui::StrokeKind::Inside);

        // The asset's viewport, mapped onto a square the height of the text
        // beside it. Stroke width is scaled with it, or the mark would thicken
        // as the text was sized down.
        let box_ = Rect::from_center_size(
            egui::pos2(rect.left() + pad.x + icon / 2.0, rect.center().y),
            egui::Vec2::splat(icon),
        );
        let scale = icon / CLEAR_MARK_BOX;
        for s in &CLEAR_MARK {
            p.line_segment(
                [box_.min + s.from.to_vec2() * scale, box_.min + s.to.to_vec2() * scale],
                Stroke::new(s.width * scale, red),
            );
        }
        p.galley(
            egui::pos2(rect.left() + pad.x + icon + gap, rect.center().y - galley.size().y / 2.0),
            galley,
            v.fg_stroke.color,
        );
    }
    resp
}

/// A time on a `track_w`-point slider, snapped to whole physical pixels.
///
/// A circle drawn at a fractional position is anti-aliased differently at every
/// offset, so a thumb that slides through fractional positions appears to
/// change shape as it goes. Snapped, its sub-pixel phase never changes and it
/// only ever translates.
fn snap_to_pixels(t: f64, span: f64, track_w: f32, points_per_px: f32) -> f64 {
    let px = (track_w * points_per_px) as f64;
    if !(span.is_finite() && t.is_finite()) || span <= 0.0 || px < 1.0 {
        return t;
    }
    let per_px = span / px;
    (t / per_px).round() * per_px
}

/// The waterfall texture's width, and how many spectrogram columns each of its
/// pixels pools, for a panel `px_w` pixels wide showing `span_cols` of history.
///
/// The columns per pixel is an integer on purpose. A scrolled image is only
/// identical to a redrawn one when a new column of data is a whole pixel of
/// travel — at 1.6 columns per pixel there is no whole-pixel shift, and a third
/// of the image comes out wrong. So the texture is sized to divide the span
/// exactly and the GPU scales it the rest of the way to the panel, which costs
/// a little sharpness and buys an image that can be scrolled.
///
/// Pooling stays a maximum over whole columns either way, so a narrow carrier
/// still survives into the picture rather than being averaged away.
fn texture_geometry(span_cols: usize, px_w: usize) -> (usize, usize) {
    let per_px = ((span_cols as f64 / px_w.max(1) as f64).round() as usize).max(1);
    let tex_w = (span_cols / per_px).max(1);
    (per_px, tex_w)
}

/// One numeric setting: a track to drag and a box to type into.
///
/// The slider's own readout is turned off and a `DragValue` put in its place,
/// because a slider can only be aimed — for anything you know the number for,
/// eleven pixels of text or a forty-five second window, typing it is the
/// shorter path. The box drags too, so the pair covers both habits, and the
/// range is enforced on the typed value as much as the dragged one.
///
/// Returns the row, so a setting whose name will not carry its whole meaning in
/// one word can hang the rest of it off a hover.
fn setting(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    suffix: &str,
    speed: f32,
    decimals: usize,
) -> egui::Response {
    // One row: name, track, value. The slider stacks its own label above its
    // track, so the name is drawn here instead, and its readout is replaced by
    // a box that can be typed into as well as dragged.
    ui.horizontal(|ui| {
        ui.add_sized([64.0, ui.spacing().interact_size.y], egui::Label::new(
            egui::RichText::new(label).weak(),
        ));
        ui.add(elegance::Slider::new(value, range.clone()).show_value(false).desired_width(140.0));
        ui.add(
            egui::DragValue::new(value)
                .range(range)
                .suffix(suffix)
                .speed(speed)
                .max_decimals(decimals),
        );
    })
    .response
}

/// A menu that stays open while its contents are used.
///
/// egui closes a menu on any click inside it by default, which is right for a
/// list of commands and wrong for a panel of settings: it swallowed the click
/// that would have put a value box into text entry, and shut the mode menu on
/// every checkbox. The items that should dismiss the menu — commands, device
/// choices — call `ui.close()` themselves and still do.
fn settings_menu<R>(
    ui: &mut egui::Ui,
    label: impl Into<egui::WidgetText>,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::Response {
    use egui::containers::menu::{MenuButton, MenuConfig};
    MenuButton::from_button(egui::Button::new(label))
        .config(MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside))
        .ui(ui, contents)
        .0
}

/// How solidly text that arrived at `at` is drawn, `now`: whole until it has
/// been on the row `keep` seconds, then fading to nothing over the next `fade`.
///
/// The ages are on the audio clock, not the wall clock, and deliberately: what
/// makes text stale is how long ago the station sent it, which is a fact about
/// the band rather than about how fast a recording of it is being played back.
/// A file running at 8× therefore ages its text 8× faster on screen, because
/// that is what it is doing to the band it is replaying.
fn solidity(now: f64, at: Option<f64>, keep: Option<f64>, fade: f64) -> f32 {
    let Some(keep) = keep else {
        return 1.0;
    };
    // Older than anything the channel still has a time for, so it is the oldest
    // text there is — see [`Channel::tail_ages`].
    let Some(at) = at else {
        return 0.0;
    };
    let over = (now - at) - keep;
    if over <= 0.0 {
        return 1.0;
    }
    if fade <= 0.0 {
        return 0.0;
    }
    (1.0 - over / fade).clamp(0.0, 1.0) as f32
}

/// The log's offset column: how long into the QSO a line landed.
fn offset(s: f64) -> String {
    format!("+{:>6}", clock(s))
}

/// A decode plus the playback time at which it should appear.
struct Frame {
    reveal_s: f64,
    decode: protocol::Decode,
}

/// Live-capture state: a rolling spectrogram fed from the audio buffer and
/// channels accumulated from the decode threads.
struct LiveMode {
    band: (f64, f64),
    modes: Vec<ModeId>,
    /// List every PCM the host advertises rather than the winnowed set.
    show_all_inputs: bool,
    audio: Option<crate::audio::LiveAudio>,
    err: Option<String>,
    frames_rx: Option<std::sync::mpsc::Receiver<Vec<protocol::Decode>>>,
    spec: Arc<Spectrogram>,
    next_col: u64,
    /// Columns ever pushed into the rolling spectrogram, including those since
    /// dropped off its front. The spectrogram is capped, so its length stops
    /// growing after eight minutes while the data keeps sliding through it;
    /// anything tracking progress by length freezes at that point, which is
    /// what left the waterfall stale and jumping.
    cols_total: i64,
    channels: ChannelSet,
    norm: (f32, f32),
    cols_since_norm: usize,
    t_now: f64,
    devices: Vec<crate::audio::AudioDevice>,
    selected: Option<String>, // current device name
    /// Counters the decode threads, the capture callback and this struct all
    /// write, and the heartbeat reads. Outlives any one capture, so the totals
    /// survive a change of device.
    health: Arc<Health>,
    /// The recording in progress, if any. Dropping it finalises the file.
    recorder: Option<Recorder>,
    /// Retires the heartbeat belonging to the previous capture when the device
    /// changes, so only one of them is ever writing health lines.
    heartbeat_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl LiveMode {
    fn start(
        band: (f64, f64),
        modes: Vec<ModeId>,
        device: Option<String>,
        show_all_inputs: bool,
        record: bool,
    ) -> LiveMode {
        let mut lm = LiveMode {
            band,
            modes,
            show_all_inputs,
            audio: None,
            err: None,
            frames_rx: None,
            spec: Arc::new(Spectrogram::empty(band.0, band.1, WINDOW / 4)),
            next_col: 0,
            cols_total: 0,
            channels: ChannelSet::new(15.0),
            norm: (0.0, 1.0),
            cols_since_norm: 0,
            t_now: 0.0,
            devices: Vec::new(),
            selected: None,
            health: Arc::new(Health::default()),
            recorder: None,
            heartbeat_stop: None,
        };
        // Before opening, so `--record` catches the first sample rather than
        // whatever is left after the interface has finished starting up.
        if record {
            lm.set_recording(true);
        }
        lm.open(device);
        lm
    }

    /// Start or stop recording the capture to disk.
    ///
    /// The recording is attached to the audio buffer rather than to a stream,
    /// so it survives the operator changing device mid-run — the new capture
    /// simply picks up the same tap, and the join in the file is logged.
    fn set_recording(&mut self, on: bool) {
        match (on, self.recorder.is_some()) {
            (true, false) => {
                match Recorder::start(&diag::data_dir(), SAMPLE_RATE, self.health.clone()) {
                    Ok(r) => {
                        if let Some(a) = &self.audio {
                            a.set_tap(Some(r.tap()));
                        }
                        self.recorder = Some(r);
                    }
                    Err(e) => {
                        self.err = Some(format!("could not start recording: {e}"));
                        diag_warn!("record", "could not start: {e}");
                    }
                }
            }
            (false, true) => {
                if let Some(a) = &self.audio {
                    a.set_tap(None);
                }
                self.recorder = None; // finalises the file on drop
            }
            _ => {}
        }
    }

    /// (Re)open capture on the given device (or the default), resetting the
    /// rolling spectrogram and channels.
    fn open(&mut self, name: Option<String>) {
        // dropping the old LiveAudio stops its stream; dropping the old receiver
        // makes the old decoder thread exit on its next send.
        self.audio = None;
        self.frames_rx = None;
        if let Some(stop) = self.heartbeat_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.spec = Arc::new(Spectrogram::empty(self.band.0, self.band.1, WINDOW / 4));
        self.next_col = 0;
        self.cols_total = 0;
        self.channels = ChannelSet::new(15.0);
        self.norm = (0.0, 1.0);
        self.cols_since_norm = 0;
        self.t_now = 0.0;
        self.devices = crate::audio::input_devices(self.show_all_inputs);
        // A remembered device may be gone — a USB interface unplugged, or
        // settings carried to another machine. Falling back to the default
        // input beats refusing to listen at all.
        let h = || self.health.clone();
        let started = crate::audio::LiveAudio::start(name.as_deref(), h()).or_else(|e| {
            if name.is_some() {
                crate::audio::LiveAudio::start(None, h())
            } else {
                Err(e)
            }
        });
        match started {
            Ok(audio) => {
                // A recording in progress follows the capture to the new
                // device. The file gets a join at that point, which is worth
                // saying rather than leaving to be puzzled over later.
                if let Some(r) = &self.recorder {
                    audio.set_tap(Some(r.tap()));
                    diag_info!("record", "capture reopened; recording continues in the same file");
                }
                self.frames_rx = Some(crate::audio::spawn_decoder(
                    audio.buf.clone(),
                    self.modes.clone(),
                    h(),
                ));
                self.heartbeat_stop = Some(crate::audio::spawn_heartbeat(&audio.buf, h()));
                self.selected = Some(audio.device_id.clone());
                self.err = None;
                self.audio = Some(audio);
            }
            Err(e) => {
                diag_warn!("audio", "could not open capture: {e}");
                self.selected = name;
                self.err = Some(e);
            }
        }
    }

    /// Change the set of modes being listened for, restarting the decode
    /// threads. Already-decoded text stays on screen.
    fn set_modes(&mut self, modes: Vec<ModeId>) {
        self.modes = modes;
        if let Some(audio) = &self.audio {
            // dropping the old receiver retires the old decode threads
            self.frames_rx = Some(crate::audio::spawn_decoder(
                audio.buf.clone(),
                self.modes.clone(),
                self.health.clone(),
            ));
        }
    }

    /// Pull new audio into the rolling spectrogram and new decodes into channels.
    fn tick(&mut self) {
        const MAX_COLS: usize = 12_000;
        let Some(audio) = &self.audio else { return };

        let hop = self.spec.hop as u64;
        let mut new_windows: Vec<Vec<f32>> = Vec::new();
        let global_len;
        {
            let b = audio.buf.lock().unwrap();
            // don't fall behind a trimmed buffer
            if self.next_col * hop < b.global_start() {
                self.next_col = b.global_start() / hop + 1;
            }
            while let Some(w) = b.window_at(self.next_col * hop) {
                new_windows.push(w);
                self.next_col += 1;
                if new_windows.len() > MAX_COLS {
                    break;
                }
            }
            global_len = b.global_len();
        }

        if !new_windows.is_empty() {
            let s = Arc::make_mut(&mut self.spec);
            for w in &new_windows {
                s.push_window(w);
            }
            self.cols_total += new_windows.len() as i64;
            if s.columns.len() > MAX_COLS {
                let drop = s.columns.len() - MAX_COLS;
                s.columns.drain(0..drop);
            }
            self.cols_since_norm += new_windows.len();
            // The waterfall's own pulse. The reported fault was that this kept
            // beating while decoding stopped, and a log that recorded only the
            // decoders could neither confirm nor contradict that.
            Health::bump(&self.health.ui_columns, new_windows.len() as u64);
        }
        Health::bump(&self.health.ui_frames, 1);

        self.t_now = global_len as f64 / SAMPLE_RATE as f64;

        if self.cols_since_norm >= 25 && !self.spec.columns.is_empty() {
            self.norm = waterfall::percentiles(&self.spec);
            self.cols_since_norm = 0;
        }

        if let Some(rx) = &self.frames_rx {
            while let Ok(frames) = rx.try_recv() {
                for d in frames {
                    self.channels.add(d);
                }
            }
        }
    }

    /// Short status for the toolbar. The device name is deliberately left out:
    /// the picker sitting next to this already shows it, and the bar has one
    /// line to work with.
    fn device_label(&self) -> String {
        match (&self.err, &self.audio) {
            (Some(e), _) => format!("audio error: {e}"),
            (None, Some(a)) => format!("LIVE {:.1} kHz", a.in_rate as f32 / 1000.0),
            _ => "LIVE".to_string(),
        }
    }
}

struct App {
    band: (f64, f64),
    duration_s: f64,

    // loaded asynchronously by a worker thread
    spec: Option<Arc<Spectrogram>>,
    norm: (f32, f32),
    frames: Vec<Frame>,
    /// Scan jobs still to report. The file scan is split by mode across
    /// threads, so text arrives in batches while playback is already running.
    pending_scans: usize,
    spec_rx: Receiver<(Arc<Spectrogram>, (f32, f32))>,
    frames_rx: Receiver<Vec<Frame>>,

    // playback
    t_now: f64,
    playing: bool,
    speed: f32,
    span_s: f32,
    scroll_secs: f32, // duration of the scroll-in animation for a new chunk

    // view
    vp: Viewport,
    text_px: f32,
    waterfall_w: f32,
    strip_w: f32,

    tex: Option<egui::TextureHandle>,
    tex_key: (usize, usize, i64, i64, usize, i64),
    /// The waterfall image on the CPU, kept between frames so new columns can
    /// be scrolled into it instead of the whole picture being redrawn, with the
    /// column and contrast bounds it currently shows.
    wf_img: Option<waterfall::Image>,
    wf_col: i64,
    wf_norm: (f32, f32),

    /// Every mode the app knows about, and whether it is being listened for.
    /// Scanning costs time per mode, so this is the app's main speed control.
    modes: Vec<(ModeId, bool)>,
    /// Kept so a change to `modes` can re-scan a file without reloading it.
    samples: Arc<Vec<f32>>,
    /// What is being monitored, for the title bar.
    source: String,
    /// The last thing worth saying, for the status bar.
    status: Option<Note>,
    /// Identifier of the audio input to prefer when going live, remembered
    /// across restarts.
    preferred_input: Option<String>,
    /// Offer every ALSA PCM in the device pickers, not just the winnowed set.
    show_all_inputs: bool,
    /// What a QSO opened by hand transmits in. Not tied to the modes being
    /// scanned for: what you listen to and what you call in are separate
    /// choices, and a QSO started by hand has no decode to take a mode from.
    default_mode: ModeId,
    /// Palette the operator asked for. egui holds the same preference, but this
    /// is what gets written to the settings file — and under Auto, egui's
    /// resolved light/dark cannot tell us what was asked for.
    theme: Theme,

    /// Open conversations, and the width of the panel they live in.
    qsos: QsoSet,
    qso_w: f32,
    /// Where transmissions go. Opened on the first send rather than at startup,
    /// so a monitor that is never used to transmit never takes an output device
    /// off anything else.
    tx: Option<Tx>,
    /// Identifier of the output to transmit on, remembered across restarts.
    preferred_output: Option<String>,

    /// True while a drag that began on the band bar is in progress, and the
    /// frequency offset between where it was grabbed and the view centre — so
    /// the grabbed point stays under the cursor instead of snapping to it.
    bar_drag: bool,
    bar_grab_hz: f64,

    /// True while a drag that began on the connector strip is resizing the
    /// waterfall, and where in the strip it was grabbed — so the strip tracks
    /// the cursor instead of snapping its edge to it.
    split_drag: bool,
    split_grab_dx: f32,

    live: Option<LiveMode>,

    /// Whether live capture should be recorded to disk.
    ///
    /// Deliberately not among the persisted [`Settings`]: a monitor that
    /// silently resumed recording every launch would fill a disk with files
    /// nobody asked for. It is set for one run, from `--record` or the
    /// diagnostics menu.
    record: bool,

    /// How long a station's text stays on its row, in minutes, and how long it
    /// takes to fade off once it has been there that long. Zero minutes turns
    /// the timeout off and keeps everything, which is what a monitor did before
    /// this existed.
    forget_min: f32,
    fade_secs: f32,
    /// Text the operator has dismissed: the instant before which a channel's
    /// text is no longer shown, by channel id, and one for the whole band from
    /// the clear button.
    ///
    /// An instant rather than a flag, because a cleared station goes on
    /// transmitting and what it says next is not what was cleared. Held here
    /// rather than applied to the channels themselves because the file view
    /// rebuilds its channels from scratch on every frame — a clear that lived
    /// in the set would last one frame — and because a log is a record: the
    /// conversations take their text from the set before this trims the copy
    /// the window is drawn from.
    cleared: HashMap<u64, f64>,
    cleared_all_at: f64,

    /// How far each row still has to slide, by channel id. See [`Slide`] and
    /// [`App::slide_rows`].
    slide: HashMap<u64, Slide>,

    /// What each row looked like last time it was drawn, so the log can say
    /// when one loses its text. See [`App::watch_rows`].
    seen_rows: HashMap<u64, RowSeen>,

    /// How the transmitter is keyed, and the supervisor doing it.
    ///
    /// The supervisor is rebuilt whenever the setting changes; dropping the old
    /// one takes the rig off the air before the new one exists, so the two can
    /// never both think they hold the key.
    rig: rig::Keying,
    ptt: Option<rig::Ptt>,
    /// The daemon's address, and the cable an Elecraft is on, kept even while
    /// nothing is keying so that turning rig control back on does not ask for
    /// them again.
    rig_host: String,
    rig_port: u16,
    rig_device: String,
    rig_baud: u32,

    /// Where the hand has pushed the dial, ahead of the rig.
    ///
    /// Tuning cannot read the rig back between one wheel click and the next:
    /// the rig is asked twice a second and a hand moves faster than that, so
    /// every click inside the same reading would work from the same stale
    /// number and ask for the same frequency again. The hand accumulates here
    /// instead, and the rig's own reading takes over once it stops.
    tuning: Option<Tuning>,

    /// Which digit the dial is pointed at, counted from the right, and whether
    /// the keys put it there. See [`App::vfo`].
    dial_pick: usize,
    dial_by_key: Option<Pos2>,
    /// Whether the rig's settings are on show under the dial.
    rig_more: bool,

    /// Whether the list of modes is showing, which it does while the pointer is
    /// on the mode or in the list itself.
    mode_menu: bool,

    /// Whether the dial has the keyboard.
    ///
    /// Kept here rather than left to egui's focus, which drops a widget that
    /// asks for focus without being a text field: the dial held the keyboard
    /// for exactly one keystroke and then lost it, whatever it told the memory
    /// about still wanting it. It is given up when a real text field takes the
    /// keyboard — the reply box is directly beneath and must win — and on
    /// Escape.
    dial_typing: bool,

    /// The frequency under the pointer, while it is somewhere that has one.
    ///
    /// Measured as the panorama is drawn and reported by the status bar, which
    /// is a panel and so is laid out before it — the reading is therefore one
    /// frame old. A frame is nothing against a hand moving a mouse, and the
    /// alternative is working the geometry out twice.
    cursor_hz: Option<f64>,

    /// The file view's channels, and how much of the revealed list has been fed
    /// into them. See [`App::channels_now`].
    file_set: ChannelSet,
    fed_frames: usize,
    /// Set when the frames themselves change, so the set is built again rather
    /// than fed decodes it has already had.
    frames_dirty: bool,

    /// What PipeWire could route into the capture, as of [`App::pw_at`].
    ///
    /// Held rather than asked for, because a menu redraws every frame and the
    /// answer costs a subprocess. Refreshed only while the menu that shows it
    /// is open — see [`PW_REFRESH_S`].
    pw: crate::pipewire::Routing,
    pw_at: Option<std::time::Instant>,

    /// The ports a rig might be on, for whichever rig panels offer a picker.
    #[cfg(feature = "serial")]
    rig_ports: SerialPorts,
}

/// Serial ports the machine says it has, and when they were last looked for.
///
/// Held rather than asked for, for the same reason as [`App::pw`]: the menu
/// that shows them redraws every frame, and finding them means reading a
/// directory or the registry. Refreshed only while that menu is open — see
/// [`PORT_REFRESH_S`].
#[cfg(feature = "serial")]
#[derive(Default)]
struct SerialPorts {
    found: Vec<rig::serial::Candidate>,
    at: Option<std::time::Instant>,
}

/// How stale the routing list may get while its menu is open.
///
/// Long enough that holding the menu open is not a poll loop, short enough that
/// a browser tab started *because* the menu was empty shows up without the
/// operator wondering whether the feature works.
const PW_REFRESH_S: f64 = 1.5;

/// How stale the list of serial ports may get while its menu is open.
///
/// Same bargain as [`PW_REFRESH_S`], and the same reason to keep it short: the
/// operator who finds the list empty goes and plugs the cable in, and wants to
/// see it appear without having closed and reopened anything.
#[cfg(feature = "serial")]
const PORT_REFRESH_S: f64 = 1.5;

/// One line for the status bar.
///
/// Two kinds, because they had been one and it showed: a failed open and a
/// successful "listening to …" both came out under a red warning triangle, so
/// the app announced good news as a fault.
#[derive(Clone, Debug, PartialEq)]
struct Note {
    text: String,
    /// Something went wrong, as against something merely happened.
    bad: bool,
}

impl App {
    /// Say what just happened.
    fn say(&mut self, text: impl Into<String>) {
        self.status = Some(Note { text: text.into(), bad: false });
    }

    /// Say what just went wrong.
    fn warn(&mut self, text: impl Into<String>) {
        self.status = Some(Note { text: text.into(), bad: true });
    }

    /// The status line along the bottom of the window.
    ///
    /// What lives here is what the operator glances at rather than acts on: how
    /// many conversations the band is carrying, and the last thing that
    /// happened. Both were on the toolbar, which has to stay on one line and
    /// was competing for it with the controls — and a message long enough to be
    /// useful ("could not listen to Firefox: …") had nowhere to go.
    ///
    /// The row is allocated whether or not there is anything to say. A bar that
    /// appears when a message does would resize the panorama above it, and a
    /// waterfall that jumps every time the app mentions something is worse than
    /// no message at all.
    fn status_bar(&mut self, ui: &mut egui::Ui, live: bool, threads: usize) {
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.set_min_height(ui.spacing().interact_size.y);
                // A counting readout that sets its own width drags everything
                // after it along each time a station appears, so the slot is
                // sized once for the widest thing it will ever hold. Measured
                // in the font actually in force, so it survives text scaling.
                let font = egui::TextStyle::Body.resolve(ui.style());
                let width = ui
                    .painter()
                    .layout_no_wrap("⏳ 8888 threads".to_owned(), font.clone(), Color32::PLACEHOLDER)
                    .size()
                    .x;
                let text = if !live && self.samples.is_empty() {
                    // Nothing loaded is not the same state as something still
                    // decoding: the slot stays reserved but empty.
                    String::new()
                } else if !live && self.pending_scans > 0 {
                    // Counting while the scan is still running: the count is
                    // real, it is just not final yet.
                    format!("⏳ {threads} thread{}", if threads == 1 { "" } else { "s" })
                } else {
                    format!("{threads} thread{}", if threads == 1 { "" } else { "s" })
                };
                ui.allocate_ui_with_layout(
                    egui::vec2(width, ui.spacing().interact_size.y),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| ui.label(text),
                );
                ui.separator();
                // Where the pointer is on the band, sized for the widest
                // reading it can hold so that a hand moving up the waterfall
                // does not drag the message along with it. It is an offset from
                // the receiver's dial, like every frequency this app shows.
                let width = ui
                    .painter()
                    .layout_no_wrap("8888.8 Hz".to_owned(), font, Color32::PLACEHOLDER)
                    .size()
                    .x;
                ui.allocate_ui_with_layout(
                    egui::vec2(width, ui.spacing().interact_size.y),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| match self.cursor_hz {
                        Some(hz) => ui.weak(format!("{hz:.1} Hz")),
                        None => ui.label(""),
                    },
                );
                ui.separator();
                match &self.status {
                    Some(n) if n.bad => {
                        let c = legible(ui.visuals().dark_mode, Color32::from_rgb(255, 150, 150));
                        ui.colored_label(c, format!("⚠ {}", n.text));
                    }
                    Some(n) => {
                        ui.label(&n.text);
                    }
                    // With nothing to say, the file being read is worth saying.
                    // Live, the toolbar already names the device.
                    None if !live && !self.source.is_empty() => {
                        ui.weak(&self.source);
                    }
                    None => {}
                }
            });
        });
    }

    /// Common view defaults shared by file and live constructors.
    fn base(band: (f64, f64)) -> App {
        let (_a, spec_rx) = channel();
        let (_b, frames_rx) = channel();
        App {
            band,
            duration_s: 0.0,
            spec: None,
            norm: (0.0, 1.0),
            frames: Vec::new(),
            pending_scans: 0,
            spec_rx,
            frames_rx,
            t_now: 0.0,
            playing: false,
            speed: 8.0,
            span_s: 45.0,
            scroll_secs: 1.0,
            vp: Viewport { f_lo: band.0, f_hi: band.1 },
            text_px: 14.0,
            waterfall_w: 300.0,
            strip_w: 35.0,
            tex: None,
            tex_key: (0, 0, 0, 0, 0, 0),
            wf_img: None,
            wf_col: i64::MIN,
            wf_norm: (0.0, 1.0),
            modes: protocol::default_modes().into_iter().map(|m| (m, true)).collect(),
            samples: Arc::new(Vec::new()),
            source: String::new(),
            status: None,
            preferred_input: None,
            show_all_inputs: false,
            default_mode: DEFAULT_QSO_MODE,
            theme: Theme::default(),
            qsos: QsoSet::new(),
            qso_w: 340.0,
            tx: None,
            preferred_output: None,
            bar_drag: false,
            bar_grab_hz: 0.0,
            split_drag: false,
            split_grab_dx: 0.0,
            live: None,
            record: false,
            forget_min: 15.0,
            fade_secs: 8.0,
            cleared: HashMap::new(),
            slide: HashMap::new(),
            seen_rows: HashMap::new(),
            rig: rig::Keying::None,
            ptt: None,
            rig_host: DEFAULT_RIG_HOST.to_string(),
            rig_port: DEFAULT_RIG_PORT,
            rig_device: DEFAULT_RIG_DEVICE.to_string(),
            rig_baud: DEFAULT_RIG_BAUD,
            cursor_hz: None,
            tuning: None,
            dial_pick: 0,
            dial_by_key: None,
            dial_typing: false,
            mode_menu: false,
            rig_more: false,
            file_set: ChannelSet::new(15.0),
            fed_frames: 0,
            frames_dirty: false,
            cleared_all_at: f64::NEG_INFINITY,
            pw: crate::pipewire::Routing::default(),
            pw_at: None,
            #[cfg(feature = "serial")]
            rig_ports: SerialPorts::default(),
        }
    }

    /// The current state of everything that persists.
    fn settings(&self) -> Settings {
        Settings {
            text_px: self.text_px,
            waterfall_w: self.waterfall_w,
            span_s: self.span_s,
            scroll_secs: self.scroll_secs,
            speed: self.speed,
            modes: self.enabled_modes().iter().map(|m| m.name()).collect(),
            input_device: self.preferred_input.clone(),
            output_device: self.preferred_output.clone(),
            show_all_inputs: self.show_all_inputs,
            qso_w: self.qso_w,
            view_hz: Some((self.vp.f_lo, self.vp.f_hi)),
            theme: self.theme.name().to_string(),
            default_mode: self.default_mode.name(),
            forget_min: self.forget_min,
            fade_secs: self.fade_secs,
            rig_keying: keying_tag(&self.rig).to_string(),
            rig_host: self.rig_host.clone(),
            rig_port: self.rig_port,
            rig_device: self.rig_device.clone(),
            rig_baud: self.rig_baud,
            rig_more: self.rig_more,
        }
    }

    /// Restore saved settings over the defaults.
    fn apply(&mut self, s: Settings) {
        self.text_px = s.text_px.clamp(8.0, 22.0);
        self.waterfall_w = s.waterfall_w;
        self.span_s = s.span_s.clamp(10.0, 120.0);
        self.scroll_secs = s.scroll_secs.clamp(0.0, 2.0);
        self.speed = s.speed.clamp(1.0, 20.0);
        self.preferred_input = s.input_device;
        self.preferred_output = s.output_device;
        self.show_all_inputs = s.show_all_inputs;
        self.qso_w = s.qso_w.clamp(MIN_QSO_W, 900.0);
        self.forget_min = s.forget_min.clamp(0.0, MAX_FORGET_MIN);
        self.fade_secs = s.fade_secs.clamp(0.0, MAX_FADE_S);
        if !s.rig_host.trim().is_empty() {
            self.rig_host = s.rig_host;
        }
        if s.rig_port != 0 {
            self.rig_port = s.rig_port;
        }
        // Anything but the word this build knows leaves the rig alone, which is
        // the setting that touches hardware and so the one to be conservative
        // about: an unreadable file must not leave something keying a radio.
        if !s.rig_device.trim().is_empty() {
            self.rig_device = s.rig_device;
        }
        if s.rig_baud != 0 {
            self.rig_baud = s.rig_baud;
        }
        self.rig_more = s.rig_more;
        let keying = self.keying_for(&s.rig_keying);
        self.set_keying(keying);
        // An unknown name leaves the theme at Auto rather than at whatever the
        // last build happened to write.
        self.theme = Theme::parse(&s.theme).unwrap_or_default();
        // A name this build does not know — an older file, or a newer one —
        // leaves the mode at the default rather than failing the whole file.
        self.default_mode = protocol::parse_mode(&s.default_mode).unwrap_or(DEFAULT_QSO_MODE);
        let (lo, hi) = s.view_hz.unwrap_or(self.band);
        self.set_view(lo, hi);

        let wanted: Vec<ModeId> = s.modes.iter().filter_map(|n| protocol::parse_mode(n)).collect();
        // An empty or wholly unrecognisable list would leave the app scanning
        // for nothing and looking broken, so it falls back to everything.
        if !wanted.is_empty() {
            for (m, on) in self.modes.iter_mut() {
                *on = wanted.contains(m);
            }
        }
    }

    /// How long text stays on a row before it starts to go, in seconds, or
    /// `None` when nothing is forgotten — which is what a zero means.
    fn forget_secs(&self) -> Option<f64> {
        (self.forget_min > 0.0).then_some(self.forget_min as f64 * 60.0)
    }

    /// The instant before which this channel's text is no longer shown: the
    /// last time it was cleared by hand, or the trailing edge of the timeout,
    /// whichever is later. `-∞` when neither applies, which is "keep
    /// everything".
    fn cut_for(&self, ch: &Channel, now: f64) -> f64 {
        let by_hand = self
            .cleared
            .get(&ch.id)
            .copied()
            .unwrap_or(f64::NEG_INFINITY)
            .max(self.cleared_all_at);
        match self.forget_secs() {
            // The fade is part of the life of the text: it is not dropped when
            // it starts fading, it is dropped when it has finished.
            Some(keep) => by_hand.max(now - keep - self.fade_secs as f64),
            None => by_hand,
        }
    }

    /// Take everything on the rows off them, as of `now`.
    ///
    /// The per-channel clears go with it: none of them can be later than this,
    /// so none of them can still have anything to say. What arrives after this
    /// instant is new text and appears as usual, including on a station that
    /// was cleared mid-over and is still transmitting.
    fn clear_all(&mut self, now: f64) {
        self.cleared.clear();
        self.cleared_all_at = now;
    }

    /// Drop the clears the clock has fallen back behind.
    ///
    /// A clear is a moment in the recording, and the clock can be put back
    /// before one: ⟲ restart, a drag of the playback slider, a new file. What
    /// was cleared has then not been said yet, so there is nothing left for the
    /// clear to hold against.
    ///
    /// Holding it anyway is what a moment means read literally, and it is
    /// wrong: restarting a demo after clearing it left the panel blank for the
    /// whole replay up to the instant clear was pressed, which reads exactly
    /// like the decoding having stopped.
    fn drop_stale_clears(&mut self, now: f64) {
        if self.cleared_all_at > now {
            self.cleared_all_at = f64::NEG_INFINITY;
        }
        self.cleared.retain(|_, at| *at <= now);
    }

    /// Drop every clear, whatever the clock says.
    ///
    /// For when the channels themselves are rebuilt rather than replayed. A
    /// clear on one station is a channel id, and an id means something only
    /// within the set that issued it: rescan the same recording for a different
    /// set of modes and id 3 is a different station, or nobody at all. The
    /// band-wide clear goes too, since the times it was aimed at belong to a
    /// scan that no longer exists.
    fn drop_clears(&mut self) {
        self.cleared.clear();
        self.cleared_all_at = f64::NEG_INFINITY;
    }

    /// Move every row on towards where its newest character belongs.
    ///
    /// Each channel owes a character's width for every character it has taken
    /// on since the last frame, and pays it off exponentially: `scroll_secs` is
    /// read as the time to settle, which an exponential reaches at about three
    /// time constants. Under half a pixel it is simply at rest, so a quiet band
    /// costs nothing and nothing creeps.
    ///
    /// Two cases are not debts and take none. A channel seen for the first time
    /// owes only its newest decode — a row that appeared mid-conversation would
    /// otherwise fly in from the far right — and a channel with *less* text than
    /// it had has been cleared or forgotten, which is not something to animate.
    /// A third is capped rather than refused: see [`SLIDE_MAX_CHARS`].
    ///
    /// The map is rebuilt from the channels on show, so it holds one small
    /// entry per row for exactly as long as the row is there.
    fn slide_rows(&mut self, channels: &ChannelSet, char_w: f32, dt: f32, panel_w: f32) {
        let decay = if self.scroll_secs > 1e-3 {
            (-dt / (self.scroll_secs / 3.0)).exp()
        } else {
            0.0
        };
        let mut next: HashMap<u64, Slide> = HashMap::with_capacity(channels.channels().len());
        for ch in channels.channels() {
            let chars = ch.next_index();
            let mut s = match self.slide.get(&ch.id) {
                Some(&was) if chars > was.chars => Slide {
                    px: was.px + (chars - was.chars) as f32 * char_w,
                    chars,
                },
                Some(&was) if chars < was.chars => Slide { px: 0.0, chars },
                Some(&was) => Slide { chars, ..was },
                None => Slide { px: ch.last_chunk_chars as f32 * char_w, chars },
            };
            // A chunk's worth at most, and never further right than the panel
            // is wide.
            s.px = (s.px * decay).min(SLIDE_MAX_CHARS * char_w).min(panel_w);
            if s.px < 0.5 {
                s.px = 0.0;
            }
            next.insert(ch.id, s);
        }
        self.slide = next;
    }

    /// Say, in the log, when a row loses the text it had.
    ///
    /// A row that clears and fills in again is a fault nobody can catch by eye:
    /// it happens in the frames either side of a station arriving, and by the
    /// time it is noticed the evidence has been drawn over. So the window keeps
    /// a note of each row as it last drew it, and writes a line when one of
    /// three things happens — text going off a row that stayed, a row returning
    /// after a gap the eye would see, or a station coming back under a new
    /// channel id, which means its text starts again from nothing.
    ///
    /// Each line carries what is needed to tell those apart without asking for
    /// another run: what the row drew against what the channel holds (the
    /// difference between text that is gone and text that is merely not drawn),
    /// where the row sat, what it was still sliding by, and the instant text is
    /// being forgotten before against the oldest the channel still has.
    ///
    /// Returns the lines rather than writing them, so that what it decides is
    /// worth saying can be tested.
    fn watch_rows(&mut self, rows: &[RowNow], wall: f64) -> Vec<String> {
        let mut said = Vec::new();
        for r in rows {
            let (hz, mode) = (r.hz, r.mode.name());
            let was = self.seen_rows.get(&r.id).copied();
            // A station whose channel was rebuilt comes back as a different id
            // at the same place on the band, holding none of its text.
            // Of the same protocol, and gone from the screen: the demo band
            // alone has a JS8 station inside an Olivia signal on the same
            // carrier, and two rows sharing a frequency is not one row coming
            // back as the other.
            let reborn = was
                .is_none()
                .then(|| {
                    self.seen_rows
                        .iter()
                        .find(|(id, s)| {
                            s.mode.protocol() == r.mode.protocol()
                                && (s.hz - hz).abs() <= r.mode.assoc_tol_hz()
                                && !rows.iter().any(|o| o.id == **id)
                        })
                        .map(|(id, s)| (*id, wall - s.at, s.drawn))
                })
                .flatten();
            let oldest = r.oldest.map_or("none".to_string(), |t| format!("{t:.1}s"));
            // The cut is minus infinity when nothing is being forgotten, which
            // in a log line has to read as words rather than as a float.
            let cut = if r.cut.is_finite() {
                format!("{:.1}s", r.cut)
            } else {
                "never".to_string()
            };

            let line = match (was, reborn) {
                // Text off a row that stayed put.
                (Some(w), _) if w.drawn >= 8 && r.drawn * 2 <= w.drawn => Some(format!(
                    "{mode} {hz:.0} Hz lost text on screen: {} characters drawn where there were \
                     {}, out of {} the channel holds (it held {}); row y={:.0}, sliding {:.0} px, \
                     forgetting before {cut} with its oldest text at {oldest}",
                    r.drawn, w.drawn, r.held, w.held, r.y, r.offset
                )),
                // A row that went away and came back.
                (Some(w), _) if wall - w.at > ROW_BLINK_S => Some(format!(
                    "{mode} {hz:.0} Hz is back after {:.1}s off screen, drawing {} characters \
                     where it drew {}; the channel holds {} (it held {}), forgetting before \
                     {cut} with its oldest text at {oldest}",
                    wall - w.at,
                    r.drawn,
                    w.drawn,
                    r.held,
                    w.held
                )),
                // The same station, a different channel.
                (None, Some((old_id, ago, drew))) => Some(format!(
                    "{mode} {hz:.0} Hz has come back as channel {} where it was channel {old_id} \
                     {ago:.1}s ago, so its text starts again: {} characters drawn where that drew \
                     {drew}",
                    r.id, r.drawn
                )),
                _ => None,
            };

            // One line a second per row. A row that flaps every frame has said
            // what it has to say in the first of them, and sixty a second would
            // bury the rest of the log.
            let quiet_since = was.map_or(f64::NEG_INFINITY, |w| w.said_at);
            let said_at = match line {
                Some(l) if wall - quiet_since >= 1.0 => {
                    said.push(l);
                    wall
                }
                _ => quiet_since,
            };
            self.seen_rows.insert(
                r.id,
                RowSeen { hz, mode: r.mode, drawn: r.drawn, held: r.held, at: wall, said_at },
            );
        }
        // A row is remembered for a while after it goes, so that one which
        // comes back is known to be the same row; past that it goes with it.
        self.seen_rows.retain(|_, s| wall - s.at <= ROW_MEMORY_S);
        said
    }

    /// The rig's frequency, as a rig shows it.
    ///
    /// Amber on black and half again the size of everything else, because it is
    /// read from across the room rather than from the mouse — and because a
    /// frequency in body text among conversations reads as another line of
    /// conversation.
    ///
    /// What it shows is what the *rig* reports, never what this app last asked
    /// for. A hand on the knob moves it; a tune this app sends does not, until
    /// the rig confirms it. Scrolling over it tunes, in steps that follow the
    /// modifier: a hundred hertz to place a signal, ten to sit exactly on one,
    /// a kilohertz to go looking.
    fn vfo(&mut self, ui: &mut egui::Ui) {
        if self.ptt.is_none() {
            return;
        }
        let st = self.ptt_state();

        let wall = unix_now();
        let digit_h = self.text_px * VFO_SCALE;
        let arrow_h = (self.text_px * 0.5).max(7.0);
        let height = digit_h + 2.0 * (arrow_h + 3.0) + 8.0;
        // The key and the dial on one row, the same height, because they are
        // one instrument: the two things an operator's hand goes to, with the
        // reply box directly beneath them.
        // An id of its own, rather than the one the layout would generate: a
        // generated id is a function of how many widgets came before, so it
        // moves when anything above it does — and focus, which is held by id,
        // goes with it. The dial held the keyboard for exactly one keystroke
        // before this.
        let dial_id = ui.id().with("vfo dial");
        let row = ui.horizontal(|ui| {
            self.tx_button(ui, height);
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), height),
                egui::Sense::hover(),
            );
            let resp = ui.interact(rect, dial_id, egui::Sense::click_and_drag());
            (rect, resp)
        });
        let (rect, resp) = row.inner;
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, VFO_BLACK);

        // While the wheel is turning the digits follow the hand; once it stops
        // they are the rig's again. Anything else means scrolling a dial whose
        // digits lag a second behind the wheel.
        let shown = match self.tuning {
            Some(t) if wall - t.at < TUNING_SETTLES_S || !t.sent => Some(t.hz),
            _ => st.dial_hz,
        };
        let reading = match shown {
            Some(hz) => dial_digits(hz),
            // Linked and silent about it, or not linked at all: either way the
            // dial is unknown, and dashes say that where a zero would lie.
            None => "---.---.---".to_string(),
        };
        let dim = shown.is_none();
        // Typed and not yet entered is drawn cooler than a frequency the radio
        // is actually on — the difference between what the dial says and what
        // the rig is doing has to be visible while it exists.
        let pending = self.tuning.is_some_and(|t| !t.sent);
        let amber = match (dim, pending) {
            (true, _) => VFO_AMBER.gamma_multiply(0.35),
            (_, true) => VFO_PENDING,
            _ => VFO_AMBER,
        };

        // Laid out first, drawn after: every digit needs its own rectangle, and
        // the only honest source for where a digit sits is the galley that
        // draws it.
        let font = FontId::monospace(digit_h);
        // Leading zeros dim: they are places to tune, not figures to read, and
        // a dial that shows 007.070.000 as brightly as the rest is one an eye
        // has to parse rather than glance at.
        let mut job = egui::text::LayoutJob::default();
        let blanks = if dim { 0 } else { leading_blanks(&reading) };
        job.append(
            &reading[..blanks],
            0.0,
            egui::TextFormat { font_id: font.clone(), color: amber.gamma_multiply(0.3), ..Default::default() },
        );
        job.append(
            &reading[blanks..],
            0.0,
            egui::TextFormat { font_id: font, color: amber, ..Default::default() },
        );
        let galley = painter.layout_job(job);
        let origin = egui::pos2(rect.right() - 10.0 - galley.size().x, rect.center().y - digit_h / 2.0);
        painter.galley(origin, galley.clone(), amber);

        // The mode, small and hard against the left, the way a rig labels its
        // own dial — and a control: point at it and the radio's modes drop out
        // of it, click one and the radio changes.
        let mode_font = FontId::monospace(self.text_px * 0.85);
        let showing = st.dial_mode.clone().unwrap_or_default();
        let mode_at = egui::pos2(rect.left() + 10.0, rect.center().y);
        let mode_box = painter.text(
            mode_at,
            egui::Align2::LEFT_CENTER,
            &showing,
            mode_font.clone(),
            VFO_AMBER.gamma_multiply(0.75),
        );
        let choices = rig::modes_for(&self.rig);
        if !choices.is_empty() && !showing.is_empty() {
            // A little room around the word, so pointing at it is not a matter
            // of hitting eight characters exactly.
            let hot = mode_box.expand2(egui::vec2(4.0, 3.0));
            // Where the pointer is, by geometry rather than by asking egui
            // whether this is hovered. Hover belongs to the topmost layer, and
            // a tooltip is a layer: with one open over the list, neither the
            // word nor the list is hovered, the list closes, and reaching for
            // an option dismisses it.
            let at = ui.input(|i| i.pointer.interact_pos());
            let over_word = at.is_some_and(|p| hot.contains(p));
            // Kept open while the pointer is on the word *or* in the list.
            // Without the second half, reaching for a mode dismisses the list
            // on the way — the same trap the digit arrows fell into.
            let open = over_word || self.mode_menu;
            self.mode_menu = false;
            if open {
                painter.rect_filled(hot, 2.0, VFO_AMBER.gamma_multiply(0.16));
                let area = egui::Area::new(dial_id.with("modes"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(egui::pos2(hot.left(), hot.bottom() + 1.0))
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).fill(VFO_BLACK).show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            for name in choices {
                                let here = showing == *name;
                                let label = egui::RichText::new(*name)
                                    .font(mode_font.clone())
                                    .color(if here { VFO_AMBER } else { VFO_AMBER.gamma_multiply(0.7) });
                                if ui.selectable_label(here, label).clicked() {
                                    if let Some(link) = &self.ptt {
                                        link.set_mode(name);
                                    }
                                }
                            }
                        });
                    });
                // Same again: the pointer inside the list's own rectangle,
                // not egui's opinion about who is hovered.
                let over_list = at.is_some_and(|p| area.response.rect.contains(p));
                self.mode_menu = over_list || over_word;
            }
        }

        // Every digit is its own control, worth its own place value: the one
        // under the pointer is the one that moves, which is how a rig with a
        // knob per decade behaves and needs no modifier to say which decade is
        // meant. Counted from the right, skipping the separators.
        let mut places: Vec<(egui::Rect, f64)> = Vec::new();
        if let Some(row) = galley.rows.first() {
            let mut power = 0i32;
            for glyph in row.glyphs.iter().rev() {
                if !glyph.chr.is_ascii_digit() && glyph.chr != '-' {
                    continue;
                }
                let at = origin + glyph.pos.to_vec2();
                let box_ = egui::Rect::from_min_size(
                    egui::pos2(at.x, rect.center().y - digit_h / 2.0),
                    egui::vec2(glyph.advance_width, digit_h),
                );
                places.push((box_, 10f64.powi(power)));
                power += 1;
            }
        }

        // What the wheel did this frame, as the window received it rather than
        // as egui interpreted it: the processed delta has had ctrl+wheel taken
        // out of it to become `zoom_delta` and shift+wheel moved onto the x
        // axis, so a dial reading that sees nothing for either.
        let scroll = ui.input(|i| {
            let mut turned = 0.0f32;
            for event in &i.events {
                if let egui::Event::MouseWheel { unit, delta, .. } = event {
                    let d = if delta.y != 0.0 { delta.y } else { delta.x };
                    turned += match unit {
                        egui::MouseWheelUnit::Line => d * SCROLL_NOTCH,
                        egui::MouseWheelUnit::Page => d * SCROLL_NOTCH * 10.0,
                        egui::MouseWheelUnit::Point => d,
                    };
                }
            }
            turned
        });

        let live = st.dial_hz.is_some();
        self.dial_pick = self.dial_pick.min(places.len().saturating_sub(1));
        let pointer = ui.input(|i| i.pointer.interact_pos());
        let focus = dial_id;

        // Where the pointer is, if it is on a digit. The mouse and the keys
        // both move one selection, and the last one used owns it: hover takes
        // it back the moment the pointer actually moves, and until then the
        // keys keep it. Two indicators — a hover highlight and a separate caret
        // — would be two answers to "which digit will move", which is one too
        // many.
        let under_pointer =
            pointer.and_then(|p| places.iter().position(|(box_, _)| box_.contains(p)));
        let pointer_moved = self.dial_by_key.is_none_or(|was| pointer.is_none_or(|p| p != was));
        if let (Some(i), true) = (under_pointer, pointer_moved) {
            self.dial_pick = i;
            self.dial_by_key = None;
        }

        // Where the arrows for the selected digit are. Known before the click
        // is interpreted, because a click on one of them is a step and must not
        // also be read as pointing the dial somewhere: an arrow is not over a
        // digit, so the rule below would have sent the cursor to the far end
        // and taken the arrow out from under the mouse in the same frame.
        let arrows = places.get(self.dial_pick).map(|(box_, _)| {
            (
                egui::Rect::from_min_size(
                    egui::pos2(box_.left(), box_.top() - arrow_h - 3.0),
                    egui::vec2(box_.width(), arrow_h),
                ),
                egui::Rect::from_min_size(
                    egui::pos2(box_.left(), box_.bottom() + 3.0),
                    egui::vec2(box_.width(), arrow_h),
                ),
            )
        });
        let on_arrow = pointer.zip(arrows).is_some_and(|(p, (up, down))| {
            up.contains(p) || down.contains(p)
        });

        // A click anywhere else on the dial takes the keys, and puts the
        // selection where it was clicked — on the leftmost digit if that was
        // not on one, which is where an operator typing a whole frequency
        // starts.
        if resp.clicked() {
            self.dial_typing = true;
            if !on_arrow {
                self.dial_pick = under_pointer.unwrap_or(places.len().saturating_sub(1));
                self.dial_by_key = None;
            }
        }
        // A text field taking the keyboard always wins: the reply box is
        // directly beneath this and an operator typing into it must not have
        // their digits stolen by a dial they happened to click earlier.
        if ui.memory(|m| m.focused().is_some()) {
            self.dial_typing = false;
        }
        let typing = self.dial_typing;
        // And a click anywhere but the dial hands the keyboard back, which is
        // what clicking away from anything else in a window does.
        if self.dial_typing && ui.input(|i| i.pointer.any_click()) && !resp.clicked() {
            self.dial_typing = false;
        }
        // A number typed and not entered is not a frequency anybody asked for.
        // Looking away is not confirmation, so it goes back to the rig's own.
        if !typing && self.tuning.is_some_and(|t| !t.sent) {
            self.tuning = None;
        }

        let mut moved: Option<f64> = None;
        let mut typed: Option<f64> = None;

        // The keys, when the dial has them. The wheel and the arrows are aimed
        // by the pointer and need no focus; the keyboard is aimed by the
        // selection and must have it, or the dial would take keystrokes meant
        // for the reply box directly beneath it.
        if typing && live {
            // Taken with `consume_key`, not read: egui moves focus between
            // widgets on the arrow keys, so a dial that only *looks* at them
            // gets one keystroke and then loses the keyboard to whatever is
            // next in the panel. Consuming them settles who they belong to.
            let (left, right, up, down, enter, escape) = ui.input_mut(|i| {
                let m = egui::Modifiers::NONE;
                (
                    i.consume_key(m, egui::Key::ArrowLeft),
                    i.consume_key(m, egui::Key::ArrowRight),
                    i.consume_key(m, egui::Key::ArrowUp),
                    i.consume_key(m, egui::Key::ArrowDown),
                    i.consume_key(m, egui::Key::Enter),
                    i.consume_key(m, egui::Key::Escape),
                )
            });
            if left {
                self.dial_pick = (self.dial_pick + 1).min(places.len() - 1);
                self.dial_by_key = pointer;
            }
            if right {
                self.dial_pick = self.dial_pick.saturating_sub(1);
                self.dial_by_key = pointer;
            }
            let place = places.get(self.dial_pick).map(|(_, p)| *p).unwrap_or(1.0);
            if up {
                moved = Some(place);
            }
            if down {
                moved = Some(-place);
            }
            // What sends a typed frequency. Nothing else does.
            if enter {
                if let Some(t) = self.tuning.filter(|t| !t.sent) {
                    if let Some(link) = &self.ptt {
                        link.tune(t.hz);
                    }
                    self.tuning = Some(Tuning { sent: true, at: wall, ..t });
                }
            }
            // Put it back to whatever the radio actually says.
            if escape {
                self.tuning = None;
                self.dial_typing = false;
            }

            // Digits from the text events rather than the key codes, so that a
            // numeric keypad counts. The column is read again for each one:
            // several can arrive in a single event — a paste, or a keyboard
            // faster than a frame — and each belongs one place along from the
            // last.
            let texts: Vec<String> = ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::Text(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect()
            });
            for text in texts {
                for c in text.chars().filter_map(|c| c.to_digit(10)) {
                    let place = places.get(self.dial_pick).map(|(_, p)| *p).unwrap_or(1.0);
                    let base = typed.unwrap_or_else(|| self.tuning_from(st.dial_hz, wall));
                    typed = Some(with_digit(base, place, c));
                    // Along to the next column, the way a keypad fills.
                    self.dial_pick = self.dial_pick.saturating_sub(1);
                    self.dial_by_key = pointer;
                }
            }
        }

        // The digit the dial is pointed at, and the arrows that step it. They
        // are the cursor: there is nothing else on screen saying which digit is
        // live, and nothing else needed.
        // `on_arrow` is in there because the arrows are widgets of their own,
        // and egui gives hover to the topmost: a pointer resting on an arrow
        // leaves the dial underneath unhovered, and without this the arrows
        // vanished from under the mouse that had reached for them.
        if live && (typing || under_pointer.is_some() || on_arrow || resp.hovered()) {
            if let (Some((box_, place)), Some((up, down))) =
                (places.get(self.dial_pick).copied(), arrows)
            {
                let up_hit = ui.interact(up, focus.with("up"), egui::Sense::click());
                let down_hit = ui.interact(down, focus.with("down"), egui::Sense::click());
                painter.rect_filled(box_, 2.0, amber.gamma_multiply(if typing { 0.28 } else { 0.16 }));
                arrow(&painter, up, true, VFO_AMBER.gamma_multiply(if up_hit.hovered() { 1.0 } else { 0.6 }));
                arrow(&painter, down, false, VFO_AMBER.gamma_multiply(if down_hit.hovered() { 1.0 } else { 0.6 }));
                if up_hit.clicked() {
                    moved = Some(place);
                } else if down_hit.clicked() {
                    moved = Some(-place);
                } else if under_pointer == Some(self.dial_pick) && scroll != 0.0 {
                    moved = Some(place * notches(scroll));
                }
            }
        }

        if live {
            // Over the dial but not over a digit: a hundred hertz a notch, the
            // step somebody sweeping for a signal wants.
            if moved.is_none() && under_pointer.is_none() && resp.hovered() && scroll != 0.0 {
                moved = Some(100.0 * notches(scroll));
            }
            // A step goes out as it is made; a typed digit waits for the rest
            // of the number.
            // A step of the wheel or an arrow takes over: whatever was typed
            // and not yet entered is consolidated with it and the lot goes to
            // the rig. The mouse is a complete intent by itself, so nothing
            // about it should wait for a keystroke.
            if let Some(moved) = moved {
                let hz = (self.tuning_from(st.dial_hz, wall) + moved).max(0.0);
                self.tuning = Some(Tuning { hz, at: wall, sent: true });
                if let Some(link) = &self.ptt {
                    link.tune(hz);
                }
            } else if let Some(hz) = typed {
                self.tuning = Some(Tuning { hz, at: wall, sent: false });
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
        }

        // Power and filter under the dial, and only when the rig reports them:
        // a station running a hundred watts into a wire wants the first at
        // hand, and the second decides how much of this app's panorama is
        // spectrum the radio can actually hear.
        // The settings are behind a handle rather than always on show. They are
        // set when a band changes and left alone for an hour after, and a
        // panel whose top is mostly controls nobody is touching is a panel with
        // less conversation in it.
        let has_settings = live && (st.power_w.is_some() || st.width_hz.is_some());
        if has_settings {
            let handle_h = (self.text_px * 0.9).max(12.0);
            let (strip, grip) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), handle_h),
                egui::Sense::click(),
            );
            if grip.clicked() {
                self.rig_more = !self.rig_more;
            }
            let lit = if grip.hovered() { 1.0 } else { 0.6 };
            // Pointing the way the panel would grow, which is the only thing a
            // reader needs it to say.
            // Big enough to be a target and to be read as a direction at a
            // glance; it is the only thing on the strip.
            let mark = egui::Rect::from_center_size(
                strip.center(),
                egui::vec2(handle_h * 2.2, handle_h * 0.8),
            );
            arrow(&ui.painter_at(strip), mark, self.rig_more, VFO_AMBER.gamma_multiply(lit));
            grip.on_hover_text(if self.rig_more {
                "hide the rig's settings"
            } else {
                "the rig's settings: power, and the receiver's bandwidth"
            });
        }
        if has_settings && self.rig_more {
            let mut set_power: Option<f64> = None;
            let mut set_width: Option<f64> = None;
            // A label column and a value column, the same grid the QSO panel
            // uses for its fields — so the two boxes start at the same x
            // whichever of them the rig reports, and the whole thing reads as
            // one form rather than two controls that happened to land in a row.
            //
            // The unit lives on the value, not in the label. `W` standing where
            // `Power` belongs made the row read as a unit and a name side by
            // side, which is two different kinds of word doing one job.
            //
            // A third column carries a slider, but only where the rig has said
            // what it will accept — see [`rig::Transmitter::power_range_w`]. A
            // track whose ends were invented here would be a control that looks
            // like it knows the radio and does not.
            egui::Grid::new("rig_settings").num_columns(3).spacing([8.0, 4.0]).show(ui, |ui| {
                if let Some(w) = st.power_w {
                    let mut watts = w;
                    // Where the rig has not said, the box keeps a clamp wide
                    // enough that nothing which used to be typeable stops
                    // being. It guards against a nonsense drag; it is not a
                    // claim about the radio, which is why no track is drawn
                    // over it.
                    let known = st.power_range_w;
                    let (lo, hi) = known.map_or(ANY_POWER_W, |r| (r.lo, r.hi));
                    ui.label("Power");
                    if ui
                        .add(power_box(&mut watts, lo, hi))
                        .on_hover_text(hint("transmit power", known.is_some()))
                        .changed()
                    {
                        set_power = Some(watts);
                    }
                    if known.is_some() && track(ui, &mut watts, lo, hi) {
                        set_power = Some(watts);
                    }
                    ui.end_row();
                }
                if let Some(hz) = st.width_hz {
                    let mut khz = hz / 1000.0;
                    let known = st.width_range_hz;
                    let (lo, hi) =
                        known.map_or(ANY_BANDWIDTH_KHZ, |r| (r.lo / 1000.0, r.hi / 1000.0));
                    ui.label("Bandwidth");
                    if ui
                        .add(bandwidth_box(&mut khz, lo, hi))
                        .on_hover_text(hint(
                            "how wide the receiver's filter is. Anything outside it is shaded \
                             on the waterfall: the app draws the band, the radio decides how \
                             much of it you can hear. Taken as centred on 1500 Hz, which is \
                             what a rig's data modes use — no rig reports the centre in a way \
                             this can rely on.",
                            known.is_some(),
                        ))
                        .changed()
                    {
                        set_width = Some(khz * 1000.0);
                    }
                    if known.is_some() && track(ui, &mut khz, lo, hi) {
                        set_width = Some(khz * 1000.0);
                    }
                    ui.end_row();
                }
            });
            if let Some(link) = &self.ptt {
                if let Some(w) = set_power {
                    link.set_power_w(w);
                }
                if let Some(hz) = set_width {
                    link.set_width_hz(hz);
                }
            }
        }

        // No tooltip while the modes are showing. It is help about the digits,
        // it is drawn in a layer above everything here, and it would sit over
        // the list it has nothing to do with — hiding the options and taking
        // the hover that keeps the list open.
        if self.mode_menu {
            return;
        }
        let hover = match (st.dial_hz, st.fault.as_deref()) {
            (_, Some(fault)) => fault.to_string(),
            (Some(_), None) => "what the rig is tuned to. Scroll or click the arrows over a \
                                digit to step it, and the rig follows at once. Click to type: \
                                ←→ to move, digits to fill, Enter to send it, Esc to put it \
                                back. 100 Hz a click off the digits."
                .to_string(),
            (None, None) => "the rig does not report its frequency".to_string(),
        };
        resp.on_hover_text(hover);
    }

    /// Where the next wheel click should count from: what the hand has already
    /// asked for, until it has stopped long enough for the rig to answer.
    fn tuning_from(&self, rig_says: Option<f64>, wall: f64) -> f64 {
        match self.tuning {
            Some(t) if wall - t.at < TUNING_SETTLES_S || !t.sent => t.hz,
            _ => rig_says.unwrap_or_default(),
        }
    }

    /// The manual key, and the only place the window says it is transmitting.
    ///
    /// It shows three states, and the third is the reason the rig is asked
    /// rather than assumed: keyed because we keyed it, keyed by somebody else —
    /// a hand on the front panel, a foot switch, a line stuck down — and not
    /// keyed. The middle one is worth its own colour, because a rig
    /// transmitting for a reason this app does not know about is the one a
    /// reader most needs to be told about and the one it cannot fix by
    /// releasing a button.
    fn tx_button(&mut self, ui: &mut egui::Ui, height: f32) {
        let st = self.ptt_state();
        let ours = st.asked;
        let theirs = st.rig_says == Some(true) && !ours;
        let live = ours || theirs;

        let label = match (live, st.keyed_for) {
            (false, _) => "TX".to_string(),
            // The count is why a key-down left by accident does not last: it is
            // the thing the eye lands on, long before the watchdog cares.
            (true, t) if t >= 1.0 => format!("TX {t:.0}s"),
            (true, _) => "TX".to_string(),
        };
        let colour = match (live, theirs) {
            (false, _) => None,
            (true, false) => Some(legible(ui.visuals().dark_mode, TX_RED)),
            (true, true) => Some(legible(ui.visuals().dark_mode, TX_ELSEWHERE)),
        };
        // Wide enough for the longest thing it says — "TX 120s" — so that a
        // key-down does not resize the control under the hand about to press
        // it again.
        let mut button = egui::Button::new(label).min_size(egui::vec2(66.0, height));
        if let Some(c) = colour {
            button = button.fill(c.gamma_multiply(0.35)).stroke(Stroke::new(1.0, c));
        }

        let hover = match (&self.rig, st.fault.as_deref(), theirs) {
            (rig::Keying::None, _, _) => {
                "nothing is keying the rig — ☰ ▸ Rig, or leave it to VOX".to_string()
            }
            (_, Some(fault), _) => fault.to_string(),
            (_, None, true) => {
                "the rig is transmitting, and not because this asked it to — click to stop it"
                    .to_string()
            }
            (_, None, false) if ours => "transmitting — click to stop".to_string(),
            _ => "key the transmitter".to_string(),
        };
        let resp = ui.add_enabled(self.ptt.is_some(), button).on_hover_text(hover);
        if resp.clicked() {
            if let Some(ptt) = &self.ptt {
                // Against whether the rig is transmitting *at all*, not against
                // whether this app is what keyed it.
                //
                // The distinction cost a real transmission. Toggling against
                // our own intent meant that a rig already on the air for any
                // other reason — and the classic one is hamlib raising DTR as
                // it opens the serial port, which keys an interface wired that
                // way before this app has said a word — read as "not keyed by
                // us", so every press sent `T 1` and keyed it harder. A control
                // whose whole purpose is to stop a transmitter must stop it
                // whoever started it. There is no state in which pressing this
                // puts a rig on the air that is already on it.
                ptt.key(!live);
            }
        }
        // A transmitting rig has to keep being drawn, or the count stops and
        // the colour is whatever the last event left behind.
        if live {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        }
    }

    /// Change how the transmitter is keyed, and stand up a supervisor for it.
    ///
    /// The old supervisor is dropped first and its drop waits, so the rig is
    /// off the air before anything else can key it. Switching to
    /// [`rig::Keying::None`] leaves no thread at all: the app is back to what it
    /// did before it could key anything.
    /// The setting a [`Keyer`] tag names, filled in from the addresses this app
    /// is already holding.
    ///
    /// A tag this build does not know is not keying at all. That covers a
    /// settings file from a build compiled with a radio this one was not, and
    /// the conservative answer is the one that matters: the alternative is a
    /// build deciding for itself which rig an unrecognised word meant.
    fn keying_for(&self, tag: &str) -> rig::Keying {
        match tag {
            "rigctld" => rig::Keying::RigCtld {
                host: self.rig_host.clone(),
                port: self.rig_port,
            },
            #[cfg(feature = "elecraft")]
            "elecraft" => rig::Keying::Elecraft {
                device: self.rig_device.clone(),
                baud: self.rig_baud,
            },
            #[cfg(feature = "yaesu")]
            "yaesu" => rig::Keying::Yaesu {
                device: self.rig_device.clone(),
                baud: self.rig_baud,
            },
            _ => rig::Keying::None,
        }
    }

    fn set_keying(&mut self, keying: rig::Keying) {
        if self.rig == keying && (self.ptt.is_some() == (keying != rig::Keying::None)) {
            return;
        }
        self.ptt = None;
        self.rig = keying;
        if self.rig != rig::Keying::None {
            self.ptt = Some(rig::Ptt::start(&self.rig, rig::Limits::default()));
            diag_info!("rig", "keying through {}", rig::open(&self.rig).describe());
        }
    }

    /// Whether the rig is in a mode where an audio passband means anything —
    /// the same four that make an audio offset a frequency.
    fn rig_passband_mode(&self) -> bool {
        matches!(
            self.ptt_state().dial_mode.as_deref(),
            Some("USB" | "LSB" | "PKTUSB" | "PKTLSB" | "DATA" | "DATA-REV")
        )
    }

    /// What the transmitter is doing, as far as anything can tell.
    fn ptt_state(&self) -> rig::State {
        self.ptt.as_ref().map(|p| p.state()).unwrap_or_default()
    }

    /// The modes currently being listened for.
    fn enabled_modes(&self) -> Vec<ModeId> {
        self.modes.iter().filter(|(_, on)| *on).map(|(m, _)| *m).collect()
    }

    /// Switch to live capture from the default input device.
    fn go_live(&mut self) {
        self.samples = Arc::new(Vec::new());
        self.spec = None;
        self.frames = Vec::new();
        self.frames_dirty = true;
        self.pending_scans = 0;
        self.status = None;
        self.source = "live input".to_string();
        self.live = Some(LiveMode::start(
            self.band,
            self.enabled_modes(),
            self.preferred_input.clone(),
            self.show_all_inputs,
            self.record,
        ));
    }

    /// Load an audio file, replacing whatever is on screen.
    ///
    /// A file at the wrong sample rate is refused rather than decoded into
    /// nonsense: every mode here is defined in terms of symbol durations, so
    /// the wrong rate is the wrong protocol.
    fn open_file(&mut self, path: &Path) {
        let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().to_string();
        let Some(p) = path.to_str() else {
            self.warn(format!("{name}: path is not valid UTF-8"));
            return;
        };
        match wav::read(p) {
            Ok((samples, rate)) if rate == SAMPLE_RATE => self.set_source(samples, &name),
            Ok((_, rate)) => {
                self.warn(format!(
                    "{name} is {rate} Hz; ragchew needs {SAMPLE_RATE} Hz mono \
                     (ffmpeg -i in -ac 1 -ar {SAMPLE_RATE} out.wav)"
                ));
            }
            Err(e) => self.warn(format!("{name}: {e}")),
        }
    }

    /// Take a new block of audio as the thing being monitored.
    fn set_source(&mut self, samples: Vec<f32>, name: &str) {
        self.live = None;
        self.status = None;
        self.source = name.to_string();
        self.duration_s = samples.len() as f64 / SAMPLE_RATE as f64;
        self.samples = Arc::new(samples);
        self.spec = None;
        self.norm = (0.0, 1.0);
        self.tex = None;
        self.t_now = 0.0;
        self.playing = false;

        // The spectrogram is fast and the decode is not, so they go to the UI on
        // separate channels: the waterfall can be up while decoding runs.
        let (spec_tx, spec_rx) = channel();
        let samples = self.samples.clone();
        let band = self.band;
        thread::spawn(move || {
            let spec = Spectrogram::compute(&samples, band.0, band.1, WINDOW / 4);
            let norm = waterfall::percentiles(&spec);
            let _ = spec_tx.send((Arc::new(spec), norm));
        });
        self.spec_rx = spec_rx;
        self.rescan();
    }

    /// Take frames from a scan that has landed, and put the list back in the
    /// order the window reveals them in.
    ///
    /// The order matters more than it looks. The file view rebuilds its
    /// channels from every frame revealed so far, on every frame it draws, and
    /// a channel set is a function of the order it was fed: stations are
    /// matched to the nearest channel of their protocol as they arrive, and ids
    /// are handed out in the order channels are created. Feed the same decodes
    /// in a different order and text can land on a different station, and the
    /// station it lands on can have a different id.
    ///
    /// Scans land one mode at a time, in whatever order they finish, so without
    /// this the list is in no order at all — and "everything revealed so far"
    /// then grows by *insertion*, not by appending: a decode revealed at
    /// twenty seconds sits in the middle of the list, so the rebuild that
    /// follows it is a different rebuild all the way down. On screen that is a
    /// row losing the text it had and filling in again from nothing, which is
    /// what the demo band did every time a scan landed.
    ///
    /// Reveal order is the one to be in. It is the order the live path delivers
    /// decodes in, and the only one where what has been revealed grows by
    /// appending, so a rebuild agrees with the rebuild before it about
    /// everything both of them saw.
    fn frames_landed(&mut self, more: Vec<Frame>) {
        self.frames.extend(more);
        self.sort_frames();
    }

    fn sort_frames(&mut self) {
        self.frames_dirty = true;
        // Total, and on the decodes themselves rather than on how they arrived:
        // two runs of the same scan must not be able to disagree.
        self.frames.sort_by(|a, b| {
            a.reveal_s
                .partial_cmp(&b.reveal_s)
                .unwrap()
                .then(a.decode.time_s.partial_cmp(&b.decode.time_s).unwrap())
                .then(a.decode.hz.partial_cmp(&b.decode.hz).unwrap())
                .then_with(|| a.decode.mode.name().cmp(&b.decode.mode.name()))
        });
    }

    /// (Re)decode the loaded file with the currently enabled modes.
    ///
    /// Scanning a minute of band costs seconds per mode, which is far longer
    /// than the operator is willing to look at a blank pane — so the scan is
    /// split one job per mode and each job's text is shown the moment it lands,
    /// while playback runs off the (much faster) spectrogram. Any batch that
    /// arrives after the clock has already passed it simply appears in place.
    fn rescan(&mut self) {
        // The channels are about to be built again from nothing, and the ids
        // any clear names go with the old ones. The clock does not move here —
        // a mode toggled mid-playback rescans in place — so this is the one
        // rebuild `drop_stale_clears` cannot see.
        self.drop_clears();
        let (frames_tx, frames_rx) = channel();
        self.frames_rx = frames_rx;
        self.frames = Vec::new();
        self.frames_dirty = true;
        self.pending_scans = 0;
        if self.samples.is_empty() {
            return;
        }
        // One thread per mode: a mode's own threshold is what keeps a signal
        // out of its results, so the scans do not need each other to run, and
        // splitting them reports in about the time of the slowest single mode
        // instead of the sum of all of them.
        //
        // They do need each other to be *reconciled*, though, which is what
        // `arbitrate_scans` does once the last one is in. Where PSK31 and a
        // narrow Olivia mode both claim a signal the PSK decode has to go, and
        // neither thread can tell on its own: each sees only its own mode.
        let modes = self.enabled_modes();
        self.pending_scans = modes.len();
        for m in modes {
            let samples = self.samples.clone();
            let tx = frames_tx.clone();
            thread::spawn(move || {
                let frames: Vec<Frame> = protocol::decode_all(&samples, 300.0, 2600.0, &[m])
                    .into_iter()
                    .map(|d| Frame { reveal_s: d.time_s + d.mode.chunk_secs(), decode: d })
                    .collect();
                let _ = tx.send(frames);
            });
        }
    }

    /// Settle what the per-mode scans found between them.
    ///
    /// Run when the last scan lands, because the rule needs every mode's
    /// results in front of it: a PSK decode is only dropped on the evidence of
    /// an Olivia decode, and until Olivia's thread has reported there is no
    /// evidence to weigh. A phantom may therefore be on screen for as long as
    /// the scan takes, which is also how long the toolbar says it is scanning.
    fn arbitrate_scans(&mut self) {
        let found: Vec<protocol::Decode> = self.frames.iter().map(|f| f.decode.clone()).collect();
        let before = found.len();
        let kept = protocol::arbitrate(found);
        if kept.len() == before {
            return;
        }
        self.frames = kept
            .into_iter()
            .map(|d| Frame { reveal_s: d.time_s + d.mode.chunk_secs(), decode: d })
            .collect();
        self.sort_frames();
    }

    /// The mode legend, doubling as the on/off control for what is scanned.
    ///
    /// Each protocol gets a menu of its modes; the colour swatch next to each
    /// is the colour that mode's channels are drawn in. Returns true if the
    /// selection changed.
    fn modes_menu(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        for p in Protocol::ALL {
            let on = self.modes.iter().filter(|(m, e)| m.protocol() == p && *e).count();
            let total = self.modes.iter().filter(|(m, _)| m.protocol() == p).count();
            let label = format!("{} {on}/{total}", p.name());
            settings_menu(ui, label, |ui| {
                // Bulk controls only earn their place when there is more than
                // one mode to apply them to. PSK has a single one, and "all"
                // over a lone checkbox is a second way to do the same thing.
                if total > 1 {
                    let set_all = |modes: &mut Vec<(ModeId, bool)>, on: bool| {
                        for (_, e) in modes.iter_mut().filter(|(m, _)| m.protocol() == p) {
                            *e = on;
                        }
                    };
                    if ui.button("all").clicked() {
                        set_all(&mut self.modes, true);
                        changed = true;
                    }
                    if ui.button("none").clicked() {
                        set_all(&mut self.modes, false);
                        changed = true;
                    }
                    ui.separator();
                }
                for (m, enabled) in self.modes.iter_mut().filter(|(m, _)| m.protocol() == p) {
                    ui.horizontal(|ui| {
                        if ui.add(elegance::Checkbox::new(enabled, "")).changed() {
                            changed = true;
                        }
                        let c = mode_text_color(ui.visuals().dark_mode, *m);
                        ui.colored_label(c, m.short_name());
                    });
                }
            });
        }
        changed
    }

    /// The QSO panel: a tab per conversation, and for the one on top an info
    /// block, the log, a reply being composed, and the button that sends it.
    ///
    /// `now_s` is the audio clock the log's offsets are measured against — the
    /// playback clock for a file, capture time when live.
    fn qso_panel(&mut self, ui: &mut egui::Ui, now_s: f64) {
        // Tab strip. A conversation is opened by clicking a station in the
        // waterfall's text panel; the button is for one that has not been heard
        // yet — calling first, or logging a contact made elsewhere.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let mut want_active = self.qsos.active();
            let mut close: Option<usize> = None;
            for (i, q) in self.qsos.qsos().iter().enumerate() {
                let on = i == self.qsos.active();
                let c = mode_text_color(ui.visuals().dark_mode, q.mode);
                let (picked, closed) = Self::qso_tab(ui, &q.label(), c, on);
                if picked {
                    want_active = i;
                }
                if closed {
                    close = Some(i);
                }
            }
            ui.add_space(6.0);
            if ui
                .button("+")
                .on_hover_text(format!("new QSO in {}", self.default_mode.name()))
                .clicked()
            {
                let hz = (self.vp.f_lo + self.vp.f_hi) / 2.0;
                self.qsos.open_blank(hz, self.default_mode, now_s);
                want_active = self.qsos.len() - 1;
            }
            self.qsos.set_active(want_active);
            if let Some(i) = close {
                self.qsos.close(i);
            }
        });
        ui.separator();

        if self.qsos.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label("No QSOs.\n\nClick a station's text to start one.");
            });
            return;
        }

        // Everything below is the active QSO. The carrier and mode of a QSO
        // that is following a channel are that channel's, and are shown rather
        // than offered: they are measurements, not choices. A QSO opened by
        // hand has nothing to measure, so there they are the operator's to set.
        let modes = self.enabled_modes();
        let busy = self.tx.as_ref().map_or(0.0, |t| t.busy_for());
        // Read before the QSO is borrowed: with a rig on the other end, a
        // station's audio offset can be said as the frequency it really is.
        let rig = self.ptt_state();
        let Some(q) = self.qsos.active_qso_mut() else { return };
        let bound = q.channel.is_some();

        // Every widget below is scoped to this QSO's id, so each tab keeps its
        // own text cursor and its own scroll position in the log. Without it
        // egui identifies the widgets by where they sit in the layout, and all
        // the tabs — being the same layout — would share one set.
        let (send, abort) = ui.push_id(q.id, |ui| Self::qso_body(ui, q, &modes, bound, busy, now_s, rig.dial_hz, rig.dial_mode))
            .inner;

        if abort {
            self.abort_active();
        }
        if send {
            self.send_active(now_s);
        }
    }

    /// One tab in the QSO strip: a plate rounded at the top and square at the
    /// bottom, so the selected one reads as joined to the panel below it. The
    /// close cross appears only while the pointer is over the tab, but its
    /// space is always reserved — tabs must not resize under the cursor.
    ///
    /// The cross is drawn, not typed: egui's default fonts carry no glyph for
    /// any of the multiplication-sign codepoints an icon font would use.
    ///
    /// Returns whether the tab was picked, and whether its cross was clicked.
    fn qso_tab(ui: &mut egui::Ui, label: &str, color: Color32, selected: bool) -> (bool, bool) {
        let font = egui::TextStyle::Button.resolve(ui.style());
        let pad = ui.spacing().button_padding;
        let cross = 9.0; // side of the cross itself, inside its hit square
        let hit = cross + 5.0;
        // The colour goes into the layout, not into the paint call: a galley
        // laid out with a colour ignores the one passed when it is drawn.
        let text_color = if selected { color } else { color.gamma_multiply(0.7) };
        let galley = ui.painter().layout_no_wrap(label.to_owned(), font, text_color);
        let size = egui::vec2(
            galley.size().x + hit + pad.x * 3.0,
            galley.size().y.max(hit) + pad.y * 2.0,
        );
        let (rect, tab) = ui.allocate_exact_size(size, egui::Sense::click());

        // The cross is interacted with after the tab, so it sits on top of it:
        // a click there closes the QSO rather than selecting it.
        let close_rect =
            Rect::from_center_size(rect.right_center() - egui::vec2(pad.x + hit / 2.0, 0.0),
                egui::Vec2::splat(hit));
        let close = ui.interact(close_rect, tab.id.with("close"), egui::Sense::click());

        if ui.is_rect_visible(rect) {
            let v = ui.visuals();
            let radius = egui::CornerRadius { nw: 4, ne: 4, sw: 0, se: 0 };
            let fill = if selected {
                v.widgets.active.weak_bg_fill
            } else if tab.hovered() {
                v.widgets.hovered.weak_bg_fill
            } else {
                v.widgets.inactive.weak_bg_fill
            };
            let p = ui.painter();
            p.rect_filled(rect, radius, fill);
            p.rect_stroke(
                rect,
                radius,
                v.widgets.noninteractive.bg_stroke,
                egui::StrokeKind::Inside,
            );
            // The selected tab is marked along its top edge, where the mark
            // cannot be mistaken for the separator under the strip.
            if selected {
                let top = Rect::from_min_max(rect.left_top(), rect.right_top() + egui::vec2(0.0, 2.0));
                p.rect_filled(top, egui::CornerRadius { nw: 4, ne: 4, sw: 0, se: 0 }, v.selection.bg_fill);
            }

            let at = egui::pos2(rect.left() + pad.x, rect.center().y - galley.size().y / 2.0);
            p.galley(at, galley, text_color);

            // Hovering anywhere on the tab reveals the cross; hovering the
            // cross itself lights it and gives it a backing plate.
            if tab.hovered() || close.hovered() {
                if close.hovered() {
                    p.rect_filled(close_rect, 3, v.widgets.hovered.bg_fill);
                }
                let stroke = Stroke::new(
                    1.4,
                    if close.hovered() {
                        v.widgets.hovered.fg_stroke.color
                    } else {
                        v.weak_text_color()
                    },
                );
                let c = close_rect.center();
                let r = cross / 2.0;
                p.line_segment([c - egui::vec2(r, r), c + egui::vec2(r, r)], stroke);
                p.line_segment([c + egui::vec2(r, -r), c - egui::vec2(r, -r)], stroke);
            }
        }

        let close = close.on_hover_text("close this QSO");
        (tab.clicked() && !close.hovered(), close.clicked())
    }

    /// Text on the air, in the composer's place: what has gone out struck
    /// through and faded, what is still to go in full strength.
    ///
    /// Not a text box. The operator cannot edit what is already leaving the
    /// sound card, and offering a cursor would say otherwise — so it is set in
    /// the same frame and background the box uses, and simply cannot be typed
    /// into until the burst ends and the box comes back empty.
    fn outgoing_text(ui: &mut egui::Ui, queue: &[crate::qso::Outgoing]) {
        let now = unix_now();
        let font = egui::TextStyle::Body.resolve(ui.style());
        let faded = ui.visuals().weak_text_color();
        let sent = egui::TextFormat {
            font_id: font.clone(),
            color: faded,
            strikethrough: Stroke::new(1.0, faded),
            ..Default::default()
        };
        let to_go = egui::TextFormat {
            font_id: font,
            color: ui.visuals().strong_text_color(),
            ..Default::default()
        };
        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = ui.available_width();
        for out in queue {
            let n = out.sent_chars(now);
            let cut = out.text.char_indices().nth(n).map_or(out.text.len(), |(i, _)| i);
            let (gone, left) = out.text.split_at(cut);
            job.append(gone, 0.0, sent.clone());
            job.append(left, 0.0, to_go.clone());
        }

        let palette = elegance::Theme::current(ui.ctx()).palette;
        egui::Frame::new()
            .fill(palette.input_bg)
            .corner_radius(6)
            .inner_margin(8)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.add(egui::Label::new(job));
            });
    }

    /// One entry's text, with a dim bar wherever two packets were joined.
    ///
    /// A JS8 over is several frames a cycle apart, and the log now reads them
    /// as one paragraph — which is how a station means them and how it types
    /// them, but it does hide something real. Where the seams fall says how the
    /// message was carried: a short sentence split across four cycles took a
    /// minute of air time and was probably being repeated by its own software.
    /// So the seams stay visible, at the weight of punctuation rather than of
    /// text, and each carries the time its frame started.
    ///
    /// One galley for the lot, rather than a row of labels, so that the entry
    /// wraps as the paragraph it is: a run of side-by-side widgets would break
    /// at every seam instead of at the edge of the panel.
    fn entry_text(ui: &mut egui::Ui, e: &crate::qso::Entry, color: Color32, from_s: f64) {
        let font = egui::TextStyle::Monospace.resolve(ui.style());
        let body = egui::TextFormat { font_id: font.clone(), color, ..Default::default() };
        let seam = egui::TextFormat {
            font_id: font,
            // Half the weight of the text it separates: present when looked
            // for, invisible when read past.
            color: color.gamma_multiply(0.4),
            ..Default::default()
        };
        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = ui.available_width();
        let mut from = 0usize;
        for &(at, _) in &e.marks {
            let cut = e.text.char_indices().nth(at).map_or(e.text.len(), |(i, _)| i);
            let head = e.text.get(from..cut).unwrap_or("");
            job.append(head, 0.0, body.clone());
            job.append("│", 0.0, seam.clone());
            from = cut;
        }
        job.append(e.text.get(from..).unwrap_or(""), 0.0, body.clone());
        // An aborted transmission stays in the log, because part of it was on
        // the air, but the log must not go on claiming the whole line was sent.
        // The marker goes into the galley rather than into `e.text`, for the
        // same reason the seams do: what is copied out of this panel is the
        // message, not the interface's notes about it.
        if e.cut {
            job.append("  — cut short", 0.0, seam.clone());
        }

        // `.wrap()` as well as the job's own width: a label in a horizontal
        // layout extends by default, and that default overrides the width set
        // on the job rather than deferring to it. Without it a long over lays
        // itself out past the edge of the window.
        let resp = ui.add(egui::Label::new(job).wrap());
        // The times go on the hover rather than into the line, where they would
        // cost more width than the text they annotate.
        if !e.marks.is_empty() {
            // `clock` rather than `offset`, which pads for the log's column and
            // would set the times ragged inside a sentence.
            let times: Vec<String> = e.marks.iter().map(|&(_, t)| clock(t - from_s)).collect();
            resp.on_hover_text(format!(
                "{} frames — this one began at {}, the rest at {}",
                e.marks.len() + 1,
                clock(e.at_s - from_s),
                times.join(", ")
            ));
        }
    }

    /// The RX/TX marker on a log line: the tag set smaller than the body text
    /// and boxed in a thin outline, so it reads as a label on the line rather
    /// than as the first word of what was said. Colour alone did not separate
    /// them — a received line and its tag were the same mode colour.
    fn dir_badge(ui: &mut egui::Ui, tag: &str, color: Color32) {
        let body = egui::TextStyle::Monospace.resolve(ui.style());
        let font = FontId::new((body.size * 0.75).max(7.0), body.family.clone());
        let galley = ui.painter().layout_no_wrap(tag.to_owned(), font, color);
        let pad = egui::vec2(3.0, 1.0);
        let box_size = galley.size() + pad * 2.0;

        // The badge is shorter than the line it marks, and `horizontal_top`
        // aligns tops, so it is given a little headroom to sit against the
        // text's first line rather than above it.
        let line_h = ui.text_style_height(&egui::TextStyle::Monospace);
        let lead = ((line_h - box_size.y) / 2.0).clamp(0.0, 3.0);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(box_size.x, box_size.y + lead), egui::Sense::hover());
        if !ui.is_rect_visible(rect) {
            return;
        }
        let boxed = Rect::from_min_size(egui::pos2(rect.left(), rect.top() + lead), box_size);
        let p = ui.painter();
        p.rect_stroke(
            boxed,
            3,
            Stroke::new(1.0, color.gamma_multiply(0.55)),
            egui::StrokeKind::Inside,
        );
        p.galley(boxed.min + pad, galley, color);
    }

    /// The active QSO's info, log, reply box and send button. Returns whether
    /// the operator asked to send, and whether they asked to abort.
    #[allow(clippy::too_many_arguments)]
    fn qso_body(
        ui: &mut egui::Ui,
        q: &mut crate::qso::Qso,
        modes: &[ModeId],
        bound: bool,
        busy: f64,
        now_s: f64,
        // What the rig is tuned to and the sideband it is in, so a station's
        // audio offset can be said as the frequency it really is.
        dial: Option<f64>,
        dial_mode: Option<String>,
    ) -> (bool, bool) {
        let (mut send, mut abort) = (false, false);
        let dark = ui.visuals().dark_mode;
        let mut ready = false;
        // The composer is a panel of its own along the *top* edge, and the log
        // takes what is left, so growing the window grows the part worth
        // reading rather than the part being typed into.
        //
        // At the top because of where the hand is. The dial and the key sit
        // directly above this panel, and an operator working a station moves
        // between those and this box and nothing else — putting the box at the
        // foot of the panel put the length of the log between the three things
        // used together. Panels are what egui gives you for "this sticks to
        // that edge"; the alternative was reserving a guessed height for the
        // composer, which it did — 96 pixels, right up until the send row grew
        // a countdown and an abort button.
        egui::Panel::top("qso_composer").show(ui, |ui| {
            // The outgoing buffer is its own pane above the entry box, not a
            // stand-in for it: what is leaving stays visible, struck through as
            // far as it has got, while the next message is being typed. A
            // composer that disappeared while transmitting made the operator
            // wait out the burst before starting the reply.
            if !q.outgoing.is_empty() {
                Self::outgoing_text(ui, &q.outgoing);
            }
            let resp = ui.add(
                elegance::TextArea::new(&mut q.draft)
                    .rows(2)
                    .desired_width(f32::INFINITY)
                    .hint("reply…"),
            );
            if q.want_focus {
                resp.request_focus();
                q.want_focus = false;
            }
            // Neither protocol can carry a newline — JS8's free-text alphabet
            // is 44 characters and has none, and Olivia would put a bare
            // control code on the air — so Enter has nothing useful to insert
            // and means send instead. Any newline arriving by other means, a
            // paste, is dropped for the same reason rather than transmitted as
            // something odd.
            let entered = resp.has_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
            if q.draft.contains('\n') {
                q.draft.retain(|c| c != '\n');
            }
            ready = !q.draft.trim().is_empty();
            if entered && ready {
                send = true;
            }
            ui.horizontal(|ui| {
                // Green for the affirmative action, red for the one that stops
                // a transmission already going out: the accents carry the
                // meaning, so neither has to be read to be told apart.
                send |= ui
                    .add(
                        elegance::Button::new("Send")
                            .accent(elegance::Accent::Green)
                            .enabled(ready),
                    )
                    .on_hover_text(match (q.mode.period_s(), busy > 0.0) {
                        (_, true) => "Enter, or click. Queued behind what is going out.".to_string(),
                        (Some(p), _) => {
                            format!("Enter, or click. Goes out on the next {p} s cycle.")
                        }
                        (None, _) => "Enter, or click. Goes out immediately.".to_string(),
                    })
                    .clicked();
                if busy > 0.0 {
                    ui.add(elegance::Indicator::new(elegance::IndicatorState::Connecting));
                    let c = legible(ui.visuals().dark_mode, Color32::from_rgb(255, 190, 120));
                    ui.colored_label(c, format!("TX {busy:.0} s"));
                }
                // The button follows *this* QSO's traffic rather than the card
                // being busy. They are not the same thing with two
                // conversations open: the app can be transmitting for the other
                // tab, and an Abort offered here would have taken back a
                // message this operator was not looking at.
                if !q.outgoing.is_empty() {
                    let waiting = q.outgoing.iter().any(|o| o.start_unix > unix_now());
                    abort = ui
                        .add(
                            elegance::Button::new("Abort")
                                .accent(elegance::Accent::Red)
                                .size(elegance::ButtonSize::Small),
                        )
                        // Two presses rather than one, and deliberately: taking
                        // the queue back leaves the burst on the air to finish,
                        // which is what an operator fixing a typo wants and is
                        // not what an operator who needs the transmitter to
                        // stop wants. The second press is the second thing.
                        .on_hover_text(if waiting {
                            "take back what has not gone out yet — it returns to the box, \
                             to edit and send again. Press again to stop the burst that \
                             is on the air."
                        } else {
                            "stop transmitting: the burst on the air is cut short, and \
                             the log keeps only what a receiving station copied"
                        })
                        .clicked();
                }
            });
        });

        egui::Grid::new("qso_info").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            let field = |ui: &mut egui::Ui, label: &str, v: &mut String| {
                ui.label(label);
                ui.add(egui::TextEdit::singleline(v).desired_width(f32::INFINITY));
                ui.end_row();
            };
            field(ui, "Call", &mut q.call);
            field(ui, "Name", &mut q.name);
            field(ui, "QTH", &mut q.qth);
            field(ui, "Grid", &mut q.grid);
            ui.label("RST");
            ui.horizontal(|ui| {
                let sent = ui
                    .add(egui::TextEdit::singleline(&mut q.rst_sent).desired_width(44.0))
                    .on_hover_text(if q.rst_sent_auto {
                        "sent — kept at the measured SNR while a JS8 station is being \
                         copied; type here and it is yours from then on"
                    } else {
                        "sent"
                    });
                // Typing hands the field over for good: the log has to record
                // what the operator chose to send, not what was measured after
                // they chose it.
                if sent.changed() {
                    q.rst_sent_auto = false;
                }
                ui.label("/");
                ui.add(egui::TextEdit::singleline(&mut q.rst_rcvd).desired_width(44.0))
                    .on_hover_text("received — what the other station gave you, so nothing \
                                    here is filled in for you");
            });
            ui.end_row();

            ui.label("Signal");
            if bound {
                let snr = match q.snr_db {
                    Some(db) => format!("{db:+.0} dB"),
                    None => "— dB".to_string(),
                };
                // With a rig on the other end of a cable, the audio offset can
                // be said as the frequency it actually is: the dial plus or
                // minus it, depending on the sideband. That is the number an
                // operator writes in a log or says on the air, and until now
                // the app knew both halves of it and printed neither.
                let on_band = on_the_band(dial, dial_mode.as_deref(), q.hz)
                    .map(|hz| format!("{}  ", megahertz(hz)))
                    .unwrap_or_default();
                ui.label(format!(
                    "{on_band}{:.0} Hz  {:+.1} drift  {snr}  ×{:.1}",
                    q.hz,
                    q.drift_hz,
                    q.quality.max(0.0)
                ))
                .on_hover_text(
                    "where the station is on the band — the dial plus this audio offset, \
                     as the sideband has it — then the offset itself, how far it has \
                     wandered since first heard, the station's signal-to-noise in 2.5 kHz \
                     — the figure a signal report means — and the last decode's confidence \
                     in multiples of the mode's noise floor",
                );
            } else {
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut q.hz).speed(1.0).range(0.0..=4000.0))
                        .on_hover_text("carrier to transmit on");
                    ui.label("Hz");
                });
            }
            ui.end_row();

            // How that SNR has been behaving, under the number it is the
            // history of. A row of its own rather than the end of the one
            // above: those numbers already want more width than the narrowest
            // panel has, and the trace is worth more wide than it is squeezed.
            // The row appears only once there is a shape in it.
            let fade = sparkline::Snr::new(&q.snr_history, q.mode, now_s)
                .color(mode_text_color(ui.visuals().dark_mode, q.mode));
            if bound && fade.would_draw() {
                ui.label("");
                // Capped rather than simply taking the width: a widget that
                // eats whatever a grid cell offers can ratchet the column wider
                // every frame, since the offer is made from the last one.
                fade.size(ui.available_width().min(220.0), 18.0).show(ui);
                ui.end_row();
            }

            ui.label("Mode");
            if bound {
                let c = mode_text_color(ui.visuals().dark_mode, q.mode);
                ui.colored_label(c, q.mode.name());
            } else {
                egui::ComboBox::from_id_salt("qso_mode")
                    .selected_text(q.mode.name())
                    .show_ui(ui, |ui| {
                        for m in modes {
                            ui.selectable_value(&mut q.mode, *m, m.name());
                        }
                    });
            }
            ui.end_row();

            ui.label("Started");
            // The elapsed time rides along as a "+m:ss" offset rather than a
            // phrase, both to match the log's offset column and to keep the row
            // inside a panel narrow enough to sit beside the waterfall.
            ui.label(format!(
                "{}  +{}",
                utc_stamp(q.started_utc),
                clock(q.elapsed_s(now_s))
            ))
            .on_hover_text(format!(
                "started (UTC, as a contact is logged), and how long ago that was\n\
                 {} on the audio clock",
                clock(q.started_s),
            ));
            ui.end_row();
        });

        ui.separator();


        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    for e in &q.log {
                        let (color, tag) = match e.dir {
                            Dir::Rx => (mode_text_color(dark, q.mode), Dir::Rx.tag()),
                            // Our own text takes the theme's own strong colour:
                            // it is us talking, not a station with a mode
                            // colour.
                            Dir::Tx => (ui.visuals().strong_text_color(), Dir::Tx.tag()),
                        };
                        ui.horizontal_top(|ui| {
                            ui.monospace(offset(e.at_s - q.started_s));
                            Self::dir_badge(ui, tag, color);
                            Self::entry_text(ui, e, color, q.started_s);
                        });
                    }
                });
        });

        (send, abort)
    }

    /// Modulate the active QSO's draft and put it on the air.
    ///
    /// The output device is opened here rather than at startup, so the failure
    /// — no device, or one that will not take the format — arrives attached to
    /// the thing the operator just asked for instead of as a mystery at launch.
    fn send_active(&mut self, now_s: f64) {
        // Fail closed, and first. Audio into a rig that did not key is silence
        // at best, and on a station left on VOX it is a transmission nobody
        // asked this app to make, out of a path the operator believed was under
        // control.
        //
        // Ahead of everything else because it is the question of whether to
        // transmit at all: settling it here means no audio device is opened and
        // nothing is modulated for a transmission that was never going out, and
        // it means the fault the operator is shown is the one that stopped
        // them. Behind the audio open, a rig that is not answering reported
        // itself as whatever the sound card had to say.
        let keying = self.ptt.as_ref().map(|p| p.state());
        if let Some(st) = keying.filter(|st| !st.linked && st.fault.is_some()) {
            let why = st.fault.unwrap_or_default();
            self.warn(format!("not transmitting: {why}"));
            return;
        }
        if self.tx.is_none() {
            match Tx::start(self.preferred_output.as_deref()) {
                Ok(t) => self.tx = Some(t),
                Err(e) => {
                    self.warn(format!("cannot transmit: {e}"));
                    return;
                }
            }
        }
        let Some(q) = self.qsos.active_qso_mut() else { return };
        // Sent as typed. A leading or trailing space is not stray whitespace
        // in a character-stream mode: it is what keeps one transmission from
        // running into the next, and both protocols carry a space. Only a
        // draft with nothing but blanks in it is refused.
        let text = q.draft.clone();
        if text.trim().is_empty() {
            return;
        }
        let (hz, mode) = (q.hz, q.mode);
        // One burst per frame rather than one buffer with the silence between
        // frames baked into it. A JS8 over is several frames a cycle apart, and
        // queueing them separately is what makes the tail of an over abortable
        // — each frame can be taken off the queue on its own, and each knows
        // which characters it was carrying.
        let frames = tx::modulate_frames(&text, hz, mode);
        // Behind anything already going out, on that mode's next boundary.
        let busy_until = self.tx.as_ref().map_or(0.0, |t| unix_now() + t.busy_for());
        let at = tx::next_start_after(mode, busy_until);

        let Some(t) = self.tx.as_mut() else { return };
        // The queue first, so the log line and the outgoing pane can both be
        // written knowing which transmissions they are about. Taking a message
        // back later is a matter of finding all three by those ids.
        let mut queued: Vec<(crate::qso::Outgoing, usize)> = Vec::new();
        let mut from = 0usize;
        for f in frames {
            let begins = at + f.at_s;
            let secs = f.secs();
            let text = f.text;
            let (burst, end) = t.send(f.samples, begins);
            let starts_at = from;
            from += text.chars().count();
            queued.push((
                crate::qso::Outgoing {
                    start_unix: begins,
                    end_unix: end,
                    burst,
                    mode,
                    // One key-down per burst, never one across the whole
                    // message: the silence between JS8 frames is seconds long
                    // and a key held across it is a bare carrier for as long as
                    // the frames either side.
                    keys: vec![(
                        begins - tx::KEY_LEAD_S,
                        begins + secs + tx::KEY_TAIL_S,
                    )],
                    text,
                },
                starts_at,
            ));
        }

        if let Some(link) = &self.ptt {
            for (o, _) in &queued {
                for &(on, off) in &o.keys {
                    link.schedule(on, off);
                }
            }
        }
        if let Some(q) = self.qsos.active_qso_mut() {
            q.draft.clear();
            // The log line is the whole message, and knows which of its
            // characters each frame carries — so an over stopped part-way is
            // trimmed to exactly the frames that went.
            q.push_tx(&text, now_s, queued.iter().map(|(o, at)| (o.burst, *at)).collect());
            // The outgoing pane keeps them until the whole queue has gone,
            // striking each through as it leaves.
            q.outgoing.extend(queued.into_iter().map(|(o, _)| o));
        }
        self.status = None;
    }

    /// Take back what has not gone out, or stop what has.
    ///
    /// Two things on one button, and which one an operator means is never in
    /// doubt: while there is text still waiting for its cycle, "abort" means
    /// give it back — a message sent a word too early is the whole reason this
    /// button was asked for, and on JS8 the wait before a frame leaves is most
    /// of a cycle, which is plenty of time to notice. Only when there is
    /// nothing left to take back does it mean the other thing, which is stop
    /// transmitting, now.
    ///
    /// The first is surgical: this QSO's queued transmissions and nothing else.
    /// The second cannot be — there is one rig and one queue, so stopping the
    /// burst on the air while leaving another tab's message to fire behind it
    /// would be an emergency stop that did not stop the emergency.
    fn abort_active(&mut self) {
        let now = unix_now();
        let waiting: Vec<u64> = self
            .qsos
            .active_qso_mut()
            .map(|q| {
                q.outgoing.iter().filter(|o| o.start_unix > now).map(|o| o.burst).collect()
            })
            .unwrap_or_default();

        if waiting.is_empty() {
            self.stop_transmitting();
        } else {
            self.take_back(&waiting);
        }
        self.rekey();
    }

    /// Drop this QSO's queued transmissions that have not begun, and put their
    /// text back in the box.
    fn take_back(&mut self, ids: &[u64]) {
        let Some(t) = self.tx.as_mut() else { return };
        // Nothing of these went out, so nothing of them is kept back.
        let stopped: Vec<(u64, usize)> =
            t.cancel_pending(ids).into_iter().map(|id| (id, 0)).collect();
        let Some(q) = self.qsos.active_qso_mut() else { return };
        let back = q.take_back(&stopped);
        q.put_back(&back);
    }

    /// Stop the lot: the queue, and the burst already going out.
    ///
    /// Nothing is dropped silently, and nothing is claimed that did not happen.
    /// A transmission that never began goes back whole to the box of the QSO
    /// that wrote it — that tab, not whichever one is on top. The one that was
    /// on the air is divided: what [`tx::delivered_chars`] can vouch for stays
    /// in the log, flagged as cut short, and the rest goes back too.
    fn stop_transmitting(&mut self) {
        let Some(t) = self.tx.as_mut() else { return };
        let caught = t.abort();
        for q in self.qsos.qsos_mut() {
            let stopped: Vec<(u64, usize)> = q
                .outgoing
                .iter()
                .filter_map(|o| {
                    if caught.never_began.contains(&o.burst) {
                        return Some((o.burst, 0));
                    }
                    // A burst neither queued nor on the air has finished, and a
                    // finished transmission is not something to take back.
                    let (id, played) = caught.in_flight?;
                    (id == o.burst)
                        .then(|| (o.burst, tx::delivered_chars(&o.text, o.mode, played)))
                })
                .collect();
            let back = q.take_back(&stopped);
            q.put_back(&back);
        }
    }

    /// Put the key schedule back in step with what is actually still going out.
    ///
    /// [`rig::Ptt`] cancels all of its schedule or none of it, so dropping one
    /// transmission's spans means dropping every span and re-scheduling the
    /// ones still owed. Until this existed, nothing cancelled a key span at
    /// all: an aborted message left its key-down behind it and the rig held a
    /// bare carrier for the length of a transmission that was no longer being
    /// made — which is worse on the air than the message would have been.
    ///
    /// A span covering this instant is re-scheduled along with the rest, so a
    /// rig transmitting for another QSO takes a few milliseconds of unkey
    /// between the cancel and the schedule catching up. That is the price of an
    /// all-or-nothing cancel, and it is only paid on an abort.
    fn rekey(&mut self) {
        let Some(link) = &self.ptt else { return };
        link.cancel();
        let now = unix_now();
        for q in self.qsos.qsos() {
            for o in &q.outgoing {
                for &(on, off) in &o.keys {
                    if off > now {
                        link.schedule(on, off);
                    }
                }
            }
        }
    }

    /// The channels as they stand at [`App::t_now`], accumulated rather than
    /// rebuilt.
    ///
    /// It used to build a set from every decode revealed so far, on every frame
    /// drawn, and that cannot be made stable. A channel set is a function of
    /// the order it is fed — stations are matched to the nearest channel of
    /// their protocol as they arrive, and ids are handed out as channels are
    /// created — and [`ChannelSet::from_decodes`] feeds in *time* order while
    /// the window reveals in *reveal* order. Those are not the same order: a
    /// JS8 Slow frame is revealed thirty seconds after the cycle it was sent
    /// in, so it lands in the middle of the time-ordered sequence, and every
    /// rebuild after it disagrees with the one before all the way down. On
    /// screen that is a row losing its text and filling in again from nothing,
    /// and a station changing channel id under everything that holds one — the
    /// slide, a clear aimed at one row, an open conversation.
    ///
    /// So the set is kept and fed each newly revealed frame in reveal order,
    /// which is what the live path does with decodes as they are decoded. It is
    /// built afresh only when the past changes rather than the present: the
    /// clock going backwards, and a scan landing with frames dated before where
    /// the clock has already reached.
    fn channels_now(&mut self) -> ChannelSet {
        let revealed = self.frames.partition_point(|f| f.reveal_s <= self.t_now);
        if self.frames_dirty || revealed < self.fed_frames {
            self.file_set = ChannelSet::new(15.0);
            self.fed_frames = 0;
            self.frames_dirty = false;
        }
        for f in &self.frames[self.fed_frames..revealed] {
            self.file_set.add(f.decode.clone());
        }
        self.fed_frames = revealed;
        self.file_set.clone()
    }

    fn waterfall_texture(
        &mut self,
        ctx: &egui::Context,
        spec: &Spectrogram,
        size: [usize; 2],
        col_hi: i64,
        col_travel: i64,
        span_cols: usize,
    ) -> egui::TextureHandle {
        let [px_w, px_h] = size;
        // A waterfall gains one column at a time and drops one off the far end:
        // the picture is the same picture, moved. Redrawing it whole cost a
        // logarithm and a colour-map lookup for every one of half a million
        // pixels, 25 times a second, which was the interface's entire frame.
        //
        // So the image is kept and scrolled, and only the new columns drawn.
        // Everything else a pixel depends on — the view, the size, the span,
        // the contrast bounds — must be unchanged for that to be valid; when
        // any of it moves, the image is rendered afresh.
        let (per_px, tex_w) = texture_geometry(span_cols, px_w);
        let span_used = tex_w * per_px;
        let key = (
            tex_w,
            px_h,
            (self.vp.f_lo * 16.0) as i64,
            (self.vp.f_hi * 16.0) as i64,
            per_px,
            span_used as i64,
        );

        // The contrast bounds are re-estimated as the noise floor drifts. A
        // small move leaves the picture honest; a large one means every pixel
        // already drawn is on the wrong scale, so the image is redrawn.
        let scale = (self.wf_norm.1 - self.wf_norm.0).abs().max(1e-6);
        let norm_moved = (self.norm.0 - self.wf_norm.0).abs() > scale * 0.02
            || (self.norm.1 - self.wf_norm.1).abs() > scale * 0.02;

        // Saturating: before anything has been drawn `wf_col` is the floor of
        // the range, and a plain subtraction overflows.
        // Saturating: before anything has been drawn `wf_col` is the floor of
        // the range, and a plain subtraction overflows.
        let advanced = col_travel.saturating_sub(self.wf_col);
        let reusable = self.tex.is_some() && self.tex_key == key && !norm_moved;

        // Nothing new, or not yet a whole pixel of it: what is on screen is
        // still exactly what a redraw would produce. `wf_col` deliberately
        // stays put, so the columns keep accumulating towards the next pixel.
        if reusable && (advanced == 0 || advanced % per_px as i64 != 0) {
            return self.tex.clone().unwrap();
        }

        let shift = advanced / per_px.max(1) as i64;
        let scrolled = reusable
            && shift > 0
            && shift < tex_w as i64
            && self.wf_img.as_mut().is_some_and(|img| {
                waterfall::scroll_window(
                    img,
                    spec,
                    &self.vp,
                    col_hi,
                    span_used,
                    self.norm,
                    shift as usize,
                )
            });

        if !scrolled {
            self.wf_img = Some(waterfall::render_window(
                spec,
                &self.vp,
                tex_w,
                px_h.max(1),
                col_hi,
                span_used.max(1),
                Some(self.norm),
            ));
        }
        if let Some(img) = &self.wf_img {
            let color = egui::ColorImage::from_rgba_unmultiplied([img.width, img.height], &img.rgba);
            self.tex = Some(ctx.load_texture("waterfall", color, egui::TextureOptions::LINEAR));
        }
        self.tex_key = key;
        self.wf_col = col_travel;
        self.wf_norm = self.norm;
        self.tex.clone().unwrap()
    }

    /// Point the view at `lo..hi`, fitted to the band.
    ///
    /// Every restored view goes through here, so a settings file from a build
    /// with a different band — or one that has been edited by hand, or written
    /// by a crash mid-drag — cannot reopen the app somewhere it cannot be
    /// navigated out of: nothing wider than the band, nothing narrower than the
    /// zoom limit, nothing hanging off either end, and nothing inverted or
    /// non-finite.
    fn set_view(&mut self, lo: f64, hi: f64) {
        let full = self.band.1 - self.band.0;
        if !(lo.is_finite() && hi.is_finite() && hi > lo) {
            self.vp = Viewport { f_lo: self.band.0, f_hi: self.band.1 };
            return;
        }
        let span = (hi - lo).clamp(MIN_SPAN_HZ.min(full), full);
        let lo = lo.clamp(self.band.0, self.band.1 - span);
        self.vp = Viewport { f_lo: lo, f_hi: lo + span };
    }

    fn zoom(&mut self, focus_hz: f64, factor: f64) {
        let span =
            ((self.vp.f_hi - self.vp.f_lo) * factor).clamp(MIN_SPAN_HZ, self.band.1 - self.band.0);
        let t = (focus_hz - self.vp.f_lo) / (self.vp.f_hi - self.vp.f_lo);
        let mut lo = focus_hz - t * span;
        let mut hi = lo + span;
        if lo < self.band.0 {
            lo = self.band.0;
            hi = lo + span;
        }
        if hi > self.band.1 {
            hi = self.band.1;
            lo = hi - span;
        }
        self.vp = Viewport { f_lo: lo, f_hi: hi };
    }

    fn pan_hz(&mut self, dhz: f64) {
        let span = self.vp.f_hi - self.vp.f_lo;
        let lo = (self.vp.f_lo + dhz).clamp(self.band.0, self.band.1 - span);
        self.vp = Viewport { f_lo: lo, f_hi: lo + span };
    }

    /// Center the current viewport on `hz` (keeping the zoom span).
    fn center_on(&mut self, hz: f64) {
        let span = self.vp.f_hi - self.vp.f_lo;
        let lo = (hz - span / 2.0).clamp(self.band.0, self.band.1 - span);
        self.vp = Viewport { f_lo: lo, f_hi: lo + span };
    }
}

impl eframe::App for App {
    /// Called by eframe periodically and on exit.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.settings());
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

impl App {
    /// Put the widget palette into egui, every frame.
    ///
    /// The two dark/light presets are a matched pair — same shape, same
    /// typography — so switching between them moves no widget by a pixel. Auto
    /// takes whatever egui resolved the system preference to, which is also
    /// what [`legible`] reads, so the palette and the accents agree about
    /// which way round the window is. Installing an unchanged theme is a
    /// no-op, so this is cheap to do unconditionally.
    fn install_theme(&self, ctx: &egui::Context) {
        let dark = match self.theme {
            Theme::Dark => true,
            Theme::Light => false,
            // egui has already resolved the system preference into an
            // active dark/light theme; take that rather than asking the
            // platform again and risking the two disagreeing.
            Theme::Auto => ctx.theme() == egui::Theme::Dark,
        };
        if dark {
            elegance::Theme::slate().install(ctx);
        } else {
            elegance::Theme::frost().install(ctx);
        }
    }

    /// A frame of the whole interface.
    ///
    /// Split out of [`eframe::App::ui`] because nothing here needs the
    /// `eframe::Frame` — and an `eframe::Frame` cannot be built outside eframe,
    /// so a test that wanted to lay the app out headlessly could not call it.
    fn draw(&mut self, ui: &mut egui::Ui) {
        // eframe hands the app a `Ui` and panels nest inside it, so `ui` is
        // borrowed mutably for most of this function. A cloned handle to the
        // context — cheap, it is an `Arc` — keeps repaints, the cursor and
        // texture uploads reachable meanwhile.
        let ctx = &ui.ctx().clone();
        self.install_theme(ctx);
        // ---- LIVE: pull audio into the rolling spectrogram + channels ----
        let mut live = false;
        let mut live_spec: Option<Arc<Spectrogram>> = None;
        let mut live_channels: Option<ChannelSet> = None;
        let mut live_t = 0.0;
        let mut live_norm = self.norm;
        let mut live_label = String::new();
        let mut live_err = false;
        let mut live_cols_total = 0i64;
        let mut live_devices: Vec<crate::audio::AudioDevice> = Vec::new();
        // (path, seconds, bytes) of the recording in progress.
        let mut live_recording: Option<(String, f64, u64)> = None;
        let mut show_all = self.show_all_inputs;
        let mut all_inputs_toggled = false;
        let mut live_selected: Option<String> = None;
        if let Some(lm) = self.live.as_mut() {
            lm.tick();
            live = true;
            live_spec = Some(lm.spec.clone());
            live_channels = Some(lm.channels.clone());
            live_t = lm.t_now;
            live_norm = lm.norm;
            live_label = lm.device_label();
            live_err = lm.err.is_some();
            live_cols_total = lm.cols_total;
            live_devices = lm.devices.clone();
            live_selected = lm.selected.clone();
            live_recording = lm
                .recorder
                .as_ref()
                .map(|r| (r.path().display().to_string(), r.seconds(), r.bytes()));
            ctx.request_repaint(); // keep the live view flowing
        }

        // ---- FILE: async load + simulated playback clock ----
        if !live {
            if self.spec.is_none() {
                if let Ok((s, n)) = self.spec_rx.try_recv() {
                    self.spec = Some(s);
                    self.norm = n;
                    // The spectrogram is the whole picture as far as the
                    // waterfall is concerned, so play from the top as soon as
                    // it lands rather than sitting on a blank pane until the
                    // scan — an order of magnitude slower — has finished.
                    self.t_now = 0.0;
                    self.playing = true;
                }
            }
            let mut landed = false;
            while let Ok(f) = self.frames_rx.try_recv() {
                self.frames_landed(f);
                self.pending_scans = self.pending_scans.saturating_sub(1);
                landed = true;
            }
            if landed && self.pending_scans == 0 {
                self.arbitrate_scans();
            }
            if self.spec.is_none() || self.pending_scans > 0 {
                ctx.request_repaint();
            }
            if self.playing {
                let dt = ctx.input(|i| i.stable_dt) as f64;
                self.t_now += dt * self.speed as f64;
                if self.t_now >= self.duration_s {
                    self.t_now = self.duration_s;
                    self.playing = false;
                }
                ctx.request_repaint();
            }
        }

        // A transmission counts itself down in the QSO panel. A paused file view
        // is otherwise repainted only when something is clicked, which would
        // leave that readout frozen at whatever it said when the send happened.
        if self.tx.as_ref().is_some_and(|t| t.busy_for() > 0.0) {
            ctx.request_repaint();
        }

        if live {
            self.norm = live_norm;
        }
        let mut channels = live_channels.unwrap_or_else(|| self.channels_now());
        let spec = if live { live_spec } else { self.spec.clone() };
        let t_now = if live { live_t } else { self.t_now };
        // Wherever the clock has got to, it is the clock the clears are held
        // against — and it can have gone backwards since the last frame.
        self.drop_stale_clears(t_now);

        let mut chosen_device: Option<String> = None;
        // Routing another application's audio in. `want_pw` is set by the menu
        // that draws the list and acted on afterwards, so the subprocess it
        // costs happens once rather than inside a closure that runs per frame.
        let mut chosen_source: Option<crate::pipewire::Source> = None;
        let mut want_pw = false;
        let pw_stale = self
            .pw_at
            .is_none_or(|t| t.elapsed().as_secs_f64() > PW_REFRESH_S);
        // Same again for the serial ports, and for the same reason: the menu
        // asks, the scan happens once afterwards rather than inside a closure
        // that runs per frame.
        #[cfg(feature = "serial")]
        let mut want_ports = false;
        #[cfg(feature = "serial")]
        let ports_stale = self
            .rig_ports
            .at
            .is_none_or(|t| t.elapsed().as_secs_f64() > PORT_REFRESH_S);
        let mut chosen_output: Option<Option<String>> = None; // Some(None) = default
        let mut modes_changed = false;
        let mut open_file: Option<PathBuf> = None;
        let mut demo_request: Option<Band> = None;
        let mut go_live = false;
        let mut record_toggled = false;
        // What is *actually* being recorded, not what was last asked for, so
        // the button and the menu entry cannot disagree — and so a recording
        // that failed to start unticks itself rather than sitting there
        // claiming to be running.
        let mut want_record =
            if live { live_recording.is_some() } else { self.record };
        let mut csv_toggled = false;
        let mut want_csv = crate::traffic::is_on();
        egui::Panel::top("bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                // Settings live behind the hamburger: they are set once and
                // then left alone, so they do not deserve permanent space on a
                // bar that has to stay on one line.
                settings_menu(ui, "☰", |ui| {
                    ui.set_min_width(300.0);
                    setting(ui, "text size", &mut self.text_px, 8.0..=22.0, " px", 0.25, 1);
                    setting(ui, "window", &mut self.span_s, 10.0..=120.0, " s", 0.5, 0);
                    setting(ui, "scroll", &mut self.scroll_secs, 0.0..=2.0, " s", 0.02, 2);
                    // How long the window keeps what it has been told. Beside
                    // the other things about the text panel, because that is
                    // what it acts on — the band is still being decoded and the
                    // conversations still keep their logs.
                    setting(ui, "forget", &mut self.forget_min, 0.0..=MAX_FORGET_MIN, " min", 0.5, 1)
                        .on_hover_text(
                            "how long a station's text stays on its row before it fades off. \
                             Zero keeps everything.",
                        );
                    setting(ui, "fade", &mut self.fade_secs, 0.0..=MAX_FADE_S, " s", 0.25, 1)
                        .on_hover_text("how long text takes to go, once it is that old");
                    if !live {
                        setting(ui, "speed", &mut self.speed, 1.0..=20.0, "×", 0.1, 1);
                    }
                    ui.separator();
                    // The modes are listed whether or not they are being
                    // scanned for: calling in a mode you are not listening to
                    // is unusual but not wrong, and refusing it here would be
                    // the interface deciding it knows better.
                    let all: Vec<ModeId> = self.modes.iter().map(|(m, _)| *m).collect();
                    let current = self.default_mode;
                    ui.menu_button(format!("New QSO in {}", current.name()), |ui| {
                        ui.set_min_width(220.0);
                        for m in all {
                            let c = mode_text_color(ui.visuals().dark_mode, m);
                            let label = egui::RichText::new(m.name()).color(c);
                            if ui.selectable_label(m == current, label).clicked() {
                                self.default_mode = m;
                                ui.close();
                            }
                        }
                    })
                    .response
                    .on_hover_text("what the + button opens a conversation in");

                    // Offered whether or not the app is listening. Live, the
                    // list is the one capture opened from and the mark is the
                    // device actually open; otherwise it is the preference that
                    // takes effect the moment it goes live — which is worth
                    // setting before you start listening, not only after.
                    ui.separator();
                    ui.menu_button("Audio input", |ui| {
                        ui.set_min_width(280.0);
                        // Asked here rather than in the submenu below: whether
                        // there is a submenu at all is one of the answers, so
                        // waiting until it is drawn would mean it never was.
                        if live && pw_stale {
                            want_pw = true;
                        }
                        let devices = if live {
                            live_devices.clone()
                        } else {
                            crate::audio::input_devices(show_all)
                        };
                        let current =
                            if live { live_selected.clone() } else { self.preferred_input.clone() };
                        for d in &devices {
                            let on = current.as_ref() == Some(&d.id);
                            // The PipeWire PCMs get a submenu, because with
                            // PipeWire the device is only half the answer: the
                            // capture is a node in a graph and what matters is
                            // what has been wired to it. Everything else is a
                            // real sound card, where the device *is* the answer.
                            if on && self.pw.available && crate::audio::is_pipewire(&d.id) {
                                // A submenu cannot show itself as selected the
                                // way the other entries do, and this is always
                                // the open device — so it says so in the label
                                // rather than losing the only mark in the list.
                                ui.menu_button(format!("✓ {}", d.label), |ui| {
                                    ui.set_min_width(320.0);
                                    ui.weak("take audio from");
                                    if self.pw.sources.is_empty() {
                                        ui.weak("  nothing is playing");
                                    }
                                    for s in &self.pw.sources {
                                        let now = self.pw.current.as_ref() == Some(s);
                                        if ui.selectable_label(now, s.label()).clicked() {
                                            chosen_source = Some(s.clone());
                                            ui.close();
                                        }
                                    }
                                    ui.separator();
                                    ui.weak(
                                        "the capture keeps running, so the route \
                                         survives being made",
                                    );
                                });
                            } else if ui.selectable_label(on, &d.label).clicked() {
                                chosen_device = Some(d.id.clone());
                                ui.close();
                            }
                        }
                        ui.separator();
                        // ALSA advertises far more PCMs than there are
                        // devices; the escape hatch matters for whoever
                        // needs the one that got winnowed out.
                        if ui
                            .add(elegance::Checkbox::new(&mut show_all, "show every ALSA PCM"))
                            .changed()
                        {
                            all_inputs_toggled = true;
                        }
                        if !live {
                            ui.weak(match current {
                                Some(_) => "opened when the app goes live",
                                None => "the default input, until one is picked",
                            });
                        }
                    });
                    // Everything needed to report a fault that took a night to
                    // appear: what was recorded, and where the log went.
                    ui.separator();
                    ui.menu_button("Diagnostics", |ui| {
                        ui.set_min_width(340.0);
                        // First, because it is the one worth leaving on: a
                        // night of traffic is kilobytes here and gigabytes as
                        // audio.
                        if ui
                            .add(elegance::Checkbox::new(
                                &mut want_csv,
                                "log decodes to a CSV file",
                            ))
                            .on_hover_text(
                                "one row per decode — utc, mode, hz, snr, quality, text — \
                                 for the station's own record. Live capture only.",
                            )
                            .changed()
                        {
                            csv_toggled = true;
                        }
                        match crate::traffic::path() {
                            Some(p) => {
                                ui.weak(format!("{}  —  {} rows", p.display(), crate::traffic::rows()));
                            }
                            None => {
                                ui.weak(format!("files go to {}", diag::data_dir().display()));
                            }
                        }
                        ui.separator();
                        ui.add_enabled_ui(live, |ui| {
                            if ui
                                .add(elegance::Checkbox::new(
                                    &mut want_record,
                                    "record this session to disk",
                                ))
                                .on_hover_text(
                                    "writes the 12 kHz mono audio the decoders see — about \
                                     86 MB an hour — so a fault can be replayed by opening \
                                     the file again",
                                )
                                .changed()
                            {
                                record_toggled = true;
                            }
                        });
                        if !live {
                            ui.weak("only live capture can be recorded");
                        }
                        match &live_recording {
                            Some((path, secs, bytes)) => {
                                ui.weak(format!(
                                    "{}  —  {}, {:.0} MB",
                                    path,
                                    hms(*secs),
                                    *bytes as f64 / 1e6
                                ));
                                ui.weak("reopen it with:  ragchew <that file>");
                            }
                            None => {
                                ui.weak(format!("files go to {}", diag::data_dir().display()));
                            }
                        }
                        ui.separator();
                        match diag::log_path() {
                            Some(p) => {
                                ui.weak(format!("log: {}", p.display()));
                                ui.weak("grep it for WARN or ERROR first");
                            }
                            None => {
                                ui.weak("not logging (--no-log)");
                            }
                        }
                    });

                    // The output is where transmissions go — the rig's codec,
                    // for anyone doing this for real.
                    ui.separator();
                    ui.menu_button("Audio output", |ui| {
                        ui.set_min_width(280.0);
                        let cur = self.tx.as_ref().map(|t| t.device_name.clone());
                        if ui.selectable_label(self.preferred_output.is_none(), "default").clicked()
                        {
                            chosen_output = Some(None);
                            ui.close();
                        }
                        for d in crate::audio::output_devices(show_all) {
                            let on = self.preferred_output.as_ref() == Some(&d.id);
                            if ui.selectable_label(on, &d.label).clicked() {
                                chosen_output = Some(Some(d.id.clone()));
                                ui.close();
                            }
                        }
                        if let Some(name) = cur {
                            ui.separator();
                            ui.weak(format!("transmitting on {name}"));
                        }
                    });
                    // Rig control sits with the audio devices, because it is
                    // the same kind of setting: which piece of hardware this
                    // instance is wired to.
                    ui.separator();
                    ui.menu_button("Rig", |ui| {
                        ui.set_min_width(300.0);
                        // Which segment is which depends on what this build was
                        // compiled with, so it is looked up rather than
                        // written down — see [`KEYERS`].
                        let here = keying_tag(&self.rig);
                        let mut pick = KEYERS.iter().position(|k| k.tag == here).unwrap_or(0);
                        if ui
                            .add(elegance::SegmentedControl::from_segments(
                                &mut pick,
                                KEYERS
                                    .iter()
                                    .map(|k| elegance::Segment::text(k.label).hover_text(k.hover)),
                            ))
                            .changed()
                        {
                            // An index the list does not have changes nothing.
                            // The control documents itself as keeping the index
                            // in range, and if that ever stopped being true,
                            // silently keying a rig the operator did not pick
                            // is the one outcome worth ruling out.
                            if let Some(k) = KEYERS.get(pick) {
                                let keying = self.keying_for(k.tag);
                                self.set_keying(keying);
                            }
                        }
                        // Every radio on a cable gets the same two settings,
                        // asked for as "is this on a cable" rather than by
                        // make: the port and the rate are the same question
                        // whoever built the radio.
                        #[cfg(feature = "serial")]
                        if self.rig.on_a_cable() {
                            let mut device = self.rig_device.clone();
                            let mut baud = self.rig_baud;
                            want_ports |= ports_stale;
                            egui::Grid::new("rig_cable")
                                .num_columns(2)
                                .spacing([8.0, 4.0])
                                .show(ui, |ui| {
                                    ui.label("Port");
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut device)
                                                .desired_width(150.0),
                                        );
                                        // The list is a shortcut, not the
                                        // setting: the field beside it stays the
                                        // answer, because a port that has not
                                        // enumerated — a cable plugged in a
                                        // moment ago, a virtual port from
                                        // something else — is still one the
                                        // operator can type.
                                        ui.menu_button("…", |ui| {
                                            ui.set_min_width(320.0);
                                            if self.rig_ports.found.is_empty() {
                                                ui.weak("no serial ports found");
                                            }
                                            for port in &self.rig_ports.found {
                                                let on = port.path == device;
                                                if ui
                                                    .selectable_label(on, &port.label)
                                                    .on_hover_text(&port.path)
                                                    .clicked()
                                                {
                                                    device = port.path.clone();
                                                    ui.close();
                                                }
                                            }
                                        })
                                        .response
                                        .on_hover_text("serial ports this machine has");
                                    });
                                    ui.end_row();

                                    ui.label("Baud");
                                    ui.horizontal(|ui| {
                                        // The four every CAT radio here offers:
                                        // an Elecraft's menu and a Yaesu's menu
                                        // 031 list the same set.
                                        for rate in [4800u32, 9600, 19200, 38400] {
                                            if ui
                                                .selectable_label(baud == rate, rate.to_string())
                                                .clicked()
                                            {
                                                baud = rate;
                                            }
                                        }
                                    });
                                    ui.end_row();
                                });
                            // Rebuilt on any edit, which closes the old port on
                            // the way past — see the note below about half-typed
                            // addresses; a half-typed device name is the same
                            // thing.
                            //
                            // Rebuilt as whatever make is selected, by asking
                            // the current setting for its own tag: the panel
                            // does not know which radio it is editing and has
                            // no reason to.
                            if device != self.rig_device || baud != self.rig_baud {
                                self.rig_device = device;
                                self.rig_baud = baud;
                                let keying = self.keying_for(keying_tag(&self.rig));
                                self.set_keying(keying);
                            }
                        }
                        if matches!(self.rig, rig::Keying::RigCtld { .. }) {
                            let mut host = self.rig_host.clone();
                            let mut port = self.rig_port;
                            // The same two-column grid as the cable panel, so
                            // the menu keeps one label column whichever rig is
                            // selected and switching between them does not
                            // reflow everything under the control.
                            //
                            // `Port` here is a TCP port and `Port` there is a
                            // device: the same word for two things, which is
                            // survivable only because the two panels are never
                            // on screen together. `Host` and `Port` is what an
                            // address is called, and inventing a different word
                            // to avoid the collision would cost more than it
                            // saved.
                            egui::Grid::new("rig_daemon")
                                .num_columns(2)
                                .spacing([8.0, 4.0])
                                .show(ui, |ui| {
                                    ui.label("Host");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut host).desired_width(140.0),
                                    );
                                    ui.end_row();
                                    ui.label("Port");
                                    ui.add(egui::DragValue::new(&mut port).range(1..=65535));
                                    ui.end_row();
                                });
                            // Rebuilt on any edit, which takes the rig off the
                            // air on the way past: an address being typed into
                            // is an address that is wrong most of the time, and
                            // a half-typed one must not be left keying anything.
                            if host != self.rig_host || port != self.rig_port {
                                self.rig_host = host;
                                self.rig_port = port;
                                self.set_keying(rig::Keying::RigCtld {
                                    host: self.rig_host.clone(),
                                    port: self.rig_port,
                                });
                            }

                        }
                        if self.rig != rig::Keying::None {
                            let st = self.ptt_state();
                            match (&st.fault, st.linked) {
                                (Some(f), _) => {
                                    let c = legible(ui.visuals().dark_mode, TX_RED);
                                    ui.colored_label(c, format!("⚠ {f}"));
                                }
                                (None, true) => {
                                    ui.weak(match st.rig_says {
                                        Some(true) => "linked — the rig is transmitting",
                                        Some(false) => "linked — the rig is receiving",
                                        // Ordinary: keying over RTS with no CAT
                                        // read-back is a whole class of station.
                                        None => "linked — it cannot say whether it is \
                                                 transmitting, only be told",
                                    });
                                }
                                (None, false) => {
                                    ui.weak("not tried yet");
                                }
                            }
                            // Keying for a fifth of a second says more than any
                            // amount of connection testing: it is the whole
                            // path, including whether the rig will do it.
                            if ui.button("Test — key for 0.2 s").clicked() {
                                if let Some(ptt) = &self.ptt {
                                    let at = unix_now();
                                    ptt.schedule(at, at + 0.2);
                                }
                            }
                        }
                    });

                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("Theme");
                        // A one-of-three choice, so a segmented track rather
                        // than three labels that only look related.
                        let mut pick = Theme::ALL.iter().position(|t| *t == self.theme).unwrap_or(0);
                        if ui
                            .add(elegance::SegmentedControl::from_segments(
                                &mut pick,
                                Theme::ALL.map(|t| {
                                    elegance::Segment::text(t.label()).hover_text(t.hover())
                                }),
                            ))
                            .changed()
                        {
                            self.theme = Theme::ALL[pick];
                            ui.ctx().set_theme(self.theme.preference());
                        }
                    });
                    ui.separator();
                    if ui.button("Reset view").clicked() {
                        self.vp = Viewport { f_lo: self.band.0, f_hi: self.band.1 };
                        ui.close();
                    }
                    // Quitting is not a source of audio. It sits at the foot of
                    // the settings menu instead, where the other things that act
                    // on the application rather than on the band already are.
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    // Which build this is, under everything else, in the weight
                    // of a caption. Nobody looks for it until something has gone
                    // wrong and they have to say what they are running — so it
                    // wants to be findable rather than visible, and the foot of
                    // the one panel already about this installation rather than
                    // about the band is where a version number is looked for.
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!("ragchew {VERSION}"))
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    )
                    .on_hover_text("the version of this build");
                })
                .on_hover_text("Settings");
                // "Source", not "File": every item under it answers "what is
                // this listening to" — a recording, a synthetic band, the sound
                // card — and only one of the three is a file.
                ui.menu_button("Source", |ui| {
                    // In the order they are worth reaching for. Live input is
                    // what the program is for and the default with no arguments
                    // at all, so it is the first thing under the menu that
                    // switches sources; a recording is the rarest of the three
                    // and goes last, behind a dialog.
                    if ui.button("Live input").clicked() {
                        go_live = true;
                        ui.close();
                    }
                    ui.separator();
                    // One item rather than four. They are all the same answer
                    // to "what is this listening to" — a band that isn't there
                    // — and listing them flat put four of the five entries in
                    // this menu on the one source nobody uses in earnest.
                    ui.menu_button("Demo bands", |ui| {
                        for band in Band::ALL {
                            if ui.button(band.label()).on_hover_text(band.hover()).clicked() {
                                demo_request = Some(band);
                                ui.close();
                            }
                        }
                    })
                    .response
                    .on_hover_text("synthetic bands, each built to show one thing");
                    ui.separator();
                    if ui.button("Open audio file…").clicked() {
                        // Blocks the UI thread while the dialog is up, which is
                        // what you want: there is nothing to interact with
                        // behind it, and it keeps the result in hand rather
                        // than needing a channel back.
                        open_file = rfd::FileDialog::new()
                            .add_filter("audio", &["wav"])
                            .set_title("Open a 12 kHz mono WAV")
                            .pick_file();
                        ui.close();
                    }
                });
                ui.separator();
                if live {
                    // The device itself is chosen from the hamburger; the bar
                    // only reports that capture is running, and at what rate.
                    // The lamp says which of those two it is — a stream that
                    // failed to open still labels itself, in words nobody reads
                    // until something is wrong.
                    let state = if live_err {
                        elegance::IndicatorState::Off
                    } else {
                        elegance::IndicatorState::On
                    };
                    ui.add(elegance::Indicator::new(state));
                    ui.strong(&live_label);
                    // Recording is easy to leave on by accident and expensive
                    // when you do, so it says so on the bar rather than only
                    // inside the menu that started it.
                    let red = legible(ui.visuals().dark_mode, Color32::from_rgb(255, 120, 120));
                    let recording = live_recording.is_some();
                    let dot = egui::RichText::new("⏺")
                        .color(if recording { red } else { ui.visuals().text_color() });
                    if ui
                        .selectable_label(recording, dot)
                        .on_hover_text(if recording {
                            "stop recording"
                        } else {
                            "record this session to disk — the audio the decoders see, \
                             about 86 MB an hour"
                        })
                        .clicked()
                    {
                        want_record = !recording;
                        record_toggled = true;
                    }
                    if let Some((path, secs, bytes)) = &live_recording {
                        // A fixed slot. The readout gains a digit as the hours
                        // and the megabytes climb, and without one everything
                        // to its right along the bar is shoved along with it —
                        // once an hour, and again every time the size crosses a
                        // power of ten.
                        let font = egui::TextStyle::Body.resolve(ui.style());
                        let w = ui
                            .painter()
                            .layout_no_wrap(
                                "00:00:00 · 8888 MB".to_owned(),
                                font,
                                Color32::PLACEHOLDER,
                            )
                            .size()
                            .x;
                        let text = format!("{} · {:.0} MB", hms(*secs), *bytes as f64 / 1e6);
                        ui.allocate_ui_with_layout(
                            egui::vec2(w, ui.spacing().interact_size.y),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| ui.colored_label(red, text).on_hover_text(path.as_str()),
                        );
                    }
                } else {
                    // Playable as soon as there is a waterfall to play; the scan
                    // catches up on its own.
                    ui.add_enabled_ui(self.spec.is_some(), |ui| {
                        // "⏸ pause" and "▶ play" do not measure the same, and a
                        // button that resizes as you click it drags the whole
                        // bar left and right with it. So the slot is the wider
                        // of the two, measured in the font and padding actually
                        // in force — a fixed width was right until a theme
                        // changed the metrics under it.
                        if transport_button(ui, self.playing).clicked() {
                            if self.t_now >= self.duration_s {
                                self.t_now = 0.0;
                            }
                            self.playing = !self.playing;
                        }
                        if ui.button("⟲ restart").clicked() {
                            self.t_now = 0.0;
                            self.playing = true;
                        }
                    });
                }
                ui.separator();
                modes_changed = self.modes_menu(ui);

                // Clearing the window is a command about the window rather than
                // a setting, and it is reached mid-session and often — you tidy
                // the rows when the band has moved on — so it sits out here
                // beside the modes instead of two clicks inside the hamburger.
                if clear_button(ui)
                    .on_hover_text(
                        "take every station's text off the rows. Right-click one row to \
                         clear just that channel; open conversations keep their logs.",
                    )
                    .clicked()
                {
                    self.clear_all(t_now);
                }

                // Last on the bar, deliberately. Its readout gains and loses
                // digits as playback runs, and anything to its right would be
                // shoved back and forth for the whole recording.
                if !live {
                    ui.separator();
                    const TRACK_W: f32 = 170.0;
                    ui.style_mut().spacing.slider_width = TRACK_W;
                    // The slider is shown a *copy* of the clock, snapped to the
                    // physical pixel grid. A circle drawn at a fractional
                    // position is anti-aliased differently every frame, so an
                    // unsnapped thumb visibly changes shape as it slides; on
                    // the grid it only ever translates.
                    //
                    // A copy, because quantising the clock itself is what the
                    // slider's own `fixed_decimals` did — and since the slider
                    // writes back every frame, at 1x the clock gained 0.017 s
                    // and lost all of it again to rounding. Playback stopped
                    // dead. So the snapped value goes back to the clock only
                    // when the operator is actually dragging it.
                    let span = self.duration_s.max(0.001);
                    let mut shown =
                        snap_to_pixels(self.t_now, span, TRACK_W, ctx.pixels_per_point());
                    // One decimal, or the readout gains and loses characters
                    // every frame — the whole widget twitching for the length
                    // of the recording.
                    if ui
                        .add(
                            egui::Slider::new(&mut shown, 0.0..=span)
                                .custom_formatter(|n, _| format!("{n:.1}"))
                                .text("s"),
                        )
                        .dragged()
                    {
                        self.t_now = shown;
                        self.playing = false;
                    }
                }
            });
        });

        if all_inputs_toggled {
            self.show_all_inputs = show_all;
            if let Some(lm) = self.live.as_mut() {
                lm.show_all_inputs = show_all;
                lm.devices = crate::audio::input_devices(show_all);
            }
        }
        if let Some(path) = open_file {
            self.open_file(&path);
        }
        if let Some(band) = demo_request {
            self.set_source(band.synth(), band.source_name());
        }
        if go_live && self.live.is_none() {
            self.go_live();
        }
        if record_toggled {
            self.record = want_record;
            if let Some(lm) = self.live.as_mut() {
                lm.set_recording(want_record);
            }
        }
        if csv_toggled {
            self.set_csv(want_csv, None);
        }

        // A different output takes effect on the next send: dropping the stream
        // here would cut off a transmission in progress.
        if let Some(id) = chosen_output {
            if id != self.preferred_output {
                self.preferred_output = id;
                if self.tx.as_ref().is_none_or(|t| t.busy_for() <= 0.0) {
                    self.tx = None;
                }
            }
        }

        // Rewire the graph, if an application was picked to listen to. Note
        // what this does *not* do: reopen the capture. The stream stays up, so
        // the route outlives being made — which is the whole point, since the
        // node a route attaches to dies with the stream that owns it.
        if let Some(s) = chosen_source {
            match crate::pipewire::route_from(&s) {
                Ok(what) => {
                    diag_info!("audio", "routed {what}");
                    self.say(format!("listening to {}", s.label()));
                }
                Err(e) => {
                    diag_warn!("audio", "could not route from {}: {e}", s.app);
                    self.warn(format!("could not listen to {}: {e}", s.app));
                }
            }
            want_pw = true;
        }
        if want_pw {
            self.pw = crate::pipewire::survey();
            self.pw_at = Some(std::time::Instant::now());
        }
        #[cfg(feature = "serial")]
        if want_ports {
            self.rig_ports.found = rig::serial::ports();
            self.rig_ports.at = Some(std::time::Instant::now());
        }

        // switch input device if the user picked a different one
        if let Some(name) = chosen_device {
            if live_selected.as_deref() != Some(name.as_str()) {
                self.preferred_input = Some(name.clone());
                if let Some(lm) = self.live.as_mut() {
                    lm.open(Some(name));
                }
            }
        }

        // a change to the listened-for modes restarts the live decoders, or
        // re-scans the file from the top
        if modes_changed {
            let enabled = self.enabled_modes();
            match self.live.as_mut() {
                Some(lm) => lm.set_modes(enabled),
                None => self.rescan(),
            }
        }

        // Conversations sit to the left of the panorama, before the central
        // panel claims what is left. New text reaches them here, so a QSO logs
        // what its station said whether or not its tab is the one on top.
        //
        // Before anything is forgotten, and from the set rather than from the
        // window's copy of it: clearing a row is about the window, and a log
        // that lost text because the operator tidied the display would be a
        // record that could not be trusted.
        self.qsos.absorb(&channels);
        self.qsos.retire_sent(unix_now());

        // Now take out what has been cleared and what has gone stale. Live,
        // what this trims is the copy the window is drawn from; the set behind
        // it is the session's own record and keeps everything it heard.
        channels.forget(|ch| self.cut_for(ch, t_now));

        // The status line spans the window, so it is claimed before the QSO
        // panel takes the left of it — shown after the toolbar's handlers above
        // so that something said this frame is read this frame. It counts what
        // the window is showing, which after a clear is not the same as what
        // the band has been carrying.
        self.status_bar(ui, live, channels.channels().len());

        // The conversations sit on the theme's card surface while the band
        // keeps the panel background. The two are a designed pair — a shade
        // apart in both palettes, lighter in the dark theme and lighter again
        // in the light one — so the QSO panel reads as its own surface without
        // needing a rule drawn between them.
        let qso_frame =
            egui::Frame::side_top_panel(ui.style()).fill(elegance::Theme::current(ctx).palette.card);
        let panel = egui::Panel::left("qsos")
            .resizable(true)
            .default_size(self.qso_w)
            .min_size(MIN_QSO_W)
            .frame(qso_frame)
            .show(ui, |ui| {
                // The rig's own readout, above the conversations: what the
                // station is doing, over what the people on it are saying.
                //
                // On its own ground, because they are two different things and
                // a reader should not have to work that out from the contents.
                // The radio's panel is the app's background rather than the
                // card the conversations sit on — the same shade the window
                // shows behind everything, so the rig reads as apparatus the
                // panel is resting on rather than as another card.
                if self.ptt.is_some() {
                    let palette = elegance::Theme::current(ctx).palette;
                    egui::Frame::new()
                        .fill(palette.bg)
                        .stroke(Stroke::new(1.0, palette.card))
                        .corner_radius(5.0)
                        .inner_margin(egui::Margin::symmetric(6, 5))
                        .show(ui, |ui| self.vfo(ui));
                    ui.add_space(6.0);
                }
                self.qso_panel(ui, t_now);
            });
        // The panel's own rect, not its contents': the contents are inset by
        // the frame margin, and feeding that back as next launch's width would
        // shave a few pixels off the panel every time the app was restarted.
        self.qso_w = panel.response.rect.width().max(MIN_QSO_W);

        // Clicking a station's text opens its conversation, and right-clicking
        // it offers to clear it. Both are handled after the panel rather than
        // inside it, where `self` is already borrowed for the drawing.
        let mut open_qso: Option<Channel> = None;
        let mut clear_one: Option<u64> = None;
        let mut clear_every = false;
        // What is left of the timeout, for the rows to fade against. Read out
        // here for the same reason: the drawing borrows `self`.
        let (keep, fade) = (self.forget_secs(), self.fade_secs as f64);
        egui::CentralPanel::default().show(ui, |ui| {
            let spec = match &spec {
                Some(s) => s.clone(),
                // Nothing loaded at all is a different state from something
                // loading, and saying "computing…" forever would be a lie.
                None if !live && self.samples.is_empty() => {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            "Nothing loaded.\n\n                             Source ▸ Open audio file…   for a 12 kHz mono WAV\n                             Source ▸ Demo band          for a synthetic band\n                             Source ▸ Live input         to decode the sound card",
                        );
                    });
                    return;
                }
                None => {
                    ui.centered_and_justified(|ui| {
                        ui.heading("⏳ computing spectrogram…");
                    });
                    return;
                }
            };

            let (resp, painter) =
                ui.allocate_painter(ui.available_size(), egui::Sense::click_and_drag());
            let rect = resp.rect;
            // the window may have been resized since the split was last set
            self.waterfall_w = clamp_waterfall_w(rect.width(), self.strip_w, self.waterfall_w);
            let g = Geometry {
                width: rect.width(),
                height: rect.height(),
                waterfall_w: self.waterfall_w,
                strip_w: self.strip_w,
            };
            let to_screen = |x: f32, y: f32| Pos2::new(rect.min.x + x, rect.min.y + y);

            // interaction. The frequency mappings snapshot the viewport rather
            // than borrowing it, so panning mid-frame does not fight them.
            let (vp_lo, vp_hi) = (self.vp.f_lo, self.vp.f_hi);
            let (band_lo, band_hi) = self.band;
            let top_y = rect.min.y;
            let h = g.height.max(1.0);
            let y_to_hz = move |y: f32| vp_hi - ((y - top_y) / h) as f64 * (vp_hi - vp_lo);
            // The band bar maps the *whole* band over the full height, whatever
            // the view is zoomed to — that is the point of it.
            let y_to_band_hz = move |y: f32| {
                let f = ((y - top_y) / h).clamp(0.0, 1.0) as f64;
                band_hi - f * (band_hi - band_lo)
            };
            let bar_x0 = g.width - BAND_BAR_W;
            let on_bar = |p: Pos2| p.x - rect.min.x >= bar_x0;
            let pointer = ui.input(|i| i.pointer.interact_pos());

            // What the pointer is pointing at, in Hz, for the status bar.
            //
            // Only over the waterfall and the band bar, because they are where
            // a vertical position *is* a frequency. Left of them the rows have
            // been nudged apart to fit, which is the whole reason the leader
            // lines exist, and a readout that answered there would be answering
            // a question about layout as though it were about the band. The bar
            // has its own mapping — the full band over the full height, however
            // far the view is zoomed in — so it reports what it shows.
            self.cursor_hz = pointer.filter(|p| rect.contains(*p)).and_then(|p| {
                let x = p.x - rect.min.x;
                match x {
                    _ if x >= bar_x0 => Some(y_to_band_hz(p.y)),
                    _ if x >= g.waterfall_x0() => Some(y_to_hz(p.y)),
                    _ => None,
                }
            });

            if resp.drag_started() {
                self.bar_drag = pointer.is_some_and(on_bar);
                if let (true, Some(p)) = (self.bar_drag, pointer) {
                    let center = (self.vp.f_lo + self.vp.f_hi) / 2.0;
                    self.bar_grab_hz = y_to_band_hz(p.y) - center;
                }
            }
            if !resp.dragged() {
                self.bar_drag = false;
            }
            if self.bar_drag {
                if let Some(p) = pointer {
                    self.center_on(y_to_band_hz(p.y) - self.bar_grab_hz);
                }
            } else if resp.clicked() && pointer.is_some_and(on_bar) {
                if let Some(p) = pointer {
                    self.center_on(y_to_band_hz(p.y));
                }
            }

            // The connector strip doubles as the splitter between the two
            // panels: it is the gutter between them, so it is where a hand
            // reaches to move the boundary.
            let on_split = |p: Pos2| {
                let x = p.x - rect.min.x;
                (g.strip_x0() - SPLIT_GRAB..=g.strip_x1() + SPLIT_GRAB).contains(&x)
            };
            if resp.drag_started() && !self.bar_drag {
                self.split_drag = pointer.is_some_and(on_split);
                if let (true, Some(p)) = (self.split_drag, pointer) {
                    self.split_grab_dx = p.x - rect.min.x - g.strip_x0();
                }
            }
            if !resp.dragged() {
                self.split_drag = false;
            }
            if self.split_drag {
                if let Some(p) = pointer {
                    let strip_x0 = p.x - rect.min.x - self.split_grab_dx;
                    let want = g.width - strip_x0 - self.strip_w;
                    self.waterfall_w = clamp_waterfall_w(g.width, self.strip_w, want);
                }
            }
            let split_hot = self.split_drag || (!self.bar_drag && pointer.is_some_and(on_split));
            if split_hot {
                ctx.set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
            // Frequency zoom comes from `zoom_delta()`, which egui synthesizes
            // from touchpad pinch AND Ctrl/Cmd+scroll (both are removed from the
            // plain scroll delta). Plain scroll pans; Shift+scroll sizes text.
            let (delta, zoom, shift) =
                ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.modifiers.shift));
            if resp.hovered() {
                if (zoom - 1.0).abs() > 1e-4 {
                    let focus = ui
                        .input(|i| i.pointer.hover_pos())
                        .map(|p| y_to_hz(p.y))
                        .unwrap_or((self.vp.f_lo + self.vp.f_hi) / 2.0);
                    self.zoom(focus, 1.0 / zoom as f64); // zoom>1 => zoom in => smaller span
                } else if delta.x != 0.0 || delta.y != 0.0 {
                    if shift {
                        // some platforms put shift+wheel on the x axis
                        let s = if delta.y != 0.0 { delta.y } else { delta.x };
                        self.text_px = (self.text_px + s * 0.02).clamp(8.0, 22.0);
                    } else if delta.y != 0.0 {
                        self.pan_hz(
                            delta.y as f64 * (self.vp.f_hi - self.vp.f_lo) / g.height as f64,
                        );
                    }
                }
            }
            if resp.dragged() && !self.bar_drag && !self.split_drag {
                let dhz =
                    resp.drag_delta().y as f64 * (self.vp.f_hi - self.vp.f_lo) / g.height as f64;
                self.pan_hz(dhz);
            }

            // waterfall: scrolling window ending at "now" — for live that's the
            // newest built column; for file playback it's the clock position.
            let hop = spec.hop as f64;
            let col_hi = if live {
                spec.columns.len() as i64
            } else {
                (t_now * SAMPLE_RATE as f64 / hop).round() as i64
            };
            // How far the waterfall has actually travelled. Live, that is not
            // `col_hi`: the spectrogram is capped, so once full its length
            // stops changing while the newest column keeps arriving and the
            // oldest keeps falling off. Scrolling has to follow the data, not
            // the index it happens to sit at.
            let col_travel = if live { live_cols_total } else { col_hi };
            let span_cols = (self.span_s as f64 * SAMPLE_RATE as f64 / hop).round() as usize;
            let ppp = ctx.pixels_per_point();
            let tex = self.waterfall_texture(
                ctx,
                &spec,
                [(self.waterfall_w * ppp) as usize, (g.height * ppp) as usize],
                col_hi,
                col_travel,
                span_cols,
            );
            let wf_rect =
                Rect::from_min_max(to_screen(g.waterfall_x0(), 0.0), to_screen(g.width, g.height));
            painter.image(
                tex.id(),
                wf_rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );

            // What the radio cannot hear, dimmed.
            //
            // The app draws the whole band it scans; the rig decides how much
            // of that reaches the sound card. With a narrow filter most of this
            // panorama is a picture of nothing, and until now nothing on screen
            // said which part. Shaded rather than cropped, because the boundary
            // is worth seeing — a station just outside it is a station to widen
            // the filter for, and one that vanished with the shading would not
            // be noticed at all.
            for band in outside_the_filter(self.ptt_state().width_hz, self.rig_passband_mode()) {
                let (top, bottom) = (g.hz_to_y(&self.vp, band.1), g.hz_to_y(&self.vp, band.0));
                if bottom <= 0.0 || top >= g.height {
                    continue;
                }
                painter.rect_filled(
                    Rect::from_min_max(
                        to_screen(g.waterfall_x0(), top.max(0.0)),
                        to_screen(g.width, bottom.min(g.height)),
                    ),
                    0.0,
                    Color32::from_black_alpha(150),
                );
            }

            if split_hot {
                painter.rect_filled(
                    Rect::from_min_max(
                        to_screen(g.strip_x0(), 0.0),
                        to_screen(g.strip_x1(), g.height),
                    ),
                    0.0,
                    Color32::from_white_alpha(12),
                );
            }
            let div = Stroke::new(1.0, Color32::from_gray(if split_hot { 150 } else { 70 }));
            painter.line_segment([to_screen(g.strip_x0(), 0.0), to_screen(g.strip_x0(), g.height)], div);
            painter.line_segment([to_screen(g.strip_x1(), 0.0), to_screen(g.strip_x1(), g.height)], div);

            // channels: nudge layout + streaming text + leaders
            let visible: Vec<&Channel> =
                channels.channels().iter().filter(|c| g.visible(&self.vp, c.hz)).collect();
            let ideals: Vec<f32> = visible.iter().map(|c| g.hz_to_y(&self.vp, c.hz)).collect();
            let line_h = self.text_px + 6.0;
            let rows = layout::place(&ideals, line_h, line_h, g.height - line_h);

            let font = FontId::monospace(self.text_px);
            let dark = ui.visuals().dark_mode;
            let char_w = self.text_px * 0.6;
            let max_chars = ((g.strip_x0() - 8.0) / char_w).max(1.0) as usize;
            // Rows move on the wall clock, like every other animation here, so
            // a file played at 8× slides no faster than a live band would —
            // except that its text arrives 8× faster, so the debt is paid at
            // that speed too or a busy row would never settle.
            let dt = ui.input(|i| i.stable_dt).min(0.25)
                * if live { 1.0 } else { self.speed.max(1e-3) };
            self.slide_rows(&channels, char_w, dt, g.strip_x0());
            // clip text to the text panel so a chunk sliding in from behind the
            // strip (and old text sliding off the left) is masked cleanly.
            let text_painter = painter.with_clip_rect(Rect::from_min_max(
                rect.min,
                to_screen(g.strip_x0(), g.height),
            ));

            // What each row ended up with, gathered as they are drawn and read
            // by [`App::watch_rows`] once they all have been.
            let mut row_now: Vec<RowNow> = Vec::with_capacity(visible.len());

            for (i, ch) in visible.iter().enumerate() {
                let color = mode_text_color(dark, ch.mode);
                let y_row = rows[i];
                let y_freq = g.hz_to_y(&self.vp, ch.hz);

                // Time since this channel's newest frame arrived, in *wall-clock*
                // seconds. File playback time runs at `speed`x (live is real-time),
                // so animations look the same regardless of playback speed.
                let eff_speed = if live { 1.0 } else { self.speed.max(1e-3) };
                let playback_age = t_now - (ch.last_heard_s + ch.mode.chunk_secs());
                let age = playback_age / eff_speed as f64;

                // scroll-in animation: the row is drawn shifted right by what
                // it still owes, and [`App::slide_rows`] pays that off.
                let offset_x = self.slide.get(&ch.id).map_or(0.0, |s| s.px);

                // render a bit more than fits, and let the clip rects trim it
                let tail = max_chars + ch.last_chunk_chars + 2;
                let shown: String = ch.text.chars().rev().take(tail).collect::<Vec<_>>()
                    .into_iter().rev().collect();

                row_now.push(RowNow {
                    id: ch.id,
                    hz: ch.hz,
                    mode: ch.mode,
                    drawn: shown.chars().count(),
                    held: ch.text.chars().count(),
                    y: y_row,
                    offset: offset_x,
                    cut: self.cut_for(ch, t_now),
                    oldest: ch.arrivals.first().map(|&(_, t)| t),
                });

                // The row in stretches of one age, oldest first, so the front
                // of it can fade off while the station carries on talking at
                // the other end. What is going keeps its columns as it goes —
                // the row is right-aligned on the newest character, so the text
                // still being read never shifts under the eye.
                let mut job = egui::text::LayoutJob::default();
                let mut left = shown.chars();
                // The newest stretch's solidity, which is the row's: when the
                // last thing a station said has faded, so has the leader out to
                // where it said it. It only ever reaches zero in the same frame
                // the channel is forgotten — both edges are `keep + fade` — so
                // a row never sits here invisible.
                let mut solid = 1.0f32;
                for (n, at) in ch.tail_ages(tail) {
                    let s: String = left.by_ref().take(n).collect();
                    solid = solidity(t_now, at, keep, fade);
                    job.append(
                        &s,
                        0.0,
                        egui::TextFormat {
                            font_id: font.clone(),
                            color: color.gamma_multiply(solid),
                            ..Default::default()
                        },
                    );
                }

                if (0.0..HIGHLIGHT_SECS).contains(&age) {
                    let a = (1.0 - age / HIGHLIGHT_SECS) as f32;
                    let w = (shown.chars().count() as f32 * char_w).min(g.strip_x0() - 8.0);
                    let r = Rect::from_min_max(
                        to_screen(g.strip_x0() - 6.0 - w + offset_x, y_row - line_h / 2.0),
                        to_screen(g.strip_x0() - 2.0 + offset_x, y_row + line_h / 2.0),
                    );
                    text_painter.rect_filled(r, 3.0, color.gamma_multiply(0.22 * a * solid));
                }

                let galley = text_painter.layout_job(job);
                text_painter.galley(
                    egui::Align2::RIGHT_CENTER
                        .anchor_size(to_screen(g.strip_x0() - 6.0 + offset_x, y_row), galley.size())
                        .min,
                    galley,
                    color,
                );

                // The leader goes with the text it leads to, not with the hover
                // card, which stays legible right up until the row is gone.
                let p = scene::leader(&g, y_row, y_freq);
                let stroke = Stroke::new(1.2, color.gamma_multiply(solid));
                for seg in p.windows(2) {
                    painter.line_segment(
                        [to_screen(seg[0].0, seg[0].1), to_screen(seg[1].0, seg[1].1)],
                        stroke,
                    );
                }
                painter.circle_filled(
                    to_screen(g.waterfall_x0(), y_freq),
                    2.5,
                    color.gamma_multiply(solid),
                );

                // Hovering a row reports what the waterfall cannot: how well
                // the station is being heard, and how far its carrier has
                // wandered since it was first found.
                let row = Rect::from_min_max(
                    to_screen(0.0, y_row - line_h / 2.0),
                    to_screen(g.strip_x0(), y_row + line_h / 2.0),
                );
                let resp = ui
                    .interact(row, ui.id().with(("channel", ch.id)), egui::Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_ui(|ui| {
                        ui.colored_label(color, ch.mode.name());
                        ui.label(format!(
                            "{:.1} Hz   {:+.1} Hz drift",
                            ch.hz,
                            ch.drift_hz()
                        ));
                        // Two numbers, because they answer two questions. The
                        // SNR is a measurement of the signal, in the 2.5 kHz a
                        // ham quotes them in, and reads the same on every row
                        // of the band. `quality` is the decoder's own
                        // confidence against its own protocol's threshold, so
                        // it is a ratio and stays one — this used to be
                        // labelled SNR, which it never was.
                        match (ch.snr_db, ch.best_snr_db) {
                            (Some(now), Some(best)) => {
                                ui.label(format!("SNR {now:+.0} dB (best {best:+.0})"))
                            }
                            _ => ui.weak("SNR not measured"),
                        };
                        // Bigger here than in a QSO, because this is the card
                        // you read while deciding who to call, and how a
                        // station has been fading is most of that decision.
                        sparkline::Snr::new(&ch.snr_history, ch.mode, t_now)
                            .size(200.0, 32.0)
                            .color(color)
                            .show(ui);
                        ui.weak(format!(
                            "confidence ×{:.1} of the mode's noise floor (best ×{:.1})",
                            ch.quality, ch.best_quality
                        ));
                        ui.weak(format!(
                            "{} decode{} · {:.1} s of audio",
                            ch.chunks,
                            if ch.chunks == 1 { "" } else { "s" },
                            (ch.last_heard_s - ch.first_heard_s) + ch.mode.chunk_secs()
                        ));
                        ui.weak("click to open a QSO · right-click to clear it");
                    });
                // Clearing one station is offered where that station is, which
                // is the only place it can be aimed: the rows are the band's
                // own order and nothing else on screen names a channel.
                let _ = resp.context_menu(|ui| {
                    ui.weak(format!("{} at {:.0} Hz", ch.mode.name(), ch.hz));
                    if ui.button("Clear this thread").clicked() {
                        clear_one = Some(ch.id);
                        ui.close();
                    }
                    if ui.button("Clear every thread").clicked() {
                        clear_every = true;
                        ui.close();
                    }
                });
                if resp.clicked() {
                    open_qso = Some((*ch).clone());
                }
            }

            // A row losing its text is a fault the eye cannot catch, so the log
            // catches it instead.
            for line in self.watch_rows(&row_now, unix_now()) {
                diag_warn!("rows", "{line}");
            }

            // The whole frequency axis lives on the right-hand edge of the
            // waterfall, overlaid: the band bar hard against the edge, then the
            // scale labels just inside it. Both belong on the axis they describe
            // rather than in the text panel, and the waterfall draws
            // newest-at-left, so they cover the oldest history and never hide a
            // signal as it arrives.

            // Band bar: the full band over the full height, with the current
            // view as the thumb. Drag it to pan, click to jump.
            let bar = Rect::from_min_max(to_screen(bar_x0, 0.0), to_screen(g.width, g.height));
            let hot = self.bar_drag || pointer.is_some_and(on_bar);
            painter.rect_filled(bar, 0.0, Color32::from_black_alpha(110));
            let y_of_band =
                |hz: f64| top_y + ((band_hi - hz) / (band_hi - band_lo)) as f32 * g.height;
            let (top, bot) = (y_of_band(self.vp.f_hi), y_of_band(self.vp.f_lo));
            // a deep zoom would otherwise leave a thumb too thin to grab
            let grow = ((MIN_THUMB_H - (bot - top)) / 2.0).max(0.0);
            painter.rect_filled(
                Rect::from_min_max(
                    Pos2::new(bar.left() + 2.0, top - grow),
                    Pos2::new(bar.right() - 2.0, bot + grow),
                ),
                3.0,
                Color32::from_white_alpha(if hot { 120 } else { 70 }),
            );

            // Scale labels, each on a dim backing so they stay legible over a
            // bright trace.
            let tick = Stroke::new(1.0, Color32::from_gray(160));
            let font = FontId::proportional(11.0);
            let step = tick_step(self.vp.f_hi - self.vp.f_lo);
            let mut hz = (self.vp.f_lo / step).ceil() * step;
            while hz <= self.vp.f_hi {
                let y = g.hz_to_y(&self.vp, hz);
                let galley =
                    painter.layout_no_wrap(format!("{hz:.0} Hz"), font.clone(), Color32::WHITE);
                let size = galley.size();
                let pos = to_screen(bar_x0 - 10.0 - size.x, y - size.y / 2.0);
                let pad = egui::vec2(4.0, 1.0);
                painter.rect_filled(
                    Rect::from_min_size(pos - pad, size + 2.0 * pad),
                    3.0,
                    Color32::from_black_alpha(170),
                );
                painter.galley(pos, galley, Color32::from_gray(210));
                painter.line_segment(
                    [to_screen(bar_x0 - 7.0, y), to_screen(bar_x0 - 1.0, y)],
                    tick,
                );
                hz += step;
            }
        });

        if let Some(ch) = open_qso {
            self.qsos.open_for_channel(&ch, t_now);
        }
        if let Some(id) = clear_one {
            self.cleared.insert(id, t_now);
        }
        if clear_every {
            self.clear_all(t_now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frequency scale stays legible at any zoom: a handful of round
    /// numbers, whether the view is the whole band or one conversation.
    #[test]
    fn tick_spacing_suits_the_zoom() {
        for span in [50.0, 120.0, 500.0, 1500.0, 4000.0] {
            let step = tick_step(span);
            let n = span / step;
            assert!((2.0..=12.0).contains(&n), "{n} labels across {span} Hz (step {step})");
        }
    }

    /// The split stays inside the window, and both panels keep some width —
    /// even when the window is too narrow to honour both minimums.
    #[test]
    fn split_stays_usable_at_any_window_width() {
        for window in [1280.0f32, 640.0, 300.0, 200.0, 120.0] {
            for want in [-50.0f32, 0.0, 100.0, 300.0, 5000.0] {
                let w = clamp_waterfall_w(window, 35.0, want);
                assert!(w >= MIN_WATERFALL_W, "waterfall {w} in a {window} window");
                assert!(w <= window.max(MIN_WATERFALL_W), "waterfall {w} exceeds {window}");
            }
        }
    }

    /// A comfortable window honours both minimums and the requested width.
    #[test]
    fn split_is_respected_when_there_is_room() {
        assert_eq!(clamp_waterfall_w(1280.0, 35.0, 420.0), 420.0);
        assert_eq!(clamp_waterfall_w(1280.0, 35.0, 5000.0), 1280.0 - 35.0 - MIN_TEXT_W);
    }

    /// Settings survive the trip through the file eframe writes.
    #[test]
    fn settings_round_trip_through_json() {
        let mut app = App::base((0.0, 4000.0));
        app.text_px = 17.0;
        app.waterfall_w = 480.0;
        app.span_s = 90.0;
        app.speed = 3.0;
        app.preferred_input = Some("USB Audio CODEC".to_string());
        app.preferred_output = Some("alsa:plughw:CARD=Rig,DEV=0".to_string());
        app.qso_w = 380.0;
        app.theme = Theme::Light;
        app.default_mode = ModeId::Olivia(ragchew::olivia::OL_16_500);
        app.forget_min = 20.0;
        app.fade_secs = 3.5;
        app.rig_host = "radio.local".to_string();
        app.rig_port = 4599;
        app.rig_device = "/dev/ttyUSB3".to_string();
        app.rig_baud = 9600;
        app.set_keying(rig::Keying::RigCtld { host: "radio.local".into(), port: 4599 });
        app.set_view(1200.0, 1400.0);
        for (m, on) in app.modes.iter_mut() {
            *on = m.protocol() == Protocol::Olivia;
        }

        let text = serde_json::to_string(&app.settings()).unwrap();
        let mut restored = App::base((0.0, 4000.0));
        restored.apply(serde_json::from_str(&text).unwrap());

        assert_eq!(restored.text_px, 17.0);
        assert_eq!(restored.waterfall_w, 480.0);
        assert_eq!(restored.span_s, 90.0);
        assert_eq!(restored.speed, 3.0);
        assert_eq!(restored.preferred_input.as_deref(), Some("USB Audio CODEC"));
        assert_eq!(restored.preferred_output.as_deref(), Some("alsa:plughw:CARD=Rig,DEV=0"));
        assert_eq!(restored.qso_w, 380.0);
        assert_eq!(restored.theme, Theme::Light);
        assert_eq!(restored.default_mode, ModeId::Olivia(ragchew::olivia::OL_16_500));
        assert_eq!(restored.forget_min, 20.0);
        assert_eq!(restored.fade_secs, 3.5);
        assert_eq!((restored.vp.f_lo, restored.vp.f_hi), (1200.0, 1400.0));
        assert_eq!(
            restored.rig,
            rig::Keying::RigCtld { host: "radio.local".into(), port: 4599 },
            "the rig came back as something else"
        );
        assert!(restored.ptt.is_some(), "restored a rig with nobody supervising it");
        // The cable is remembered even while the daemon is the one in use, so
        // that switching back does not ask for it again.
        assert_eq!(restored.rig_device, "/dev/ttyUSB3");
        assert_eq!(restored.rig_baud, 9600);
        assert_eq!(restored.enabled_modes(), app.enabled_modes());
        assert!(restored.enabled_modes().iter().all(|m| m.protocol() == Protocol::Olivia));
    }

    /// The timeout is on out of the box, and a settings file from before it
    /// existed takes that rather than the zero that means "off".
    ///
    /// Which is the whole weight of the default: `#[serde(default)]` fills a
    /// missing field from `Settings::default`, so this is not just what a fresh
    /// install gets but what every installation already out there gets on its
    /// next launch. Only a file that names the field — one saved since — keeps
    /// its own answer.
    #[test]
    fn a_fresh_install_forgets_after_a_quarter_of_an_hour() {
        let quarter_hour = Some(15.0 * 60.0);
        assert_eq!(App::base((0.0, 4000.0)).forget_secs(), quarter_hour);

        // A file from a build that had never heard of the setting: everything
        // else present, this one absent.
        let mut app = App::base((0.0, 4000.0));
        let old = r#"{"text_px":17.0,"qso_w":380.0,"modes":["JS8 Normal"]}"#;
        app.apply(serde_json::from_str(old).unwrap());
        assert_eq!(app.forget_secs(), quarter_hour, "an old settings file turned it off");
        assert_eq!(app.fade_secs, 8.0);

        // And a file that does name it is the operator's own answer, including
        // when the answer is "never".
        let mut app = App::base((0.0, 4000.0));
        app.apply(serde_json::from_str(r#"{"forget_min":0.0}"#).unwrap());
        assert_eq!(app.forget_secs(), None, "a saved zero was overruled by the default");
    }

    /// A theme name this build does not know — from a settings file written by
    /// a later one — falls back to Auto and leaves the rest of the file intact.
    /// A file written before the theme existed does the same.
    #[test]
    fn an_unknown_theme_falls_back_without_taking_the_settings_with_it() {
        for theme in [r#""aubergine""#, r#""""#] {
            let json = format!(
                r#"{{"text_px":17.0,"qso_w":380.0,"modes":["JS8 Normal"],"theme":{theme}}}"#
            );
            let mut app = App::base((0.0, 4000.0));
            app.theme = Theme::Dark;
            app.apply(serde_json::from_str(&json).unwrap());
            assert_eq!(app.theme, Theme::Auto, "{theme} should not have been honoured");
            assert_eq!(app.text_px, 17.0, "{theme} took the other settings down with it");
        }

        // and a file from before the setting existed
        let mut app = App::base((0.0, 4000.0));
        app.theme = Theme::Dark;
        app.apply(serde_json::from_str(r#"{"text_px":17.0}"#).unwrap());
        assert_eq!(app.theme, Theme::Auto);
    }

    /// The palette is pitched for a dark panel, so on a light one every accent
    /// has to come down far enough to read against near-white.
    #[test]
    fn light_mode_darkens_what_would_otherwise_wash_out() {
        for (m, _) in App::base((0.0, 4000.0)).modes {
            let dark = mode_text_color(true, m);
            let light = mode_text_color(false, m);
            assert_eq!(dark, mode_color(m), "the dark palette must be left alone");
            // Rec. 601 luma is enough to say "this is not nearly white".
            let luma = |c: Color32| {
                0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32
            };
            assert!(
                luma(light) < 128.0,
                "{} stays at luma {:.0} in light mode",
                m.name(),
                luma(light)
            );
        }
    }

    /// The mode a hand-opened QSO starts in survives a restart, and a name this
    /// build does not know falls back rather than taking the file down.
    #[test]
    fn an_unknown_default_mode_falls_back() {
        // Genuinely unparseable: an Olivia name is any tone/bandwidth pair the
        // codec can build, so "Olivia 128/2000" is a real mode, not a typo.
        for name in [r#""JS8 Hyper""#, r#""aubergine""#, r#""""#] {
            let json = format!(r#"{{"text_px":17.0,"default_mode":{name}}}"#);
            let mut app = App::base((0.0, 4000.0));
            app.default_mode = ModeId::Olivia(ragchew::olivia::OL_8_250);
            app.apply(serde_json::from_str(&json).unwrap());
            assert_eq!(app.default_mode, DEFAULT_QSO_MODE, "{name} should not have been honoured");
            assert_eq!(app.text_px, 17.0, "{name} took the other settings with it");
        }
        // and a file from before the setting existed
        let mut app = App::base((0.0, 4000.0));
        app.default_mode = ModeId::Olivia(ragchew::olivia::OL_8_250);
        app.apply(serde_json::from_str(r#"{"text_px":17.0}"#).unwrap());
        assert_eq!(app.default_mode, DEFAULT_QSO_MODE);
    }

    /// The + button opens a QSO in the chosen mode, whether or not that mode is
    /// one of the ones being scanned for.
    #[test]
    fn a_hand_opened_qso_takes_the_default_mode() {
        let mut app = App::base((0.0, 4000.0));
        let wanted = ModeId::Olivia(ragchew::olivia::OL_32_1000);
        app.default_mode = wanted;
        // scanning for something else entirely
        for (m, on) in app.modes.iter_mut() {
            *on = matches!(m, ModeId::Js8(_));
        }
        app.qsos.open_blank((app.vp.f_lo + app.vp.f_hi) / 2.0, app.default_mode, 0.0);
        assert_eq!(app.qsos.qsos()[0].mode, wanted);
    }

    /// A settings file written before the view was saved leaves it alone: the
    /// whole band, as it always opened.
    #[test]
    fn settings_without_a_saved_view_open_on_the_whole_band() {
        let json = r#"{"text_px":14.0,"waterfall_w":300.0,"span_s":45.0,"scroll_secs":1.0,
                       "speed":8.0,"modes":[],"input_device":null,"show_all_inputs":false}"#;
        let mut app = App::base((0.0, 4000.0));
        app.set_view(1000.0, 1100.0);
        app.apply(serde_json::from_str(json).unwrap());
        assert_eq!((app.vp.f_lo, app.vp.f_hi), (0.0, 4000.0));
    }

    /// Whatever is in the settings file, the app opens somewhere it can be
    /// navigated out of: inside the band, and wide enough to see.
    #[test]
    fn a_restored_view_is_fitted_to_the_band() {
        let band = (0.0, 4000.0);
        let mut app = App::base(band);
        for (lo, hi) in [
            (1200.0, 1400.0),        // the ordinary case, kept as-is
            (-5000.0, 9000.0),       // wider than the band
            (3900.0, 4600.0),        // hanging off the top
            (-600.0, 200.0),         // hanging off the bottom
            (1500.0, 1500.5),        // narrower than the zoom limit
            (2000.0, 1000.0),        // inverted
            (f64::NAN, f64::NAN),    // and not a number at all
            (0.0, f64::INFINITY),    //
        ] {
            app.set_view(lo, hi);
            let (l, h) = (app.vp.f_lo, app.vp.f_hi);
            assert!(l >= band.0 && h <= band.1, "{lo}..{hi} restored as {l}..{h}, outside the band");
            assert!(h - l >= MIN_SPAN_HZ, "{lo}..{hi} restored as {l}..{h}, too narrow to use");
        }
        // a view that was already sane is not "fixed"
        app.set_view(1200.0, 1400.0);
        assert_eq!((app.vp.f_lo, app.vp.f_hi), (1200.0, 1400.0));
    }

    /// A settings file from another build must not be able to brick startup.
    #[test]
    fn unusable_saved_settings_fall_back() {
        let mut app = App::base((0.0, 4000.0));
        let all = app.enabled_modes().len();

        // every mode name unrecognised — scanning for nothing would look like a
        // broken app, so the default set stands
        app.apply(Settings { modes: vec!["Hellschreiber".into()], ..Settings::default() });
        assert_eq!(app.enabled_modes().len(), all);

        // and an empty list likewise
        app.apply(Settings { modes: Vec::new(), ..Settings::default() });
        assert_eq!(app.enabled_modes().len(), all);

        // absurd values are clamped rather than trusted
        app.apply(Settings {
            text_px: 1e6,
            span_s: -3.0,
            speed: 0.0,
            forget_min: -60.0,
            fade_secs: 1e6,
            ..Settings::default()
        });
        assert!((8.0..=22.0).contains(&app.text_px), "text_px {}", app.text_px);
        assert!((10.0..=120.0).contains(&app.span_s), "span_s {}", app.span_s);
        assert!((1.0..=20.0).contains(&app.speed), "speed {}", app.speed);
        // A negative timeout is not "forget everything at once": it comes back
        // as the zero that means keep it all.
        assert_eq!(app.forget_secs(), None, "forget_min {}", app.forget_min);
        assert!((0.0..=MAX_FADE_S).contains(&app.fade_secs), "fade_secs {}", app.fade_secs);
    }

    /// Lay the whole interface out, QSO panel and all, with no window and no
    /// GPU behind it.
    ///
    /// egui is pure layout until something rasterises it, so this runs in CI
    /// where the app itself cannot. It is a smoke test, not a screenshot: what
    /// it catches is a panic, a bad rect, or a widget id clash in a panel that
    /// otherwise only gets exercised by someone sitting in front of it.
    fn lay_out(app: &mut App) -> egui::FullOutput {
        lay_out_pointed_at(app, None).1
    }

    /// Lay the window out with the pointer somewhere, and hand back where the
    /// window ended up as well as what it drew.
    ///
    /// The size is the one `lay_out` uses, so a caller placing a pointer can
    /// work out what it is pointing at.
    fn lay_out_pointed_at(app: &mut App, at: Option<Pos2>) -> (Rect, egui::FullOutput) {
        let window = Rect::from_min_size(Pos2::ZERO, egui::vec2(1280.0, 800.0));
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(window),
            events: at.map(egui::Event::PointerMoved).into_iter().collect(),
            ..Default::default()
        };
        (window, ctx.run_ui(input, |ui| app.draw(ui)))
    }


    /// A dial under a hand: one context, one frame per gesture, so that hover
    /// and focus mean what they mean in a running window.
    ///
    /// Everything about this widget depends on state carried between frames —
    /// egui resolves hover against the frame before, focus outlives a frame,
    /// and the selection is deliberately sticky — so a helper that builds a
    /// fresh context per frame (which is what `lay_out` does) can test none of
    /// it.
    struct Hand {
        ctx: egui::Context,
        window: Rect,
        at: Pos2,
    }

    impl Hand {
        fn new() -> Hand {
            Hand {
                ctx: egui::Context::default(),
                window: Rect::from_min_size(Pos2::ZERO, egui::vec2(1280.0, 800.0)),
                at: Pos2::new(-100.0, -100.0),
            }
        }

        fn frame(&mut self, app: &mut App, events: Vec<egui::Event>) -> egui::FullOutput {
            let input = egui::RawInput {
                screen_rect: Some(self.window),
                events,
                ..Default::default()
            };
            self.ctx.clone().run_ui(input, |ui| app.draw(ui))
        }

        /// Move the pointer there and let the window see it.
        fn hover(&mut self, app: &mut App, at: Pos2) -> egui::FullOutput {
            self.at = at;
            self.frame(app, vec![egui::Event::PointerMoved(at)])
        }

        fn wheel(&mut self, app: &mut App, notches: f32) {
            self.frame(
                app,
                vec![egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, notches * SCROLL_NOTCH),
                    modifiers: egui::Modifiers::NONE,
                    phase: egui::TouchPhase::Move,
                }],
            );
        }

        fn click(&mut self, app: &mut App) {
            let at = self.at;
            let mut events = vec![egui::Event::PointerMoved(at)];
            events.extend(
                [true, false]
                    .into_iter()
                    .map(|pressed| egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    }),
            );
            self.frame(app, events);
        }

        fn type_text(&mut self, app: &mut App, text: &str) {
            self.frame(app, vec![egui::Event::Text(text.to_string())]);
        }

        /// Where a digit is drawn, in this hand's own window.
        ///
        /// Measured here rather than from a `lay_out` elsewhere because a
        /// layout is a property of the context that produced it: a coordinate
        /// taken from one context can be a few points out in another, which for
        /// a control the size of a digit is the difference between hitting it
        /// and missing.
        fn digit(&mut self, app: &mut App, from_right: usize) -> Pos2 {
            // Twice: the first frame of a context lays out, the second settles.
            let at = self.at;
            self.frame(app, vec![egui::Event::PointerMoved(at)]);
            let out = self.frame(app, vec![egui::Event::PointerMoved(at)]);
            let (origin, galley) = out
                .shapes
                .iter()
                .find_map(|cs| match &cs.shape {
                    egui::Shape::Text(t)
                        if t.galley.text().matches('.').count() == 2 && t.pos.x < 400.0 =>
                    {
                        Some((t.pos, t.galley.clone()))
                    }
                    _ => None,
                })
                .expect("the dial was not drawn");
            let row = galley.rows.first().expect("the reading had no row");
            let glyph = row
                .glyphs
                .iter()
                .rev()
                .filter(|g| g.chr.is_ascii_digit())
                .nth(from_right)
                .expect("that digit is not on the dial");
            // `glyph.pos` is a baseline position, so only its x is of use; the
            // widget lays its boxes down the middle of the row.
            egui::pos2(
                origin.x + glyph.pos.x + glyph.advance_width / 2.0,
                origin.y + app.text_px * VFO_SCALE / 2.0,
            )
        }

        /// The middle of a drawn piece of text, for clicking the widget it
        /// labels. Laid out twice for the same reason [`Hand::digit`] is.
        fn text_at(&mut self, app: &mut App, want: &str) -> Option<Pos2> {
            let at = self.at;
            self.frame(app, vec![egui::Event::PointerMoved(at)]);
            let out = self.frame(app, vec![egui::Event::PointerMoved(at)]);
            out.shapes.iter().find_map(|cs| match &cs.shape {
                egui::Shape::Text(t) if t.galley.text() == want => {
                    Some(t.pos + t.galley.size() / 2.0)
                }
                _ => None,
            })
        }

        fn press(&mut self, app: &mut App, key: egui::Key) {
            self.frame(
                app,
                vec![egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        }
    }

    /// An app with a dial on it, reading `hz`.
    fn app_with_a_dial(hz: f64) -> App {
        let mut app = app_with_a_waterfall(30.0);
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.rig = rig::Keying::RigCtld { host: "x".into(), port: 1 };
        app.ptt =
            Some(rig::Ptt::with_transmitter(Box::new(Dial(flag, hz)), rig::Limits::default()));
        let began = std::time::Instant::now();
        while app.ptt_state().dial_hz.is_none() {
            assert!(began.elapsed() < std::time::Duration::from_secs(2), "the dial never read");
            std::thread::sleep(std::time::Duration::from_millis(5));
            lay_out(&mut app);
        }
        app
    }



    /// The play/pause button must not change size or shift its icon when it is
    /// clicked: the bar is one row, and a control that twitches under the
    /// cursor is the first thing the eye goes to.
    ///
    /// Both faces are asked for from the same `Ui` and compared. The icon is
    /// painted rather than typed precisely so this holds — the glyphs ⏸ and ▶
    /// come from different bundled fonts at different em heights.
    #[test]
    fn the_transport_button_does_not_twitch_when_clicked() {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1280.0, 800.0))),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            elegance::Theme::slate().install(ui.ctx());
            let playing = transport_button(ui, true).rect;
            let paused = transport_button(ui, false).rect;
            assert_eq!(
                playing.size(),
                paused.size(),
                "the button is {:?} playing and {:?} paused",
                playing.size(),
                paused.size()
            );
        });
    }

    /// Playback must advance at every speed, including the slowest.
    ///
    /// The slider is shown a copy of the clock for exactly this reason: it
    /// writes its value back every frame, so anything that quantises what it
    /// is given — `fixed_decimals`, a step — quantises the clock. At 1x the
    /// clock gained 0.017 s per frame and lost all of it again to rounding.
    /// The waterfall sat still and the app looked dead.
    #[test]
    fn the_playback_clock_advances_at_every_speed() {
        for speed in [1.0f32, 2.0, 8.0, 20.0] {
            let mut app = app_with_traffic(0.0);
            app.duration_s = 600.0;
            app.speed = speed;
            app.playing = true;
            app.t_now = 30.0;
            let start = app.t_now;
            for _ in 0..5 {
                lay_out(&mut app);
            }
            assert!(
                app.t_now > start,
                "at {speed}x the clock stayed at {} over five frames",
                app.t_now
            );
        }
    }

    /// The playback thumb must land on whole physical pixels, at every zoom
    /// and every clock value, or it is anti-aliased differently at each offset
    /// and appears to change shape as it slides.
    #[test]
    fn the_playback_thumb_lands_on_pixels() {
        const TRACK_W: f32 = 170.0;
        for ppp in [1.0f32, 1.5, 2.0] {
            let px = (TRACK_W * ppp) as f64;
            for span in [0.5f64, 12.7, 600.0] {
                let mut phase = None;
                for i in 0..200 {
                    let t = span * (i as f64) / 199.0;
                    let shown = snap_to_pixels(t, span, TRACK_W, ppp);
                    assert!((shown - t).abs() <= span / px, "{shown} is not near {t}");
                    // Where the thumb sits, in physical pixels along the track.
                    let at = shown / span * px;
                    let frac = (at - at.round()).abs();
                    assert!(frac < 1e-6, "at {t} the thumb sits {frac} of a pixel off the grid");
                    let ph = *phase.get_or_insert(frac);
                    assert!((ph - frac).abs() < 1e-6, "the sub-pixel phase moved");
                }
            }
        }
        // Degenerate spans are passed through rather than dividing by zero.
        assert_eq!(snap_to_pixels(3.0, 0.0, TRACK_W, 1.0), 3.0);
        assert_eq!(snap_to_pixels(3.0, f64::NAN, TRACK_W, 1.0), 3.0);
    }

    /// The waterfall texture must divide its span into a whole number of
    /// columns per pixel, because that is the only arrangement in which a new
    /// column of data is a whole pixel of travel and the image can be scrolled
    /// rather than redrawn.
    #[test]
    fn the_waterfall_texture_divides_its_span_exactly() {
        // 45 s of span at 25 columns a second, against a range of panels
        for px_w in [37usize, 80, 300, 700, 1125, 4000] {
            for span in [250usize, 1000, 1125, 3000] {
                let (per_px, tex_w) = texture_geometry(span, px_w);
                assert!(per_px >= 1 && tex_w >= 1, "degenerate geometry for {px_w}/{span}");
                assert_eq!(
                    (tex_w * per_px) % per_px,
                    0,
                    "the span shown must be whole pixels at {px_w}/{span}"
                );
                // The window shown is short of the one asked for by less than a
                // single pixel of time.
                let shown = tex_w * per_px;
                assert!(
                    span - shown < per_px,
                    "{px_w}/{span}: showing {shown} of {span} columns loses more than a pixel"
                );
                // And the texture stays within sight of the panel it fills.
                assert!(tex_w <= span.max(1), "texture wider than the data at {px_w}/{span}");
            }
        }
        // Degenerate sizes must not divide by zero.
        assert_eq!(texture_geometry(0, 0), (1, 1));
        assert_eq!(texture_geometry(1125, 0), (1125, 1));
    }

    /// The waterfall must keep moving once the spectrogram is full.
    ///
    /// It is capped at eight minutes of columns, and from then on its length
    /// stops changing while the newest column still arrives and the oldest
    /// falls off the front. Tracking progress by that length freezes the
    /// picture — it then only moves when something else forces a redraw, which
    /// arrives as a jump.
    #[test]
    fn the_waterfall_keeps_moving_when_the_spectrogram_is_full() {
        let ctx = egui::Context::default();
        let spec = Spectrogram::compute(&crate::demo::synth(), 0.0, 4000.0, WINDOW / 4);
        let (w, h, span) = (120usize, 64usize, 600usize);
        let (per_px, _) = texture_geometry(span, w);
        let mut app = App::base((0.0, 4000.0));
        app.norm = waterfall::percentiles(&spec);

        // A full spectrogram: the index of the newest column never changes.
        let col_hi = spec.columns.len() as i64;
        let travel = 100_000i64;
        let _ = app.waterfall_texture(&ctx, &spec, [w, h], col_hi, travel, span);
        let first = app.wf_img.as_ref().expect("an image").rgba.clone();

        // Nothing has travelled: the picture is already right.
        let _ = app.waterfall_texture(&ctx, &spec, [w, h], col_hi, travel, span);
        assert_eq!(app.wf_img.as_ref().unwrap().rgba, first, "redrawn for no reason");

        // A pixel's worth of data has gone by, with the index still standing
        // still. The picture has to move.
        let _ = app.waterfall_texture(&ctx, &spec, [w, h], col_hi, travel + per_px as i64, span);
        assert_ne!(
            app.wf_img.as_ref().unwrap().rgba,
            first,
            "the waterfall stopped moving once the spectrogram was full"
        );
        assert_eq!(app.wf_col, travel + per_px as i64, "travel was not recorded");
    }

    /// Every text the app drew, with where it was drawn.
    fn drawn(out: &egui::FullOutput) -> Vec<(String, f32)> {
        out.shapes
            .iter()
            .filter_map(|cs| match &cs.shape {
                egui::Shape::Text(t) => Some((t.galley.text().to_string(), t.pos.y)),
                _ => None,
            })
            .collect()
    }

    /// What the app has to say goes along the foot of the window, with the
    /// channel count, and not on the toolbar.
    ///
    /// The toolbar has to stay on one line and was competing for it with the
    /// controls, so a message long enough to be useful — "could not listen to
    /// Firefox: …" — had nowhere to go.
    #[test]
    fn what_the_app_has_to_say_goes_at_the_foot_of_the_window() {
        let mut app = app_with_traffic(30.0);
        app.say("listening to Firefox — Northern Utah WebSDR");
        let out = lay_out(&mut app);
        let drawn = drawn(&out);

        let find = |needle: &str| {
            drawn
                .iter()
                .find(|(s, _)| s.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} was not drawn at all; saw {drawn:?}"))
                .1
        };
        let msg = find("listening to Firefox");
        let count = find("threads");

        // 800 tall, per `lay_out`. Both on the same row at the bottom.
        assert!(msg > 700.0, "the message sits at y={msg} of 800, not at the foot");
        assert!(
            (msg - count).abs() < 1.0,
            "the message ({msg}) and the thread count ({count}) are not on one line"
        );
    }

    /// News that something worked is not dressed up as a fault.
    ///
    /// Both went through one `Option<String>` before, so "listening to Firefox"
    /// came out under the same red warning triangle as a failed open — the app
    /// announcing good news as a problem.
    #[test]
    fn good_news_is_not_shown_as_a_warning() {
        let mut app = app_with_traffic(30.0);
        app.say("listening to Firefox");
        assert!(
            drawn(&lay_out(&mut app)).iter().any(|(s, _)| s == "listening to Firefox"),
            "an ordinary note should be drawn as it was written"
        );

        let mut app = app_with_traffic(30.0);
        app.warn("could not listen to Firefox");
        assert!(
            drawn(&lay_out(&mut app)).iter().any(|(s, _)| s == "⚠ could not listen to Firefox"),
            "a fault should be marked as one"
        );
    }

    /// The whole composer sits at the head of the QSO panel — the reply box as
    /// well as the button, one directly above the other.
    ///
    /// Both halves, because they came apart once: a layout change left the
    /// reply box up against the log while the button sat at the far end of the
    /// panel, and a test that looked only for "Send" passed it.
    ///
    /// At the head rather than the foot because of where the hand is. The dial
    /// and the key are drawn directly above this panel, and working a station
    /// means moving between those and this box — with the composer at the foot,
    /// the whole length of the log sat between the three controls used
    /// together.
    #[test]
    fn the_composer_sits_at_the_head_of_the_qso_panel() {
        let mut app = app_with_traffic(30.0);
        lay_out(&mut app);
        let channels = app.channels_now();
        app.qsos.open_for_channel(&channels.channels()[0], app.t_now);

        let out = lay_out(&mut app);
        let (mut reply_y, mut send_y, mut highest_log_y) = (None, None, f32::MAX);
        let mut tab_y = None;
        for cs in &out.shapes {
            if let egui::Shape::Text(t) = &cs.shape {
                match t.galley.text() {
                    "Send" => send_y = Some(t.pos.y),
                    "reply…" => reply_y = Some(t.pos.y),
                    // The tab strip is the ceiling: the composer belongs under
                    // the tabs, not over them.
                    // The new-QSO button, which is the top of the tab strip.
                    s if s.trim() == "+" => tab_y = Some(t.pos.y),
                    // A log line by its offset stamp — "+  0:05" — which
                    // nothing else in the panel draws. Matching the station's
                    // call would find the tab it labels, at the very top.
                    // A log line by its offset stamp — "+  0:00" — which
                    // nothing else in the panel draws. Matching the station's
                    // call would find the tab it labels, at the very top.
                    s if s.trim_start().starts_with('+') && s.contains(':') => {
                        highest_log_y = highest_log_y.min(t.pos.y)
                    }
                    _ => {}
                }
            }
        }
        let send_y = send_y.expect("the Send button should be drawn");
        let reply_y = reply_y.expect("the reply box should be drawn");
        assert!(reply_y < send_y, "the reply box should be above its button");
        assert!(
            send_y - reply_y < 80.0,
            "the reply box at {reply_y} and its button at {send_y} have come apart"
        );
        if let Some(tab_y) = tab_y {
            assert!(reply_y > tab_y, "the composer at {reply_y} is above the tabs at {tab_y}");
        }
        assert!(
            highest_log_y < f32::MAX && highest_log_y > send_y,
            "the log ({highest_log_y}) should be below the composer ({send_y})"
        );
    }





    /// A PSK over stays one log entry even when the clock jumps over it, and
    /// the whole of it is drawn.
    ///
    /// This is the end-to-end version of what `qso.rs` checks with decodes it
    /// places by hand: the real demo band, the real frame loop, and the real
    /// character times the decoder produces. It regressed once, and visibly —
    /// the entry that was open when the tab was opened stopped growing at 46
    /// characters while the rest of the over went into a second one. The cause
    /// was timing a silence between two absorbs, which measures how often the
    /// app looked rather than how long the station stopped for.
    #[test]
    fn a_psk_over_survives_a_jump_of_the_clock() {
        let mut app = App::base((300.0, 2600.0));
        let samples = crate::demo::synth();
        app.frames =
            protocol::decode_all(&samples, 300.0, 2600.0, &[ModeId::Psk(ragchew::psk::PSK31)])
                .into_iter()
                .map(|d| Frame { reveal_s: d.time_s + d.mode.chunk_secs(), decode: d })
                .collect();
        app.duration_s = 60.0;
        app.samples = Arc::new(samples);

        // Open the QSO part-way in, then jump the clock past the end of the
        // over in one frame — scrubbing a recording, or a window that was not
        // being drawn while the app was in the background.
        app.t_now = 12.0;
        lay_out(&mut app);
        let early = app.channels_now();
        app.qsos.open_for_channel(&early.channels()[0], app.t_now);
        let opened_with = app.qsos.qsos()[0].log[0].text.chars().count();
        app.t_now = 58.0;
        let out = lay_out(&mut app);

        let q = &app.qsos.qsos()[0];
        assert_eq!(q.log.len(), 1, "the over split into {} entries", q.log.len());
        let text = q.log[0].text.clone();
        assert!(
            text.chars().count() > opened_with * 3,
            "the entry stopped growing: {} characters, opened with {opened_with}",
            text.chars().count()
        );
        assert_eq!(q.call, "KB9PSK");

        // And it reaches the screen. A log the panel cannot draw is not a log.
        let drawn: usize = out
            .shapes
            .iter()
            .filter_map(|cs| match &cs.shape {
                egui::Shape::Text(t) if t.galley.text().contains("kb9psk") => {
                    Some(t.galley.text().chars().count())
                }
                _ => None,
            })
            .sum();
        assert_eq!(drawn, text.chars().count(), "the panel drew {drawn} of {} characters", text.chars().count());
    }


    /// The seams between joined frames reach the screen, in the text and not
    /// beside it.
    ///
    /// The bar is drawn as part of the entry's own galley so the paragraph
    /// wraps at the panel edge rather than at every join — which means it has
    /// to turn up *inside* the laid-out text, and a test that only counted
    /// shapes would not know the difference.
    #[test]
    fn joined_frames_are_drawn_with_their_seams() {
        let mut app = app_with_traffic(40.0);
        lay_out(&mut app);
        let channels = app.channels_now();
        app.qsos.open_for_channel(&channels.channels()[0], app.t_now);
        let out = lay_out(&mut app);

        let q = &app.qsos.qsos()[0];
        assert_eq!(q.log.len(), 1, "the two frames did not join");
        assert_eq!(q.log[0].marks.len(), 1, "no seam between them");

        let drawn: String = out
            .shapes
            .iter()
            .filter_map(|cs| match &cs.shape {
                egui::Shape::Text(t) if t.galley.text().contains("FN20") => {
                    Some(t.galley.text().to_string())
                }
                _ => None,
            })
            .collect();
        assert!(!drawn.is_empty(), "the joined entry never reached the screen");
        assert!(drawn.contains('│'), "the entry was drawn without its seam: {drawn:?}");
        // The text itself stays what the station said, bars and all being the
        // panel's business: what is logged and copied has none in it.
        assert!(!q.log[0].text.contains('│'), "a seam got into the log text");

        // And a long over still wraps inside the panel. The seams are drawn by
        // laying the whole entry out as one galley, which is what keeps the
        // paragraph wrapping at the panel edge — a row of labels, one per
        // frame, would wrap at the seams instead and would pass every
        // assertion above.
        let long = "FN20 5W ".repeat(20);
        let marks = (1..8).map(|i| (i * 40, i as f64)).collect();
        app.qsos.qsos_mut()[0].log[0] = crate::qso::Entry {
            at_s: 0.0,
            dir: Dir::Rx,
            text: long,
            marks,
            bursts: Vec::new(),
            cut: false,
        };
        let out = lay_out(&mut app);
        let widest = out
            .shapes
            .iter()
            .filter_map(|cs| match &cs.shape {
                egui::Shape::Text(t) if t.galley.text().contains("FN20") => {
                    Some(t.galley.size().x)
                }
                _ => None,
            })
            .fold(0.0_f32, f32::max);
        assert!(widest > 0.0, "the long entry never reached the screen");
        assert!(widest <= app.qso_w, "the entry laid out {widest:.0} wide in {} points", app.qso_w);
    }

    /// A station's fading reaches the screen, and only once there is fading to
    /// show.
    ///
    /// The sparkline is the one thing in the QSO panel that draws no text, so a
    /// broken one is invisible to every other test here — and it is drawn from
    /// a series the panel is handed rather than one it can see being built.
    /// Counting the paths it puts on the screen is what tells the two apart.
    #[test]
    fn a_fading_station_is_drawn_in_the_qso_panel() {
        let paths = |out: &egui::FullOutput| {
            out.shapes.iter().filter(|cs| matches!(cs.shape, egui::Shape::Path(_))).count()
        };

        let mut app = app_with_traffic(30.0);
        lay_out(&mut app);
        let channels = app.channels_now();
        app.qsos.open_for_channel(&channels.channels()[0], app.t_now);
        // Nothing measured yet: the panel is the same panel it was.
        let bare = paths(&lay_out(&mut app));

        // Twenty frames of a station swinging 14 dB, ending at the clock.
        app.qsos.qsos_mut()[0].snr_history = (0..20)
            .map(|i| (app.t_now - (19 - i) as f64 * 1.5, -10.0 + 7.0 * (i as f32 * 0.7).sin()))
            .collect();
        let fading = paths(&lay_out(&mut app));
        assert!(fading > bare, "the SNR history drew nothing ({bare} paths either way)");

        // It has a row to itself, so the narrowest panel the operator can drag
        // to is still a panel it lays out in. The plot takes the width it is
        // offered there rather than the width it would like.
        app.qso_w = MIN_QSO_W;
        assert!(paths(&lay_out(&mut app)) > bare, "nothing drawn in a narrow panel");

        // Four samples is not a shape, and the row does not appear for it.
        app.qsos.qsos_mut()[0].snr_history.truncate(sparkline::MIN_POINTS - 1);
        assert_eq!(paths(&lay_out(&mut app)), bare, "a plot was drawn from too little");
    }

    /// The file view reconciles what its per-mode scans found, once they are
    /// all in.
    ///
    /// Each scan runs one mode, so no thread can see that its PSK decode sits
    /// on top of a narrow Olivia signal — `tests/psk.rs` has the band where
    /// that really happens. This checks the wiring: that the app applies the
    /// rule when the last scan lands, and rebuilds its reveal times from what
    /// survived.
    #[test]
    fn the_file_view_arbitrates_across_its_per_mode_scans() {
        let mut app = App::base((300.0, 2600.0));
        let d = |mode: ModeId, hz: f64, at: f64, text: &str| protocol::Decode {
            snr_db: None,
            mode,
            hz,
            time_s: at,
            quality: 4.0,
            text: text.to_string(),
            ends_over: false,
        };
        let olivia = ModeId::Olivia(ragchew::olivia::OL_4_125);
        let psk = ModeId::Psk(ragchew::psk::PSK31);
        app.frames = [
            // The Olivia station, from Olivia's thread.
            d(olivia, 745.0, 6.0, "CQ DE TEST K "),
            // The same signal read as PSK, from PSK's thread.
            d(psk, 749.0, 6.2, "n teIer"),
            // A real station elsewhere, which must survive.
            d(psk, 1500.0, 7.0, "cq de w1aw "),
        ]
        .into_iter()
        .map(|d| Frame { reveal_s: d.time_s + d.mode.chunk_secs(), decode: d })
        .collect();
        app.pending_scans = 0;

        app.arbitrate_scans();

        // What survived, in frequency order rather than the list's: the frames
        // are kept in the order they are revealed, and a decode's reveal time
        // is its own mode's, so the list order is not the order they were
        // scanned in. See [`App::channels_now`] for why that matters.
        let mut kept: Vec<(ModeId, f64)> =
            app.frames.iter().map(|f| (f.decode.mode, f.decode.hz)).collect();
        kept.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        assert_eq!(kept, [(olivia, 745.0), (psk, 1500.0)], "kept {kept:?}");
        // Reveal times are rebuilt with the frames, not left behind.
        for f in &app.frames {
            let want = f.decode.time_s + f.decode.mode.chunk_secs();
            assert!((f.reveal_s - want).abs() < 1e-9, "reveal time not rebuilt");
        }
    }

    /// A band with two stations in it, revealed at `t`.
    /// A row being typed into may only ever move left.
    ///
    /// Text is right-aligned on its newest character, so the slide is what
    /// keeps the characters already on screen still while the row grows past
    /// them. What it must never do is put one back where it was a frame ago —
    /// that is text springing right, and doing it five times a second is what
    /// a flickering row is.
    ///
    /// The ease this replaced sprang for a reason worth keeping in view. Its
    /// offset was a function of the *age* of the newest text, and a decode's
    /// age when it reaches the screen is its own pipeline delay: measured over
    /// a real capture, a median of 0.14 s with a tail past 0.3 s. So each
    /// character landed at a different point along the ease from the last, and
    /// the row jumped back and forth by the difference. A debt cannot do that,
    /// because it never reads the clock — it only ever counts characters.
    #[test]
    fn a_streaming_row_only_ever_moves_left() {
        let mut app = App::base((0.0, 4000.0));
        app.scroll_secs = 1.0;
        let char_w = 8.0;
        let psk = ModeId::Psk(ragchew::psk::PSK31);
        let chunk = psk.chunk_secs();

        // Sixty frames a second, a character every fifth of a second, each
        // reaching the screen after a delay of its own: a live PSK31 station.
        let delay = |k: usize| if k.is_multiple_of(3) { 0.32 } else { 0.12 };
        let mut set = ChannelSet::new(15.0);
        let mut typed = 0usize;
        let mut worst = f32::INFINITY;
        let mut sprang_under_the_ease = 0.0f32;
        let mut was_under_the_ease = f32::INFINITY;
        for frame in 1..600 {
            let t = frame as f64 / 60.0;
            let sent = typed as f64 / 5.0;
            if t >= sent + delay(typed) {
                set.add(protocol::Decode {
                    snr_db: None,
                    mode: psk,
                    hz: 1500.0,
                    time_s: sent,
                    quality: 30.0,
                    text: "e".to_string(),
                    ends_over: false,
                });
                typed += 1;
            }
            app.slide_rows(&set, char_w, 1.0 / 60.0, 400.0);
            let Some(ch) = set.channels().first() else { continue };

            // Where the oldest character on the row sits, relative to the
            // row's right edge: everything typed since has pushed it left by a
            // width apiece, and the slide holds it back by what is still owed.
            let held = ch.next_index() as f32 * char_w;
            let x = app.slide[&ch.id].px - held;
            assert!(x <= worst + 1e-3, "frame {frame}: the row sprang {:.2} px right", x - worst);
            worst = worst.min(x);

            // And the same character under the ease, which is where the
            // springing came from. Kept as arithmetic rather than as a second
            // implementation: it is here to say how much this changed.
            let age = t - (ch.last_heard_s + chunk);
            let eased = if (0.0..app.scroll_secs as f64).contains(&age) {
                (1.0 - age as f32 / app.scroll_secs).powi(3) * ch.last_chunk_chars as f32 * char_w
            } else {
                0.0
            };
            let x_eased = eased - held;
            sprang_under_the_ease = sprang_under_the_ease.max(x_eased - was_under_the_ease);
            was_under_the_ease = was_under_the_ease.min(x_eased);
        }

        assert!(typed > 40, "only {typed} characters were typed; the test proved little");
        assert!(
            sprang_under_the_ease > 1.0,
            "the ease sprang by only {sprang_under_the_ease:.2} px, so this test is no longer \
             about the fault it was written for"
        );
    }

    /// Playing a file forward only ever adds to what a row says.
    ///
    /// The file view shows "everything revealed so far", and it used to work
    /// that out from scratch on every frame it drew. That cannot be stable.
    /// A channel set depends on the order it is fed, and the order decodes are
    /// *revealed* in is not the order they were *sent* in: a JS8 Slow frame is
    /// revealed thirty seconds after the cycle it belongs to, so it arrives in
    /// the middle of a time-ordered rebuild and every station created after it
    /// comes out differently — different text on the row, and a different
    /// channel id under the slide, the clears and any open conversation.
    ///
    /// On the demo band it happened within the first few seconds and kept
    /// happening: rows losing two thirds of their text and filling in again.
    /// So: play it through, and hold each channel to the two things a reader
    /// is entitled to assume — that a row keeps its station, and that what it
    /// has said only ever grows.
    #[test]
    fn playing_a_file_forward_only_ever_adds_to_a_row() {
        let mut app = App::base((300.0, 2600.0));
        let samples = crate::demo::synth();
        // One mode revealed long after it was sent, one revealed as it is sent,
        // and one in between: the mismatch between the two orders is the whole
        // subject, and six Olivia modes would only make it slower to show.
        let modes = [
            ModeId::Js8(ragchew::js8::Mode::Slow),
            ModeId::Js8(ragchew::js8::Mode::Normal),
            ModeId::Olivia(ragchew::olivia::OL_8_250),
            ModeId::Psk(ragchew::psk::PSK31),
        ];
        let landing: Vec<Frame> = protocol::decode_all(&samples, 300.0, 2600.0, &modes)
            .into_iter()
            .map(|d| Frame { reveal_s: d.time_s + d.mode.chunk_secs(), decode: d })
            .collect();
        assert!(landing.len() > 50, "only {} decodes; the band was too quiet", landing.len());
        app.frames_landed(landing);
        app.duration_s = samples.len() as f64 / SAMPLE_RATE as f64;

        // (mode, characters said) per channel id, as it was a moment ago.
        let mut was: HashMap<u64, (ModeId, usize)> = HashMap::new();
        let mut grew = 0;
        let mut t = 0.0;
        while t < app.duration_s {
            app.t_now = t;
            for ch in app.channels_now().channels() {
                let now = (ch.mode, ch.next_index());
                if let Some(&(mode, said)) = was.get(&ch.id) {
                    assert_eq!(
                        mode, now.0,
                        "at {t:.1}s channel {} changed from {} to {}",
                        ch.id,
                        mode.name(),
                        now.0.name()
                    );
                    assert!(
                        now.1 >= said,
                        "at {t:.1}s channel {} ({} {:.0} Hz) held {} characters, down from {said}",
                        ch.id,
                        ch.mode.name(),
                        ch.hz,
                        now.1
                    );
                    grew += usize::from(now.1 > said);
                }
                was.insert(ch.id, now);
            }
            t += 0.25;
        }
        assert!(grew > 100, "the rows only grew {grew} times; the test proved little");
    }

    fn a_row(id: u64, hz: f64, drawn: usize, held: usize) -> RowNow {
        RowNow {
            id,
            hz,
            mode: ModeId::Psk(ragchew::psk::PSK31),
            drawn,
            held,
            y: 100.0,
            offset: 0.0,
            cut: f64::NEG_INFINITY,
            oldest: Some(1.0),
        }
    }

    /// The log says when a row loses its text, and says nothing at all while
    /// one is merely being typed into.
    ///
    /// A row that clears and fills in again cannot be caught by eye — it is
    /// over within a frame or two of a station arriving — so the fault has to
    /// be written down as it happens or it cannot be chased at all.
    #[test]
    fn the_log_notices_a_row_losing_its_text() {
        let mut app = App::base((0.0, 4000.0));
        let mut wall = 1000.0;

        for n in 1..40 {
            wall += 1.0 / 60.0;
            let said = app.watch_rows(&[a_row(1, 1500.0, n, n)], wall);
            assert!(said.is_empty(), "complained about a row being typed into: {said:?}");
        }

        wall += 1.0 / 60.0;
        let said = app.watch_rows(&[a_row(1, 1500.0, 2, 2)], wall);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("lost text on screen"), "{}", said[0]);
        assert!(said[0].contains("2 characters drawn where there were 39"), "{}", said[0]);

        // Once a second, not once a frame: a row that flaps has said what it
        // has to say, and sixty lines a second would bury the rest of the log.
        wall += 1.0 / 60.0;
        let said = app.watch_rows(&[a_row(1, 1500.0, 1, 1)], wall);
        assert!(said.is_empty(), "wrote a line every frame: {said:?}");
    }

    /// A row that goes and comes back says so, with how long it was away.
    #[test]
    fn the_log_notices_a_row_coming_back() {
        let mut app = App::base((0.0, 4000.0));
        let mut wall = 1000.0;
        app.watch_rows(&[a_row(1, 1500.0, 40, 40)], wall);

        wall += 3.0; // three seconds of frames in which it was not drawn
        let said = app.watch_rows(&[a_row(1, 1500.0, 40, 40)], wall);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("back after 3.0s off screen"), "{}", said[0]);
    }

    /// A station back under a different channel id is named as that, because it
    /// means something quite different: the channel was rebuilt, so the text
    /// genuinely starts again rather than the row having been redrawn.
    #[test]
    fn the_log_names_a_station_that_came_back_as_a_new_channel() {
        let mut app = App::base((0.0, 4000.0));
        let mut wall = 1000.0;
        app.watch_rows(&[a_row(7, 1500.0, 40, 40)], wall);

        wall += 2.0;
        let said = app.watch_rows(&[a_row(8, 1501.0, 1, 1)], wall);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("come back as channel 8"), "{}", said[0]);
        assert!(said[0].contains("was channel 7"), "{}", said[0]);
    }

    /// What the status bar drew, found by the one thing always on it: the
    /// channel count. Anything on that line is on the bar — which the bottom of
    /// the window is not, since the frequency scale's last label ("0 Hz") sits
    /// just above it and would otherwise be read as a cursor readout.
    fn in_the_status_bar(out: &egui::FullOutput) -> Vec<String> {
        let all = drawn(out);
        let Some((_, bar_y)) = all.iter().find(|(s, _)| s.ends_with(" threads")) else {
            panic!("no thread count, so no status bar to read");
        };
        let bar_y = *bar_y;
        all.iter().filter(|(_, y)| (y - bar_y).abs() < 1.0).map(|(s, _)| s.clone()).collect()
    }

    /// The status bar says what frequency the pointer is on, and says nothing
    /// where a vertical position is not one.
    ///
    /// Over the waterfall it is the view's own mapping, so it agrees with the
    /// scale printed up the side. Left of the strip it is a row position — the
    /// rows have been nudged apart to fit and the leader lines exist to bridge
    /// exactly that gap — so there is no frequency to report and none is
    /// reported.
    #[test]
    fn the_status_bar_reads_the_frequency_under_the_pointer() {
        let mut app = app_with_a_waterfall(30.0);
        app.vp = Viewport { f_lo: 0.0, f_hi: 4000.0 };
        let (window, _) = lay_out_pointed_at(&mut app, None);

        // Over the waterfall, at three heights. The status bar is a panel, so
        // it is laid out before the panorama it reads: the reading appears on
        // the frame after the move, which is why each is asked for twice.
        let x = window.max.x - app.waterfall_w / 2.0;
        let read = |app: &mut App, y: f32| -> f64 {
            lay_out_pointed_at(app, Some(Pos2::new(x, y)));
            let (_, out) = lay_out_pointed_at(app, Some(Pos2::new(x, y)));
            // Picked out by where it is, not by what it says: the readout is
            // bare now, and the frequency scale up the waterfall's edge is full
            // of numbers that would otherwise match.
            let said = in_the_status_bar(&out)
                .into_iter()
                .find(|s| s.ends_with(" Hz"))
                .expect("the status bar said nothing about the pointer");
            let hz: f64 = said
                .trim_end_matches(" Hz")
                .parse()
                .unwrap_or_else(|e| panic!("could not read {said:?}: {e}"));
            // What the bar prints is what the app measured, to the tenth of a
            // hertz it prints to.
            let want = app.cursor_hz.expect("the app did not measure the pointer");
            assert!((hz - want).abs() < 0.05, "the bar said {hz:.1}, the app measured {want:.1}");
            hz
        };

        // The panorama is shorter than the window — a toolbar above it and this
        // very bar below — so the arithmetic worth pinning is not which
        // frequency lands on which pixel but that the axis runs the right way
        // and stays inside the view: high at the top, low at the bottom.
        let high = read(&mut app, window.min.y + 60.0);
        let mid = read(&mut app, window.center().y);
        let low = read(&mut app, window.max.y - 60.0);
        assert!(high > mid && mid > low, "read {high:.0}, {mid:.0}, {low:.0} down the waterfall");
        for hz in [high, mid, low] {
            assert!(
                (app.vp.f_lo..=app.vp.f_hi).contains(&hz),
                "{hz:.0} Hz is outside the {:.0}–{:.0} Hz on show",
                app.vp.f_lo,
                app.vp.f_hi
            );
        }

        // Over the text panel there is no frequency to give.
        let on_a_row = Pos2::new(window.min.x + 10.0, window.center().y);
        lay_out_pointed_at(&mut app, Some(on_a_row));
        let (_, out) = lay_out_pointed_at(&mut app, Some(on_a_row));
        assert_eq!(app.cursor_hz, None, "read a frequency off a row position");
        assert!(
            !in_the_status_bar(&out).iter().any(|s| s.ends_with(" Hz")),
            "the bar answered where there was nothing to answer"
        );
    }

    /// A radio on a cable is remembered as one.
    #[test]
    #[cfg(feature = "elecraft")]
    fn an_elecraft_on_a_cable_survives_a_restart() {
        let mut app = App::base((0.0, 4000.0));
        app.rig_device = "/dev/ttyUSB1".to_string();
        app.rig_baud = 38400;
        app.set_keying(rig::Keying::Elecraft {
            device: "/dev/ttyUSB1".into(),
            baud: 38400,
        });
        let text = serde_json::to_string(&app.settings()).unwrap();

        let mut restored = App::base((0.0, 4000.0));
        restored.apply(serde_json::from_str(&text).unwrap());
        assert_eq!(
            restored.rig,
            rig::Keying::Elecraft { device: "/dev/ttyUSB1".into(), baud: 38400 }
        );
    }

    /// Every row of the menu names a setting, and that setting names the row
    /// back.
    ///
    /// The pair is what lets the segmented control be indexed by position and
    /// the settings file be written by name without the two drifting. A radio
    /// added to [`KEYERS`] and not to [`keying_tag`] would draw a segment that
    /// selects itself, saves as something else, and comes back as the wrong
    /// rig — so this fails the moment half the job is done.
    #[test]
    fn every_way_of_keying_round_trips_through_its_tag() {
        let app = App::base((0.0, 4000.0));
        for k in KEYERS {
            let keying = app.keying_for(k.tag);
            assert_eq!(keying_tag(&keying), k.tag, "{} does not name itself back", k.tag);
        }
        // And the tags are distinct, since a duplicate would make `position`
        // pick the first of them however the second was selected.
        for (i, k) in KEYERS.iter().enumerate() {
            assert!(
                !KEYERS[..i].iter().any(|e| e.tag == k.tag),
                "two ways of keying both called {}",
                k.tag
            );
        }
        // Whatever else is compiled in, doing nothing is always on offer and
        // is always what an unselected control falls back to.
        assert_eq!(KEYERS[0].tag, "none", "the first choice is not the harmless one");
    }

    /// A reading from the rig reaches the screen as the rig gave it.
    ///
    /// This is a bug that shipped. egui's `DragValue` and `Slider` both clamp
    /// the value they are handed, write the clamped number back through the
    /// borrow, and report it as a change — and this app sends changes to the
    /// radio. A rig sitting on a 12 kHz AM filter was therefore narrowed to the
    /// top of whatever range the app happened to be using, and a 50 Hz CW
    /// filter widened to the bottom of it, without anybody asking and every
    /// frame the panel was open.
    ///
    /// The distinction being pinned: this app may decline to *ask* for a value
    /// outside a range, and may never alter one the rig reported.
    #[test]
    fn a_reading_outside_the_range_is_not_altered_or_sent_back() {
        // Deliberately narrow, as the shipped ranges were.
        let (lo, hi) = (0.1, 9.99);
        for reading in [12.0_f64, 0.05, 30.0] {
            let mut khz = reading;
            let mut changed = None;
            egui::__run_test_ui(|ui| {
                changed = Some(ui.add(bandwidth_box(&mut khz, lo, hi)).changed());
            });
            assert_eq!(khz, reading, "rewrote a {reading} kHz reading");
            assert_eq!(changed, Some(false), "reported a {reading} kHz reading as an edit");
        }

        // The same for power, and for the slider, which clamps by its own
        // separate setting and would otherwise put the reading back.
        let mut watts = 1500.0_f64;
        egui::__run_test_ui(|ui| {
            ui.add(power_box(&mut watts, 0.0, 100.0));
        });
        assert_eq!(watts, 1500.0, "rewrote a power reading above the rig's stated maximum");

        let mut wide = 12.0_f64;
        let mut moved = None;
        egui::__run_test_ui(|ui| {
            moved = Some(track(ui, &mut wide, lo, hi));
        });
        assert_eq!(wide, 12.0, "the slider rewrote a reading outside its track");
        assert_eq!(moved, Some(false), "the slider reported an untouched reading as a drag");
    }

    /// The fallback bounds are a guard against a runaway drag, not a claim
    /// about any radio, so nothing a real rig reports comes near them.
    ///
    /// The numbers they replace — 100 Hz to 4 kHz — were a KX3 on a data mode
    /// written down as if it were every radio, which is how a 12 kHz AM filter
    /// came to be narrowed.
    #[test]
    fn the_fallback_bounds_are_wider_than_any_radio() {
        let (lo, hi) = ANY_BANDWIDTH_KHZ;
        assert!(lo < 0.05, "a 50 Hz CW filter is below the floor");
        assert!(hi > 30.0, "a 30 kHz FM filter is above the ceiling");
        let (lo, hi) = ANY_POWER_W;
        assert!(lo <= 0.0, "a rig turned down to nothing is below the floor");
        assert!(hi > 1500.0, "a legal-limit station is above the ceiling");
    }

    /// A control with no slider says why, and one with a slider does not.
    ///
    /// Which settings get a track is the radio's business, not the interface's:
    /// a `rigctld` rig states its power and not its passband, an Elecraft the
    /// other way round. On screen that is one control with a track and its
    /// neighbour without, which reads as a bug unless something says otherwise.
    /// This text is the only thing that does, so it has to appear exactly when
    /// the slider does not.
    #[test]
    fn a_missing_slider_explains_itself() {
        let ranged = hint("transmit power", true);
        assert_eq!(ranged, "transmit power", "explained away a slider that is there");

        let bare = hint("transmit power", false);
        assert!(bare.starts_with("transmit power"), "lost what the setting is: {bare:?}");
        assert!(bare.contains("no track to drag"), "did not say why there is no slider: {bare:?}");
        // The continuation escapes in that literal are easy to get wrong, and a
        // tooltip reading "no  track" or "notrack" would be the sort of thing
        // nobody reports and everybody notices.
        assert!(!bare.contains("  "), "double space in a tooltip: {bare:?}");
        assert!(!bare.contains('\t'), "tab in a tooltip: {bare:?}");
    }

    /// Switching between two radios on a cable keeps the cable.
    ///
    /// The port and the rate are one shared pair rather than one per make, so
    /// changing which radio is on the other end does not ask for them again —
    /// and the settings panel rebuilds whichever make is selected by asking the
    /// current setting for its own tag, rather than naming one.
    #[test]
    #[cfg(all(feature = "elecraft", feature = "yaesu"))]
    fn changing_make_keeps_the_port_and_rate() {
        let mut app = App::base((0.0, 4000.0));
        app.rig_device = "/dev/ttyUSB4".to_string();
        app.rig_baud = 19200;

        let elecraft = app.keying_for("elecraft");
        assert_eq!(
            elecraft,
            rig::Keying::Elecraft { device: "/dev/ttyUSB4".into(), baud: 19200 }
        );

        let yaesu = app.keying_for("yaesu");
        assert_eq!(yaesu, rig::Keying::Yaesu { device: "/dev/ttyUSB4".into(), baud: 19200 });

        // And both are radios the port-and-rate panel offers itself for, which
        // is what the panel asks rather than naming a make.
        assert!(elecraft.on_a_cable() && yaesu.on_a_cable());
        assert!(!rig::Keying::None.on_a_cable());
        assert!(!rig::Keying::RigCtld { host: "x".into(), port: 1 }.on_a_cable());
    }

    /// The rate the settings start at is the one the radio leaves the factory
    /// at. Two constants, in two crates' worth of reasons to change
    /// independently, and only one of them is what the operator first sees.
    #[test]
    #[cfg(feature = "elecraft")]
    fn the_default_rate_is_the_one_the_radio_ships_with() {
        assert_eq!(DEFAULT_RIG_BAUD, rig::elecraft::KX3_BAUD);
    }

    /// Nothing keys a rig because a settings file said something odd.
    ///
    /// This is the one setting that reaches out and moves hardware, so an
    /// unreadable answer has to mean "no" rather than "the last thing that
    /// happened to be in the file". A build compiled without a given radio
    /// meets exactly this case when it reads a file that names one.
    #[test]
    fn an_unreadable_rig_setting_leaves_the_transmitter_alone() {
        let mut app = App::base((0.0, 4000.0));
        app.apply(Settings { rig_keying: "flrig-over-carrier-pigeon".into(), ..Settings::default() });
        assert_eq!(app.rig, rig::Keying::None);
        assert!(app.ptt.is_none(), "started keying something on an unreadable setting");
    }

    /// A build that cannot offer a radio still hands back the port it was on.
    ///
    /// This is what a rig feature being off looks like from the settings file:
    /// a tag with no radio behind it. The choice of rig resets — that is the
    /// point, nothing should key over a link this build does not have — but
    /// the port and the rate are the tedious part to retype, so they come
    /// through untouched and the next build that *does* have the radio finds
    /// them waiting.
    #[test]
    fn a_rig_this_build_lacks_still_keeps_its_port_and_rate() {
        let mut app = App::base((0.0, 4000.0));
        let saved = Settings {
            rig_keying: "a-radio-this-build-was-not-compiled-with".into(),
            rig_device: "/dev/ttyUSB7".into(),
            rig_baud: 19200,
            ..Settings::default()
        };
        app.apply(saved);
        assert_eq!(app.rig, rig::Keying::None, "keyed a rig it has no code for");

        let written = app.settings();
        assert_eq!(written.rig_device, "/dev/ttyUSB7", "lost the port");
        assert_eq!(written.rig_baud, 19200, "lost the rate");
    }

    /// Turning rig control off takes the rig off the air on the way past.
    #[test]
    fn turning_the_rig_off_stops_it_transmitting_first() {
        let mut app = App::base((0.0, 4000.0));
        let bench = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.rig = rig::Keying::RigCtld { host: "x".into(), port: 1 };
        app.ptt = Some(rig::Ptt::with_transmitter(
            Box::new(Flag(bench.clone())),
            rig::Limits::default(),
        ));
        app.ptt.as_ref().unwrap().key(true);
        let began = std::time::Instant::now();
        while !bench.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(began.elapsed() < std::time::Duration::from_secs(2), "never keyed");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        app.set_keying(rig::Keying::None);
        assert!(!bench.load(std::sync::atomic::Ordering::SeqCst), "left it transmitting");
        assert!(app.ptt.is_none());
    }

    /// A transmitter that only records whether it is keyed.
    struct Flag(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl rig::Transmitter for Flag {
        fn key(&mut self, on: bool) -> Result<(), rig::Fault> {
            self.0.store(on, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn keyed(&mut self) -> Result<Option<bool>, rig::Fault> {
            Ok(Some(self.0.load(std::sync::atomic::Ordering::SeqCst)))
        }
        fn describe(&self) -> String {
            "a flag".to_string()
        }
    }

    /// The list of modes stays up while the pointer travels into it, and
    /// nothing is drawn over it while it is there.
    ///
    /// Both were the same fault. The dial's tooltip is a layer above the list:
    /// it covered the options, and while the pointer was over *it* neither the
    /// word nor the list counted as hovered — so reaching for an option closed
    /// the thing being reached for.
    #[test]
    fn the_mode_list_survives_being_reached_for() {
        let mut app = app_with_a_dial(14_074_000.0);
        app.rig = rig::Keying::RigCtld { host: "x".into(), port: 1 };
        let mut hand = Hand::new();

        // Found in this hand's own window: a layout belongs to the context that
        // produced it, and one measured elsewhere is a few points out.
        hand.hover(&mut app, Pos2::new(-50.0, -50.0));
        let out = hand.hover(&mut app, Pos2::new(-50.0, -50.0));
        let word = out
            .shapes
            .iter()
            .find_map(|cs| match &cs.shape {
                egui::Shape::Text(t) if t.galley.text() == "PKTUSB" => Some(t.pos),
                _ => None,
            })
            .expect("the mode was not drawn");
        let on_word = egui::pos2(word.x + 8.0, word.y + 6.0);

        hand.hover(&mut app, on_word);
        hand.hover(&mut app, on_word);
        assert!(app.mode_menu, "pointing at the mode did not open the list");

        // What is drawn while it is open: the modes, and not the tooltip.
        let out = hand.hover(&mut app, on_word);
        let drawn_now: Vec<String> =
            drawn(&out).into_iter().map(|(s, _)| s).collect();
        assert!(drawn_now.iter().any(|s| s == "CW"), "the list is not showing: {drawn_now:?}");
        assert!(
            !drawn_now.iter().any(|s| s.starts_with("what the rig is tuned to")),
            "the dial's tooltip is drawn over the list"
        );

        // Now move down into the list, which is below the word — the travel
        // that used to dismiss it.
        let into_list = egui::pos2(on_word.x, on_word.y + 24.0);
        hand.hover(&mut app, into_list);
        assert!(app.mode_menu, "the list closed while the pointer moved into it");
    }

    /// Every decade has a column, whatever the frequency.
    ///
    /// At 7.070.000 with the megahertz written as a single digit there is no
    /// tens column, so 14 MHz cannot be typed or scrolled to at all — the
    /// cursor has nowhere to stand. The places are fixed and the leading zeros
    /// are blanked, as a radio's own dial does it.
    #[test]
    fn the_decades_keep_their_places_below_ten_megahertz() {
        assert_eq!(dial_digits(7_070_000.0), "007.070.000");
        assert_eq!(dial_digits(14_074_000.0), "014.074.000");
        assert_eq!(dial_digits(144_200_000.0), "144.200.000");
        // Blanked, not written: two at most, so a dial never reads as empty.
        assert_eq!(leading_blanks("007.070.000"), 2);
        assert_eq!(leading_blanks("014.074.000"), 1);
        assert_eq!(leading_blanks("144.200.000"), 0);
        assert_eq!(leading_blanks("000.000.000"), 2);

        // And the column is there to be used: ten megahertz up from seven.
        let mut app = app_with_a_dial(7_070_000.0);
        let mut hand = Hand::new();
        let tens_of_mhz = hand.digit(&mut app, 7);
        hand.hover(&mut app, tens_of_mhz);
        hand.hover(&mut app, tens_of_mhz);
        hand.wheel(&mut app, 1.0);
        assert_eq!(
            app.tuning.map(|t| t.hz),
            Some(17_070_000.0),
            "the tens-of-megahertz column is not there to tune"
        );
    }

    /// The rig's settings stay out of the way until they are asked for.
    ///
    /// They are set when a band changes and left alone for the hour after, and
    /// this panel is mostly meant to hold conversation. So: a handle, and
    /// nothing behind it until it is used.
    #[test]
    fn the_rigs_settings_hide_behind_a_handle() {
        let mut app = app_with_a_dial(14_074_000.0);
        let mut hand = Hand::new();
        hand.hover(&mut app, Pos2::new(-50.0, -50.0));

        let shown = |out: &egui::FullOutput| -> bool {
            drawn(out).iter().any(|(s, _)| s.contains("kHz"))
        };
        let out = hand.hover(&mut app, Pos2::new(-50.0, -50.0));
        assert!(!shown(&out), "the settings were on show without being asked for");

        // The handle is directly under the dial and spans the panel, so it is
        // found by walking down from the digits rather than by guessing at the
        // spacing between them.
        let dial = hand.digit(&mut app, 0);
        let below = dial.y + app.text_px * VFO_SCALE / 2.0 + (app.text_px * 0.5).max(7.0) + 3.0;
        let mut found = None;
        for step in 0..12 {
            let at = egui::pos2(dial.x, below + step as f32 * 2.0);
            hand.hover(&mut app, at);
            hand.click(&mut app);
            if app.rig_more {
                found = Some(at.y - below);
                break;
            }
        }
        let reach = found.expect("no handle under the dial");
        assert!(reach < 24.0, "the handle is {reach:.0} px below the dial, which is not under it");

        let out = hand.hover(&mut app, Pos2::new(-50.0, -50.0));
        assert!(shown(&out), "opened, and the settings still are not drawn");

        // And it is remembered, being a thing about how the window is laid out.
        let text = serde_json::to_string(&app.settings()).unwrap();
        let mut restored = App::base((0.0, 4000.0));
        restored.apply(serde_json::from_str(&text).unwrap());
        assert!(restored.rig_more, "the window forgot that they were open");
    }

    /// The build's version is in the settings, and only in the settings.
    ///
    /// It is the thing an operator is asked for when they report something odd,
    /// so it has to be somewhere they can find without being told where — and
    /// the foot of the settings panel is where a version number is looked for.
    /// Equally it is of no use while working a station, so it does not get a
    /// place on the bar.
    ///
    /// The string is checked against the manifest rather than against a
    /// literal: a test that spelled the number out would have to be edited on
    /// every release, and one that is edited on every release stops being read.
    #[test]
    fn the_version_is_at_the_foot_of_the_settings() {
        let mut app = App::base((0.0, 4000.0));
        let mut hand = Hand::new();
        let want = format!("ragchew {}", env!("CARGO_PKG_VERSION"));

        let out = hand.hover(&mut app, Pos2::new(-50.0, -50.0));
        assert!(
            !drawn(&out).iter().any(|(s, _)| s == &want),
            "the version was on the bar without the settings being opened"
        );

        let menu = hand.text_at(&mut app, "☰").expect("the settings button was not drawn");
        hand.hover(&mut app, menu);
        hand.click(&mut app);

        let out = hand.hover(&mut app, menu);
        let shown = drawn(&out);
        let at = |what: &str| {
            shown
                .iter()
                .find(|(s, _)| s == what)
                .unwrap_or_else(|| panic!("no {what:?} in the settings; they hold {shown:?}"))
                .1
        };
        // At the foot of it: below the last of the commands, which is Quit.
        assert!(
            at(&want) > at("Quit"),
            "the version at {} is above Quit at {}",
            at(&want),
            at("Quit")
        );
    }

    /// What falls outside the rig's filter, in the audio this app draws.
    #[test]
    fn the_band_outside_the_filter_is_the_part_to_dim() {
        // A wide data filter leaves almost all of the panorama audible.
        let wide = outside_the_filter(Some(2700.0), true);
        assert_eq!(wide.len(), 2);
        assert_eq!(wide[0].1, 150.0, "the low edge of a 2.7 kHz filter at 1500");
        assert_eq!(wide[1].0, 2850.0, "the high edge of a 2.7 kHz filter at 1500");

        // A narrow one leaves a slot, and the rest is a picture of nothing.
        let narrow = outside_the_filter(Some(400.0), true);
        assert_eq!(narrow[0].1, 1300.0);
        assert_eq!(narrow[1].0, 1700.0);

        // Nothing is dimmed on a guess: no width, no filter; and a mode where
        // an audio passband means nothing gets no shading either.
        assert!(outside_the_filter(None, true).is_empty());
        assert!(outside_the_filter(Some(2700.0), false).is_empty(), "shaded an FM panorama");
        assert!(outside_the_filter(Some(0.0), true).is_empty());
    }

    /// An audio offset becomes a frequency on the band, with the sideband
    /// deciding which way.
    ///
    /// The sign is the whole of it. On upper sideband a station heard at
    /// 1500 Hz is 1500 Hz above the dial; on lower it is that far below, and a
    /// log written from the wrong one is off by three kilohertz.
    #[test]
    fn an_offset_becomes_the_frequency_it_really_is() {
        let dial = Some(14_074_000.0);
        assert_eq!(on_the_band(dial, Some("USB"), 1500.0), Some(14_075_500.0));
        assert_eq!(on_the_band(dial, Some("PKTUSB"), 1500.0), Some(14_075_500.0));
        assert_eq!(on_the_band(dial, Some("DATA"), 1500.0), Some(14_075_500.0));
        assert_eq!(on_the_band(dial, Some("LSB"), 1500.0), Some(14_072_500.0));
        assert_eq!(on_the_band(dial, Some("DATA-REV"), 1500.0), Some(14_072_500.0));

        // Nothing is printed where an offset is not a displacement from the
        // dial, or where the dial is unknown.
        assert_eq!(on_the_band(dial, Some("FM"), 1500.0), None);
        assert_eq!(on_the_band(dial, Some("CW"), 1500.0), None);
        assert_eq!(on_the_band(dial, None, 1500.0), None);
        assert_eq!(on_the_band(None, Some("USB"), 1500.0), None);
    }

    /// A frequency reads as a frequency, in the groups an operator says out
    /// loud rather than as seven undifferentiated digits.
    #[test]
    fn the_dial_reads_the_way_an_operator_says_it() {
        assert_eq!(megahertz(14_074_000.0), "14.074.000");
        assert_eq!(megahertz(7_078_000.0), "7.078.000");
        assert_eq!(megahertz(3_573_250.0), "3.573.250");
        assert_eq!(megahertz(1_840_000.0), "1.840.000");
        // All the way down to single hertz: the modes here are tens of hertz
        // wide, so the last digit is one an operator uses.
        assert_eq!(megahertz(14_074_004.0), "14.074.004");
        assert_eq!(megahertz(0.0), "0.000.000");
    }

    /// A wheel is worth what it was turned, not one step a frame.
    ///
    /// Taking the sign alone — which this did at first — meant a flick moved
    /// one step however hard the wheel was spun, so crossing a band was a
    /// thousand flicks. It felt, in the operator's words, very slow.
    #[test]
    fn the_wheel_is_worth_what_it_was_turned() {
        assert_eq!(notches(SCROLL_NOTCH), 1.0);
        assert_eq!(notches(-SCROLL_NOTCH), -1.0);
        assert_eq!(notches(SCROLL_NOTCH * 6.0), 6.0);
        // A touchpad's dribble still moves it, and by a whole step.
        assert_eq!(notches(3.0), 1.0);
        assert_eq!(notches(-3.0), -1.0);
    }

    /// Consecutive clicks accumulate, rather than each one starting again from
    /// what the rig last said.
    ///
    /// This is what made tuning feel stuck. The dial is read twice a second and
    /// a hand moves faster, so every click inside one reading computed the same
    /// "current + 100" and asked for the same frequency — a second of scrolling
    /// moved the rig one step.
    #[test]
    fn clicks_accumulate_instead_of_re_reading_a_stale_dial() {
        let mut app = App::base((0.0, 4000.0));
        let rig_says = Some(14_074_000.0);
        let t0 = 1000.0;

        // Four clicks inside one reading of the rig, which does not move.
        let mut target = app.tuning_from(rig_says, t0);
        for i in 0..4 {
            let at = t0 + i as f64 * 0.05;
            target = app.tuning_from(rig_says, at) + 100.0 * notches(SCROLL_NOTCH);
            app.tuning = Some(Tuning { hz: target, at, sent: true });
        }
        assert_eq!(target, 14_074_400.0, "four clicks of 100 Hz should be 400 Hz");

        // Once the hand stops — measured from the last click, not the first —
        // the rig's own answer takes over again.
        let quiet = app.tuning.unwrap().at + TUNING_SETTLES_S + 0.1;
        assert_eq!(app.tuning_from(Some(14_074_400.0), quiet), 14_074_400.0);
        assert_eq!(
            app.tuning_from(Some(7_000_000.0), quiet),
            7_000_000.0,
            "the rig's own dial should win once the hand is off the wheel"
        );
        // And while the hand is still on it, the rig moving underneath does not
        // drag the digits out from under the wheel.
        let mid = app.tuning.unwrap().at + 0.2;
        assert_eq!(app.tuning_from(Some(7_000_000.0), mid), 14_074_400.0);
    }

    /// Every digit is its own control, worth its own decade.
    ///
    /// This is what a rig with a knob per decade does, and it says which step
    /// is meant by where the pointer is rather than by a modifier — which
    /// matters here because the modifiers were never available: ctrl+wheel is
    /// taken by egui to become `zoom_delta` and shift+wheel is moved onto the
    /// x axis before any widget sees it.
    #[test]
    fn each_digit_tunes_its_own_decade() {
        // Counting from the right of 14.074.000: hertz, tens, hundreds, then
        // the kilohertz group, then megahertz.
        let steps = [
            (0usize, 1.0),
            (1, 10.0),
            (2, 100.0),
            (4, 10_000.0),
            (6, 1_000_000.0),
            (7, 10_000_000.0),
        ];
        for (from_right, step) in steps {
            let mut app = app_with_a_dial(14_074_000.0);
            let mut hand = Hand::new();
            let at = hand.digit(&mut app, from_right);
            hand.hover(&mut app, at);
            hand.hover(&mut app, at);
            hand.wheel(&mut app, 1.0);
            assert_eq!(
                app.tuning.map(|t| t.hz),
                Some(14_074_000.0 + step),
                "a notch over the digit {from_right} places from the right should move {step} Hz"
            );
        }
    }

    /// The arrows step the digit the dial is pointed at, and pointing is done
    /// by hovering it.
    #[test]
    fn the_arrows_step_the_digit_they_sit_over() {
        for (up, want) in [(true, 14_074_100.0), (false, 14_073_900.0)] {
            let mut app = app_with_a_dial(14_074_000.0);
            let mut hand = Hand::new();
            let hundreds = hand.digit(&mut app, 2);
            hand.hover(&mut app, hundreds);
            hand.hover(&mut app, hundreds);

            let arrow_h = (app.text_px * 0.5).max(7.0);
            let digit_h = app.text_px * VFO_SCALE;
            let reach = digit_h / 2.0 + 3.0 + arrow_h / 2.0;
            let at = egui::pos2(hundreds.x, hundreds.y + if up { -reach } else { reach });
            hand.hover(&mut app, at);
            hand.click(&mut app);

            assert_eq!(
                app.tuning.map(|t| t.hz),
                Some(want),
                "the {} arrow over the hundreds digit should have stepped it",
                if up { "up" } else { "down" }
            );
            assert!(app.tuning.is_some_and(|t| t.sent), "a step should go straight to the rig");
        }
    }

    /// Typing fills the dial a digit at a time and waits for Enter.
    ///
    /// Entered a digit at a time, a frequency passes through others on the way:
    /// 14.074.000 typed towards 7.078.000 goes through 74.074.000 and
    /// 70.074.000, each of them somewhere the radio would actually go. So the
    /// digits are shown, and nothing reaches the rig until Enter.
    #[test]
    fn typing_a_frequency_waits_for_enter() {
        let mut app = app_with_a_dial(14_074_000.0);
        let mut hand = Hand::new();
        let leftmost = hand.digit(&mut app, 7);
        // Click the dial to take the keys, with the cursor where it was clicked.
        hand.hover(&mut app, leftmost);
        hand.hover(&mut app, leftmost);
        hand.click(&mut app);

        hand.type_text(&mut app, "0");
        hand.type_text(&mut app, "7");
        hand.type_text(&mut app, "0");
        hand.type_text(&mut app, "7");
        hand.type_text(&mut app, "8");
        let typed = app.tuning.expect("nothing was typed in");
        assert_eq!(typed.hz, 7_078_000.0, "the digits did not fill from the left");
        assert!(!typed.sent, "a half-typed frequency reached the rig");

        hand.press(&mut app, egui::Key::Enter);
        assert!(app.tuning.is_some_and(|t| t.sent), "Enter did not send it");
        assert_eq!(app.tuning.map(|t| t.hz), Some(7_078_000.0));
    }

    /// Escape puts back whatever the radio actually says.
    #[test]
    fn escape_abandons_what_was_typed() {
        let mut app = app_with_a_dial(14_074_000.0);
        let mut hand = Hand::new();
        let leftmost = hand.digit(&mut app, 7);
        hand.hover(&mut app, leftmost);
        hand.hover(&mut app, leftmost);
        hand.click(&mut app);
        hand.type_text(&mut app, "07");
        assert!(app.tuning.is_some_and(|t| !t.sent));

        hand.press(&mut app, egui::Key::Escape);
        assert_eq!(app.tuning, None, "Escape left the typed frequency on the dial");
    }

    /// A hand that types and then reaches for the wheel gets both: the digits
    /// it typed, stepped, and sent — no keystroke needed for a gesture that
    /// never used the keyboard to finish.
    #[test]
    fn the_wheel_consolidates_what_was_typed() {
        let mut app = app_with_a_dial(14_074_000.0);
        let mut hand = Hand::new();
        let leftmost = hand.digit(&mut app, 7);
        hand.hover(&mut app, leftmost);
        hand.hover(&mut app, leftmost);
        hand.click(&mut app);
        hand.type_text(&mut app, "07078");
        assert_eq!(app.tuning.map(|t| t.hz), Some(7_078_000.0));
        assert!(app.tuning.is_some_and(|t| !t.sent));

        // Onto the hundreds digit, and a notch.
        let hundreds = hand.digit(&mut app, 2);
        hand.hover(&mut app, hundreds);
        hand.hover(&mut app, hundreds);
        hand.wheel(&mut app, 1.0);

        let now = app.tuning.expect("the wheel lost the edit");
        assert_eq!(now.hz, 7_078_100.0, "the wheel did not build on what was typed");
        assert!(now.sent, "a wheel step should go to the rig without Enter");
    }

    /// The keys move the cursor, and the mouse takes it back when it moves —
    /// the last device used owns it, so there is never a question of which
    /// digit is live.
    #[test]
    fn the_cursor_belongs_to_whichever_was_used_last() {
        let mut app = app_with_a_dial(14_074_000.0);
        let mut hand = Hand::new();
        let hundreds = hand.digit(&mut app, 2);
        hand.hover(&mut app, hundreds);
        hand.hover(&mut app, hundreds);
        hand.click(&mut app);
        assert_eq!(app.dial_pick, 2, "clicking a digit should point at it");

        // Left, twice: onto the tens of kilohertz.
        hand.press(&mut app, egui::Key::ArrowLeft);
        hand.press(&mut app, egui::Key::ArrowLeft);
        assert_eq!(app.dial_pick, 4, "the arrow keys did not move the cursor");

        // The pointer has not moved, so hovering does not drag it back.
        hand.frame(&mut app, Vec::new());
        assert_eq!(app.dial_pick, 4, "hover took the cursor back without the mouse moving");

        // And when the mouse does move, it does.
        let tens = hand.digit(&mut app, 1);
        hand.hover(&mut app, tens);
        hand.hover(&mut app, tens);
        assert_eq!(app.dial_pick, 1, "the mouse did not take the cursor back");
    }

    /// Up and down step the digit under the cursor, as the arrows do.
    #[test]
    fn the_up_and_down_keys_step_the_selected_digit() {
        let mut app = app_with_a_dial(14_074_000.0);
        let mut hand = Hand::new();
        let hundreds = hand.digit(&mut app, 2);
        hand.hover(&mut app, hundreds);
        hand.hover(&mut app, hundreds);
        hand.click(&mut app);

        hand.press(&mut app, egui::Key::ArrowUp);
        assert_eq!(app.tuning.map(|t| t.hz), Some(14_074_100.0));
        hand.press(&mut app, egui::Key::ArrowDown);
        hand.press(&mut app, egui::Key::ArrowDown);
        assert_eq!(app.tuning.map(|t| t.hz), Some(14_073_900.0));
        assert!(app.tuning.is_some_and(|t| t.sent), "a key step should reach the rig");
    }

    /// The readout shows what the rig says, in amber, larger than the text
    /// around it — and says so plainly when the rig will not say.
    #[test]
    fn the_vfo_shows_the_rigs_own_frequency() {
        let mut app = app_with_a_waterfall(30.0);
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.rig = rig::Keying::RigCtld { host: "x".into(), port: 1 };
        app.ptt = Some(rig::Ptt::with_transmitter(
            Box::new(Dial(flag.clone(), 14_074_000.0)),
            rig::Limits::default(),
        ));

        let began = std::time::Instant::now();
        let (reading, size, colour) = loop {
            let out = lay_out(&mut app);
            let found = out.shapes.iter().find_map(|cs| match &cs.shape {
                egui::Shape::Text(t) if t.galley.text().contains("14.074") => Some((
                    t.galley.text().to_string(),
                    t.galley.job.sections[0].format.font_id.size,
                    t.fallback_color,
                )),
                _ => None,
            });
            if let Some(f) = found {
                break f;
            }
            assert!(began.elapsed() < std::time::Duration::from_secs(2), "no reading was drawn");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(reading, "014.074.000");
        assert!(size > app.text_px * 1.5, "drawn at {size}, no larger than the text at {}", app.text_px);
        assert_eq!(colour, VFO_AMBER, "the dial is not amber");
    }

    /// A rig that does not report its frequency gets dashes, not a zero: a
    /// station reading 0.000.00 off its own window would be a station that
    /// believes it.
    #[test]
    fn a_dial_that_cannot_be_read_shows_dashes() {
        let mut app = app_with_a_waterfall(30.0);
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.rig = rig::Keying::RigCtld { host: "x".into(), port: 1 };
        app.ptt =
            Some(rig::Ptt::with_transmitter(Box::new(Flag(flag)), rig::Limits::default()));

        let out = lay_out(&mut app);
        assert!(
            drawn(&out).iter().any(|(s, _)| s.starts_with("---")),
            "a silent dial did not say so: {:?}",
            drawn(&out).iter().map(|(s, _)| s.clone()).take(8).collect::<Vec<_>>()
        );
    }

    /// A transmitter that also has a dial on it.
    struct Dial(std::sync::Arc<std::sync::atomic::AtomicBool>, f64);
    impl rig::Transmitter for Dial {
        fn key(&mut self, on: bool) -> Result<(), rig::Fault> {
            self.0.store(on, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn keyed(&mut self) -> Result<Option<bool>, rig::Fault> {
            Ok(Some(self.0.load(std::sync::atomic::Ordering::SeqCst)))
        }
        fn dial_hz(&mut self) -> Result<Option<f64>, rig::Fault> {
            Ok(Some(self.1))
        }
        fn tune(&mut self, hz: f64) -> Result<(), rig::Fault> {
            self.1 = hz;
            Ok(())
        }
        fn dial_mode(&mut self) -> Result<Option<String>, rig::Fault> {
            Ok(Some("PKTUSB".to_string()))
        }
        fn power_w(&mut self) -> Result<Option<f64>, rig::Fault> {
            Ok(Some(10.0))
        }
        fn width_hz(&mut self) -> Result<Option<f64>, rig::Fault> {
            Ok(Some(2700.0))
        }
        fn describe(&self) -> String {
            "a dial".to_string()
        }
    }

    /// Nothing goes on the air through a rig that is not answering.
    ///
    /// Audio into a rig that did not key is silence at best; on a station left
    /// on VOX it is a transmission nobody asked this app to make, out of a path
    /// the operator believed was under control. So a link that has failed stops
    /// the transmission and says why, rather than playing into the dark.
    #[test]
    fn a_rig_that_is_not_answering_stops_the_transmission() {
        let mut app = app_with_a_waterfall(30.0);
        // A daemon that is not there: the link faults on its first exchange.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        app.rig = rig::Keying::RigCtld { host: "127.0.0.1".into(), port };
        app.ptt = Some(rig::Ptt::with_transmitter(
            Box::new(rig::RigCtld::new("127.0.0.1", port).with_timeout(std::time::Duration::from_millis(50))),
            rig::Limits::default(),
        ));
        let began = std::time::Instant::now();
        while app.ptt_state().fault.is_none() {
            assert!(began.elapsed() < std::time::Duration::from_secs(2), "no fault was reported");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let channels = app.channels_now();
        app.qsos.open_for_channel(&channels.channels()[0], app.t_now);
        if let Some(q) = app.qsos.active_qso_mut() {
            q.draft = "CQ DE W1AW K".to_string();
        }
        app.send_active(app.t_now);

        assert!(app.tx.is_none() || app.tx.as_ref().is_some_and(|t| t.busy_for() == 0.0),
            "audio was queued into a rig that is not answering");
        let said = app.status.as_ref().map(|n| n.text.clone()).unwrap_or_default();
        assert!(said.contains("not transmitting"), "said nothing useful: {said:?}");
        assert!(
            app.qsos.active_qso_mut().is_some_and(|q| q.outgoing.is_empty()),
            "logged an outgoing transmission that never went"
        );
    }

    /// The button stops a transmission it did not start.
    ///
    /// It used to toggle against its own intent, so a rig transmitting for any
    /// other reason read as "not keyed by us" and every press sent another key
    /// command. On a real rig, keyed by hamlib raising DTR as it opened the
    /// port, that was a transmitter that could not be stopped from the one
    /// control on the screen for stopping it.
    #[test]
    fn the_button_stops_a_transmission_it_did_not_start() {
        let mut app = app_with_a_waterfall(30.0);
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.rig = rig::Keying::RigCtld { host: "x".into(), port: 1 };
        app.ptt =
            Some(rig::Ptt::with_transmitter(Box::new(Flag(flag.clone())), rig::Limits::default()));

        // Somebody else put it on the air; this app never asked for it.
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        let began = std::time::Instant::now();
        while app.ptt_state().rig_says != Some(true) {
            assert!(began.elapsed() < std::time::Duration::from_secs(2), "never saw it keyed");
            std::thread::sleep(std::time::Duration::from_millis(5));
            lay_out(&mut app);
        }
        assert!(!app.ptt_state().asked, "the test set this up wrong: it thinks it keyed it");

        // What the button does with a click in that state.
        let live = app.ptt_state().asked || app.ptt_state().rig_says == Some(true);
        app.ptt.as_ref().unwrap().key(!live);

        let began = std::time::Instant::now();
        while flag.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(
                began.elapsed() < std::time::Duration::from_secs(2),
                "the button would not take the rig off the air"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The window says TX while the rig is keyed, and says nothing when it is
    /// not. The key sits beside the dial, at the head of the QSO panel, where
    /// the hand that works a station already is.
    ///
    /// Drawn rather than asserted on the state, because the state being right
    /// while the button stays grey is exactly the failure that matters: this is
    /// the only thing on screen that says the station is transmitting.
    #[test]
    fn the_window_says_when_the_rig_is_keyed() {
        let mut app = app_with_a_waterfall(30.0);
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.rig = rig::Keying::RigCtld { host: "x".into(), port: 1 };
        app.ptt =
            Some(rig::Ptt::with_transmitter(Box::new(Flag(flag.clone())), rig::Limits::default()));

        let out = lay_out(&mut app);
        assert!(drawn(&out).iter().any(|(s, _)| s == "TX"), "no TX button at all");

        app.ptt.as_ref().unwrap().key(true);
        let began = std::time::Instant::now();
        loop {
            let out = lay_out(&mut app);
            let says_tx = drawn(&out).iter().any(|(s, _)| s.starts_with("TX"));
            if says_tx && app.ptt_state().asked {
                break;
            }
            assert!(began.elapsed() < std::time::Duration::from_secs(2), "the bar never said TX");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(flag.load(std::sync::atomic::Ordering::SeqCst), "said TX without keying anything");
    }

    /// A row that is nudged aside keeps its text.
    ///
    /// When a station appears next to one already on screen, the layout moves
    /// rows apart to make room. That is a change of *place*, and nothing about
    /// a row's place has anything to do with what it has said: the text on a
    /// nudged row must be the text it had a frame ago, whole.
    #[test]
    fn a_row_nudged_aside_by_a_newcomer_keeps_its_text() {
        let psk = ModeId::Psk(ragchew::psk::PSK31);
        let d = |hz: f64, at: f64, text: &str| protocol::Decode {
            snr_db: None,
            mode: psk,
            hz,
            time_s: at,
            quality: 30.0,
            text: text.to_string(),
            ends_over: false,
        };

        let mut app = App::base((0.0, 4000.0));
        // One station typing from the start, and a second ten seconds later
        // sixty hertz away: far enough apart to be two channels rather than one
        // (the set associates within fifteen), close enough that their rows
        // cannot both sit at their own frequency, so the layout must nudge.
        let mut frames: Vec<Frame> = Vec::new();
        for i in 0..200 {
            let t = 1.0 + i as f64 * 0.2;
            frames.push(Frame { reveal_s: t + psk.chunk_secs(), decode: d(1000.0, t, "a") });
            if t >= 11.0 {
                frames.push(Frame { reveal_s: t + psk.chunk_secs(), decode: d(1060.0, t, "b") });
            }
        }
        app.frames = frames;
        app.duration_s = 60.0;
        app.samples = Arc::new(vec![0.0; SAMPLE_RATE as usize * 40]);
        app.spec = Some(Arc::new(Spectrogram::compute(&app.samples, 0.0, 4000.0, WINDOW / 4)));

        let mut longest = 0usize;
        let mut moved = false;
        let mut was_at: Option<f32> = None;
        for step in 0..150 {
            app.t_now = 5.0 + step as f64 * 0.1;
            let out = lay_out(&mut app);
            let mine: Vec<(String, f32)> = out
                .shapes
                .iter()
                .filter_map(|cs| match &cs.shape {
                    egui::Shape::Text(t) if t.pos.x > app.qso_w => {
                        Some((t.galley.text().to_string(), t.pos.y))
                    }
                    _ => None,
                })
                .filter(|(s, _)| s.starts_with('a'))
                .collect();
            let Some((text, y)) = mine.first() else { continue };
            moved |= was_at.is_some_and(|prev| (prev - y).abs() > 0.5);
            was_at = Some(*y);

            assert!(
                text.chars().count() >= longest,
                "at t={:.1} the row lost text: {} characters, down from {longest}",
                app.t_now,
                text.chars().count()
            );
            longest = text.chars().count();
        }
        assert_eq!(
            app.channels_now().channels().len(),
            2,
            "the two stations were read as one, so nothing was ever nudged"
        );
        assert!(moved, "the newcomer never moved the row, so this test proved nothing");
        assert!(longest > 20, "the row only ever held {longest} characters");
    }

    /// Text filled in behind a station is there, not swept in.
    ///
    /// A PSK receiver opened part-way through an over is caught up from where
    /// the station started transmitting, so a hundred characters can reach a
    /// row that already had text in one frame. Charged in full, that is more
    /// debt than the panel is wide: the row is drawn off the right-hand edge of
    /// its own strip, blank, and then glides back in oldest-first over a
    /// second — which reads as the row clearing and redrawing itself.
    #[test]
    fn a_catch_up_does_not_sweep_the_row_off_its_panel() {
        let mut app = App::base((0.0, 4000.0));
        app.scroll_secs = 1.0;
        let (char_w, panel_w) = (8.0, 400.0);
        let psk = ModeId::Psk(ragchew::psk::PSK31);
        let mut set = ChannelSet::new(15.0);
        let say = |set: &mut ChannelSet, at: f64, text: &str| {
            set.add(protocol::Decode {
                snr_db: None,
                mode: psk,
                hz: 1500.0,
                time_s: at,
                quality: 30.0,
                text: text.to_string(),
                ends_over: false,
            });
        };

        // A station already on the row, then its receiver opening and handing
        // over a window's worth of what it had been saying all along.
        say(&mut set, 1.0, "cq de w1aw k ");
        app.slide_rows(&set, char_w, 1.0 / 60.0, panel_w);
        for (i, c) in "the quick brown fox jumps over the lazy dog and keeps on typing for a \
                       good long while yet"
            .chars()
            .enumerate()
        {
            say(&mut set, 2.0 + i as f64 * 0.2, &c.to_string());
        }
        app.slide_rows(&set, char_w, 1.0 / 60.0, panel_w);

        let id = set.channels()[0].id;
        let px = app.slide[&id].px;
        assert!(
            px <= SLIDE_MAX_CHARS * char_w,
            "a catch-up of {} characters shifted the row {px:.0} px, {:.0} of them past the \
             largest thing anyone says at once",
            set.channels()[0].next_index(),
            px - SLIDE_MAX_CHARS * char_w
        );
        assert!(px < panel_w * 0.5, "the row was pushed {px:.0} px across a {panel_w:.0} px panel");
    }

    fn app_with_traffic(t: f64) -> App {
        let mut app = App::base((0.0, 4000.0));
        let d = |hz: f64, at: f64, text: &str| protocol::Decode {
            snr_db: None,
            mode: ModeId::Js8(ragchew::js8::Mode::Normal),
            hz,
            time_s: at,
            quality: 4.2,
            text: text.to_string(),
            ends_over: false,
        };
        app.frames = [
            d(1000.0, 1.0, "CQ CQ DE K2N "),
            d(1000.0, 16.0, "FN20 5W "),
            d(2000.0, 3.0, "DE VE3XYZ "),
        ]
        .into_iter()
        .map(|d| Frame { reveal_s: d.time_s + d.mode.chunk_secs(), decode: d })
        .collect();
        app.duration_s = 60.0;
        app.t_now = t;
        app.samples = Arc::new(vec![0.0; 12_000]);
        app
    }

    /// The mark is painted, in the asset's colour, inside its own button.
    ///
    /// Two strokes and nothing typed, so nothing else here would notice if it
    /// were drawn off the edge of the button, or not at all: the button would
    /// measure the same and the word beside it would still be there.
    #[test]
    fn the_clear_button_paints_its_mark_inside_itself() {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1280.0, 800.0))),
            ..Default::default()
        };
        let (mut button, mut red) = (Rect::NOTHING, Color32::PLACEHOLDER);
        let out = ctx.run_ui(input, |ui| {
            elegance::Theme::slate().install(ui.ctx());
            red = legible(ui.visuals().dark_mode, CLEAR_MARK_RED);
            button = clear_button(ui).rect;
        });

        let strokes: Vec<[Pos2; 2]> = out
            .shapes
            .iter()
            .filter_map(|cs| match cs.shape {
                egui::Shape::LineSegment { points, stroke } if stroke.color == red => Some(points),
                _ => None,
            })
            .collect();
        assert_eq!(strokes.len(), 2, "the mark is not two strokes on screen: {strokes:?}");
        for seg in &strokes {
            for p in seg {
                assert!(button.contains(*p), "the mark is drawn at {p:?}, outside {button:?}");
            }
        }
        // One stroke down to the right and one up to it: an X, not a V.
        let slope = |s: &[Pos2; 2]| (s[1].y - s[0].y).signum() * (s[1].x - s[0].x).signum();
        assert_eq!(slope(&strokes[0]), -slope(&strokes[1]), "the strokes do not cross");
    }

    /// The mark on the clear button is the one in the file it came from.
    ///
    /// [`CLEAR_MARK`] is transcribed from `gui/assets/Red_X.svg` rather than
    /// rendered out of it, which is only honest while the two agree — so this
    /// reads the asset and checks. It is also what says the shortcut is still
    /// allowed: each stroke has to be a straight line, which in that file is a
    /// cubic whose control points sit on its own endpoint. Give one a real
    /// curve and this fails rather than the app quietly drawing something else.
    #[test]
    fn the_clear_mark_is_the_one_in_the_asset() {
        let svg = include_str!("../assets/Red_X.svg");
        let field = |s: &str, key: &str, end: char| {
            let rest = s.split(key).nth(1).unwrap_or_else(|| panic!("no {key} in the asset"));
            rest.split(end).next().expect("unterminated").to_owned()
        };

        let paths: Vec<&str> = svg.split("<path").skip(1).collect();
        assert_eq!(paths.len(), CLEAR_MARK.len(), "the asset is no longer two strokes");
        for (path, s) in paths.iter().zip(CLEAR_MARK.iter()) {
            let d = field(path, " d=\"", '"');
            let n: Vec<f32> = d.split([' ', ',']).filter_map(|t| t.parse().ok()).collect();
            assert_eq!(Pos2::new(n[0], n[1]), s.from, "the stroke starts elsewhere in the asset");
            for p in n[2..].chunks(2) {
                assert_eq!(Pos2::new(p[0], p[1]), s.to, "the stroke is a curve, not a line: {d}");
            }

            let w: f32 = field(path, "stroke-width:", ';').parse().expect("a number");
            let drawn = s.width;
            assert!((w - drawn).abs() < 1e-4, "the asset draws that stroke {w} wide, not {drawn}");

            let rgb = field(path, "stroke:#", ';');
            let hex = |i: usize| u8::from_str_radix(&rgb[i..i + 2], 16).expect("hex");
            assert_eq!(
                Color32::from_rgb(hex(0), hex(2), hex(4)),
                CLEAR_MARK_RED,
                "the asset is drawn in #{rgb}"
            );
        }
    }

    /// The same band with a waterfall behind it, so the panorama is drawn
    /// rather than the "computing spectrogram…" that stands in for it.
    fn app_with_a_waterfall(t: f64) -> App {
        let mut app = app_with_traffic(t);
        app.samples = Arc::new(vec![0.0; SAMPLE_RATE as usize * 40]);
        app.spec = Some(Arc::new(Spectrogram::compute(&app.samples, 0.0, 4000.0, WINDOW / 4)));
        app
    }

    /// What was drawn to the right of the conversations, which is where the
    /// rows are.
    ///
    /// Picked out by position because a station's text is not unique to its
    /// row: an open QSO's log repeats it word for word, and by the time either
    /// is a shape they are the same string.
    ///
    /// A row is one galley however many stretches it was written in, so this
    /// reads what is on the row and not how much of it is fading — text on its
    /// way out is still in the galley, holding its columns.
    fn rows(app: &App, out: &egui::FullOutput) -> Vec<String> {
        out.shapes
            .iter()
            .filter_map(|cs| match &cs.shape {
                egui::Shape::Text(t) if t.pos.x > app.qso_w => {
                    Some(t.galley.text().to_string())
                }
                _ => None,
            })
            .collect()
    }

    /// Clearing takes the text off the rows and leaves the conversations alone:
    /// a log is a record of what was said, and tidying the window is not
    /// editing it.
    #[test]
    fn clearing_empties_the_rows_and_not_the_log() {
        let mut app = app_with_a_waterfall(30.0);
        lay_out(&mut app);
        let channels = app.channels_now();
        app.qsos.open_for_channel(&channels.channels()[0], app.t_now);
        let out = lay_out(&mut app);
        assert!(
            rows(&app, &out).iter().any(|s| s.contains("CQ CQ DE K2N")),
            "the station was not on a row to begin with: {:?}",
            rows(&app, &out)
        );

        app.clear_all(app.t_now);
        let out = lay_out(&mut app);
        for station in ["K2N", "VE3XYZ"] {
            assert!(
                !rows(&app, &out).iter().any(|s| s.contains(station)),
                "{station} is still on a row: {:?}",
                rows(&app, &out)
            );
        }
        assert!(
            drawn(&out).iter().any(|(s, _)| s == "0 threads"),
            "the count still has them: {:?}",
            drawn(&out)
        );
        assert!(
            app.qsos.qsos()[0].log[0].text.contains("CQ CQ DE K2N"),
            "the conversation lost its log: {:?}",
            app.qsos.qsos()[0].log
        );
    }

    /// Putting the clock back behind a clear replays what was cleared.
    ///
    /// A clear is a moment in the recording, and taken literally that outlives
    /// a restart: everything decoded before the instant it was pressed arrives
    /// already cleared, so the panel stays empty for the whole replay up to
    /// that point. Which is what it looked like — clear a demo band, hit ⟲
    /// restart, and the app appears to have stopped decoding.
    #[test]
    fn restarting_replays_what_was_cleared() {
        let mut app = app_with_a_waterfall(30.0);
        lay_out(&mut app);
        app.clear_all(app.t_now);
        let out = lay_out(&mut app);
        assert!(
            !rows(&app, &out).iter().any(|s| s.contains("K2N")),
            "nothing was cleared, so this proves nothing"
        );

        // ⟲ restart, then far enough in for both stations to be back.
        app.t_now = 0.0;
        lay_out(&mut app);
        assert!(app.cleared_all_at.is_infinite(), "the clear outlived the clock");
        app.t_now = 30.0;
        let out = lay_out(&mut app);
        for station in ["CQ CQ DE K2N", "DE VE3XYZ"] {
            assert!(
                rows(&app, &out).iter().any(|s| s.contains(station)),
                "{station} did not come back: {:?}",
                rows(&app, &out)
            );
        }
    }

    /// A clear does not outlive the channels it names.
    ///
    /// It holds a channel id, and an id means something only within the set
    /// that issued it. Rescanning — a mode toggled, a new recording — builds
    /// the channels again from nothing, and the station that comes back as id 3
    /// is not the one that was cleared. The clock does not move for a rescan,
    /// so nothing else would have noticed.
    #[test]
    fn a_clear_does_not_carry_over_to_a_new_scan() {
        let mut app = app_with_traffic(30.0);
        lay_out(&mut app);
        let id = app.channels_now().channels()[0].id;
        app.cleared.insert(id, app.t_now);
        app.clear_all(app.t_now);

        app.rescan();
        assert!(app.cleared.is_empty(), "a station's clear survived the rebuild");
        assert!(app.cleared_all_at.is_infinite(), "the band-wide clear survived the rebuild");
    }

    /// Clearing one channel clears that one. The other stations on the band are
    /// somebody else's conversation and stay where they were.
    #[test]
    fn clearing_one_channel_leaves_the_rest_of_the_band_alone() {
        let mut app = app_with_a_waterfall(30.0);
        lay_out(&mut app);
        let id = app.channels_now().channels()[0].id;
        app.cleared.insert(id, app.t_now);

        let out = lay_out(&mut app);
        assert!(
            !rows(&app, &out).iter().any(|s| s.contains("K2N")),
            "the cleared channel is still there: {:?}",
            rows(&app, &out)
        );
        assert!(
            rows(&app, &out).iter().any(|s| s.contains("DE VE3XYZ")),
            "the other station went with it: {:?}",
            rows(&app, &out)
        );
    }

    /// The timeout forgets a station's text a piece at a time: what it said too
    /// long ago goes, what it said since stays on the row, and a station that
    /// has said nothing at all for that long leaves the window.
    #[test]
    fn text_from_too_far_back_is_forgotten() {
        // Fourteen seconds, and nothing kept beyond it — K2N's second frame
        // landed at 16 s and its first at 1 s, so the cut falls between them.
        let mut app = app_with_a_waterfall(30.0);
        app.forget_min = 14.0 / 60.0;
        app.fade_secs = 0.0;

        let out = lay_out(&mut app);
        let row = rows(&app, &out)
            .into_iter()
            .find(|s| s.contains("FN20"))
            .expect("the station's newest frame should still be on its row");
        assert_eq!(row, "FN20 5W ", "the older frame is still on the row");
        assert!(
            !rows(&app, &out).iter().any(|s| s.contains("VE3XYZ")),
            "a station last heard 27 s ago is still shown: {:?}",
            rows(&app, &out)
        );
        assert!(
            drawn(&out).iter().any(|(s, _)| s == "1 thread"),
            "the count still has both: {:?}",
            drawn(&out)
        );
    }

    /// Text on its way out is drawn on its way out: dimmer than the row it
    /// belongs to, and still there to be read.
    #[test]
    fn text_being_forgotten_fades_rather_than_vanishing() {
        let mut app = app_with_a_waterfall(30.0);
        // K2N's newest frame is 14 s old, so 10 s of keeping and 8 s of fading
        // puts it half way out.
        app.forget_min = 10.0 / 60.0;
        app.fade_secs = 8.0;
        let out = lay_out(&mut app);

        let alphas: Vec<u8> = out
            .shapes
            .iter()
            .filter_map(|cs| match &cs.shape {
                egui::Shape::Text(t) if t.galley.text().contains("FN20") => Some(t),
                _ => None,
            })
            .flat_map(|t| t.galley.job.sections.iter().map(|s| s.format.color.a()))
            .collect();
        assert!(!alphas.is_empty(), "the row was not drawn at all");
        assert!(
            alphas.iter().all(|&a| a < 255) && alphas.iter().any(|&a| a > 0),
            "a row half way out was drawn at {alphas:?}"
        );
    }

    /// The fade is the whole of the text's last stretch of life: solid until it
    /// is old, gone once it has finished fading, and nothing is forgotten at
    /// all while the timeout is off.
    #[test]
    fn text_is_solid_then_fades_then_goes() {
        assert_eq!(solidity(100.0, Some(0.0), None, 8.0), 1.0, "the timeout was off");
        assert_eq!(solidity(100.0, Some(95.0), Some(10.0), 8.0), 1.0, "faded before its time");
        assert_eq!(solidity(100.0, Some(85.0), Some(10.0), 8.0), 0.375, "5 s into an 8 s fade");
        assert_eq!(solidity(100.0, Some(80.0), Some(10.0), 8.0), 0.0, "outlived the fade");
        assert_eq!(solidity(100.0, Some(0.0), Some(10.0), 0.0), 0.0, "no fade is a clean cut");
        assert_eq!(solidity(100.0, None, Some(10.0), 8.0), 0.0, "older than anything on record");
    }

    /// The interface lays out with nothing loaded, with a band up, and with a
    /// conversation open on a station in it.
    #[test]
    fn the_interface_lays_out_headlessly() {
        lay_out(&mut App::base((0.0, 4000.0)));

        let mut app = app_with_traffic(30.0);
        lay_out(&mut app);

        let channels = app.channels_now();
        assert_eq!(channels.channels().len(), 2, "expected two stations");
        app.qsos.open_for_channel(&channels.channels()[0], app.t_now);
        app.qsos.open_blank(1500.0, ModeId::Olivia(ragchew::olivia::OL_8_250), 30.0);
        lay_out(&mut app);
        assert_eq!(app.qsos.len(), 2);
    }

    /// A QSO opened on a station follows it: text decoded after the tab was
    /// opened arrives in its log, and the tab takes the station's call.
    #[test]
    fn an_open_qso_follows_its_station() {
        // Far enough in for the station's first frame to have been revealed,
        // and not far enough for its second.
        let mut app = app_with_traffic(20.0);
        lay_out(&mut app);
        let early = app.channels_now();
        app.qsos.open_for_channel(&early.channels()[0], app.t_now);
        assert_eq!(app.qsos.qsos()[0].label(), "K2N");
        assert_eq!(app.qsos.qsos()[0].log.len(), 1);

        // let the clock run past the station's second transmission
        app.t_now = 40.0;
        lay_out(&mut app);
        let q = &app.qsos.qsos()[0];
        // One over, still: the two frames are 15 s apart on a 15 s cycle, and
        // the station never said it had finished. The seam between them is
        // what shows there were two.
        assert_eq!(q.log.len(), 1, "one over came out as {} entries", q.log.len());
        assert!(q.log[0].text.contains("FN20"), "logged {:?}", q.log[0].text);
        assert_eq!(q.log[0].marks.len(), 1, "the join between the frames is unmarked");
    }

    /// Every step is a round number a reader can do arithmetic with.
    #[test]
    fn tick_steps_are_round() {
        for span in [40.0, 77.0, 333.0, 999.0, 4000.0, 8000.0] {
            let step = tick_step(span);
            let mantissa = step / 10f64.powf(step.log10().floor());
            assert!(
                [1.0, 2.0, 5.0].iter().any(|m| (m - mantissa).abs() < 1e-9),
                "step {step} is not a 1/2/5 multiple"
            );
        }
    }

    /// A logged time that is wrong is worse than no logged time, and the date
    /// arithmetic is the only place here that can be quietly wrong: leap years,
    /// the century rule, and the 400-year exception.
    #[test]
    fn utc_stamps_are_right() {
        use std::time::{Duration, UNIX_EPOCH};
        let at = |s: u64| utc_stamp(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "1970-01-01 00:00:00Z");
        assert_eq!(at(86_399), "1970-01-01 23:59:59Z");
        assert_eq!(at(86_400), "1970-01-02 00:00:00Z");
        // 2000 is a leap year (the 400-year exception), 1900 was not.
        assert_eq!(at(951_782_400), "2000-02-29 00:00:00Z");
        assert_eq!(at(951_868_800), "2000-03-01 00:00:00Z");
        // 2100 is not a leap year (the century rule).
        assert_eq!(at(4_107_456_000), "2100-02-28 00:00:00Z");
        assert_eq!(at(4_107_542_400), "2100-03-01 00:00:00Z");
        // an ordinary leap day, and the last second of a year
        assert_eq!(at(1_709_164_800), "2024-02-29 00:00:00Z");
        assert_eq!(at(1_767_225_599), "2025-12-31 23:59:59Z");
        // a clock set before the epoch still prints a date rather than panicking
        assert_eq!(utc_stamp(UNIX_EPOCH - Duration::from_secs(1)), "1969-12-31 23:59:59Z");
    }
}
