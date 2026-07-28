//! Live audio capture (cpal) → 12 kHz mono, with a rolling buffer and the
//! decode threads that feed off it. Desktop-only.
//!
//! Untested in CI (no audio device / display here); verified to compile.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use ragchew::protocol::{self, ModeId};
use ragchew::spectrogram::WINDOW;

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
}

impl AudioBuf {
    fn new(cap: usize) -> AudioBuf {
        AudioBuf { samples: Vec::new(), global_start: 0, start_unix: 0.0, cap, started: false }
    }
    fn append(&mut self, s: &[f32]) {
        if !self.started {
            self.start_unix = unix_now();
            self.started = true;
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
    pub fn start(id: Option<&str>) -> Result<LiveAudio, String> {
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

        let buf = Arc::new(Mutex::new(AudioBuf::new(RATE as usize * 300))); // 5 min
        let err_fn = |e| eprintln!("audio stream error: {e}");

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
/// The two protocol families need opposite scheduling, so they get a thread
/// each: JS8 is *cycle-aligned* — its frames start on UTC multiples of the
/// submode's period, so a decode is triggered the moment each cycle's audio is
/// complete — while Olivia is *continuous*, with no schedule at all, so it is
/// re-scanned on a rolling window. Dropping the returned receiver retires both
/// threads at their next send, which is how a change of modes takes effect.
pub fn spawn_decoder(
    buf: Arc<Mutex<AudioBuf>>,
    modes: Vec<ModeId>,
) -> Receiver<Vec<protocol::Decode>> {
    let (tx, rx) = channel();
    let js8: Vec<ModeId> = modes.iter().copied().filter(|m| matches!(m, ModeId::Js8(_))).collect();
    let olivia: Vec<ModeId> =
        modes.iter().copied().filter(|m| matches!(m, ModeId::Olivia(_))).collect();
    if !js8.is_empty() {
        spawn_js8(buf.clone(), js8, tx.clone());
    }
    if !olivia.is_empty() {
        spawn_olivia(buf, olivia, tx);
    }
    rx
}

/// Cycle-aligned JS8 decoding: each submode has its own UTC grid (Slow 30 s,
/// Normal 15 s, Fast 10 s, Turbo 6 s) and each cycle is decoded shortly after
/// its frame would have finished. Decodes carry capture-relative `time_s`.
fn spawn_js8(buf: Arc<Mutex<AudioBuf>>, modes: Vec<ModeId>, tx: Sender<Vec<protocol::Decode>>) {
    thread::spawn(move || {
        // next cycle-start (UTC seconds) to decode, per mode
        let period = |m: &ModeId| m.period_s().unwrap_or(15) as f64;
        let ready = |m: &ModeId| m.chunk_secs() + 1.0; // frame done, plus slack
        let mut next: Vec<f64> =
            modes.iter().map(|m| (unix_now() / period(m)).floor() * period(m)).collect();

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

            let wait = cycle_start + ready(&mode) - unix_now();
            if wait > 0.0 {
                thread::sleep(Duration::from_secs_f64(wait));
            }

            // half a second either side of the cycle, for clock skew
            let (samples, win_start) =
                slice_around(&buf, cycle_start - 0.5, period(&mode) + 1.0);
            if (samples.len() as f64) < mode.chunk_secs() * RATE as f64 {
                continue;
            }
            let pass = std::time::Instant::now();
            let decodes = shift_times(
                protocol::decode_all(&samples, BAND.0, BAND.1, &[mode]),
                win_start,
            );
            if !decodes.is_empty() && tx.send(decodes).is_err() {
                break; // UI gone
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
}

/// Rolling-window Olivia decoding.
///
/// Olivia never tells you where a transmission starts, so there is nothing to
/// align to: the last [`OLIVIA_WINDOW_S`] seconds are simply re-scanned every
/// [`OLIVIA_STEP_S`]. Windows overlap heavily, both so a block straddling a
/// boundary is not lost and because the decoder needs several consecutive
/// blocks in view before it will trust a signal — so the same block gets
/// decoded repeatedly and duplicates are filtered on the way out.
fn spawn_olivia(buf: Arc<Mutex<AudioBuf>>, modes: Vec<ModeId>, tx: Sender<Vec<protocol::Decode>>) {
    thread::spawn(move || {
        let mut seen: Vec<(ModeId, f64, f64)> = Vec::new(); // (mode, hz, time_s)
        // Stretches past OLIVIA_STEP_S on a machine — or in a build — that
        // cannot scan the band in that long. See the back-off below.
        let mut step = OLIVIA_STEP_S;
        loop {
            thread::sleep(Duration::from_secs_f64(step));
            let pass = std::time::Instant::now();
            let now = match buf.lock().unwrap().started() {
                true => unix_now(),
                false => continue,
            };
            let (samples, win_start) =
                slice_around(&buf, now - OLIVIA_WINDOW_S, OLIVIA_WINDOW_S);
            if samples.len() < RATE as usize {
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
            let cutoff = win_start - OLIVIA_WINDOW_S;
            seen.retain(|&(_, _, t)| t >= cutoff);

            if !fresh.is_empty() && tx.send(fresh).is_err() {
                break; // UI gone
            }

            // Scanning six Olivia modes across the whole band costs 3.5 s in a
            // release build and forty in a debug one, against a 4 s step. Left
            // alone the thread runs flat out for ever, and the interface —
            // sharing a CPU and a buffer lock with it — stops answering. The
            // step therefore stretches to whatever the pass actually cost:
            // decoding takes at most half the wall clock, and at 24 s windows
            // there is still overlap to spare.
            step = OLIVIA_STEP_S.max(pass.elapsed().as_secs_f64());
        }
    });
}

/// Audio window an Olivia pass looks at. It must hold several FEC blocks of the
/// slowest mode for the decoder to sync at all.
const OLIVIA_WINDOW_S: f64 = 24.0;

/// How often an Olivia pass runs. Text appears this long after it is sent, so
/// shorter is livelier — at the cost of re-scanning the band more often.
const OLIVIA_STEP_S: f64 = 4.0;

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
