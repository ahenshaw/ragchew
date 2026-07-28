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

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread;

use eframe::egui::{self, Color32, FontId, Pos2, Rect, Stroke};

use ragchew::protocol::{self, ModeId, Protocol};
use ragchew::spectrogram::WINDOW;
use ragchew::{wav, Spectrogram, SAMPLE_RATE};

use crate::channels::{Channel, ChannelSet};
use crate::layout;
use crate::qso::{Dir, QsoSet};
use crate::scene::{self, Geometry};
use crate::tx::{self, Tx};
use crate::waterfall::{self, Viewport};

const HIGHLIGHT_SECS: f64 = 2.5;

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
    #[arg(long, conflicts_with_all = ["file", "demo", "weak"])]
    live: bool,

    /// Synthetic band carrying both protocols, eleven stations.
    #[arg(long, conflicts_with_all = ["file", "weak"])]
    demo: bool,

    /// Synthetic Olivia band, every station below the noise floor.
    #[arg(long, alias = "olivia", conflicts_with = "file")]
    weak: bool,
}

pub fn run() -> eframe::Result<()> {
    let args = <Args as clap::Parser>::parse();

    // A monitor with nothing on the command line should be monitoring: opening
    // deaf and waiting to be told to listen is a worse default than the one
    // thing the program is for. A file, a demo, or an explicit --live still win.
    let live = args.live || (args.file.is_none() && !args.demo && !args.weak);

    let title = if live {
        "ragchew — live"
    } else if args.weak {
        "ragchew — weak-signal demo"
    } else if args.demo {
        "ragchew — demo"
    } else {
        "ragchew"
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
            .with_app_id("ragchew"),
        persist_window: true,
        ..Default::default()
    };
    // Build the app inside the creator so a non-Send audio stream never crosses
    // a thread boundary.
    eframe::run_native(
        title,
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
            if live {
                app.go_live();
            } else if args.weak {
                app.set_source(crate::demo::synth_weak(), "weak-signal demo");
            } else if args.demo {
                app.set_source(crate::demo::synth(), "demo band");
            } else if let Some(path) = &args.file {
                app.open_file(path);
            }
            Ok(Box::new(app))
        }),
    )
}

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
/// Olivia violet through cyan — so you can tell the two families apart at a
/// glance and still read the individual mode off the shade.
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
    channels: ChannelSet,
    norm: (f32, f32),
    cols_since_norm: usize,
    t_now: f64,
    devices: Vec<crate::audio::AudioDevice>,
    selected: Option<String>, // current device name
}

impl LiveMode {
    fn start(
        band: (f64, f64),
        modes: Vec<ModeId>,
        device: Option<String>,
        show_all_inputs: bool,
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
            channels: ChannelSet::new(15.0),
            norm: (0.0, 1.0),
            cols_since_norm: 0,
            t_now: 0.0,
            devices: Vec::new(),
            selected: None,
        };
        lm.open(device);
        lm
    }

    /// (Re)open capture on the given device (or the default), resetting the
    /// rolling spectrogram and channels.
    fn open(&mut self, name: Option<String>) {
        // dropping the old LiveAudio stops its stream; dropping the old receiver
        // makes the old decoder thread exit on its next send.
        self.audio = None;
        self.frames_rx = None;
        self.spec = Arc::new(Spectrogram::empty(self.band.0, self.band.1, WINDOW / 4));
        self.next_col = 0;
        self.channels = ChannelSet::new(15.0);
        self.norm = (0.0, 1.0);
        self.cols_since_norm = 0;
        self.t_now = 0.0;
        self.devices = crate::audio::input_devices(self.show_all_inputs);
        // A remembered device may be gone — a USB interface unplugged, or
        // settings carried to another machine. Falling back to the default
        // input beats refusing to listen at all.
        let started = crate::audio::LiveAudio::start(name.as_deref())
            .or_else(|e| if name.is_some() { crate::audio::LiveAudio::start(None) } else { Err(e) });
        match started {
            Ok(audio) => {
                self.frames_rx =
                    Some(crate::audio::spawn_decoder(audio.buf.clone(), self.modes.clone()));
                self.selected = Some(audio.device_id.clone());
                self.err = None;
                self.audio = Some(audio);
            }
            Err(e) => {
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
            self.frames_rx =
                Some(crate::audio::spawn_decoder(audio.buf.clone(), self.modes.clone()));
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
            if s.columns.len() > MAX_COLS {
                let drop = s.columns.len() - MAX_COLS;
                s.columns.drain(0..drop);
            }
            self.cols_since_norm += new_windows.len();
        }

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
            (None, Some(a)) => format!("● LIVE {:.1} kHz", a.in_rate as f32 / 1000.0),
            _ => "● LIVE".to_string(),
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
    tex_key: (usize, usize, i64, i64, i64, i64),

    /// Every mode the app knows about, and whether it is being listened for.
    /// Scanning costs time per mode, so this is the app's main speed control.
    modes: Vec<(ModeId, bool)>,
    /// Kept so a change to `modes` can re-scan a file without reloading it.
    samples: Arc<Vec<f32>>,
    /// What is being monitored, for the title bar.
    source: String,
    /// Why the last open failed, if it did.
    status: Option<String>,
    /// Identifier of the audio input to prefer when going live, remembered
    /// across restarts.
    preferred_input: Option<String>,
    /// Offer every ALSA PCM in the device pickers, not just the winnowed set.
    show_all_inputs: bool,
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
}

impl App {
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
            modes: protocol::default_modes().into_iter().map(|m| (m, true)).collect(),
            samples: Arc::new(Vec::new()),
            source: String::new(),
            status: None,
            preferred_input: None,
            show_all_inputs: false,
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
        // An unknown name leaves the theme at Auto rather than at whatever the
        // last build happened to write.
        self.theme = Theme::parse(&s.theme).unwrap_or_default();
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

    /// The modes currently being listened for.
    fn enabled_modes(&self) -> Vec<ModeId> {
        self.modes.iter().filter(|(_, on)| *on).map(|(m, _)| *m).collect()
    }

    /// Switch to live capture from the default input device.
    fn go_live(&mut self) {
        self.samples = Arc::new(Vec::new());
        self.spec = None;
        self.frames = Vec::new();
        self.pending_scans = 0;
        self.status = None;
        self.source = "live input".to_string();
        self.live = Some(LiveMode::start(
            self.band,
            self.enabled_modes(),
            self.preferred_input.clone(),
            self.show_all_inputs,
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
            self.status = Some(format!("{name}: path is not valid UTF-8"));
            return;
        };
        match wav::read(p) {
            Ok((samples, rate)) if rate == SAMPLE_RATE => self.set_source(samples, &name),
            Ok((_, rate)) => {
                self.status = Some(format!(
                    "{name} is {rate} Hz; ragchew needs {SAMPLE_RATE} Hz mono \
                     (ffmpeg -i in -ac 1 -ar {SAMPLE_RATE} out.wav)"
                ));
            }
            Err(e) => self.status = Some(format!("{name}: {e}")),
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

    /// (Re)decode the loaded file with the currently enabled modes.
    ///
    /// Scanning a minute of band costs seconds per mode, which is far longer
    /// than the operator is willing to look at a blank pane — so the scan is
    /// split one job per mode and each job's text is shown the moment it lands,
    /// while playback runs off the (much faster) spectrogram. Any batch that
    /// arrives after the clock has already passed it simply appears in place.
    fn rescan(&mut self) {
        let (frames_tx, frames_rx) = channel();
        self.frames_rx = frames_rx;
        self.frames = Vec::new();
        self.pending_scans = 0;
        if self.samples.is_empty() {
            return;
        }
        // One thread per mode. Every mode's scan is independent of every other —
        // nothing arbitrates between them afterwards, a signal is kept out of a
        // neighbouring mode's results by that mode's own threshold — so this
        // reports exactly what one pass over all of them would, in about the
        // time of the slowest single mode.
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
            ui.menu_button(label, |ui| {
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
                for (m, enabled) in self.modes.iter_mut().filter(|(m, _)| m.protocol() == p) {
                    ui.horizontal(|ui| {
                        if ui.checkbox(enabled, "").changed() {
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
            if ui.button("+").on_hover_text("new QSO").clicked() {
                let hz = (self.vp.f_lo + self.vp.f_hi) / 2.0;
                let mode = self.enabled_modes().first().copied().unwrap_or(ModeId::Js8(
                    ragchew::js8::Mode::Normal,
                ));
                self.qsos.open_blank(hz, mode, now_s);
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
        let Some(q) = self.qsos.active_qso_mut() else { return };
        let bound = q.channel.is_some();

        // Every widget below is scoped to this QSO's id, so each tab keeps its
        // own text cursor and its own scroll position in the log. Without it
        // egui identifies the widgets by where they sit in the layout, and all
        // the tabs — being the same layout — would share one set.
        let (send, abort) = ui.push_id(q.id, |ui| Self::qso_body(ui, q, &modes, bound, busy, now_s)).inner;

        if abort {
            if let Some(t) = self.tx.as_mut() {
                t.abort();
            }
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
    fn qso_body(
        ui: &mut egui::Ui,
        q: &mut crate::qso::Qso,
        modes: &[ModeId],
        bound: bool,
        busy: f64,
        now_s: f64,
    ) -> (bool, bool) {
        let (mut send, mut abort) = (false, false);
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
                ui.add(egui::TextEdit::singleline(&mut q.rst_sent).desired_width(44.0))
                    .on_hover_text("sent");
                ui.label("/");
                ui.add(egui::TextEdit::singleline(&mut q.rst_rcvd).desired_width(44.0))
                    .on_hover_text("received");
            });
            ui.end_row();

            ui.label("Signal");
            if bound {
                ui.label(format!(
                    "{:.0} Hz  {:+.1} drift  ×{:.1}",
                    q.hz,
                    q.drift_hz,
                    q.quality.max(0.0)
                ))
                .on_hover_text(
                    "carrier, how far it has wandered since first heard, and the last \
                     decode's confidence in multiples of the mode's noise floor",
                );
            } else {
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut q.hz).speed(1.0).range(0.0..=4000.0))
                        .on_hover_text("carrier to transmit on");
                    ui.label("Hz");
                });
            }
            ui.end_row();

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

        // The log takes whatever height is left over once the reply box and the
        // send button have had theirs, so growing the window grows the part
        // worth reading rather than the part being typed into.
        let reserved = 96.0;
        let log_h = (ui.available_height() - reserved).max(60.0);
        let dark = ui.visuals().dark_mode;
        egui::ScrollArea::vertical().stick_to_bottom(true).max_height(log_h).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            for e in &q.log {
                let (color, tag) = match e.dir {
                    Dir::Rx => (mode_text_color(dark, q.mode), Dir::Rx.tag()),
                    // Our own text takes the theme's own strong colour: it
                    // is us talking, not a station with a mode colour.
                    Dir::Tx => (ui.visuals().strong_text_color(), Dir::Tx.tag()),
                };
                ui.horizontal_top(|ui| {
                    ui.monospace(offset(e.at_s - q.started_s));
                    Self::dir_badge(ui, tag, color);
                    ui.add(
                        egui::Label::new(egui::RichText::new(&e.text).monospace().color(color))
                            .wrap(),
                    );
                });
            }
        });

        ui.separator();
        ui.add(
            egui::TextEdit::multiline(&mut q.draft)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text("reply…"),
        );
        ui.horizontal(|ui| {
            let ready = !q.draft.trim().is_empty();
            send = ui
                .add_enabled(ready && busy <= 0.0, egui::Button::new("Send"))
                .on_hover_text(match q.mode.period_s() {
                    Some(p) => format!("goes out on the next {p} s cycle"),
                    None => "goes out immediately".to_string(),
                })
                .clicked();
            if busy > 0.0 {
                let c = legible(ui.visuals().dark_mode, Color32::from_rgb(255, 190, 120));
                ui.colored_label(c, format!("● TX {busy:.0} s"));
                abort = ui.small_button("abort").clicked();
            }
        });

        (send, abort)
    }

    /// Modulate the active QSO's draft and put it on the air.
    ///
    /// The output device is opened here rather than at startup, so the failure
    /// — no device, or one that will not take the format — arrives attached to
    /// the thing the operator just asked for instead of as a mystery at launch.
    fn send_active(&mut self, now_s: f64) {
        if self.tx.is_none() {
            match Tx::start(self.preferred_output.as_deref()) {
                Ok(t) => self.tx = Some(t),
                Err(e) => {
                    self.status = Some(format!("cannot transmit: {e}"));
                    return;
                }
            }
        }
        let Some(q) = self.qsos.active_qso_mut() else { return };
        let text = q.draft.trim().to_string();
        if text.is_empty() {
            return;
        }
        let samples = tx::modulate(&text, q.hz, q.mode);
        let at = tx::next_start(q.mode);
        q.draft.clear();
        q.push_tx(&text, now_s);
        if let Some(t) = self.tx.as_mut() {
            t.send(samples, at);
        }
        self.status = None;
    }

    fn channels_now(&self) -> ChannelSet {
        ChannelSet::from_decodes(
            self.frames
                .iter()
                .filter(|f| f.reveal_s <= self.t_now)
                .map(|f| f.decode.clone()),
            15.0,
        )
    }

    fn waterfall_texture(
        &mut self,
        ctx: &egui::Context,
        spec: &Spectrogram,
        px_w: usize,
        px_h: usize,
        col_hi: i64,
        span_cols: usize,
    ) -> egui::TextureHandle {
        let key = (
            px_w,
            px_h,
            (self.vp.f_lo * 16.0) as i64,
            (self.vp.f_hi * 16.0) as i64,
            col_hi,
            span_cols as i64,
        );
        if self.tex.is_none() || self.tex_key != key {
            let img = waterfall::render_window(
                spec, &self.vp, px_w.max(1), px_h.max(1), col_hi, span_cols.max(1), Some(self.norm),
            );
            let color = egui::ColorImage::from_rgba_unmultiplied([img.width, img.height], &img.rgba);
            self.tex = Some(ctx.load_texture("waterfall", color, egui::TextureOptions::LINEAR));
            self.tex_key = key;
        }
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
        // ---- LIVE: pull audio into the rolling spectrogram + channels ----
        let mut live = false;
        let mut live_spec: Option<Arc<Spectrogram>> = None;
        let mut live_channels: Option<ChannelSet> = None;
        let mut live_t = 0.0;
        let mut live_norm = self.norm;
        let mut live_label = String::new();
        let mut live_devices: Vec<crate::audio::AudioDevice> = Vec::new();
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
            live_devices = lm.devices.clone();
            live_selected = lm.selected.clone();
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
            while let Ok(f) = self.frames_rx.try_recv() {
                self.frames.extend(f);
                self.pending_scans = self.pending_scans.saturating_sub(1);
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
        let channels = live_channels.unwrap_or_else(|| self.channels_now());
        let spec = if live { live_spec } else { self.spec.clone() };
        let t_now = if live { live_t } else { self.t_now };

        let mut chosen_device: Option<String> = None;
        let mut chosen_output: Option<Option<String>> = None; // Some(None) = default
        let mut modes_changed = false;
        let mut open_file: Option<PathBuf> = None;
        let mut demo_request: Option<bool> = None; // Some(weak?)
        let mut go_live = false;
        egui::Panel::top("bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                // Settings live behind the hamburger: they are set once and
                // then left alone, so they do not deserve permanent space on a
                // bar that has to stay on one line.
                ui.menu_button("☰", |ui| {
                    ui.set_min_width(240.0);
                    ui.style_mut().spacing.slider_width = 150.0;
                    ui.add(egui::Slider::new(&mut self.text_px, 8.0..=22.0).text("text size"));
                    ui.add(egui::Slider::new(&mut self.span_s, 10.0..=120.0).text("window (s)"));
                    ui.add(
                        egui::Slider::new(&mut self.scroll_secs, 0.0..=2.0).text("scroll (s)"),
                    );
                    if !live {
                        ui.add(egui::Slider::new(&mut self.speed, 1.0..=20.0).text("speed ×"));
                    }
                    if live {
                        ui.separator();
                        ui.menu_button("Audio input", |ui| {
                            ui.set_min_width(280.0);
                            for d in &live_devices {
                                let on = live_selected.as_ref() == Some(&d.id);
                                if ui.selectable_label(on, &d.label).clicked() {
                                    chosen_device = Some(d.id.clone());
                                    ui.close();
                                }
                            }
                            ui.separator();
                            // ALSA advertises far more PCMs than there are
                            // devices; the escape hatch matters for whoever
                            // needs the one that got winnowed out.
                            if ui.checkbox(&mut show_all, "show every ALSA PCM").changed() {
                                all_inputs_toggled = true;
                            }
                        });
                    }
                    // The output is where transmissions go — the rig's codec,
                    // for anyone doing this for real — so it is offered whether
                    // or not the app is listening to a live input.
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
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("Theme");
                        for t in Theme::ALL {
                            let pick = ui.selectable_label(self.theme == t, t.label());
                            let pick = match t {
                                Theme::Auto => pick.on_hover_text("follow the desktop"),
                                _ => pick,
                            };
                            if pick.clicked() {
                                self.theme = t;
                                ui.ctx().set_theme(t.preference());
                            }
                        }
                    });
                    ui.separator();
                    if ui.button("Reset view").clicked() {
                        self.vp = Viewport { f_lo: self.band.0, f_hi: self.band.1 };
                        ui.close();
                    }
                })
                .response
                .on_hover_text("Settings");
                ui.menu_button("File", |ui| {
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
                    ui.separator();
                    if ui.button("Demo band").clicked() {
                        demo_request = Some(false);
                        ui.close();
                    }
                    if ui.button("Weak-signal band").clicked() {
                        demo_request = Some(true);
                        ui.close();
                    }
                    if ui.button("Live input").clicked() {
                        go_live = true;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.separator();
                if live {
                    // The device itself is chosen from the hamburger; the bar
                    // only reports that capture is running, and at what rate.
                    ui.strong(&live_label);
                } else {
                    // Playable as soon as there is a waterfall to play; the scan
                    // catches up on its own.
                    ui.add_enabled_ui(self.spec.is_some(), |ui| {
                        // Fixed width: "⏸ pause" and "▶ play" do not measure the
                        // same, and a button that resizes as you click it drags
                        // the whole bar left and right with it.
                        let h = ui.spacing().interact_size.y;
                        let label = if self.playing { "⏸ pause" } else { "▶ play" };
                        if ui.add_sized([78.0, h], egui::Button::new(label)).clicked() {
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
                {
                    // A counting readout that sets its own width drags the rest
                    // of the bar along every time a station appears, so the slot
                    // is sized once for the widest thing it will ever hold.
                    // Measured rather than guessed, so it survives text scaling.
                    let font = egui::TextStyle::Body.resolve(ui.style());
                    let width = ui
                        .painter()
                        .layout_no_wrap("⏳ 8888 channels".to_owned(), font, Color32::PLACEHOLDER)
                        .size()
                        .x;
                    let n = channels.channels().len();
                    let text = if !live && self.samples.is_empty() {
                        // Nothing loaded is not the same state as something
                        // still decoding: the slot stays reserved but empty.
                        String::new()
                    } else if !live && self.pending_scans > 0 {
                        // Counting while the scan is still running: the count is
                        // real, it is just not final yet.
                        format!("⏳ {n} channels")
                    } else {
                        format!("{n} channels")
                    };
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, ui.spacing().interact_size.y),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| ui.label(text),
                    );
                }
                ui.separator();
                match &self.status {
                    Some(err) => {
                        let c = legible(ui.visuals().dark_mode, Color32::from_rgb(255, 150, 150));
                        ui.colored_label(c, format!("⚠ {err}"));
                    }
                    None if !live && !self.source.is_empty() => {
                        ui.weak(&self.source);
                    }
                    None => {}
                }
                ui.separator();
                modes_changed = self.modes_menu(ui);

                // Last on the bar, deliberately. Its readout gains and loses
                // digits as playback runs, and anything to its right would be
                // shoved back and forth for the whole recording.
                if !live {
                    ui.separator();
                    ui.style_mut().spacing.slider_width = 170.0;
                    if ui
                        .add(
                            egui::Slider::new(&mut self.t_now, 0.0..=self.duration_s.max(0.001))
                                .text("s"),
                        )
                        .dragged()
                    {
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
        if let Some(weak) = demo_request {
            let samples = if weak { crate::demo::synth_weak() } else { crate::demo::synth() };
            self.set_source(samples, if weak { "weak-signal demo" } else { "demo band" });
        }
        if go_live && self.live.is_none() {
            self.go_live();
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
        self.qsos.absorb(&channels);
        let panel = egui::Panel::left("qsos")
            .resizable(true)
            .default_size(self.qso_w)
            .min_size(MIN_QSO_W)
            .show(ui, |ui| self.qso_panel(ui, t_now));
        // The panel's own rect, not its contents': the contents are inset by
        // the frame margin, and feeding that back as next launch's width would
        // shave a few pixels off the panel every time the app was restarted.
        self.qso_w = panel.response.rect.width().max(MIN_QSO_W);

        // Clicking a station's text opens its conversation. The click is
        // handled after the panel rather than inside it, where `self` is
        // already borrowed for the drawing.
        let mut open_qso: Option<Channel> = None;
        egui::CentralPanel::default().show(ui, |ui| {
            let spec = match &spec {
                Some(s) => s.clone(),
                // Nothing loaded at all is a different state from something
                // loading, and saying "computing…" forever would be a lie.
                None if !live && self.samples.is_empty() => {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            "Nothing loaded.\n\n                             File ▸ Open audio file…   for a 12 kHz mono WAV\n                             File ▸ Demo band          for a synthetic band\n                             File ▸ Live input         to decode the sound card",
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
            let span_cols = (self.span_s as f64 * SAMPLE_RATE as f64 / hop).round() as usize;
            let ppp = ctx.pixels_per_point();
            let tex = self.waterfall_texture(
                ctx,
                &spec,
                (self.waterfall_w * ppp) as usize,
                (g.height * ppp) as usize,
                col_hi,
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
            // clip text to the text panel so a chunk sliding in from behind the
            // strip (and old text sliding off the left) is masked cleanly.
            let text_painter = painter.with_clip_rect(Rect::from_min_max(
                rect.min,
                to_screen(g.strip_x0(), g.height),
            ));

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

                // scroll-in animation: when a new chunk just arrived, the row
                // starts shifted right by the chunk width and eases back to rest.
                let chunk_px = ch.last_chunk_chars as f32 * char_w;
                let offset_x = if self.scroll_secs > 1e-3 && (0.0..self.scroll_secs as f64).contains(&age) {
                    let p = (age / self.scroll_secs as f64) as f32;
                    let eased = 1.0 - (1.0 - p).powi(3); // ease-out
                    (1.0 - eased) * chunk_px
                } else {
                    0.0
                };

                // render a bit more than fits, and let the clip rects trim it
                let tail = max_chars + ch.last_chunk_chars + 2;
                let shown: String = ch.text.chars().rev().take(tail).collect::<Vec<_>>()
                    .into_iter().rev().collect();

                if (0.0..HIGHLIGHT_SECS).contains(&age) {
                    let a = (1.0 - age / HIGHLIGHT_SECS) as f32;
                    let w = (shown.chars().count() as f32 * char_w).min(g.strip_x0() - 8.0);
                    let r = Rect::from_min_max(
                        to_screen(g.strip_x0() - 6.0 - w + offset_x, y_row - line_h / 2.0),
                        to_screen(g.strip_x0() - 2.0 + offset_x, y_row + line_h / 2.0),
                    );
                    text_painter.rect_filled(r, 3.0, color.gamma_multiply(0.22 * a));
                }

                text_painter.text(
                    to_screen(g.strip_x0() - 6.0 + offset_x, y_row),
                    egui::Align2::RIGHT_CENTER,
                    shown,
                    font.clone(),
                    color,
                );

                let p = scene::leader(&g, y_row, y_freq);
                let stroke = Stroke::new(1.2, color);
                for seg in p.windows(2) {
                    painter.line_segment(
                        [to_screen(seg[0].0, seg[0].1), to_screen(seg[1].0, seg[1].1)],
                        stroke,
                    );
                }
                painter.circle_filled(to_screen(g.waterfall_x0(), y_freq), 2.5, color);

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
                        // `quality` is normalised to the protocol's own noise
                        // floor, so it is a ratio rather than a dB figure — see
                        // protocol::Decode. Saying "x4.4" is honest where "13 dB"
                        // would imply a channel SNR this is not.
                        ui.label(format!(
                            "SNR ×{:.1} above noise (best ×{:.1})",
                            ch.quality, ch.best_quality
                        ));
                        ui.weak(format!(
                            "{} decode{} · {:.1} s of audio",
                            ch.chunks,
                            if ch.chunks == 1 { "" } else { "s" },
                            (ch.last_heard_s - ch.first_heard_s) + ch.mode.chunk_secs()
                        ));
                        ui.weak("click to open a QSO");
                    });
                if resp.clicked() {
                    open_qso = Some((*ch).clone());
                }
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
        assert_eq!((restored.vp.f_lo, restored.vp.f_hi), (1200.0, 1400.0));
        assert_eq!(restored.enabled_modes(), app.enabled_modes());
        assert!(restored.enabled_modes().iter().all(|m| m.protocol() == Protocol::Olivia));
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
        app.apply(Settings { text_px: 1e6, span_s: -3.0, speed: 0.0, ..Settings::default() });
        assert!((8.0..=22.0).contains(&app.text_px), "text_px {}", app.text_px);
        assert!((10.0..=120.0).contains(&app.span_s), "span_s {}", app.span_s);
        assert!((1.0..=20.0).contains(&app.speed), "speed {}", app.speed);
    }

    /// Lay the whole interface out, QSO panel and all, with no window and no
    /// GPU behind it.
    ///
    /// egui is pure layout until something rasterises it, so this runs in CI
    /// where the app itself cannot. It is a smoke test, not a screenshot: what
    /// it catches is a panic, a bad rect, or a widget id clash in a panel that
    /// otherwise only gets exercised by someone sitting in front of it.
    fn lay_out(app: &mut App) -> egui::FullOutput {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1280.0, 800.0))),
            ..Default::default()
        };
        // `run_ui` hands the callback a `Ui` covering the whole window, which
        // is exactly what eframe gives `App::ui`.
        ctx.run_ui(input, |ui| app.draw(ui))
    }

    /// A band with two stations in it, revealed at `t`.
    fn app_with_traffic(t: f64) -> App {
        let mut app = App::base((0.0, 4000.0));
        let d = |hz: f64, at: f64, text: &str| protocol::Decode {
            mode: ModeId::Js8(ragchew::js8::Mode::Normal),
            hz,
            time_s: at,
            quality: 4.2,
            text: text.to_string(),
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
        assert_eq!(q.log.len(), 2, "second decode did not reach the log");
        assert!(q.log[1].text.contains("FN20"), "logged {:?}", q.log[1].text);
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
