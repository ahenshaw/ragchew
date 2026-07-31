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

use crate::diag::{self, Health, RunningGuard, ThreadHealth};
use crate::record::Tap;
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
    /// Where a recording in progress is fed from, if any. It hangs off the
    /// buffer rather than off the stream because the callback already holds
    /// this lock — a separate one would be a second thing for the audio thread
    /// to wait on.
    tap: Option<Tap>,
}

impl AudioBuf {
    fn new(cap: usize) -> AudioBuf {
        AudioBuf {
            samples: Vec::new(),
            global_start: 0,
            start_unix: 0.0,
            cap,
            started: false,
            tap: None,
        }
    }
    /// Start or stop feeding a recording from this buffer.
    pub fn set_tap(&mut self, tap: Option<Tap>) {
        self.tap = tap;
    }
    fn append(&mut self, s: &[f32]) {
        if !self.started {
            self.start_unix = unix_now();
            self.started = true;
        }
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
        let ready = |m: &ModeId| m.chunk_secs() + 1.0; // frame done, plus slack
        let mut next: Vec<f64> =
            modes.iter().map(|m| (unix_now() / period(m)).floor() * period(m)).collect();
        // How late each mode's frames arrive, learned from the audio. Zero is
        // the cycle grid, which is right for a radio and is where every mode
        // starts; see `measure_offset`.
        let mut offset = vec![0.0f64; modes.len()];
        let mut dead = vec![0usize; modes.len()];
        let mut last_search = unix_now();

        loop {
            let now = unix_now();
            // skip any cycles whose decode time already passed
            for (i, m) in modes.iter().enumerate() {
                while next[i] + ready(m) <= now {
                    next[i] += period(m);
                }
            }
            // decode whichever mode comes due first
            let best = (0..modes.len())
                .min_by(|&a, &b| {
                    (next[a] + ready(&modes[a]))
                        .partial_cmp(&(next[b] + ready(&modes[b])))
                        .unwrap()
                })
                .unwrap();
            let mode = modes[best];
            let cycle_start = next[best];
            next[best] += period(&mode);

            let wait = cycle_start + offset[best] + ready(&mode) - unix_now();
            if wait > 0.0 {
                thread::sleep(Duration::from_secs_f64(wait));
            }

            // half a second either side of the cycle, for clock skew — shifted
            // by however late this mode's frames are actually arriving, which
            // is zero off a radio and seconds off a stream.
            stats.pass_begun();
            let (samples, win_start) =
                slice_around(&buf, cycle_start + offset[best] - 0.5, period(&mode) + 1.0);
            if (samples.len() as f64) < mode.chunk_secs() * RATE as f64 {
                // The buffer did not hold the window this cycle asked for.
                // Counted rather than logged: on a lagging capture it happens
                // every cycle, and the heartbeat's running total says so once.
                Health::bump(&stats.short, 1);
                continue;
            }
            let pass = std::time::Instant::now();
            let decodes = shift_times(
                protocol::decode_all(&samples, BAND.0, BAND.1, &[mode]),
                win_start,
            );
            stats.pass_done(pass.elapsed().as_secs_f64() * 1e3);

            // A mode that has gone quiet may have nothing to hear, or may be
            // looking in the wrong place. The first is far commoner, so this
            // waits for several dead cycles and then looks at most once a
            // minute across every mode.
            dead[best] = if decodes.is_empty() { dead[best] + 1 } else { 0 };
            if dead[best] >= DEAD_CYCLES && unix_now() - last_search >= SEARCH_INTERVAL_S {
                last_search = unix_now();
                if let Some(found) = measure_offset(&buf, mode, period(&mode)) {
                    // Only a real move. Frames arrive a few tens of
                    // milliseconds apart from pass to pass, and chasing that
                    // would rewrite the offset for no gain.
                    if (found - offset[best]).abs() > 0.5 {
                        offset[best] = found;
                        dead[best] = 0;
                    }
                }
            }

            if !decodes.is_empty() {
                Health::bump(&stats.decodes, decodes.len() as u64);
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
            // Cycles that pass while a decode runs are skipped at the top of
            // the loop, so falling behind costs coverage rather than building a
            // backlog — but with four modes due every six to thirty seconds and
            // a pass costing seconds, the next is always already due and the
            // thread never sleeps at all. Yielding for as long as the pass took
            // holds it to half the wall clock.
            thread::sleep(pass.elapsed());
        }
    });
    if let Err(e) = spawned {
        diag_warn!("decode", "could not spawn the cycle-aligned thread: {e}");
    }
}

/// Rolling-window decoding for the modes that run continuously.
///
/// Neither Olivia nor PSK31 tells you where a transmission starts, so there is
/// nothing to align to: the last [`CONTINUOUS_WINDOW_S`] seconds are simply
/// re-scanned every [`CONTINUOUS_STEP_S`]. Windows overlap heavily, both so a
/// block straddling a boundary is not lost and because either decoder needs
/// several consecutive blocks in view before it will trust a signal — so the
/// same block gets decoded repeatedly and duplicates are filtered on the way
/// out.
fn spawn_continuous(
    buf: Arc<Mutex<AudioBuf>>,
    modes: Vec<ModeId>,
    tx: Sender<Vec<protocol::Decode>>,
    health: Arc<Health>,
) {
    let spawned = thread::Builder::new().name("ragchew-cont".into()).spawn(move || {
        let guard = RunningGuard::new(health, |h| &h.continuous);
        let stats = guard.stats();
        let mut seen: Vec<(ModeId, f64, f64)> = Vec::new(); // (mode, hz, time_s)
        // Stretches past CONTINUOUS_STEP_S on a machine — or in a build — that
        // cannot scan the band in that long. See the back-off below.
        let mut step = CONTINUOUS_STEP_S;
        loop {
            thread::sleep(Duration::from_secs_f64(step));
            let pass = std::time::Instant::now();
            let now = match buf.lock().unwrap().started() {
                true => unix_now(),
                false => continue,
            };
            stats.pass_begun();
            let (samples, win_start) =
                slice_around(&buf, now - CONTINUOUS_WINDOW_S, CONTINUOUS_WINDOW_S);
            if samples.len() < RATE as usize {
                Health::bump(&stats.short, 1);
                continue;
            }

            let fresh: Vec<protocol::Decode> =
                shift_times(protocol::decode_all(&samples, BAND.0, BAND.1, &modes), win_start)
                    .into_iter()
                    .filter(|d| {
                        // The same block decoded in the previous window comes
                        // back at the same time and frequency, give or take the
                        // sync search's resolution.
                        !seen.iter().any(|&(m, hz, t)| {
                            m == d.mode
                                && (hz - d.hz).abs() < d.mode.bandwidth_hz() / 2.0
                                && (t - d.time_s).abs() < d.mode.chunk_secs() / 2.0
                        })
                    })
                    .collect();

            seen.extend(fresh.iter().map(|d| (d.mode, d.hz, d.time_s)));
            let cutoff = win_start - CONTINUOUS_WINDOW_S;
            seen.retain(|&(_, _, t)| t >= cutoff);
            stats.pass_done(pass.elapsed().as_secs_f64() * 1e3);

            if !fresh.is_empty() {
                Health::bump(&stats.decodes, fresh.len() as u64);
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
            // release build and forty in a debug one, against a 4 s step —
            // PSK31 adds a couple of milliseconds a block to that and is not
            // what makes this expensive. Left
            // alone the thread runs flat out for ever, and the interface —
            // sharing a CPU and a buffer lock with it — stops answering. The
            // step therefore stretches to whatever the pass actually cost:
            // decoding takes at most half the wall clock, and at 24 s windows
            // there is still overlap to spare.
            step = CONTINUOUS_STEP_S.max(pass.elapsed().as_secs_f64());
        }
    });
    if let Err(e) = spawned {
        diag_warn!("decode", "could not spawn the continuous thread: {e}");
    }
}

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
            let (lag, held_s, started) = {
                let b = buf.lock().unwrap();
                (b.lag_s(), b.samples.len() as f64 / RATE as f64, b.started())
            };
            if !started {
                continue;
            }
            let now = Snapshot::of(&health);
            let d = now.since(&last);
            last = now;

            diag_info!(
                "health",
                "lag={lag:+.2}s held={held_s:.0}s cols=+{} frames=+{} | \
                 js8 {} | cont {} | stream_err={} rec_drop={}",
                d.ui_columns,
                d.ui_frames,
                thread_summary(&health.js8, d.js8_passes, d.js8_decodes, d.js8_short),
                thread_summary(
                    &health.continuous,
                    d.cont_passes,
                    d.cont_decodes,
                    d.cont_short
                ),
                diag::get(&health.stream_errors),
                diag::get(&health.record_dropped)
            );

            // Then the same facts as judgements, so the first thing to do with
            // a night's log is `grep -E 'WARN|ERROR'` and the answer is there.
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

fn thread_summary(h: &ThreadHealth, passes: u64, decodes: u64, short: u64) -> String {
    let ago = h.since_pass().map_or("never".to_string(), |s| format!("{s:.0}s ago"));
    format!(
        "passes=+{passes} decodes=+{decodes} short=+{short} last={ago} cost={}ms run={}",
        h.last_ms.load(Ordering::Relaxed),
        u8::from(h.running.load(Ordering::Relaxed))
    )
}

/// Say so, loudly, when a decode thread has stopped or gone quiet for too long.
fn check_stall(h: &ThreadHealth, what: &str, limit: f64) {
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
    js8_passes: u64,
    js8_decodes: u64,
    js8_short: u64,
    cont_passes: u64,
    cont_decodes: u64,
    cont_short: u64,
}

impl Snapshot {
    fn of(h: &Health) -> Snapshot {
        Snapshot {
            ui_columns: diag::get(&h.ui_columns),
            ui_frames: diag::get(&h.ui_frames),
            js8_passes: diag::get(&h.js8.passes),
            js8_decodes: diag::get(&h.js8.decodes),
            js8_short: diag::get(&h.js8.short),
            cont_passes: diag::get(&h.continuous.passes),
            cont_decodes: diag::get(&h.continuous.decodes),
            cont_short: diag::get(&h.continuous.short),
        }
    }
    fn since(&self, then: &Snapshot) -> Snapshot {
        Snapshot {
            ui_columns: self.ui_columns - then.ui_columns,
            ui_frames: self.ui_frames - then.ui_frames,
            js8_passes: self.js8_passes - then.js8_passes,
            js8_decodes: self.js8_decodes - then.js8_decodes,
            js8_short: self.js8_short - then.js8_short,
            cont_passes: self.cont_passes - then.cont_passes,
            cont_decodes: self.cont_decodes - then.cont_decodes,
            cont_short: self.cont_short - then.cont_short,
        }
    }
}

/// Audio window a continuous-mode pass looks at. It must hold several FEC
/// blocks of the slowest Olivia mode for that decoder to sync at all, which is
/// also comfortably several of PSK31's 8.2-second analysis blocks.
const CONTINUOUS_WINDOW_S: f64 = 24.0;

/// How often a continuous-mode pass runs. Text appears this long after it is
/// sent, so shorter is livelier — at the cost of re-scanning the band more
/// often.
const CONTINUOUS_STEP_S: f64 = 4.0;

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
    let start_unix = {
        let b = buf.lock().unwrap();
        if !b.started() {
            return None;
        }
        b.start_unix()
    };
    let (samples, win_start) = slice_around(buf, unix_now() - span, span);
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
