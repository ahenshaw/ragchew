//! Transmit: text → modulated audio → the sound card. Desktop-only.
//!
//! What this is *not* is a transceiver control path. There is no PTT and no CAT
//! here: the audio is played out of whichever device the operator chose, which
//! is how a rig fed from a USB codec with VOX — or a speaker held to a
//! microphone — actually gets driven. Keying a radio is a separate job.
//!
//! The protocols want opposite scheduling, and it is the same asymmetry the
//! decoder deals with. Olivia and PSK31 are continuous and start the instant
//! you press send. JS8 lives on a UTC grid, so a transmission waits for the
//! next cycle boundary; a receiver looking for frames on that grid will not
//! find one that started whenever the operator happened to finish typing.
//!
//! Untested in CI (no audio device here); verified to compile.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use ragchew::js8::message::{self, IType};
use ragchew::js8::{modem, submode};
use ragchew::olivia;
use ragchew::psk;
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
/// A JS8 frame holds only a few characters — more of them if the words are ones
/// JS8Call's list knows, since each frame goes out compressed where that
/// carries more than the character coding does — one frame goes per cycle, and a
/// message longer than one frame is therefore a *sequence* of frames a cycle
/// apart — silence and all, so that what comes back is one buffer that can be
/// handed to the card and forgotten about. Olivia and PSK31 have no such
/// structure: the whole message is one continuous transmission however long it
/// is, idle either side of it in PSK31's case.
pub fn modulate(text: &str, hz: f64, mode: ModeId) -> Vec<f32> {
    modulate_spans(text, hz, mode).0
}

/// The same, and where in it the signal actually is: `(from, to)` in seconds
/// from the start, one pair per burst.
///
/// The gaps matter to anything holding a transmitter down. A JS8 message too
/// long for one frame is a sequence of them a cycle apart, and the silence in
/// between is not small — four seconds in Slow, near two in the others — so a
/// key held across the whole buffer is a bare carrier on the air for as long
/// again as the frames it separates. Olivia and PSK31 have one span each, being
/// one transmission however long.
pub fn modulate_spans(text: &str, hz: f64, mode: ModeId) -> (Vec<f32>, Vec<(f64, f64)>) {
    let whole = |a: Vec<f32>| {
        let secs = a.len() as f64 / RATE as f64;
        (a, vec![(0.0, secs)])
    };
    match mode {
        ModeId::Olivia(m) => whole(olivia::encode(text, hz, m)),
        ModeId::Psk(m) => whole(psk::encode(text, hz, m)),
        ModeId::Js8(m) => {
            let sm = submode::of(m);
            let period = sm.period_s as usize * RATE as usize;
            let offset = (JS8_CYCLE_OFFSET_S * RATE as f64) as usize;
            // How the message divides into frames, found by packing each in
            // turn to see how much it takes. A pass of its own because the
            // last frame has to be *marked* as the last, and which one that is
            // cannot be known until the division is done: the itype flags go
            // in before the CRC is computed over them, so a frame cannot be
            // flagged after it is packed.
            let mut parts: Vec<&str> = Vec::new();
            let mut rest = text;
            while !rest.trim().is_empty() {
                let (_, used) = message::best_frame(rest, IType::None);
                // `freetext` reporting nothing consumed would spin forever on
                // text it cannot pack at all, so that ends the transmission.
                if used == 0 {
                    break;
                }
                let cut = rest.char_indices().nth(used).map_or(rest.len(), |(i, _)| i);
                let (head, tail) = rest.split_at(cut);
                parts.push(head);
                rest = tail;
            }

            let mut out: Vec<f32> = Vec::new();
            let mut spans: Vec<(f64, f64)> = Vec::new();
            for (cycle, part) in parts.iter().enumerate() {
                // The end of an over is the receiving station's only reliable
                // sign that we have finished talking — JS8Call draws it, and
                // this crate now splits its log on it. A station that never
                // flags one is a station whose overs run together at the far
                // end, which is what ours did.
                let itype = match (cycle + 1 == parts.len(), parts.len()) {
                    (true, 1) => IType::FirstAndLast,
                    (true, _) => IType::Last,
                    _ => IType::None,
                };
                let (a87, _) = message::best_frame(part, itype);
                let burst = modem::encode_audio_sm(&a87, hz, &sm);
                let start = cycle * period + offset;
                if out.len() < start + burst.len() {
                    out.resize(start + burst.len(), 0.0);
                }
                out[start..start + burst.len()].copy_from_slice(&burst);
                spans.push((
                    start as f64 / RATE as f64,
                    (start + burst.len()) as f64 / RATE as f64,
                ));
            }
            (out, spans)
        }
    }
}

/// When the key must be down for a transmission starting at `at`, given where
/// its bursts are.
///
/// The lead and the tail are the whole of it: a rig wants the key down before
/// the audio and up after it, and how long before depends on the rig rather
/// than on us.
pub fn key_spans(at: f64, spans: &[(f64, f64)], lead: f64, tail: f64) -> Vec<(f64, f64)> {
    spans.iter().map(|(from, to)| (at + from - lead, at + to + tail)).collect()
}

/// How long the key goes down before the audio starts, and stays down after it
/// ends. A tenth of a second either side covers a relay and a linear.
pub const KEY_LEAD_S: f64 = 0.1;
pub const KEY_TAIL_S: f64 = 0.1;

/// When a transmission in `mode` may start, in UTC seconds.
///
/// Now, for a continuous mode. For a cycle-aligned one, the next boundary of
/// its own grid — skipping one that is too close to make.
pub fn next_start(mode: ModeId) -> f64 {
    next_start_after(mode, unix_now())
}

/// The same, for a transmission that cannot begin before `earliest` — the one
/// already going out has to finish first.
///
/// A free-running mode simply follows on; a scheduled one waits for the first
/// cycle boundary at or after that point, so queueing behind a burst does not
/// put the next frame half a cycle out.
pub fn next_start_after(mode: ModeId, earliest: f64) -> f64 {
    let from = earliest.max(unix_now());
    match mode.period_s() {
        None => from,
        Some(p) => {
            let p = p as f64;
            let mut t = ((from / p).floor() + 1.0) * p;
            if t - from < CYCLE_MARGIN_S {
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
    /// Compression buys cycles, which is the only currency that matters here:
    /// a JS8 frame goes once per cycle, so a message that packs into fewer
    /// frames is a message that is finished sooner and holds the frequency for
    /// less time.
    #[test]
    fn compressing_the_words_saves_whole_cycles() {
        let text = "JS8CALL KN4CRD 599 599 73 TNX FER QSO GL SK JS8CALL KN4CRD 599";
        let mode = ModeId::Js8(ragchew::js8::Mode::Normal);

        let frames = |pack: fn(&str, IType) -> ([u8; 87], usize)| {
            let mut n = 0;
            let mut rest = text;
            while !rest.trim().is_empty() {
                let (_, used) = pack(rest, IType::None);
                if used == 0 {
                    break;
                }
                let cut = rest.char_indices().nth(used).map_or(rest.len(), |(i, _)| i);
                rest = &rest[cut..];
                n += 1;
            }
            n
        };

        let as_chars = frames(message::freetext);
        let as_words = frames(message::best_frame);
        assert!(
            as_words < as_chars,
            "{as_words} frames compressed against {as_chars} uncompressed — no cycles saved"
        );

        // And what comes off the air is still what went on it.
        let audio = modulate(text, 1500.0, mode);
        let got: String = ragchew::protocol::decode_all(&audio, 1000.0, 2000.0, &[mode])
            .iter()
            .map(|d| d.text.clone())
            .collect::<Vec<_>>()
            .join("");
        assert!(
            got.replace("<>", "").trim() == text,
            "went out as {text:?}, came back as {got:?}"
        );
    }

    /// A key-down per burst, bracketing the audio — and for JS8, one per
    /// frame rather than one across the message.
    ///
    /// The gaps are the point: a JS8 Slow message leaves four seconds of
    /// silence between frames, and a key held across it puts a bare carrier on
    /// the air for that long, every cycle.
    #[test]
    fn the_key_goes_down_around_each_burst_and_not_between_them() {
        let mode = ModeId::Js8(ragchew::js8::Mode::Normal);
        let text = "CQ CQ DE W1AW W1AW K THE STATION HERE IS RUNNING FIFTY WATTS TO A WIRE \
                    AND THE WEATHER IS FINE";
        let (audio, spans) = modulate_spans(text, 1500.0, mode);
        assert!(spans.len() > 1, "the test message fits in one frame; it proves nothing");

        let period = 15.0;
        for (i, (from, to)) in spans.iter().enumerate() {
            // Each frame sits half a second into its own cycle.
            assert!(
                (from - (i as f64 * period + JS8_CYCLE_OFFSET_S)).abs() < 1e-6,
                "frame {i} starts at {from}, not on its cycle"
            );
            assert!(to > from, "frame {i} has no length");
            if i > 0 {
                let gap = from - spans[i - 1].1;
                assert!(gap > 1.0, "only {gap:.2}s between frames {} and {i}", i - 1);
            }
        }
        // And the audio is as long as the last frame, not longer.
        let secs = audio.len() as f64 / RATE as f64;
        assert!((secs - spans.last().unwrap().1).abs() < 0.01, "{secs} of audio for {spans:?}");

        // The key brackets each of them, and lets go in between.
        let at = 1_000_000.0;
        let keys = key_spans(at, &spans, KEY_LEAD_S, KEY_TAIL_S);
        assert_eq!(keys.len(), spans.len(), "one key-down per burst");
        for ((on, off), (from, to)) in keys.iter().zip(&spans) {
            assert!((on - (at + from - KEY_LEAD_S)).abs() < 1e-9, "keyed late");
            assert!((off - (at + to + KEY_TAIL_S)).abs() < 1e-9, "unkeyed early");
        }
        for pair in keys.windows(2) {
            assert!(pair[1].0 > pair[0].1, "the key never came up between frames");
        }
    }

    /// A continuous mode is one burst however long it runs.
    #[test]
    fn a_continuous_transmission_is_one_key_down() {
        for mode in [
            ModeId::Olivia(ragchew::olivia::OL_8_250),
            ModeId::Psk(ragchew::psk::PSK31),
        ] {
            let (audio, spans) = modulate_spans("CQ DE W1AW K", 1500.0, mode);
            assert_eq!(spans.len(), 1, "{} broke into bursts", mode.name());
            assert_eq!(spans[0].0, 0.0);
            let secs = audio.len() as f64 / RATE as f64;
            assert!((spans[0].1 - secs).abs() < 1e-9, "{} span is not the whole of it", mode.name());
        }
    }

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

    /// Our own overs say where they end, and only the last frame says it.
    ///
    /// This is the flag the far end splits its log on — JS8Call draws it, and
    /// so does this crate now. A transmitter that never sets it is a
    /// transmitter whose overs run into whatever it says next, at every station
    /// receiving it. Ours never set it until this test existed.
    #[test]
    fn only_the_last_frame_of_an_over_is_flagged_as_last() {
        let sm = submode::NORMAL;
        let long = "CQ CQ CQ DE W1AW W1AW W1AW FN31 PSE K TNX FOR THE CALL 73 GL ";
        let audio = modulate(long, 1200.0, ModeId::Js8(sm.mode));
        let mut got = js8::modem::decode_all_sm(&audio, 1000.0, 1400.0, 30, &sm);
        got.sort_by_key(|r| r.offset);
        assert!(got.len() > 1, "long message fitted in {} frame(s)", got.len());
        let flags: Vec<bool> = got.iter().map(|r| js8::message::ends_over(&r.a87)).collect();
        let want: Vec<bool> = (0..got.len()).map(|i| i + 1 == got.len()).collect();
        assert_eq!(flags, want, "end-of-over flags across {} frames", got.len());

        // And a message that fits in one frame is that frame: first and last.
        let audio = modulate("CQ DE W1AW ", 1200.0, ModeId::Js8(sm.mode));
        let got = js8::modem::decode_all_sm(&audio, 1000.0, 1400.0, 30, &sm);
        assert_eq!(got.len(), 1, "expected one frame");
        assert!(js8::message::ends_over(&got[0].a87), "a single-frame over did not end");
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

    /// A second transmission goes behind the first, on a boundary — not half a
    /// cycle into it, and not before the card has finished the burst it has.
    ///
    /// The deadlines here are placed against a *known* boundary rather than
    /// against the wall clock. Timing this test relative to `now` made its
    /// answer depend on where the machine happened to be in the 15 s cycle when
    /// it ran, and it failed 2.67% of the time — which is 0.4/15, the
    /// [`CYCLE_MARGIN_S`] window in which the schedule deliberately gives up on
    /// the coming boundary and takes the one after. The bound below allows for
    /// that skip, which the earlier `<= one cycle` did not.
    #[test]
    fn a_queued_transmission_waits_for_the_one_before_it() {
        let js8 = ModeId::Js8(js8::Mode::Normal);
        let p = js8.period_s().expect("JS8 is cycle-aligned") as f64;
        assert_eq!(p, 15.0, "these cases are written around a 15 s cycle");
        // A boundary comfortably in the future, so `unix_now()` inside
        // `next_start_after` never becomes the binding constraint.
        let base = (unix_now() / p).ceil() * p + 10.0 * p;

        // Mid-burst: the next boundary, and no further.
        let busy_until = base + 7.0;
        let at = next_start_after(js8, busy_until);
        assert!(at >= busy_until, "queued on top of a burst still going out");
        assert!((at % p).abs() < 1e-6, "{at} is not on a {p} s boundary");
        assert!(
            (at - (base + p)).abs() < 1e-6,
            "waited {} s when the next boundary was {} s away",
            at - busy_until,
            base + p - busy_until
        );

        // Ending just too close to a boundary to make it: that cycle is given
        // up and the next one taken, so the wait exceeds a full cycle.
        let busy_until = base + p - CYCLE_MARGIN_S / 2.0;
        let at = next_start_after(js8, busy_until);
        assert!(
            (at - (base + 2.0 * p)).abs() < 1e-6,
            "a boundary {} s away should have been skipped as unmakeable",
            base + p - busy_until
        );
        assert!(
            at - busy_until <= p + CYCLE_MARGIN_S,
            "waited {} s, longer than a cycle plus the margin",
            at - busy_until
        );

        // A free-running mode simply follows on.
        let olivia = ModeId::Olivia(olivia::OL_8_250);
        assert!((next_start_after(olivia, base + 7.0) - (base + 7.0)).abs() < 1e-6);
        // and a past deadline never schedules in the past
        let now = unix_now();
        assert!(next_start_after(olivia, 0.0) >= now);
    }
}
