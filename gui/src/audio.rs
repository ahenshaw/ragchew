//! Live audio capture (cpal) → 12 kHz mono, with a rolling buffer and a
//! cycle-aligned decode thread. Desktop-only.
//!
//! Untested in CI (no audio device / display here); verified to compile.

use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use js8::{message, modem};

use crate::channels::Decode;

/// JS8 works at 12 kHz.
const RATE: u32 = 12000;

fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

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
        if self.samples.len() > self.cap {
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
    /// The window at global index `g0` of length [`modem::SAMPLES_PER_SYMBOL`],
    /// or `None` if not fully held yet.
    pub fn window_at(&self, g0: u64) -> Option<Vec<f32>> {
        let n = modem::SAMPLES_PER_SYMBOL as u64;
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
    pub device_name: String,
    pub in_rate: u32,
    _stream: cpal::Stream,
}

/// Names of available input devices (best-effort).
pub fn input_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devs) => devs.filter_map(|d| d.name().ok()).collect(),
        Err(_) => Vec::new(),
    }
}

impl LiveAudio {
    /// Start capturing from `name` (by device name) or the default input.
    pub fn start(name: Option<&str>) -> Result<LiveAudio, String> {
        let host = cpal::default_host();
        let device = match name {
            Some(n) => host
                .input_devices()
                .map_err(|e| e.to_string())?
                .find(|d| d.name().map(|dn| dn == n).unwrap_or(false))
                .ok_or_else(|| format!("input device not found: {n}"))?,
            None => host
                .default_input_device()
                .ok_or_else(|| "no default input device".to_string())?,
        };
        let device_name = device.name().unwrap_or_else(|_| "?".to_string());
        let supported = device.default_input_config().map_err(|e| e.to_string())?;
        let in_rate = supported.sample_rate().0;
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
                    &config,
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
                    &config,
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
                    &config,
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
        Ok(LiveAudio { buf, device_name, in_rate, _stream: stream })
    }
}

/// Spawn the cycle-aligned decoder. Each submode is decoded on its own UTC
/// grid (Slow 30 s, Normal 15 s, Fast 10 s, Turbo 6 s); a cycle is decoded
/// shortly after its frame completes. Frames carry capture-relative `time_s`.
pub fn spawn_decoder(buf: Arc<Mutex<AudioBuf>>) -> Receiver<Vec<Decode>> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let modes = js8::submode::ALL;
        // next cycle-start (UTC seconds) to decode, per mode
        let mut next: Vec<f64> = modes
            .iter()
            .map(|sm| (unix_now() / sm.period_s as f64).floor() * sm.period_s as f64)
            .collect();

        loop {
            let now = unix_now();
            // skip any cycles whose decode time already passed
            for (i, sm) in modes.iter().enumerate() {
                while next[i] + sm.frame_secs() + 1.0 <= now {
                    next[i] += sm.period_s as f64;
                }
            }
            // pick the mode with the earliest upcoming decode time
            let mut best = 0usize;
            let mut best_t = f64::MAX;
            for (i, sm) in modes.iter().enumerate() {
                let t = next[i] + sm.frame_secs() + 1.0;
                if t < best_t {
                    best_t = t;
                    best = i;
                }
            }
            let sm = modes[best];
            let cycle_start = next[best];
            next[best] += sm.period_s as f64;

            let wait = best_t - unix_now();
            if wait > 0.0 {
                thread::sleep(Duration::from_secs_f64(wait));
            }

            let win = sm.period_s as f64;
            let (samples, win_start_global, rate) = {
                let b = buf.lock().unwrap();
                if !b.started() {
                    (Vec::new(), 0u64, RATE as f64)
                } else {
                    let rate = RATE as f64;
                    let su = b.start_unix();
                    let g0 = (((cycle_start - 0.5 - su) * rate).max(0.0)) as u64;
                    let g1 = (((cycle_start + win + 0.5 - su) * rate).max(0.0)) as u64;
                    let start = g0.max(b.global_start());
                    (b.slice_global(g0, g1), start, rate)
                }
            };

            if samples.len() < sm.frame_samples() {
                continue;
            }
            let frames: Vec<Decode> = modem::decode_all_sm(&samples, 300.0, 2600.0, 30, &sm)
                .iter()
                .map(|r| Decode {
                    hz: r.hz,
                    time_s: (win_start_global as f64 + r.offset as f64) / rate,
                    text: message::unpack(&r.a87).text(),
                    mode: r.mode,
                })
                .collect();
            if !frames.is_empty() && tx.send(frames).is_err() {
                break; // UI gone
            }
        }
    });
    rx
}
