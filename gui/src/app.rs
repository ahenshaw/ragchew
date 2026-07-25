//! Interactive panorama app (eframe).
//!
//! Everything is drawn into one canvas (not separate egui panels) so leader
//! lines can span from the text on the left to the waterfall on the right. The
//! geometry, nudge layout, and leader routing are shared with the headless PNG
//! validator (`scene`, `layout`, `waterfall`).
//!
//! Loading is off the UI thread: the window opens immediately, the waterfall
//! appears as soon as the (fast) spectrogram is ready, and decoded text streams
//! in once the (slower) full-band decode finishes.
//!
//! Playback: frames are *revealed* as a simulated clock advances (each frame
//! appears when its ~12.6 s of audio would have finished), so text streams into
//! channels in the same ~13-char per 15 s chunks a live receiver produces, and
//! the waterfall scrolls. Stand-in for the live cycle-aligned loop.

use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread;

use eframe::egui::{self, Color32, FontId, Pos2, Rect, Stroke};

use js8::modem::{N_SYMBOLS, SAMPLES_PER_SYMBOL, SAMPLE_RATE};
use js8::{message, modem, wav, Spectrogram};

use crate::channels::{Channel, ChannelSet, Decode};
use crate::layout;
use crate::scene::{self, Geometry};
use crate::waterfall::{self, Viewport};

const FRAME_SECS: f64 = N_SYMBOLS as f64 * SAMPLES_PER_SYMBOL as f64 / SAMPLE_RATE as f64;
const HIGHLIGHT_SECS: f64 = 2.5;

pub fn run() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let demo = args.iter().any(|a| a == "--demo");
    let live = args.iter().any(|a| a == "--live");
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "tests/vectors/john_3_16.wav".to_string());

    let title = if live {
        "JS8 Panorama — live"
    } else if demo {
        "JS8 Panorama — demo"
    } else {
        "JS8 Panorama"
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    // Build the app inside the creator so a non-Send audio stream never crosses
    // a thread boundary.
    eframe::run_native(
        title,
        options,
        Box::new(move |_cc| {
            let app = if live {
                App::live()
            } else if demo {
                App::from_samples(crate::demo::synth())
            } else {
                App::load(&path)
            };
            Ok(Box::new(app))
        }),
    )
}

/// Per-submode color, so a channel's color tells you its detected mode.
fn mode_color(m: js8::Mode) -> Color32 {
    match m {
        js8::Mode::Slow => Color32::from_rgb(130, 200, 255),   // blue
        js8::Mode::Normal => Color32::from_rgb(150, 255, 150), // green
        js8::Mode::Fast => Color32::from_rgb(255, 195, 120),   // orange
        js8::Mode::Turbo => Color32::from_rgb(255, 140, 200),  // pink
    }
}

/// A decoded frame plus the playback time at which it should appear.
struct Frame {
    reveal_s: f64,
    decode: Decode,
}

/// Live-capture state: a rolling spectrogram fed from the audio buffer and
/// channels accumulated from the cycle-aligned decoder.
struct LiveMode {
    band: (f64, f64),
    audio: Option<crate::audio::LiveAudio>,
    err: Option<String>,
    frames_rx: Option<std::sync::mpsc::Receiver<Vec<Decode>>>,
    spec: Arc<Spectrogram>,
    next_col: u64,
    channels: ChannelSet,
    norm: (f32, f32),
    cols_since_norm: usize,
    t_now: f64,
    devices: Vec<String>,
    selected: Option<String>, // current device name
}

impl LiveMode {
    fn start(band: (f64, f64)) -> LiveMode {
        let mut lm = LiveMode {
            band,
            audio: None,
            err: None,
            frames_rx: None,
            spec: Arc::new(Spectrogram::empty(band.0, band.1, SAMPLES_PER_SYMBOL / 4)),
            next_col: 0,
            channels: ChannelSet::new(15.0),
            norm: (0.0, 1.0),
            cols_since_norm: 0,
            t_now: 0.0,
            devices: crate::audio::input_devices(),
            selected: None,
        };
        lm.open(None);
        lm
    }

    /// (Re)open capture on the given device (or the default), resetting the
    /// rolling spectrogram and channels.
    fn open(&mut self, name: Option<String>) {
        // dropping the old LiveAudio stops its stream; dropping the old receiver
        // makes the old decoder thread exit on its next send.
        self.audio = None;
        self.frames_rx = None;
        self.spec = Arc::new(Spectrogram::empty(self.band.0, self.band.1, SAMPLES_PER_SYMBOL / 4));
        self.next_col = 0;
        self.channels = ChannelSet::new(15.0);
        self.norm = (0.0, 1.0);
        self.cols_since_norm = 0;
        self.t_now = 0.0;
        self.devices = crate::audio::input_devices();
        match crate::audio::LiveAudio::start(name.as_deref()) {
            Ok(audio) => {
                self.frames_rx = Some(crate::audio::spawn_decoder(audio.buf.clone()));
                self.selected = Some(audio.device_name.clone());
                self.err = None;
                self.audio = Some(audio);
            }
            Err(e) => {
                self.selected = name;
                self.err = Some(e);
            }
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

        self.t_now = global_len as f64 / js8::modem::SAMPLE_RATE as f64;

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

    fn device_label(&self) -> String {
        match (&self.err, &self.audio) {
            (Some(e), _) => format!("audio error: {e}"),
            (None, Some(a)) => format!("● LIVE — {} @ {} Hz", a.device_name, a.in_rate),
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
    frames_ready: bool,
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
            frames_ready: false,
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
            live: None,
        }
    }

    /// Live capture from the default input device.
    fn live() -> App {
        let band = (0.0, 4000.0);
        let mut app = App::base(band);
        app.live = Some(LiveMode::start(band));
        app
    }

    fn load(path: &str) -> App {
        let (samples, _rate) = wav::read(path).unwrap_or((Vec::new(), SAMPLE_RATE));
        App::from_samples(samples)
    }

    fn from_samples(samples: Vec<f32>) -> App {
        let band = (0.0, 4000.0);
        let duration_s = samples.len() as f64 / SAMPLE_RATE as f64;

        // Do the heavy work (spectrogram + full-band decode) off the UI thread.
        let (spec_tx, spec_rx) = channel();
        let (frames_tx, frames_rx) = channel();
        thread::spawn(move || {
            let spec = Spectrogram::compute(&samples, band.0, band.1, SAMPLES_PER_SYMBOL / 4);
            let norm = waterfall::percentiles(&spec);
            let _ = spec_tx.send((Arc::new(spec), norm));

            let mut frames: Vec<Frame> = modem::decode_all_multi(&samples, 300.0, 2600.0, 30)
                .into_iter()
                .map(|r| {
                    let time_s = r.offset as f64 / SAMPLE_RATE as f64;
                    Frame {
                        reveal_s: time_s + FRAME_SECS,
                        decode: Decode {
                            hz: r.hz,
                            time_s,
                            text: message::unpack(&r.a87).text(),
                            mode: r.mode,
                        },
                    }
                })
                .collect();
            frames.sort_by(|a, b| a.reveal_s.partial_cmp(&b.reveal_s).unwrap());
            let _ = frames_tx.send(frames);
        });

        let mut app = App::base(band);
        app.duration_s = duration_s;
        app.spec_rx = spec_rx;
        app.frames_rx = frames_rx;
        app
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

    fn zoom(&mut self, focus_hz: f64, factor: f64) {
        let span = ((self.vp.f_hi - self.vp.f_lo) * factor).clamp(50.0, self.band.1 - self.band.0);
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ---- LIVE: pull audio into the rolling spectrogram + channels ----
        let mut live = false;
        let mut live_spec: Option<Arc<Spectrogram>> = None;
        let mut live_channels: Option<ChannelSet> = None;
        let mut live_t = 0.0;
        let mut live_norm = self.norm;
        let mut live_label = String::new();
        let mut live_devices: Vec<String> = Vec::new();
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
                    // hold at t=0 until decode is done
                }
            }
            if !self.frames_ready {
                if let Ok(f) = self.frames_rx.try_recv() {
                    self.frames = f;
                    self.frames_ready = true;
                    self.t_now = 0.0;
                    self.playing = true; // play cleanly from the start
                }
            }
            if self.spec.is_none() || !self.frames_ready {
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

        if live {
            self.norm = live_norm;
        }
        let channels = live_channels.unwrap_or_else(|| self.channels_now());
        let spec = if live { live_spec } else { self.spec.clone() };
        let t_now = if live { live_t } else { self.t_now };

        let mut chosen_device: Option<String> = None;
        egui::TopBottomPanel::top("bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if live {
                    ui.strong(&live_label);
                    ui.separator();
                    egui::ComboBox::from_label("input")
                        .selected_text(live_selected.clone().unwrap_or_else(|| "—".to_string()))
                        .show_ui(ui, |ui| {
                            for d in &live_devices {
                                if ui
                                    .selectable_label(live_selected.as_deref() == Some(d.as_str()), d)
                                    .clicked()
                                {
                                    chosen_device = Some(d.clone());
                                }
                            }
                        });
                } else {
                    ui.add_enabled_ui(self.frames_ready, |ui| {
                        if ui.button(if self.playing { "⏸ pause" } else { "▶ play" }).clicked() {
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
                    ui.add(egui::Slider::new(&mut self.speed, 1.0..=20.0).text("speed×"));
                    ui.separator();
                    ui.style_mut().spacing.slider_width = 240.0;
                    if ui
                        .add(egui::Slider::new(&mut self.t_now, 0.0..=self.duration_s.max(0.001)).text("t (s)"))
                        .dragged()
                    {
                        self.playing = false;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label(format!("view {:.0}–{:.0} Hz", self.vp.f_lo, self.vp.f_hi));
                if ui.button("reset view").clicked() {
                    self.vp = Viewport { f_lo: self.band.0, f_hi: self.band.1 };
                }
                ui.add(egui::Slider::new(&mut self.text_px, 8.0..=22.0).text("text"));
                ui.add(egui::Slider::new(&mut self.span_s, 10.0..=120.0).text("window s"));
                ui.add(egui::Slider::new(&mut self.scroll_secs, 0.0..=2.0).text("scroll s"));
                ui.separator();
                if !live && !self.frames_ready {
                    ui.label("⏳ decoding…");
                } else {
                    ui.label(format!("{} channels", channels.channels().len()));
                }
                ui.separator();
                for m in [js8::Mode::Slow, js8::Mode::Normal, js8::Mode::Fast, js8::Mode::Turbo] {
                    ui.colored_label(mode_color(m), m.name());
                }
                ui.separator();
                ui.weak("scroll: pan · pinch or ctrl+scroll: zoom · shift+scroll: text · drag: pan");
            });
        });

        // switch input device if the user picked a different one
        if let Some(name) = chosen_device {
            if live_selected.as_deref() != Some(name.as_str()) {
                if let Some(lm) = self.live.as_mut() {
                    lm.open(Some(name));
                }
            }
        }

        // Frequency scrollbar (full band = full height; thumb = current view).
        egui::SidePanel::left("freqbar")
            .exact_width(16.0)
            .resizable(false)
            .show(ctx, |ui| {
                let (resp, painter) =
                    ui.allocate_painter(ui.available_size(), egui::Sense::click_and_drag());
                let rect = resp.rect;
                let (blo, bhi) = self.band;
                let span = bhi - blo;
                let h = rect.height();
                let y_of = |hz: f64| rect.top() + ((bhi - hz) / span) as f32 * h;

                painter.rect_filled(rect.shrink(3.0), 4.0, Color32::from_gray(28));
                let thumb = Rect::from_min_max(
                    Pos2::new(rect.left() + 3.0, y_of(self.vp.f_hi)),
                    Pos2::new(rect.right() - 3.0, y_of(self.vp.f_lo)),
                );
                let hot = resp.hovered() || resp.dragged();
                painter.rect_filled(thumb, 4.0, Color32::from_gray(if hot { 120 } else { 80 }));

                if resp.dragged() {
                    self.pan_hz(-(resp.drag_delta().y as f64) * span / h as f64);
                } else if resp.clicked() {
                    if let Some(p) = resp.interact_pointer_pos() {
                        self.center_on(bhi - ((p.y - rect.top()) / h) as f64 * span);
                    }
                }
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let spec = match &spec {
                Some(s) => s.clone(),
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
            let g = Geometry {
                width: rect.width(),
                height: rect.height(),
                waterfall_w: self.waterfall_w,
                strip_w: self.strip_w,
            };
            let to_screen = |x: f32, y: f32| Pos2::new(rect.min.x + x, rect.min.y + y);

            // interaction
            let y_to_hz = |y: f32| {
                let f = (y - rect.min.y) / g.height.max(1.0);
                self.vp.f_hi - f as f64 * (self.vp.f_hi - self.vp.f_lo)
            };
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
            if resp.dragged() {
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

            let div = Stroke::new(1.0, Color32::from_gray(70));
            painter.line_segment([to_screen(g.strip_x0(), 0.0), to_screen(g.strip_x0(), g.height)], div);
            painter.line_segment([to_screen(g.strip_x1(), 0.0), to_screen(g.strip_x1(), g.height)], div);

            // channels: nudge layout + streaming text + leaders
            let visible: Vec<&Channel> =
                channels.channels().iter().filter(|c| g.visible(&self.vp, c.hz)).collect();
            let ideals: Vec<f32> = visible.iter().map(|c| g.hz_to_y(&self.vp, c.hz)).collect();
            let line_h = self.text_px + 6.0;
            let rows = layout::place(&ideals, line_h, line_h, g.height - line_h);

            let font = FontId::monospace(self.text_px);
            let char_w = self.text_px * 0.6;
            let max_chars = ((g.strip_x0() - 8.0) / char_w).max(1.0) as usize;
            // clip text to the text panel so a chunk sliding in from behind the
            // strip (and old text sliding off the left) is masked cleanly.
            let text_painter = painter.with_clip_rect(Rect::from_min_max(
                rect.min,
                to_screen(g.strip_x0(), g.height),
            ));

            for (i, ch) in visible.iter().enumerate() {
                let color = mode_color(ch.mode);
                let y_row = rows[i];
                let y_freq = g.hz_to_y(&self.vp, ch.hz);

                // Time since this channel's newest frame arrived, in *wall-clock*
                // seconds. File playback time runs at `speed`x (live is real-time),
                // so animations look the same regardless of playback speed.
                let eff_speed = if live { 1.0 } else { self.speed.max(1e-3) };
                let playback_age = t_now - (ch.last_heard_s + FRAME_SECS);
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
            }

            // frequency ticks
            let tick = Stroke::new(1.0, Color32::from_gray(90));
            for khz in 0..=4 {
                let hz = khz as f64 * 1000.0;
                if g.visible(&self.vp, hz) {
                    let y = g.hz_to_y(&self.vp, hz);
                    painter.line_segment([to_screen(0.0, y), to_screen(8.0, y)], tick);
                    painter.text(
                        to_screen(10.0, y),
                        egui::Align2::LEFT_CENTER,
                        format!("{hz:.0} Hz"),
                        FontId::proportional(11.0),
                        Color32::from_gray(120),
                    );
                }
            }
        });
    }
}
