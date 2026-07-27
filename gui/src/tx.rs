//! Transmit: text → modulated audio → the sound card. Desktop-only.
//!
//! What this is *not* is a transceiver control path. There is no PTT and no CAT
//! here: the audio is played out of whichever device the operator chose, which
//! is how a rig fed from a USB codec with VOX — or a speaker held to a
//! microphone — actually gets driven. Keying a radio is a separate job.
//!
//! The two protocols want opposite scheduling, and it is the same asymmetry the
//! decoder deals with. Olivia is continuous and starts the instant you press
//! send. JS8 lives on a UTC grid, so a transmission waits for the next cycle
//! boundary; a receiver looking for frames on that grid will not find one that
//! started whenever the operator happened to finish typing.
//!
//! Untested in CI (no audio device here); verified to compile.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use ragchew::js8::message::{self, IType};
use ragchew::js8::{modem, submode};
use ragchew::olivia;
use ragchew::protocol::ModeId;

/// Everything is modulated at the crate's internal rate and resampled up to
/// whatever the card wants on the way out.
const RATE: u32 = ragchew::SAMPLE_RATE;

/// How far into its cycle a JS8 frame starts. The reference implementation
/// leaves the same gap, and the decoder searches around it either way; what it
/// buys is a receiver that started listening exactly on the boundary still
/// catching the whole frame.
const JS8_CYCLE_OFFSET_S: f64 = 0.5;

/// A transmission is not queued for a boundary this close: the card is filling
/// its buffer several milliseconds ahead, and a cycle we cannot actually make
/// is better spent waiting for the next one.
const CYCLE_MARGIN_S: f64 = 0.4;

fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

/// Modulate `text` at `hz` in `mode`, at the crate's 12 kHz internal rate.
///
/// A JS8 frame holds only a few characters, one frame goes per cycle, and a
/// message longer than one frame is therefore a *sequence* of frames a cycle
/// apart — silence and all, so that what comes back is one buffer that can be
/// handed to the card and forgotten about. Olivia has no such structure: the
/// whole message is one continuous transmission however long it is.
pub fn modulate(text: &str, hz: f64, mode: ModeId) -> Vec<f32> {
    match mode {
        ModeId::Olivia(m) => olivia::encode(text, hz, m),
        ModeId::Js8(m) => {
            let sm = submode::of(m);
            let period = sm.period_s as usize * RATE as usize;
            let offset = (JS8_CYCLE_OFFSET_S * RATE as f64) as usize;
            let mut out: Vec<f32> = Vec::new();
            let mut rest = text;
            let mut cycle = 0;
            while !rest.trim().is_empty() {
                let (a87, used) = message::freetext(rest, IType::None);
                // `freetext` reporting nothing consumed would spin forever on
                // text it cannot pack at all, so that ends the transmission.
                if used == 0 {
                    break;
                }
                let burst = modem::encode_audio_sm(&a87, hz, &sm);
                let start = cycle * period + offset;
                if out.len() < start + burst.len() {
                    out.resize(start + burst.len(), 0.0);
                }
                out[start..start + burst.len()].copy_from_slice(&burst);
                rest = &rest[rest.char_indices().nth(used).map_or(rest.len(), |(i, _)| i)..];
                cycle += 1;
            }
            out
        }
    }
}

/// When a transmission in `mode` may start, in UTC seconds.
///
/// Now, for a continuous mode. For a cycle-aligned one, the next boundary of
/// its own grid — skipping one that is too close to make.
pub fn next_start(mode: ModeId) -> f64 {
    let now = unix_now();
    match mode.period_s() {
        None => now,
        Some(p) => {
            let p = p as f64;
            let mut t = ((now / p).floor() + 1.0) * p;
            if t - now < CYCLE_MARGIN_S {
                t += p;
            }
            t
        }
    }
}

/// One queued transmission.
struct Burst {
    /// UTC second to begin on.
    start_unix: f64,
    samples: Vec<f32>,
}

/// A running output stream and its queue of transmissions.
///
/// Not `Send`: a cpal stream is bound to the thread that made it, exactly like
/// [`crate::audio::LiveAudio`] on the capture side.
pub struct Tx {
    queue: Arc<Mutex<VecDeque<Burst>>>,
    /// What the operator picked, for showing in the UI.
    pub device_name: String,
    pub out_rate: u32,
    /// UTC second the queue runs dry, so the UI can say "transmitting" without
    /// reaching into the audio callback for it.
    busy_until: f64,
    _stream: cpal::Stream,
}

impl Tx {
    /// Open the output device with this id, or the default output.
    pub fn start(id: Option<&str>) -> Result<Tx, String> {
        let host = cpal::default_host();
        let device = match id {
            Some(want) => host
                .output_devices()
                .map_err(|e| e.to_string())?
                .find(|d| d.id().map(|i| i.to_string()).as_deref() == Ok(want))
                .ok_or_else(|| format!("output device not found: {want}"))?,
            None => host
                .default_output_device()
                .ok_or_else(|| "no default output device".to_string())?,
        };
        let device_name = device.to_string();
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        let out_rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let fmt = supported.sample_format();
        let config: cpal::StreamConfig = supported.config();

        let queue: Arc<Mutex<VecDeque<Burst>>> = Arc::new(Mutex::new(VecDeque::new()));
        let err_fn = |e| eprintln!("output stream error: {e}");

        let stream = match fmt {
            cpal::SampleFormat::F32 => {
                let mut p = Player::new(out_rate, channels, queue.clone());
                device.build_output_stream(
                    config,
                    move |data: &mut [f32], _: &_| p.fill(data, |v| v),
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let mut p = Player::new(out_rate, channels, queue.clone());
                device.build_output_stream(
                    config,
                    move |data: &mut [i16], _: &_| {
                        p.fill(data, |v| (v * 32767.0).clamp(-32768.0, 32767.0) as i16)
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let mut p = Player::new(out_rate, channels, queue.clone());
                device.build_output_stream(
                    config,
                    move |data: &mut [u16], _: &_| {
                        p.fill(data, |v| {
                            ((v * 32767.0).clamp(-32768.0, 32767.0) + 32768.0) as u16
                        })
                    },
                    err_fn,
                    None,
                )
            }
            other => return Err(format!("unsupported sample format {other:?}")),
        }
        .map_err(|e| e.to_string())?;

        stream.play().map_err(|e| e.to_string())?;
        Ok(Tx { queue, device_name, out_rate, busy_until: 0.0, _stream: stream })
    }

    /// Queue 12 kHz samples to begin at `start_unix`. Returns when they finish.
    pub fn send(&mut self, samples: Vec<f32>, start_unix: f64) -> f64 {
        let end = start_unix + samples.len() as f64 / RATE as f64;
        self.busy_until = self.busy_until.max(end);
        self.queue.lock().unwrap().push_back(Burst { start_unix, samples });
        end
    }

    /// Seconds until the queue runs dry: 0 when nothing is going out.
    pub fn busy_for(&self) -> f64 {
        (self.busy_until - unix_now()).max(0.0)
    }

    /// Drop everything queued. What is already in the card's buffer is a few
    /// milliseconds of audio and cannot be recalled.
    pub fn abort(&mut self) {
        self.queue.lock().unwrap().clear();
        self.busy_until = 0.0;
    }
}

/// Pulls 12 kHz bursts out of the queue on their schedule and writes them to
/// the card's rate.
struct Player {
    channels: usize,
    /// 12 kHz samples per output sample.
    step: f64,
    queue: Arc<Mutex<VecDeque<Burst>>>,
    /// The burst being played and the read position in it, in (fractional)
    /// 12 kHz samples.
    cur: Option<(Vec<f32>, f64)>,
}

impl Player {
    fn new(out_rate: u32, channels: usize, queue: Arc<Mutex<VecDeque<Burst>>>) -> Player {
        Player { channels: channels.max(1), step: RATE as f64 / out_rate as f64, queue, cur: None }
    }

    /// Fill one callback's worth of interleaved output.
    ///
    /// A burst starts when its time comes round, and starts *where* it would
    /// have been had we begun exactly on time — a callback is several
    /// milliseconds of audio, and a cycle-aligned mode would otherwise drift a
    /// little later with every transmission.
    fn fill<T>(&mut self, data: &mut [T], conv: impl Fn(f32) -> T)
    where
        T: Copy,
    {
        let now = unix_now();
        if self.cur.is_none() {
            let mut q = self.queue.lock().unwrap();
            if q.front().is_some_and(|b| now >= b.start_unix) {
                let b = q.pop_front().unwrap();
                let skip = ((now - b.start_unix) * RATE as f64).max(0.0);
                self.cur = Some((b.samples, skip));
            }
        }

        for frame in data.chunks_mut(self.channels) {
            let mut v = 0.0f32;
            if let Some((s, pos)) = self.cur.as_mut() {
                let i = *pos as usize;
                if i + 1 < s.len() {
                    let f = (*pos - i as f64) as f32;
                    v = s[i] * (1.0 - f) + s[i + 1] * f;
                    *pos += self.step;
                } else {
                    self.cur = None;
                }
            }
            let out = conv(v);
            for x in frame.iter_mut() {
                *x = out;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ragchew::js8;

    /// What goes out has to come back: modulate a message and decode the result.
    #[test]
    fn an_olivia_transmission_decodes_back() {
        let mode = olivia::OL_8_250;
        let audio = modulate("DE W1AW TEST ", 1500.0, ModeId::Olivia(mode));
        let got = olivia::decode_all(&audio, 1200.0, 1800.0, &[mode]);
        let text: String = got.iter().map(|r| r.text()).collect();
        assert!(text.contains("DE W1AW TEST"), "decoded {text:?}");
    }

    #[test]
    fn a_js8_transmission_decodes_back() {
        let sm = submode::NORMAL;
        let audio = modulate("CQ DE W1AW ", 1200.0, ModeId::Js8(sm.mode));
        let got = js8::modem::decode_all_sm(&audio, 1000.0, 1400.0, 30, &sm);
        assert_eq!(got.len(), 1, "expected one frame");
        let text = js8::message::unpack(&got[0].a87).text();
        assert!(text.contains("CQ DE W1AW"), "decoded {text:?}");
    }

    /// A message too long for one JS8 frame goes out as several, one per cycle,
    /// and every one of them decodes.
    #[test]
    fn a_long_js8_message_becomes_one_frame_per_cycle() {
        let sm = submode::NORMAL;
        let text = "CQ CQ CQ DE W1AW W1AW W1AW FN31 PSE K TNX FOR THE CALL 73 GL ";
        let audio = modulate(text, 1200.0, ModeId::Js8(sm.mode));
        let got = js8::modem::decode_all_sm(&audio, 1000.0, 1400.0, 30, &sm);
        assert!(got.len() > 1, "long message fitted in {} frame(s)", got.len());

        let period = sm.period_s as usize * RATE as usize;
        for r in &got {
            let into_cycle = r.offset % period;
            let want = (JS8_CYCLE_OFFSET_S * RATE as f64) as usize;
            assert!(
                into_cycle.abs_diff(want) < RATE as usize / 10,
                "frame at {} is {into_cycle} samples into its cycle",
                r.offset
            );
        }
        let joined: String =
            got.iter().map(|r| js8::message::unpack(&r.a87).text()).collect::<Vec<_>>().join("");
        assert!(joined.contains("CQ CQ CQ DE W1AW"), "decoded {joined:?}");
    }

    /// Continuous modes go now; cycle-aligned ones wait for their grid.
    #[test]
    fn scheduling_follows_the_mode() {
        let now = unix_now();
        let olivia = next_start(ModeId::Olivia(olivia::OL_8_250));
        assert!((olivia - now).abs() < 1.0, "Olivia waited {}s", olivia - now);

        for m in [js8::Mode::Slow, js8::Mode::Normal, js8::Mode::Fast, js8::Mode::Turbo] {
            let mode = ModeId::Js8(m);
            let p = mode.period_s().unwrap() as f64;
            let t = next_start(mode);
            assert!((t % p).abs() < 1e-6, "{m:?} start {t} is off its {p}s grid");
            assert!(t > now && t - now >= CYCLE_MARGIN_S, "{m:?} start is too close to make");
            assert!(t - now <= 2.0 * p, "{m:?} waited {}s for a {p}s cycle", t - now);
        }
    }
}
