//! Live audio capture (cpal) → 12 kHz mono, with a rolling buffer and the
//! decode threads that feed off it. Desktop-only.
//!
//! Untested in CI (no audio device / display here); verified to compile.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use ragchew::protocol::{self, ModeId};
use ragchew::spectrogram::WINDOW;

use crate::continuous::Continuous;
use crate::diag::{self, Health, RunningGuard, ThreadHealth};
use crate::record::Tap;
use crate::traffic;
use crate::{diag_info, diag_warn};

/// Everything is decoded at the crate's internal rate; the sound card is
/// resampled to it on the way in.
const RATE: u32 = ragchew::SAMPLE_RATE;

/// Band scanned for signals (Hz). Wider costs time on every decode pass.
const BAND: (f64, f64) = (300.0, 2600.0);

fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

/// How far past its capacity the audio buffer is allowed to run before it is
/// cut back. Thirty seconds of slack costs 1.4 MB and turns a memmove on every
/// audio callback into one every thirty seconds.
const TRIM_BLOCK: usize = RATE as usize * 30;

/// A rolling buffer of 12 kHz mono samples with absolute (global) indexing and
/// a wall-clock anchor, so any UTC time maps to a sample range.
pub struct AudioBuf {
    samples: Vec<f32>,
    global_start: u64, // global index of samples[0]
    start_unix: f64,   // wall-clock time of global sample 0
    cap: usize,
    started: bool,
    /// When the last block of samples arrived, so a block that arrives late
    /// carrying no more audio than usual can be recognised as a gap.
    last_append: f64,
    /// Wall clock the capture has lost to stalls and never made up, total, and
    /// how much of it the heartbeat has yet to report. See [`AudioBuf::append`].
    slipped_s: f64,
    unreported_slip_s: f64,
    /// Where a recording in progress is fed from, if any. It hangs off the
    /// buffer rather than off the stream because the callback already holds
    /// this lock — a separate one would be a second thing for the audio thread
    /// to wait on.
    tap: Option<Tap>,
}

/// Wall clock unaccounted for by arriving audio before the capture is taken to
/// have stalled and the anchor is moved.
///
/// Well above callback jitter, and it cannot fire on a merely *late* callback:
/// a block that arrives 300 ms late carrying 300 ms of audio accounts for its
/// own delay, because the comparison is against the samples in that block
/// rather than against an expected cadence. Only audio that was never captured
/// registers. Half a second is the margin the cycle-aligned decoder allows
/// either side of a cycle, so anything past it is already breaking decodes.
const STALL_GAP_S: f64 = 0.5;

impl AudioBuf {
    fn new(cap: usize) -> AudioBuf {
        AudioBuf {
            samples: Vec::new(),
            global_start: 0,
            start_unix: 0.0,
            cap,
            started: false,
            last_append: 0.0,
            slipped_s: 0.0,
            unreported_slip_s: 0.0,
            tap: None,
        }
    }
    /// Start or stop feeding a recording from this buffer.
    pub fn set_tap(&mut self, tap: Option<Tap>) {
        self.tap = tap;
    }

    /// Take a block of captured samples, and keep the wall-clock anchor honest.
    ///
    /// Every decoder that asks for a window in UTC gets it through
    /// `start_unix` plus a sample count at a nominal 12 kHz, so that anchor is
    /// the only thing tying the audio to the clock. A capture that stalls —
    /// a browser tab throttled, a PipeWire link that idles, a USB glitch —
    /// silently breaks it: the samples that were never captured are never made
    /// up, so from then on every window is cut however many seconds early, and
    /// eventually off the end of the buffer entirely. The waterfall walks the
    /// buffer by sample index and is unaffected, which is what makes this look
    /// like a band that went quiet rather than a fault.
    ///
    /// So a stall moves the anchor by what it cost. The alternative — reopening
    /// the capture, which builds a fresh buffer — is not always available: an
    /// operator patching audio in from elsewhere may only be able to make that
    /// route to a stream that is already running, and dropping the stream
    /// destroys it.
    ///
    /// This corrects the mapping for everything captured *after* the stall,
    /// which is what any decoder is looking at. Audio still held from before it
    /// is now misdated by the correction — unavoidable with one anchor, and the
    /// lesser error by far.
    fn append(&mut self, s: &[f32]) {
        let now = unix_now();
        if !self.started {
            self.start_unix = now;
            self.started = true;
        } else {
            let gap = (now - self.last_append) - s.len() as f64 / RATE as f64;
            if gap > STALL_GAP_S {
                self.start_unix += gap;
                self.slipped_s += gap;
                self.unreported_slip_s += gap;
            }
        }
        self.last_append = now;
        if let Some(t) = &self.tap {
            t.push(s);
        }
        self.samples.extend_from_slice(s);
        // Trimming moves every sample that survives, so cutting back to exactly
        // `cap` on the callback that first exceeds it means moving the whole
        // buffer on *every* callback from then on — 14 MB a time, tens to
        // hundreds of times a second, inside the audio callback and holding the
        // lock the UI thread and both decoders need. The buffer is allowed to
        // overshoot and is then cut in one block, which is the same memmove a
        // few thousand times less often.
        if self.samples.len() > self.cap + TRIM_BLOCK {
            let drop = self.samples.len() - self.cap;
            self.samples.drain(0..drop);
            self.global_start += drop as u64;
        }
    }
    /// Global index one past the newest sample (i.e. total samples ever seen).
    pub fn global_len(&self) -> u64 {
        self.global_start + self.samples.len() as u64
    }
    pub fn global_start(&self) -> u64 {
        self.global_start
    }
    pub fn start_unix(&self) -> f64 {
        self.start_unix
    }
    pub fn started(&self) -> bool {
        self.started
    }
    /// How far the captured audio has fallen behind the wall clock, in seconds.
    ///
    /// **The number to look at first when decoding has stopped but the
    /// waterfall has not.** The two halves of the app address this buffer
    /// differently: the waterfall walks it by sample index, so it is correct
    /// whatever the clock says, while every decoder asks for a window in UTC
    /// and gets it translated through `start_unix` and a nominal sample rate.
    ///
    /// That translation assumes samples arrive at exactly [`RATE`] for ever.
    /// They do not. A sound card's clock differs from the system's by tens of
    /// parts per million, and — far larger — every overrun, USB glitch or
    /// suspend drops samples that are never made up, because `append` has no
    /// idea anything is missing and simply carries on. Both errors accumulate
    /// in one direction over a long run.
    ///
    /// Once this exceeds a decoder's tolerance for where its audio should be,
    /// that decoder asks for a window the buffer no longer holds, gets a short
    /// slice or none, and stops decoding — silently, and with the waterfall
    /// still perfect. Hours of drift look exactly like a band that went quiet.
    pub fn lag_s(&self) -> f64 {
        if !self.started {
            return 0.0;
        }
        unix_now() - (self.start_unix + self.global_len() as f64 / RATE as f64)
    }
    /// Wall clock lost to stalls in total, and how much of it is new since this
    /// was last asked. The heartbeat asks, so a stall is reported once and the
    /// running total says how bad the capture has been overall.
    fn take_slip(&mut self) -> (f64, f64) {
        let fresh = self.unreported_slip_s;
        self.unreported_slip_s = 0.0;
        (self.slipped_s, fresh)
    }
    /// Copy samples for global index range `[g0, g1)` (clamped to what's held).
    pub fn slice_global(&self, g0: u64, g1: u64) -> Vec<f32> {
        let lo = g0.max(self.global_start);
        let hi = g1.min(self.global_len());
        if hi <= lo {
            return Vec::new();
        }
        let a = (lo - self.global_start) as usize;
        let b = (hi - self.global_start) as usize;
        self.samples[a..b].to_vec()
    }
    /// The spectrogram window at global index `g0`, or `None` if not fully
    /// held yet.
    pub fn window_at(&self, g0: u64) -> Option<Vec<f32>> {
        let n = WINDOW as u64;
        if g0 < self.global_start || g0 + n > self.global_len() {
            return None;
        }
        Some(self.slice_global(g0, g0 + n))
    }
}

// ---- resampling to 12 kHz ----

/// RBJ biquad low-pass (anti-alias before decimation).
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}
impl Biquad {
    fn low_pass(fs: f32, fc: f32, q: f32) -> Biquad {
        let w0 = 2.0 * std::f32::consts::PI * (fc / fs).min(0.49);
        let (sn, cs) = w0.sin_cos();
        let alpha = sn / (2.0 * q);
        let b1 = 1.0 - cs;
        let b0 = b1 / 2.0;
        let b2 = b0;
        let a0 = 1.0 + alpha;
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: (-2.0 * cs) / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        // transposed direct form II
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Streaming mono downmix + anti-aliased linear resampler to 12 kHz.
struct Resampler {
    channels: usize,
    step: f64, // input samples per output sample
    t: f64,    // position of next output, in input-index units
    n: u64,    // input index counter
    prev: f32,
    lp: Biquad,
}
impl Resampler {
    fn new(in_rate: u32, channels: usize) -> Resampler {
        Resampler {
            channels: channels.max(1),
            step: in_rate as f64 / RATE as f64,
            t: 0.0,
            n: 0,
            prev: 0.0,
            lp: Biquad::low_pass(in_rate as f32, 5000.0, 0.707),
        }
    }
    /// Feed interleaved device-rate frames, get 12 kHz mono out.
    fn process(&mut self, data: &[f32]) -> Vec<f32> {
        let ch = self.channels;
        let mut out = Vec::with_capacity(data.len() / ch + 4);
        for frame in data.chunks(ch) {
            let mono = frame.iter().copied().sum::<f32>() / ch as f32;
            let x = self.lp.process(mono);
            while self.t <= self.n as f64 {
                let frac = (self.t - (self.n as f64 - 1.0)).clamp(0.0, 1.0) as f32;
                out.push(self.prev * (1.0 - frac) + x * frac);
                self.t += self.step;
            }
            self.prev = x;
            self.n += 1;
        }
        out
    }
}

// ---- live capture ----

/// A running capture: keeps the cpal stream alive and exposes the rolling buffer.
pub struct LiveAudio {
    pub buf: Arc<Mutex<AudioBuf>>,
    /// Stable identifier of the device in use, for remembering the choice.
    pub device_id: String,
    /// Human name of the device in use, for showing it.
    pub device_name: String,
    pub in_rate: u32,
    _stream: cpal::Stream,
}

impl LiveAudio {
    /// Start or stop feeding a recording from this capture.
    pub fn set_tap(&self, tap: Option<Tap>) {
        self.buf.lock().unwrap().set_tap(tap);
    }
}

/// An audio endpoint — capture or playback — as offered to the operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    /// Stable identifier. cpal documents `DeviceId` as safe to persist, which
    /// a device *name* is not: a machine here offers twenty inputs all called
    /// "sof-hda-dsp", and picking one by name would be a coin toss.
    pub id: String,
    /// What to show in the picker.
    pub label: String,
}

/// ALSA plugin PCMs that are genuine capture routes. The rest of what ALSA
/// advertises — `null`, `lavrate`, `samplerate`, `speexrate`, `speex`, `upmix`,
/// `vdownmix`, `oss` — are format converters and routers with no source of
/// their own, and offering them as recording inputs is just noise.
const ROUTE_PCMS: &[&str] = &["default", "pipewire", "pulse", "jack"];

/// Whether a device id names a PCM that goes through the sound server rather
/// than straight at a card.
///
/// It is the sound server that makes routing a question at all: opening one of
/// these puts the capture into a graph, where what feeds it is a separate
/// choice from which device was opened. `default` counts because on a PipeWire
/// desktop that is what it resolves to — and it is what most people have
/// selected without ever choosing it.
///
/// Matched whole, not up to the first colon: `default:CARD=PCH` is a *card's*
/// own default PCM and goes straight at that hardware, while a bare `default`
/// is the server's.
pub fn is_pipewire(id: &str) -> bool {
    let pcm = id.strip_prefix("alsa:").unwrap_or(id);
    matches!(pcm, "default" | "pipewire" | "pulse")
}

/// Ways ALSA exposes a hardware PCM, best first.
///
/// `plughw` leads because it converts rate and format, so it opens at the
/// config we ask for where raw `hw` often refuses.
const HW_PREFIXES: &[&str] = &["plughw", "hw", "dsnoop", "sysdefault", "front"];

/// Available input devices.
///
/// `all` lists every PCM the host advertises. The default is the winnowed set,
/// because ALSA advertises an enormous number of aliases: on the machine this
/// was written on, thirty entries turned out to be three real capture endpoints,
/// each visible five ways and under two different card names, plus eleven
/// plugins. See [`winnow`].
///
/// Labels that would still be indistinguishable get their identifier appended,
/// so the picker never offers the same string twice.
pub fn input_devices(all: bool) -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let Ok(devs) = host.input_devices() else { return Vec::new() };
    offer(devs, all)
}

/// Available output devices, winnowed the same way as the inputs.
///
/// ALSA is no tidier about playback PCMs than it is about capture ones, and the
/// operator picking where a transmission goes is choosing between the same
/// handful of real endpoints — the rig's codec, or the speakers.
pub fn output_devices(all: bool) -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let Ok(devs) = host.output_devices() else { return Vec::new() };
    offer(devs, all)
}

fn offer(devs: impl Iterator<Item = cpal::Device>, all: bool) -> Vec<AudioDevice> {
    let mut out: Vec<AudioDevice> = devs
        .filter_map(|d| Some(AudioDevice { id: d.id().ok()?.to_string(), label: d.to_string() }))
        .collect();
    if !all {
        out = winnow(out);
    }
    disambiguate(&mut out);
    out
}

/// Reduce ALSA's advertised PCMs to the ones worth offering.
///
/// Three rules, all working off the PCM name because cpal's device description
/// cannot tell these apart — it reports every one of them as an input-capable
/// device of unknown type:
///
/// 1. Plugin PCMs survive only if they are in [`ROUTE_PCMS`].
/// 2. A card addressed by number is dropped when any card is addressed by name,
///    since ALSA advertises every card both ways and they are the same hardware.
/// 3. Of the several ways one `(card, device)` is exposed, keep the most
///    useful — see [`HW_PREFIXES`]. `sysdefault:CARD=x` carries no device
///    number and means device 0, so it competes there.
fn winnow(devs: Vec<AudioDevice>) -> Vec<AudioDevice> {
    let pcm = |id: &str| id.split_once(':').map_or(id, |(_, rest)| rest).to_string();
    let field = |s: &str, key: &str| {
        s.split(',').find_map(|f| f.trim().strip_prefix(key)).map(|v| v.to_string())
    };
    // The card lives after the PCM prefix — `plughw:CARD=x,DEV=0` — so the
    // prefix has to come off before looking for it.
    let card_of = |id: &str| {
        let p = pcm(id);
        p.split_once(':').and_then(|(_, rest)| field(rest, "CARD="))
    };

    let named_card_exists = devs
        .iter()
        .filter_map(|d| card_of(&d.id))
        .any(|c| !c.chars().all(|ch| ch.is_ascii_digit()));

    let mut best: Vec<(String, String, usize, AudioDevice)> = Vec::new(); // card, dev, rank
    let mut routes: Vec<AudioDevice> = Vec::new();
    for d in devs {
        let pcm = pcm(&d.id);
        let Some((prefix, rest)) = pcm.split_once(':') else {
            if ROUTE_PCMS.contains(&pcm.as_str()) {
                routes.push(d);
            }
            continue;
        };
        let Some(rank) = HW_PREFIXES.iter().position(|p| *p == prefix) else { continue };
        let Some(card) = field(rest, "CARD=") else { continue };
        if named_card_exists && card.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let dev = field(rest, "DEV=").unwrap_or_else(|| "0".to_string());
        match best.iter_mut().find(|(c, v, ..)| *c == card && *v == dev) {
            Some(slot) if rank < slot.2 => *slot = (card, dev, rank, d),
            Some(_) => {}
            None => best.push((card, dev, rank, d)),
        }
    }

    routes.extend(best.into_iter().map(|(.., d)| d));
    routes
}

/// Make every label distinct by appending the identifier where two would
/// otherwise read the same.
///
/// Only the host prefix is stripped from the identifier: what distinguishes
/// `alsa:hw:CARD=x,DEV=0` from `alsa:plughw:CARD=x,DEV=0` is the part in the
/// middle, so trimming to the last colon would leave the labels identical again.
fn disambiguate(devs: &mut [AudioDevice]) {
    let dupes: Vec<String> = devs
        .iter()
        .filter(|a| devs.iter().filter(|b| b.label == a.label).count() > 1)
        .map(|d| d.label.clone())
        .collect();
    for d in devs.iter_mut().filter(|d| dupes.contains(&d.label)) {
        let tail = d.id.split_once(':').map_or(d.id.as_str(), |(_, rest)| rest);
        d.label = format!("{} [{}]", d.label.trim().trim_end_matches(','), tail);
    }
}

impl LiveAudio {
    /// Start capturing from the device with this id, or the default input.
    ///
    /// `health` gets the stream's error count; the log gets the negotiated
    /// format, which is the first thing to check when a capture that worked on
    /// one machine hears nothing on another.
    pub fn start(id: Option<&str>, health: Arc<Health>) -> Result<LiveAudio, String> {
        let host = cpal::default_host();
        let device = match id {
            Some(want) => host
                .input_devices()
                .map_err(|e| e.to_string())?
                .find(|d| d.id().map(|i| i.to_string()).as_deref() == Ok(want))
                .ok_or_else(|| format!("input device not found: {want}"))?,
            None => host
                .default_input_device()
                .ok_or_else(|| "no default input device".to_string())?,
        };
        let device_id = device.id().map(|i| i.to_string()).unwrap_or_default();
        let device_name = device.to_string();
        let supported = device.default_input_config().map_err(|e| e.to_string())?;
        let in_rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let fmt = supported.sample_format();
        let config: cpal::StreamConfig = supported.config();

        diag_info!(
            "audio",
            "opening {device_name} [{device_id}] at {in_rate} Hz, {channels} ch, {fmt:?} \
             -> {RATE} Hz mono"
        );

        let buf = Arc::new(Mutex::new(AudioBuf::new(RATE as usize * 300))); // 5 min
        // These arrive on cpal's own thread long after `start` has returned, and
        // used to go to stderr — where nobody running a windowed app ever sees
        // them. A stream that fails at three in the morning is precisely the
        // thing this is all for.
        let err_fn = {
            let health = health.clone();
            move |e| {
                Health::bump(&health.stream_errors, 1);
                diag_warn!("audio", "stream error: {e}");
            }
        };

        let stream = match fmt {
            cpal::SampleFormat::F32 => {
                let mut rs = Resampler::new(in_rate, channels);
                let b = buf.clone();
                device.build_input_stream(
                    config,
                    move |data: &[f32], _: &_| {
                        let out = rs.process(data);
                        b.lock().unwrap().append(&out);
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let mut rs = Resampler::new(in_rate, channels);
                let b = buf.clone();
                device.build_input_stream(
                    config,
                    move |data: &[i16], _: &_| {
                        let f: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
                        let out = rs.process(&f);
                        b.lock().unwrap().append(&out);
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let mut rs = Resampler::new(in_rate, channels);
                let b = buf.clone();
                device.build_input_stream(
                    config,
                    move |data: &[u16], _: &_| {
                        let f: Vec<f32> =
                            data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0).collect();
                        let out = rs.process(&f);
                        b.lock().unwrap().append(&out);
                    },
                    err_fn,
                    None,
                )
            }
            other => return Err(format!("unsupported sample format {other:?}")),
        }
        .map_err(|e| e.to_string())?;

        stream.play().map_err(|e| e.to_string())?;
        Ok(LiveAudio { buf, device_id, device_name, in_rate, _stream: stream })
    }
}

/// Spawn the decode threads for `modes` and return the channel they report on.
///
/// Scheduling splits the protocols in two, so they get a thread each. JS8 is
/// *cycle-aligned*: its frames start on UTC multiples of the submode's period,
/// so a decode is triggered the moment each cycle's audio is complete. Olivia
/// and PSK31 are *continuous*, with no schedule at all, so they are re-scanned
/// together on a rolling window — together and not one thread each, because
/// they contend for the same signals and [`protocol::decode_all`] can only
/// arbitrate between them if it sees both. Dropping the returned receiver
/// retires both threads at their next send, which is how a change of modes
/// takes effect.
pub fn spawn_decoder(
    buf: Arc<Mutex<AudioBuf>>,
    modes: Vec<ModeId>,
    health: Arc<Health>,
) -> Receiver<Vec<protocol::Decode>> {
    let (tx, rx) = channel();
    let js8: Vec<ModeId> = modes.iter().copied().filter(|m| m.period_s().is_some()).collect();
    let continuous: Vec<ModeId> =
        modes.iter().copied().filter(|m| m.period_s().is_none()).collect();
    let names = |ms: &[ModeId]| ms.iter().map(|m| m.name()).collect::<Vec<_>>().join(", ");
    diag_info!(
        "decode",
        "starting threads — cycle-aligned: [{}]; continuous: [{}]",
        names(&js8),
        names(&continuous)
    );
    // Set both ways round, not just on: modes are changed while running, and a
    // thread switched off is as deliberate as one switched on.
    health.js8.wanted.store(!js8.is_empty(), Ordering::Relaxed);
    health.continuous.wanted.store(!continuous.is_empty(), Ordering::Relaxed);
    if !js8.is_empty() {
        spawn_js8(buf.clone(), js8, tx.clone(), health.clone());
    }
    if !continuous.is_empty() {
        spawn_continuous(buf, continuous, tx, health);
    }
    rx
}

/// Cycle-aligned JS8 decoding: each submode has its own UTC grid (Slow 30 s,
/// Normal 15 s, Fast 10 s, Turbo 6 s) and each cycle is decoded shortly after
/// its frame would have finished. Decodes carry capture-relative `time_s`.
///
/// The grid is the truth for audio off a radio, and it is where every submode
/// starts. It is *not* the truth for audio piped in from somewhere: a browser
/// or a WebSDR is seconds behind the clock, frames land nowhere near the grid,
/// and a window half a second wide around it catches nothing however strong
/// the signal — the mode simply appears dead. So each submode also carries an
/// offset, learned from the audio by [`measure_offset`] when several cycles in
/// a row come back empty, and the window is shifted by it.
///
/// Nothing about that disturbs a working radio. The offset starts at zero and
/// only moves on the evidence of a decoded frame, so a band that is merely
/// quiet leaves it alone; a move under half a second is ignored as jitter; and
/// the search is rate-limited to one wide pass a minute across every submode,
/// so the cost on a silent band is one extra decode a minute rather than one
/// per cycle. The search itself emits nothing — it is a measurement, and
/// letting it print would double up whatever the next ordinary pass finds.
///
/// What a locked stream costs is honesty about *when*: a delay of more than one
/// period is indistinguishable from a shorter one, since every cycle looks like
/// the last, so decodes off a badly delayed stream are stamped a whole number
/// of cycles late.
///
/// # Keeping up
///
/// Cycles are queued as their audio completes and worked through oldest first,
/// rather than the thread taking whichever is due soonest and stepping over the
/// others. Four submodes on four grids need eleven passes every thirty seconds
/// — Turbo five, Fast three, Normal two, Slow one — and a pass costs the better
/// part of a second, so a cycle ripening while another mode is being decoded is
/// the normal case rather than the exceptional one. Stepping over those lost
/// one cycle in five, measured off a live capture, which on a band carrying one
/// frame in twelve minutes is most of the difference between hearing a station
/// and not.
///
/// The queue is bounded by [`MAX_BACKLOG_S`], because a cycle is only worth
/// decoding while the capture still holds its audio. What gets dropped is
/// counted rather than lost quietly.
fn spawn_js8(
    buf: Arc<Mutex<AudioBuf>>,
    modes: Vec<ModeId>,
    tx: Sender<Vec<protocol::Decode>>,
    health: Arc<Health>,
) {
    let spawned = thread::Builder::new().name("ragchew-js8".into()).spawn(move || {
        let guard = RunningGuard::new(health, |h| &h.js8);
        let stats = guard.stats();
        // next cycle-start (UTC seconds) to decode, per mode
        let period = |m: &ModeId| m.period_s().unwrap_or(15) as f64;
        let ready = window_ready_s; // the whole window captured, not just the frame
        let mut next: Vec<f64> =
            modes.iter().map(|m| (unix_now() / period(m)).floor() * period(m)).collect();
        // How late each mode's frames arrive, learned from the audio. Zero is
        // the cycle grid, which is right for a radio and is where every mode
        // starts; see `measure_offset`.
        let mut offset = vec![0.0f64; modes.len()];
        let mut dead = vec![0usize; modes.len()];
        let mut last_search = unix_now();
        // When each mode's cycle has all its audio: the frame is complete, plus
        // whatever the stream is running late by.
        let done = |i: usize, cycle: f64, offset: &[f64]| cycle + offset[i] + ready(&modes[i]);
        // Cycles whose audio is complete and which have not been decoded yet.
        let mut queue: Vec<(usize, f64)> = Vec::new();

        loop {
            let now = unix_now();
            // Every cycle that has ripened since the last look goes on the
            // queue. It used to advance past them instead and decode only the
            // one due soonest, which quietly threw away every cycle that came
            // due while another mode's pass was running — with all four
            // submodes enabled that was one cycle in five, measured, and on a
            // band where JS8 is sparse it is the difference between catching a
            // frame and never seeing it.
            let after: Vec<f64> =
                (0..modes.len()).map(|i| offset[i] + ready(&modes[i])).collect();
            let periods: Vec<f64> = modes.iter().map(period).collect();
            queue.extend(ripe_cycles(&mut next, &periods, &after, now));
            // A backlog past what the capture buffer holds is not worth
            // keeping: that audio has rolled away, and the pass would come back
            // short. Dropped oldest first and counted, so the heartbeat says so
            // rather than the work silently vanishing as it used to.
            let stale = queue.len();
            queue.retain(|&(i, cycle)| now - done(i, cycle, &offset) < MAX_BACKLOG_S);
            if queue.len() < stale {
                Health::bump(&stats.skipped, (stale - queue.len()) as u64);
            }
            queue.sort_by(|a, b| {
                done(a.0, a.1, &offset).partial_cmp(&done(b.0, b.1, &offset)).unwrap()
            });

            let Some((best, cycle_start)) = queue.first().copied() else {
                // Nothing ripe: wait for whichever cycle completes first. An
                // empty queue means every cycle is still in the future, so this
                // is positive — floored anyway, since a loop that decides to
                // sleep for nothing at all would spin.
                let wait = (0..modes.len())
                    .map(|i| done(i, next[i], &offset) - unix_now())
                    .fold(f64::INFINITY, f64::min);
                thread::sleep(Duration::from_secs_f64(wait.clamp(0.01, 30.0)));
                continue;
            };
            queue.remove(0);
            let mode = modes[best];

            // half a second either side of the cycle, for clock skew — shifted
            // by however late this mode's frames are actually arriving, which
            // is zero off a radio and seconds off a stream.
            stats.pass_begun();
            let pass = std::time::Instant::now();
            let (samples, win_start) =
                slice_around(&buf, cycle_start + offset[best] - 0.5, window_len_s(&mode));
            // A window the buffer could not fill is a dead cycle, not a reason
            // to skip the rest of the loop. It used to `continue` here, which
            // stepped over the offset search below — so a capture whose clock
            // had slipped could never learn where its frames were, which is
            // exactly the case the search was built for. Counted rather than
            // logged: it happens every cycle when it happens at all, and the
            // heartbeat's running total says so once.
            let decodes = if (samples.len() as f64) < mode.chunk_secs() * RATE as f64 {
                Health::bump(&stats.short, 1);
                Vec::new()
            } else {
                let found = shift_times(
                    protocol::decode_all(&samples, BAND.0, BAND.1, &[mode]),
                    win_start,
                );
                stats.pass_done(pass.elapsed().as_secs_f64() * 1e3);
                found
            };

            // A mode that has gone quiet may have nothing to hear, or may be
            // looking in the wrong place. The first is far commoner, so this
            // waits for several dead cycles and then looks at most once a
            // minute across every mode.
            dead[best] = if decodes.is_empty() { dead[best] + 1 } else { 0 };
            if dead[best] >= DEAD_CYCLES && unix_now() - last_search >= SEARCH_INTERVAL_S {
                last_search = unix_now();
                Health::bump(&stats.searches, 1);
                if let Some(found) = measure_offset(&buf, mode, period(&mode)) {
                    // Only a real move. Frames arrive a few tens of
                    // milliseconds apart from pass to pass, and chasing that
                    // would rewrite the offset for no gain.
                    if (found - offset[best]).abs() > 0.5 {
                        diag_info!(
                            "js8",
                            "{} frames are arriving {:+.2}s into the cycle, not {:+.2}s; \
                             moving the window to follow them",
                            mode.name(),
                            found,
                            offset[best]
                        );
                        offset[best] = found;
                        dead[best] = 0;
                    }
                }
            }

            if !decodes.is_empty() {
                Health::bump(&stats.decodes, decodes.len() as u64);
                log_traffic(&buf, &decodes);
                for d in &decodes {
                    diag_info!(
                        "js8",
                        "{:>14} {:7.1} Hz t={:9.2}s q={:4.1} snr={} {:?}",
                        d.mode.name(),
                        d.hz,
                        d.time_s,
                        d.quality,
                        d.snr_db.map_or("--".to_string(), |s| format!("{s:+.0}")),
                        d.text
                    );
                }
                if tx.send(decodes).is_err() {
                    diag_info!("js8", "receiver gone; thread retiring");
                    break; // UI gone
                }
            }
            // On a band that is keeping up, yield for as long as the pass took,
            // so a decoder scanning four submodes is not simply pinning a core
            // for the whole session.
            //
            // With cycles waiting, don't: the yield was doubling the cost of
            // every pass, which is most of how the thread came to be running
            // eight passes in the thirty seconds that four submodes need eleven.
            // A backlog means cycles are about to be dropped for good, and
            // being a good citizen at that moment costs decodes that do not
            // come again. The work is bounded either way — the queue is capped,
            // and a pass is real work rather than a spin.
            if queue.is_empty() {
                thread::sleep(pass.elapsed());
            }
        }
    });
    if let Err(e) = spawned {
        diag_warn!("decode", "could not spawn the cycle-aligned thread: {e}");
    }
}

/// Live decoding of the modes that run continuously.
///
/// The work is [`Continuous`], which acquires PSK stations on a block scan and
/// then tracks them with a streaming receiver so their text arrives at the rate
/// it was typed. This thread's whole job is to hand it *contiguous* audio: a
/// receiver's timing and phase loops carry from one call to the next, so a gap
/// in the stream is a gap in its lock.
///
/// That is the change from what came before, which asked the buffer for the
/// last twenty-four seconds every four and let the overlap sort itself out.
/// Here the thread remembers where it has read up to and takes only what has
/// arrived since. Olivia is unaffected in substance — same window, same
/// cadence, same gate — but it now reaches the interface through `Continuous`
/// rather than through a scan written out here.
fn spawn_continuous(
    buf: Arc<Mutex<AudioBuf>>,
    modes: Vec<ModeId>,
    tx: Sender<Vec<protocol::Decode>>,
    health: Arc<Health>,
) {
    let spawned = thread::Builder::new().name("ragchew-cont".into()).spawn(move || {
        let guard = RunningGuard::new(health, |h| &h.continuous);
        let stats = guard.stats();
        let mut cont = Continuous::new(modes.clone(), BAND);
        // Global index of the next sample to read, and the wall-clock-relative
        // time of the sample the decoder counts from, so its times come back
        // capture-relative like every other decode's.
        let mut next_g: Option<u64> = None;
        let mut origin_s = 0.0f64;

        loop {
            thread::sleep(Duration::from_secs_f64(FEED_STEP_S));
            let pass = std::time::Instant::now();

            let (samples, restart_at) = {
                let b = buf.lock().unwrap();
                if !b.started() {
                    continue;
                }
                // A hole in the stream if the buffer has rolled past where we
                // had read to. A receiver cannot be carried across one:
                // pretending otherwise reads the gap as a step in phase and
                // timing, and it never recovers.
                let restart = next_g.is_none_or(|g| g < b.global_start());
                let g0 = if restart { b.global_start() } else { next_g.unwrap() };
                let g1 = b.global_len();
                next_g = Some(g1);
                (b.slice_global(g0, g1), restart.then_some(g0))
            };

            if let Some(g0) = restart_at {
                if origin_s != 0.0 {
                    diag_warn!(
                        "cont",
                        "the audio buffer overran the decoder; its receivers are being \
                         restarted and the audio they missed is gone"
                    );
                }
                cont = Continuous::new(modes.clone(), BAND);
                origin_s = g0 as f64 / RATE as f64;
            }
            if samples.is_empty() {
                Health::bump(&stats.short, 1);
                continue;
            }

            stats.pass_begun();
            let fresh = shift_times(cont.feed(&samples), origin_s);
            stats.pass_done(pass.elapsed().as_secs_f64() * 1e3);

            if !fresh.is_empty() {
                Health::bump(&stats.decodes, fresh.len() as u64);
                log_traffic(&buf, &fresh);
                for d in &fresh {
                    diag_info!(
                        "cont",
                        "{:>14} {:7.1} Hz t={:9.2}s q={:4.1} snr={} {:?}",
                        d.mode.name(),
                        d.hz,
                        d.time_s,
                        d.quality,
                        d.snr_db.map_or("--".to_string(), |s| format!("{s:+.0}")),
                        d.text
                    );
                }
                if tx.send(fresh).is_err() {
                    diag_info!("cont", "receiver gone; thread retiring");
                    break; // UI gone
                }
            }

            // Scanning six Olivia modes across the whole band costs 3.5 s in a
            // release build and forty in a debug one. Left alone the thread
            // runs flat out for ever, and the interface — sharing a CPU and a
            // buffer lock with it — stops answering. So it yields for as long
            // as the pass overran, holding decoding to half the wall clock.
            //
            // Falling behind now costs latency rather than coverage: the audio
            // waits in the buffer and is read whole on the next pass, where the
            // sliding window this replaced simply missed whatever it stepped
            // over.
            let owed = pass.elapsed().as_secs_f64() - FEED_STEP_S;
            if owed > 0.0 {
                thread::sleep(Duration::from_secs_f64(owed));
            }
        }
    });
    if let Err(e) = spawned {
        diag_warn!("decode", "could not spawn the continuous thread: {e}");
    }
}

/// How often the continuous thread collects new audio.
///
/// Short, because a streaming receiver can only report a character once the
/// audio carrying it has been handed over, so this is the floor on how promptly
/// that happens. It is not the cost of decoding: acquisition keeps its own
/// four-second cadence inside [`Continuous`], and a pass that merely feeds
/// receivers is a few milliseconds.
const FEED_STEP_S: f64 = 0.25;

// ---- the heartbeat ----

/// How often the health line is written. An overnight run costs about a
/// thousand lines, which is nothing to keep and short enough to read.
const HEARTBEAT_S: f64 = 30.0;

/// Longest a decode thread may go between passes before the log complains.
///
/// The cycle-aligned thread waits for whichever submode is due first, so on
/// JS8 Slow alone it can legitimately be quiet for thirty seconds plus the
/// pass; the continuous one steps every four seconds unless a pass overran.
/// These are several times either, so a warning means *stuck*, not busy.
const JS8_STALL_S: f64 = 90.0;
const CONT_STALL_S: f64 = 60.0;

/// Lag past which the capture's timeline is far enough from the wall clock to
/// be moving decode windows off the signal. Half a second is the whole margin
/// the cycle-aligned decoder allows either side of a cycle, so two seconds is
/// already well into "this is why nothing decodes".
const LAG_WARN_S: f64 = 2.0;

/// Log a line of health every [`HEARTBEAT_S`] for as long as the capture lives.
///
/// This is the record that answers "it was still running but nothing decoded".
/// Each line carries both pulses — spectrogram columns consumed by the
/// interface, and decode passes run by each thread — so the two can be told
/// apart after the fact instead of being inferred from an empty screen.
///
/// It holds a [`Weak`] reference, so a capture that goes away retires it at the
/// next tick. The returned flag retires it *now*, which is what reopening on
/// another device wants: the old decode threads keep the old buffer alive until
/// their next send, so waiting for the reference to drop would leave two
/// heartbeats interleaving deltas in the log and neither of them adding up.
pub fn spawn_heartbeat(buf: &Arc<Mutex<AudioBuf>>, health: Arc<Health>) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let weak = Arc::downgrade(buf);
    let mine = stop.clone();
    let spawned = thread::Builder::new().name("ragchew-health".into()).spawn(move || {
        let mut last = Snapshot::of(&health);
        loop {
            thread::sleep(Duration::from_secs_f64(HEARTBEAT_S));
            if mine.load(Ordering::Relaxed) {
                return;
            }
            let Some(buf) = Weak::upgrade(&weak) else {
                diag_info!("health", "capture closed; heartbeat retiring");
                return;
            };
            let (lag, held_s, started, slipped, fresh_slip) = {
                let mut b = buf.lock().unwrap();
                let (slipped, fresh) = b.take_slip();
                (b.lag_s(), b.samples.len() as f64 / RATE as f64, b.started(), slipped, fresh)
            };
            if !started {
                continue;
            }
            let now = Snapshot::of(&health);
            let d = now.since(&last);
            last = now;

            diag_info!(
                "health",
                "lag={lag:+.2}s slipped={slipped:.1}s held={held_s:.0}s cols=+{} frames=+{} | \
                 js8 {} | cont {} | stream_err={} rec_drop={}",
                d.ui_columns,
                d.ui_frames,
                thread_summary(&health.js8, d.js8),
                thread_summary(&health.continuous, d.cont),
                diag::get(&health.stream_errors),
                diag::get(&health.record_dropped)
            );

            // Then the same facts as judgements, so the first thing to do with
            // a night's log is `grep -E 'WARN|ERROR'` and the answer is there.
            if fresh_slip > 0.0 {
                diag_warn!(
                    "health",
                    "the capture stalled and lost {fresh_slip:.1}s of audio ({slipped:.1}s in \
                     all); the clock anchor has been moved to follow it, so decode windows \
                     stay on the signal. Anything decoded from before the stall is dated \
                     {fresh_slip:.1}s early"
                );
            }
            if lag.abs() > LAG_WARN_S {
                diag_warn!(
                    "health",
                    "captured audio is {lag:+.1}s off the wall clock — decode windows are \
                     being cut from the wrong place; the waterfall is unaffected"
                );
            }
            check_stall(&health.js8, "cycle-aligned (JS8)", JS8_STALL_S);
            check_stall(&health.continuous, "continuous (Olivia/PSK)", CONT_STALL_S);
        }
    });
    if let Err(e) = spawned {
        diag_warn!("health", "could not spawn the heartbeat: {e}");
    }
    stop
}

fn thread_summary(h: &ThreadHealth, d: ThreadDelta) -> String {
    let ago = h.since_pass().map_or("never".to_string(), |s| format!("{s:.0}s ago"));
    // Searching and skipping are JS8's alone, so they are left off a thread
    // that does neither rather than reported as permanent zeros.
    let searches = if d.searches > 0 || h.searches.load(Ordering::Relaxed) > 0 {
        format!(" searches=+{}", d.searches)
    } else {
        String::new()
    };
    // Skipped cycles are coverage lost, so once any have been they stay on
    // every line — a rate that has gone back to zero is the thing to see.
    let skipped = if h.skipped.load(Ordering::Relaxed) > 0 {
        format!(" skipped=+{}", d.skipped)
    } else {
        String::new()
    };
    format!(
        "passes=+{} decodes=+{} short=+{}{searches}{skipped} last={ago} cost={}ms run={}",
        d.passes,
        d.decodes,
        d.short,
        h.last_ms.load(Ordering::Relaxed),
        u8::from(h.running.load(Ordering::Relaxed))
    )
}

/// One thread's counters over one heartbeat.
#[derive(Clone, Copy, Default)]
struct ThreadDelta {
    passes: u64,
    decodes: u64,
    short: u64,
    searches: u64,
    skipped: u64,
}

/// Say so, loudly, when a decode thread has stopped or gone quiet for too long.
fn check_stall(h: &ThreadHealth, what: &str, limit: f64) {
    // A thread nobody asked for is not a fault: with every mode of one kind
    // switched off, its thread is never spawned and saying so every heartbeat
    // only drowns the warnings that mean something.
    if !h.wanted.load(Ordering::Relaxed) {
        return;
    }
    if !h.running.load(Ordering::Relaxed) {
        diag_warn!("health", "the {what} decode thread is no longer running");
        return;
    }
    if let Some(since) = h.since_pass().filter(|s| *s > limit) {
        diag_warn!("health", "the {what} decode thread has not started a pass for {since:.0}s");
    }
}

/// Counter readings at one instant, so the heartbeat can report *rates* rather
/// than totals — a total that has stopped growing is easy to miss halfway down
/// a night's log, where `passes=+0` is not.
#[derive(Clone, Copy, Default)]
struct Snapshot {
    ui_columns: u64,
    ui_frames: u64,
    js8: ThreadDelta,
    cont: ThreadDelta,
}

impl ThreadDelta {
    fn of(h: &ThreadHealth) -> ThreadDelta {
        ThreadDelta {
            passes: diag::get(&h.passes),
            decodes: diag::get(&h.decodes),
            short: diag::get(&h.short),
            searches: diag::get(&h.searches),
            skipped: diag::get(&h.skipped),
        }
    }
    fn since(&self, then: &ThreadDelta) -> ThreadDelta {
        ThreadDelta {
            passes: self.passes - then.passes,
            decodes: self.decodes - then.decodes,
            short: self.short - then.short,
            searches: self.searches - then.searches,
            skipped: self.skipped - then.skipped,
        }
    }
}

impl Snapshot {
    fn of(h: &Health) -> Snapshot {
        Snapshot {
            ui_columns: diag::get(&h.ui_columns),
            ui_frames: diag::get(&h.ui_frames),
            js8: ThreadDelta::of(&h.js8),
            cont: ThreadDelta::of(&h.continuous),
        }
    }
    fn since(&self, then: &Snapshot) -> Snapshot {
        Snapshot {
            ui_columns: self.ui_columns - then.ui_columns,
            ui_frames: self.ui_frames - then.ui_frames,
            js8: self.js8.since(&then.js8),
            cont: self.cont.since(&then.cont),
        }
    }
}

/// `secs` seconds of audio starting at UTC time `from`, plus the capture-relative
/// time of the first sample returned.
fn slice_around(buf: &Arc<Mutex<AudioBuf>>, from: f64, secs: f64) -> (Vec<f32>, f64) {
    let b = buf.lock().unwrap();
    if !b.started() {
        return (Vec::new(), 0.0);
    }
    let rate = RATE as f64;
    let g0 = (((from - b.start_unix()) * rate).max(0.0)) as u64;
    let g1 = g0 + (secs * rate) as u64;
    let start = g0.max(b.global_start());
    (b.slice_global(g0, g1), start as f64 / rate)
}

/// Every cycle whose audio was complete at or before `now` and which has not
/// been handed out yet, in the order the cycles completed. `next` is advanced
/// past them, so no cycle is ever returned twice.
///
/// `complete_after` is how long past a cycle's start its audio is all in: the
/// frame's own length, slack, and however late the stream is running.
///
/// Split out from the decode loop because it is the rule that had the bug — the
/// loop used to advance `next` past a ripe cycle without decoding it whenever
/// another mode's pass had been running, which lost one cycle in five with four
/// submodes enabled. As a function it can be tested against a clock that jumps,
/// which is what a busy decoder looks like from here.
fn ripe_cycles(
    next: &mut [f64],
    period: &[f64],
    complete_after: &[f64],
    now: f64,
) -> Vec<(usize, f64)> {
    let mut out: Vec<(usize, f64)> = Vec::new();
    for i in 0..next.len() {
        while next[i] + complete_after[i] <= now {
            out.push((i, next[i]));
            next[i] += period[i];
        }
    }
    out.sort_by(|a, b| {
        (a.1 + complete_after[a.0]).partial_cmp(&(b.1 + complete_after[b.0])).unwrap()
    });
    out
}

/// How far behind the cycle-aligned decoder may fall before it gives up on the
/// oldest cycles rather than working through them.
///
/// A queued cycle is worth decoding only while its audio is still held, and the
/// capture buffer keeps five minutes. Two is well inside that, and a decoder
/// two minutes behind has bigger problems than the cycles it is about to drop —
/// which the heartbeat's `skipped` now counts rather than losing silently.
const MAX_BACKLOG_S: f64 = 120.0;

/// The audio one cycle-aligned pass reads: the cycle, and half a second either
/// side of it for clock skew.
fn window_len_s(m: &ModeId) -> f64 {
    m.period_s().unwrap_or(15) as f64 + 1.0
}

/// How long after a cycle starts that whole window has been captured.
///
/// Derived from the window rather than from the frame, because asking for audio
/// that has not arrived yet is precisely what this used to do: it waited for
/// the *frame* to finish, `chunk_secs + 1.0`, and then asked for a window
/// running 1.86 s past that. [`slice_around`] clamps to what is held without
/// complaining, so what came back was a window with its end cut off, and a
/// frame arriving more than a second after the grid lost its tail and did not
/// decode. Off a WebSDR running 1.2 s late that was eight frames in nine over
/// six minutes, every one of them recoverable from the audio.
///
/// Waiting the extra second and a half buys about 2.5 s of stream latency
/// tolerated with no learning at all. Past that the offset search takes over.
fn window_ready_s(m: &ModeId) -> f64 {
    window_len_s(m) - 0.5
}

/// Consecutive dead cycles before a mode goes looking for where its frames
/// actually are. Three, so an ordinary quiet spell on a real band does not
/// trigger it.
const DEAD_CYCLES: usize = 3;

/// Shortest interval between those searches, across all modes.
///
/// A search costs one wide decode and finds nothing on a band that simply has
/// no JS8 on it, which is the common case — so it is rate-limited hard. Once a
/// minute is far more often than a stream's latency changes.
const SEARCH_INTERVAL_S: f64 = 60.0;

/// How far a stream may be behind the clock and still be locked onto.
///
/// The search looks at one period plus a frame of the most recent audio, so any
/// delay is found: what a delay of more than a period costs is only that a
/// decode is stamped a whole number of cycles late, since one cycle looks
/// exactly like the next.
const SEARCH_SPAN_S: f64 = 2.0;

/// Where this mode's frames are actually arriving, as a phase within the cycle.
///
/// Live audio off a radio arrives as it is heard, and the cycle grid is the
/// truth. Audio piped from a browser or a WebSDR is *seconds* late, so frames
/// land nowhere near the grid and the narrow window around it catches nothing —
/// the mode is silent no matter how strong the signal. This finds them: it
/// decodes a period-and-a-frame of the most recent audio, where a frame must
/// be whatever the delay, and reports the phase the newest one arrived at.
///
/// Purely observational. It does not assume where in a cycle a transmission
/// begins, so it cannot be wrong about that convention, and on undelayed audio
/// it measures the phase real frames already arrive at — which is what the grid
/// was approximating anyway.
///
/// `None` when nothing decodes, and the caller then leaves the offset alone: a
/// band with no JS8 on it must not move a working one.
fn measure_offset(buf: &Arc<Mutex<AudioBuf>>, mode: ModeId, period: f64) -> Option<f64> {
    let span = period + mode.chunk_secs() + SEARCH_SPAN_S;
    // Addressed by sample index rather than by UTC, unlike every ordinary pass.
    // What this looks at has to be the newest audio the buffer actually holds:
    // asking for it in wall-clock time goes through the very anchor whose being
    // wrong is half of what the search exists to recover from, and on a capture
    // that has slipped it returns nothing at all.
    let (samples, win_start, start_unix) = {
        let b = buf.lock().unwrap();
        if !b.started() {
            return None;
        }
        let g1 = b.global_len();
        let g0 = g1.saturating_sub((span * RATE as f64) as u64).max(b.global_start());
        (b.slice_global(g0, g1), g0 as f64 / RATE as f64, b.start_unix())
    };
    if (samples.len() as f64) < mode.chunk_secs() * RATE as f64 {
        return None;
    }
    // The newest frame, so a stream whose latency just changed locks onto where
    // it is now rather than where it was.
    let newest = protocol::decode_all(&samples, BAND.0, BAND.1, &[mode])
        .into_iter()
        .map(|d| d.time_s)
        .fold(f64::NEG_INFINITY, f64::max);
    if !newest.is_finite() {
        return None;
    }
    Some((start_unix + win_start + newest).rem_euclid(period))
}

/// Record decodes in the traffic log, stamped with the UTC time each
/// transmission's audio *began* rather than the moment it was decoded.
///
/// The two differ by seconds and sometimes tens of them — a continuous-mode
/// pass reports blocks from anywhere in a 24-second window — and a station log
/// that said "decoded at" would misdate every entry in it by however long the
/// decoder took to get round to that block.
fn log_traffic(buf: &Arc<Mutex<AudioBuf>>, decodes: &[protocol::Decode]) {
    if !traffic::is_on() || decodes.is_empty() {
        return;
    }
    // `time_s` counts from global sample 0, which is what `start_unix` dates.
    let start_unix = buf.lock().unwrap().start_unix();
    for d in decodes {
        traffic::log(Some(start_unix + d.time_s), d.time_s, d);
    }
}

/// Rebase decode times from window-relative to capture-relative.
fn shift_times(decodes: Vec<protocol::Decode>, win_start: f64) -> Vec<protocol::Decode> {
    decodes
        .into_iter()
        .map(|mut d| {
            d.time_s += win_start;
            d
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, label: &str) -> AudioDevice {
        AudioDevice { id: id.to_string(), label: label.to_string() }
    }

    /// Every PCM a real Linux box advertised for input: thirty entries that
    /// are, in truth, three capture endpoints on one card plus a few routes.
    fn real_alsa_listing() -> Vec<AudioDevice> {
        [
            ("alsa:null", "Discard all samples"),
            ("alsa:lavrate", "Rate Converter Plugin Using Libav/FFmpeg Library"),
            ("alsa:samplerate", "Rate Converter Plugin Using Samplerate Library"),
            ("alsa:speexrate", "Rate Converter Plugin Using Speex Resampler"),
            ("alsa:jack", "JACK Audio Connection Kit"),
            ("alsa:oss", "Open Sound System"),
            ("alsa:pipewire", "PipeWire Sound Server"),
            ("alsa:pulse", "PulseAudio Sound Server"),
            ("alsa:speex", "Plugin using Speex DSP"),
            ("alsa:upmix", "Plugin for channel upmix (4,6,8)"),
            ("alsa:vdownmix", "Plugin for channel downmix (stereo)"),
            ("alsa:default", "Default ALSA Output"),
            ("alsa:usbstream:CARD=NVidia", "HDA NVidia"),
            ("alsa:hw:CARD=sofhdadsp,DEV=0", "sof-hda-dsp, "),
            ("alsa:hw:CARD=sofhdadsp,DEV=6", "sof-hda-dsp, "),
            ("alsa:hw:CARD=sofhdadsp,DEV=7", "sof-hda-dsp, "),
            ("alsa:plughw:CARD=sofhdadsp,DEV=0", "sof-hda-dsp, "),
            ("alsa:plughw:CARD=sofhdadsp,DEV=6", "sof-hda-dsp, "),
            ("alsa:plughw:CARD=sofhdadsp,DEV=7", "sof-hda-dsp, "),
            ("alsa:sysdefault:CARD=sofhdadsp", "sof-hda-dsp"),
            ("alsa:dsnoop:CARD=sofhdadsp,DEV=0", "sof-hda-dsp, "),
            ("alsa:dsnoop:CARD=sofhdadsp,DEV=6", "sof-hda-dsp, "),
            ("alsa:dsnoop:CARD=sofhdadsp,DEV=7", "sof-hda-dsp, "),
            ("alsa:usbstream:CARD=sofhdadsp", "sof-hda-dsp"),
            ("alsa:hw:CARD=1,DEV=0", "sof-hda-dsp, "),
            ("alsa:plughw:CARD=1,DEV=0", "sof-hda-dsp, "),
            ("alsa:hw:CARD=1,DEV=6", "sof-hda-dsp, "),
            ("alsa:plughw:CARD=1,DEV=6", "sof-hda-dsp, "),
            ("alsa:hw:CARD=1,DEV=7", "sof-hda-dsp, "),
            ("alsa:plughw:CARD=1,DEV=7", "sof-hda-dsp, "),
        ]
        .iter()
        .map(|(id, label)| dev(id, label))
        .collect()
    }

    /// Appending to a full buffer must not cost what the buffer is big.
    ///
    /// Cutting back to exactly `cap` means moving every surviving sample on
    /// every callback once full — 14 MB a time, inside the audio callback,
    /// holding the lock the interface and both decoders need. It measured
    /// 970 us per callback; the whole app stopped answering five minutes in,
    /// which is exactly when a five-minute buffer fills.
    #[test]
    fn appending_to_a_full_buffer_stays_cheap() {
        use std::time::Instant;
        let cap = RATE as usize * 300;
        let mut b = AudioBuf::new(cap);
        let chunk = vec![0.0f32; 512];
        // Up to and through the first trim. Not `while len < cap + TRIM_BLOCK`:
        // the append that crosses the line cuts back to `cap`, so that
        // condition is true again immediately and the loop never ends.
        while b.global_start == 0 {
            b.append(&chunk);
        }
        // Long enough to cross several trim blocks, so the amortised cost is
        // what is being measured rather than the gap between two of them.
        let n = 20_000;
        let t = Instant::now();
        for _ in 0..n {
            b.append(&chunk);
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        assert!(us < 50.0, "{us:.1} us per append, moving the buffer too often");
        // And the bound still holds: it rolls, it does not grow.
        assert!(
            b.samples.len() <= cap + TRIM_BLOCK,
            "buffer grew to {} past its {} cap",
            b.samples.len(),
            cap
        );
        assert!(b.global_start > 0, "nothing was ever trimmed");
    }

    /// Lag measures what it claims to: the gap between how much audio has
    /// arrived and how much wall-clock time has passed.
    ///
    /// This is the number the overnight report turns on. A capture that has
    /// dropped samples — an overrun, a USB glitch, a suspend — keeps a perfect
    /// waterfall, because that is drawn by sample index, while every decoder
    /// asks for its window in UTC and is quietly handed the wrong audio. The
    /// two cases are constructed here directly.
    #[test]
    fn lag_is_the_gap_between_the_audio_and_the_clock() {
        let mut b = AudioBuf::new(RATE as usize * 300);
        assert_eq!(b.lag_s(), 0.0, "nothing captured yet is not lag");

        // Ten seconds of audio that took ten seconds to arrive: no lag.
        b.append(&vec![0.0f32; RATE as usize * 10]);
        b.start_unix = unix_now() - 10.0;
        assert!(b.lag_s().abs() < 0.05, "healthy capture reported {:.3}s of lag", b.lag_s());

        // Thirty seconds of wall clock, but only ten seconds of audio ever
        // appended: twenty seconds have been dropped somewhere, and nothing in
        // `append` could have known. This is what a night of glitches looks
        // like, and it is enough to put every decode window off its signal.
        b.start_unix = unix_now() - 30.0;
        assert!(
            (b.lag_s() - 20.0).abs() < 0.05,
            "20 s of dropped audio reported as {:.3}s",
            b.lag_s()
        );

        // The waterfall is untouched by any of it: it walks the buffer by
        // index, so it still finds every window it has ever been given.
        assert!(b.window_at(0).is_some());
        assert!(b.window_at(RATE as u64 * 9).is_some());
    }

    /// The four JS8 submodes as the scheduler sees them: cycle period, and how
    /// long past a cycle's start its audio is all in.
    fn js8_grids() -> (Vec<f64>, Vec<f64>) {
        let modes = [
            ragchew::js8::Mode::Turbo,
            ragchew::js8::Mode::Fast,
            ragchew::js8::Mode::Normal,
            ragchew::js8::Mode::Slow,
        ];
        let ids: Vec<ModeId> = modes.iter().map(|m| ModeId::Js8(*m)).collect();
        let period = ids.iter().map(|m| m.period_s().unwrap() as f64).collect();
        let after = ids.iter().map(window_ready_s).collect();
        (period, after)
    }

    /// Cycles of one mode whose audio is all in by `span`, counting from a
    /// cycle starting at zero. Not simply `span / period`: a cycle is not ripe
    /// until `after` seconds past its start, so the last one or two to begin
    /// are still being captured when the clock runs out.
    fn ripe_by(span: f64, period: f64, after: f64) -> usize {
        ((span - after) / period).floor() as usize + 1
    }

    /// A pass never asks for audio that has not been captured yet.
    ///
    /// It used to. The window a pass reads is the cycle plus half a second
    /// either side, but the pass ran as soon as the *frame* would have
    /// finished — 1.86 s before the end of the window it then asked for, at
    /// JS8 Normal. `slice_around` clamps to what is held without complaining,
    /// so the window came back with its tail cut off, and any frame arriving
    /// more than a second after the cycle grid lost symbols and failed to
    /// decode. Off a WebSDR running 1.2 s late that cost eight frames in nine
    /// over six minutes, every one of them sitting in the recording.
    ///
    /// The tolerance this buys is worth stating: how late a stream may run
    /// before frames stop fitting the window, before the offset search has
    /// learned anything at all.
    #[test]
    fn a_pass_waits_for_the_window_it_reads() {
        for m in [
            ragchew::js8::Mode::Turbo,
            ragchew::js8::Mode::Fast,
            ragchew::js8::Mode::Normal,
            ragchew::js8::Mode::Slow,
        ] {
            let id = ModeId::Js8(m);
            // The window starts half a second before the cycle, so this much
            // of it exists by the time the pass runs.
            let captured = window_ready_s(&id) + 0.5;
            assert!(
                captured >= window_len_s(&id),
                "{}: reads {:.2}s but only {captured:.2}s has arrived",
                id.name(),
                window_len_s(&id)
            );

            let slack = window_len_s(&id) - 0.5 - id.chunk_secs();
            assert!(
                slack > 2.5,
                "{}: a stream more than {slack:.2}s late loses its frames",
                id.name()
            );
        }
    }

    /// No cycle is lost because the decoder was busy when it ripened.
    ///
    /// This is the bug the recordings caught. The loop used to take only the
    /// cycle due soonest and step `next` past the rest, so every cycle that
    /// completed while another submode's pass was running was thrown away
    /// unlooked-at. With all four submodes enabled that measured one in five:
    /// eight passes in the thirty seconds the grids demand eleven. On a band
    /// carrying one JS8 frame in twelve minutes it is the difference between
    /// catching it and never knowing it was there.
    #[test]
    fn no_cycle_is_lost_because_the_decoder_was_busy() {
        let (period, after) = js8_grids();
        let span = 300.0;

        // A decoder that wakes once in five minutes must still be handed every
        // cycle of those five minutes.
        let mut next = vec![0.0; period.len()];
        let slow = ripe_cycles(&mut next, &period, &after, span);
        let want: usize =
            period.iter().zip(&after).map(|(p, a)| ripe_by(span, *p, *a)).sum();
        assert_eq!(slow.len(), want, "a busy decoder was handed {} of {want} cycles", slow.len());

        // And one that wakes constantly must be handed exactly the same ones.
        let mut next = vec![0.0; period.len()];
        let mut brisk = Vec::new();
        let mut t = 0.0;
        while t <= span {
            brisk.extend(ripe_cycles(&mut next, &period, &after, t));
            t += 0.25;
        }
        assert_eq!(brisk, slow, "how often the decoder looks changed which cycles it got");

        // Every cycle exactly once, on its own grid.
        for (i, p) in period.iter().enumerate() {
            let mine: Vec<f64> = slow.iter().filter(|c| c.0 == i).map(|c| c.1).collect();
            let mut want = 0.0;
            for (k, got) in mine.iter().enumerate() {
                assert_eq!(*got, want, "mode {i} cycle {k} is not on its grid");
                want += p;
            }
        }
    }

    /// All four submodes fit in the time available, at the pass cost a real
    /// capture measured.
    ///
    /// Queueing the cycles is only worth anything if the decoder can then work
    /// through them; otherwise it moves the loss from cycles skipped to cycles
    /// gone stale. The live heartbeats put a pass at 0.79–1.08 s, so this walks
    /// a virtual clock through ten minutes at the worst of that and checks
    /// almost every cycle is decoded and the backlog never approaches
    /// [`MAX_BACKLOG_S`].
    #[test]
    fn the_four_submodes_fit_in_the_time_available() {
        let (period, after) = js8_grids();
        let pass_cost = 1.08;
        let span = 600.0;

        let mut next = vec![0.0; period.len()];
        let mut queue: Vec<(usize, f64)> = Vec::new();
        let (mut now, mut decoded, mut worst_wait) = (0.0f64, 0usize, 0.0f64);
        while now < span {
            queue.extend(ripe_cycles(&mut next, &period, &after, now));
            let Some(&(i, cycle)) = queue.first() else {
                now += 0.25;
                continue;
            };
            worst_wait = worst_wait.max(now - (cycle + after[i]));
            queue.remove(0);
            decoded += 1;
            now += pass_cost;
        }

        let want: usize =
            period.iter().zip(&after).map(|(p, a)| ripe_by(span, *p, *a)).sum();
        assert!(decoded * 100 >= want * 99, "decoded {decoded} of {want} cycles");
        assert!(
            worst_wait < MAX_BACKLOG_S / 4.0,
            "a cycle waited {worst_wait:.1}s to be decoded; the cap is {MAX_BACKLOG_S}"
        );
    }

    /// Cycles come back in the order their audio completed, so a backlog is
    /// worked through oldest first and the interface is told things in the
    /// order they were said.
    #[test]
    fn a_backlog_is_handed_over_in_the_order_it_arrived() {
        let (period, after) = js8_grids();
        let mut next = vec![0.0; period.len()];
        let got = ripe_cycles(&mut next, &period, &after, 120.0);
        assert!(got.len() > 20, "only {} cycles; the test proved little", got.len());

        let mut last = f64::NEG_INFINITY;
        for &(i, cycle) in &got {
            let done = cycle + after[i];
            assert!(done >= last, "cycle completing at {done} came after {last}");
            last = done;
        }
    }

    /// Which devices put the capture into a sound server's graph, and so have
    /// a routing choice behind them that the device name does not settle.
    #[test]
    fn the_sound_server_pcms_are_the_routable_ones() {
        for id in ["alsa:pipewire", "alsa:default", "pipewire", "default", "pulse"] {
            assert!(is_pipewire(id), "{id} should offer routing");
        }
        for id in [
            "alsa:plughw:CARD=USB,DEV=0",
            "alsa:hw:CARD=PCH,DEV=2",
            "alsa:sysdefault:CARD=Audio",
            "alsa:front:CARD=PCH,DEV=0",
        ] {
            assert!(!is_pipewire(id), "{id} is a sound card, not a graph");
        }
        // `default:CARD=x` is a card's own default PCM, not the server's.
        assert!(!is_pipewire("alsa:default:CARD=PCH"), "a card's default is not the server");
    }

    /// A decode thread nobody asked for raises no alarm.
    ///
    /// Switching every mode of one kind off is a thing an operator does — the
    /// recordings that found the scheduler bug were made with the continuous
    /// modes off on purpose — and it used to put a WARN in the log every thirty
    /// seconds for the rest of the session, which is exactly what `grep WARN`
    /// then finds instead of the fault being looked for.
    #[test]
    fn a_thread_that_was_never_wanted_is_not_reported_as_a_fault() {
        let h = Health::default();
        assert!(!h.continuous.wanted.load(Ordering::Relaxed));
        assert!(!h.continuous.running.load(Ordering::Relaxed));
        // Nothing to say about a thread that was never asked for.
        check_stall(&h.continuous, "continuous (Olivia/PSK)", CONT_STALL_S);

        // But one that was asked for and is not running is still a fault, and
        // the counters behind that judgement are the ones the heartbeat reads.
        h.continuous.wanted.store(true, Ordering::Relaxed);
        assert!(
            h.continuous.wanted.load(Ordering::Relaxed)
                && !h.continuous.running.load(Ordering::Relaxed),
            "the state check_stall warns on"
        );
    }

    /// A capture that stalls moves its own anchor, instead of keeping the lost
    /// time for ever.
    ///
    /// The reported case, from a log: audio piped in from a browser through
    /// PipeWire stopped for ninety seconds and then resumed. Nothing in the
    /// stream says so — no error, no gap in the samples, and the waterfall kept
    /// scrolling — but every decode window is cut by translating UTC through
    /// this anchor, so from then on each one was asked for ninety seconds past
    /// the newest sample held and came back empty. JS8 went to `short=+55` of
    /// 55 passes and stayed there.
    #[test]
    fn a_stalled_capture_moves_its_anchor_rather_than_losing_the_clock() {
        let mut b = AudioBuf::new(RATE as usize * 300);

        // Ten seconds of audio, which arrived starting a hundred seconds ago
        // and stopped ninety seconds ago. That is the broken state: the buffer
        // is ninety seconds adrift and nothing in it knows.
        b.append(&vec![0.0f32; RATE as usize * 10]);
        b.start_unix = unix_now() - 100.0;
        b.last_append = unix_now() - 90.0;
        assert!((b.lag_s() - 90.0).abs() < 0.1, "set up wrong: lag is {:.1}s", b.lag_s());

        // The stream resumes with one ordinary block.
        b.append(&vec![0.0f32; 512]);
        assert!(
            b.lag_s().abs() < 0.1,
            "still {:.1}s adrift after the anchor should have caught up",
            b.lag_s()
        );

        // Reported once, with a running total — the heartbeat says "this
        // happened", not "this is happening", every thirty seconds for ever.
        let (total, fresh) = b.take_slip();
        assert!((total - 90.0).abs() < 0.2, "lost {total:.1}s, expected 90");
        assert!((fresh - 90.0).abs() < 0.2, "reported {fresh:.1}s as new");
        assert_eq!(b.take_slip().1, 0.0, "reported the same stall twice");
    }

    /// And a window asked for in UTC then lands on audio again, which is the
    /// only thing any of it was for.
    #[test]
    fn a_decode_window_lands_on_audio_after_a_stall() {
        let buf = Arc::new(Mutex::new(AudioBuf::new(RATE as usize * 300)));
        {
            let mut b = buf.lock().unwrap();
            b.append(&vec![0.1f32; RATE as usize * 10]);
            b.start_unix = unix_now() - 100.0;
            b.last_append = unix_now() - 90.0;
            // Before the anchor moves, the window is cut ninety seconds past
            // the end of the buffer and there is nothing there at all.
            drop(b);
            let (early, _) = slice_around(&buf, unix_now() - 5.0, 4.0);
            assert!(early.is_empty(), "expected the broken case to return nothing");
        }
        buf.lock().unwrap().append(&vec![0.1f32; 512]);

        let (samples, _) = slice_around(&buf, unix_now() - 5.0, 4.0);
        let got = samples.len() as f64 / RATE as f64;
        assert!((got - 4.0).abs() < 0.1, "asked for 4s of audio and got {got:.2}s");
    }

    /// A merely *late* block is not a stall. A capture that hiccups and then
    /// delivers the audio it owed accounts for its own delay, and moving the
    /// anchor for that would walk a healthy capture off the clock a hiccup at a
    /// time.
    #[test]
    fn a_late_block_that_brings_its_audio_with_it_moves_nothing() {
        let mut b = AudioBuf::new(RATE as usize * 300);
        b.append(&vec![0.0f32; RATE as usize]);
        b.start_unix = unix_now() - 1.0;

        // Three seconds late, carrying three seconds of audio.
        b.last_append = unix_now() - 3.0;
        b.append(&vec![0.0f32; RATE as usize * 3]);

        assert_eq!(b.take_slip().0, 0.0, "a late block was counted as lost audio");
    }

    /// Recording taps the buffer, so what reaches the file is what reached the
    /// decoders — the property that makes a recording worth having.
    #[test]
    fn the_recording_tap_gets_what_the_decoders_get() {
        use crate::record::Recorder;

        let dir = std::env::temp_dir().join(format!("ragchew-tap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let health = Arc::new(Health::default());
        let rec = Recorder::start(&dir, RATE, health).unwrap();

        let mut b = AudioBuf::new(RATE as usize * 300);
        b.set_tap(Some(rec.tap()));
        let sent: Vec<f32> = (0..RATE as usize).map(|i| (i as f32 * 0.01).sin() * 0.4).collect();
        b.append(&sent);

        // What the decoders would slice out of the buffer...
        let from_buf = b.slice_global(0, sent.len() as u64);
        let path = rec.path().to_path_buf();
        drop(rec);
        // ...and what the file holds, are the same audio.
        let (from_file, rate) = ragchew::wav::read(path.to_str().unwrap()).unwrap();
        assert_eq!(rate, RATE);
        assert_eq!(from_file.len(), from_buf.len());
        let worst =
            from_buf.iter().zip(&from_file).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 2.0 / 32_768.0, "the file differs from the buffer by {worst}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stream that is seconds behind the clock is found and reported as the
    /// phase its frames actually arrive at.
    ///
    /// This is the whole of the auto-lock's cleverness. The rest is applying
    /// the number, and the number is measured rather than assumed: the test
    /// places a frame at an arbitrary delay and expects that delay back.
    #[test]
    fn a_delayed_stream_is_located() {
        use ragchew::js8::message::{self, IType};
        use ragchew::js8::modem;

        let mode = ModeId::Js8(ragchew::js8::Mode::Normal);
        let period = 15.0;
        // Deliberately not a whole second and not near a cycle boundary, so
        // nothing can pass by rounding to the grid it is supposed to ignore.
        let delay = 6.37;
        let span = period + mode.chunk_secs() + SEARCH_SPAN_S;
        let total = span + 4.0;

        let (a87, _) = message::freetext("DE W1AW EM73", IType::None);
        let frame = modem::encode_audio(&a87, 1500.0);

        // A quiet band rather than a dead one: the Costas sync score is a
        // ratio against the other tones, and digital silence gives it nothing
        // to divide by.
        let mut seed = 0x5EEDu64;
        let noise: Vec<f32> = (0..(total * RATE as f64) as usize)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                ((seed >> 40) as f32 / (1u32 << 24) as f32 - 0.5) * 0.02
            })
            .collect();

        let buf = Arc::new(Mutex::new(AudioBuf::new(RATE as usize * 120)));
        {
            let mut b = buf.lock().unwrap();
            b.append(&noise);
            // Anchor the buffer so its newest sample is about now, which is
            // where `measure_offset` looks.
            let now = unix_now();
            b.start_unix = now - total;
            // The latest whole frame that fits inside the window the search
            // will ask for, at the phase a stream this late would deliver.
            let target = now - mode.chunk_secs() - 1.0;
            let at_unix = ((target - delay) / period).floor() * period + delay;
            let at = ((at_unix - b.start_unix) * RATE as f64) as usize;
            for (i, s) in frame.iter().enumerate() {
                if at + i < b.samples.len() {
                    b.samples[at + i] += s * 0.5;
                }
            }
        }

        let got = measure_offset(&buf, mode, period).expect("the frame is in there to be found");
        assert!(
            (got - delay).abs() < 0.25,
            "frames arrive at {delay} s into the cycle; measured {got}"
        );
    }

    /// A band with no JS8 on it reports nothing, so a mode that is merely quiet
    /// keeps the offset it has. A search that guessed here would walk a working
    /// radio off its own frames.
    #[test]
    fn silence_moves_nothing() {
        let mode = ModeId::Js8(ragchew::js8::Mode::Normal);
        let period = 15.0;
        let total = period + mode.chunk_secs() + SEARCH_SPAN_S + 4.0;
        let buf = Arc::new(Mutex::new(AudioBuf::new(RATE as usize * 120)));
        {
            let mut b = buf.lock().unwrap();
            b.append(&vec![0.0f32; (total * RATE as f64) as usize]);
            b.start_unix = unix_now() - total;
        }
        assert!(measure_offset(&buf, mode, period).is_none());
    }

    /// Thirty advertised PCMs come down to the handful a person would
    /// recognise — the same set another recorder on this machine offers.
    #[test]
    fn winnowing_matches_what_a_person_would_expect() {
        let kept = winnow(real_alsa_listing());
        let ids: Vec<&str> = kept.iter().map(|d| d.id.as_str()).collect();

        assert_eq!(
            ids,
            vec![
                "alsa:jack",
                "alsa:pipewire",
                "alsa:pulse",
                "alsa:default",
                "alsa:plughw:CARD=sofhdadsp,DEV=0",
                "alsa:plughw:CARD=sofhdadsp,DEV=6",
                "alsa:plughw:CARD=sofhdadsp,DEV=7",
            ]
        );
    }

    /// The individual rules, so a failure says which one broke.
    #[test]
    fn winnowing_rules() {
        let kept = winnow(real_alsa_listing());
        let has = |s: &str| kept.iter().any(|d| d.id.contains(s));

        assert!(!has("lavrate") && !has("upmix") && !has("null"), "kept a converter plugin");
        assert!(!has("CARD=1"), "kept a card addressed by number as well as by name");
        assert!(!has("hw:CARD=sofhdadsp,DEV=0") || has("plughw:CARD=sofhdadsp,DEV=0"));
        assert!(!has("dsnoop") && !has("sysdefault"), "kept a lesser view of a device");
        assert!(!has("usbstream"), "kept a usbstream alias");
        // one entry per real capture endpoint
        assert_eq!(kept.iter().filter(|d| d.id.contains("sofhdadsp")).count(), 3);
    }

    /// With nothing named, cards addressed by number are all there is.
    #[test]
    fn numeric_cards_survive_when_they_are_the_only_ones() {
        let kept = winnow(vec![
            dev("alsa:plughw:CARD=0,DEV=0", "Card zero"),
            dev("alsa:hw:CARD=0,DEV=0", "Card zero"),
        ]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "alsa:plughw:CARD=0,DEV=0");
    }

    /// A picker that offers the same string twice is a picker you cannot use.
    /// Real hardware here presents twenty inputs all called "sof-hda-dsp".
    #[test]
    fn duplicate_labels_are_made_distinct() {
        let mut devs = vec![
            dev("alsa:hw:CARD=sofhdadsp,DEV=0", "sof-hda-dsp, "),
            dev("alsa:plughw:CARD=sofhdadsp,DEV=0", "sof-hda-dsp, "),
            dev("alsa:hw:CARD=sofhdadsp,DEV=6", "sof-hda-dsp, "),
            dev("alsa:pipewire", "PipeWire Sound Server"),
        ];
        disambiguate(&mut devs);

        let mut labels: Vec<&String> = devs.iter().map(|d| &d.label).collect();
        let n = labels.len();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), n, "labels still collide: {devs:#?}");

        // the one that was already unique is left alone
        assert_eq!(devs[3].label, "PipeWire Sound Server");
        // and hw/plughw stay apart, which trimming to the last colon would lose
        assert!(devs[0].label.contains("hw:CARD"));
        assert!(devs[1].label.contains("plughw:CARD"));
    }
}
