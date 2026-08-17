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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use ragchew::js8::message::{self, IType};
use ragchew::js8::{modem, submode};
use ragchew::olivia;
use ragchew::olivia::mode::SYMBOLS_PER_BLOCK;
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

/// One burst of a transmission, on its own.
///
/// A JS8 message longer than one frame is several of these a cycle apart; a
/// continuous mode is always exactly one however long the message. Keeping
/// them separate — rather than one buffer with the silence baked into it — is
/// what makes a message abortable between frames: the queue can be relieved of
/// the frames that have not gone, and each one knows which characters it was
/// carrying, so what comes back to the operator is exactly the text that did
/// not reach anybody.
pub struct Frame {
    /// This burst alone, at the crate's 12 kHz internal rate.
    pub samples: Vec<f32>,
    /// Seconds from the start of the message to the start of this burst.
    pub at_s: f64,
    /// The characters this burst carries.
    pub text: String,
}

impl Frame {
    /// How long the burst itself lasts.
    pub fn secs(&self) -> f64 {
        self.samples.len() as f64 / RATE as f64
    }
}

/// Modulate `text` at `hz` in `mode`, one burst at a time.
///
/// The division into frames is the modem's, not ours: [`message::best_frame`]
/// is asked how much of the text it can pack, and what it took is what that
/// frame carries. So the character boundaries here are exactly the boundaries
/// on the air, which is what lets an aborted message be split at one.
pub fn modulate_frames(text: &str, hz: f64, mode: ModeId) -> Vec<Frame> {
    let whole = |samples: Vec<f32>| {
        vec![Frame { samples, at_s: 0.0, text: text.to_string() }]
    };
    match mode {
        ModeId::Olivia(m) => whole(olivia::encode(text, hz, m)),
        ModeId::Psk(m) => whole(psk::encode(text, hz, m)),
        ModeId::Js8(m) => {
            let sm = submode::of(m);
            let period = sm.period_s as f64;
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

            parts
                .iter()
                .enumerate()
                .map(|(cycle, part)| {
                    // The end of an over is the receiving station's only
                    // reliable sign that we have finished talking — JS8Call
                    // draws it, and this crate now splits its log on it. A
                    // station that never flags one is a station whose overs run
                    // together at the far end, which is what ours did.
                    let itype = match (cycle + 1 == parts.len(), parts.len()) {
                        (true, 1) => IType::FirstAndLast,
                        (true, _) => IType::Last,
                        _ => IType::None,
                    };
                    let (a87, _) = message::best_frame(part, itype);
                    Frame {
                        samples: modem::encode_audio_sm(&a87, hz, &sm),
                        at_s: cycle as f64 * period + JS8_CYCLE_OFFSET_S,
                        text: (*part).to_string(),
                    }
                })
                .collect()
        }
    }
}

/// How much of `text`, going out in `mode`, has certainly reached the far end
/// once `secs` of its burst have played.
///
/// Certainly, not probably: this is the number an aborted message is split on,
/// and everything past it is handed back to the operator as un-sent. So it
/// counts in whatever unit the mode delivers atomically, and rounds down.
///
/// - **JS8** delivers a frame or nothing. The 79 symbols are one LDPC codeword
///   under three Costas arrays; a frame cut anywhere leaves the receiver with
///   no message at all, not a short one.
/// - **Olivia** delivers an FEC block or nothing, for the same reason one level
///   down: the characters of a block are spread across all 64 of its symbols by
///   the Walsh transform, so a block that stopped early decodes to nothing.
///   Its characters are given back even if the cut fell in the last symbol.
/// - **PSK31** is the one true character stream, and the boundary really is a
///   character. Varicode has no interleave and no block, but its codes vary in
///   length, so where a character ends is asked of the encoder rather than
///   guessed from the rate.
///
/// Erring is not symmetric and this errs deliberately. Handing back a character
/// that did go out puts it on the air twice, in a box the operator is looking
/// at and can edit. Failing to hand back one that did not go loses it silently.
pub fn delivered_chars(text: &str, mode: ModeId, secs: f64) -> usize {
    let chars = text.chars().count();
    if secs <= 0.0 {
        return 0;
    }
    match mode {
        // One frame per burst, so this is the whole of it or none of it.
        ModeId::Js8(m) => {
            let sm = submode::of(m);
            let frame_s = (modem::N_SYMBOLS * sm.nsps) as f64 / RATE as f64;
            if secs >= frame_s {
                chars
            } else {
                0
            }
        }
        ModeId::Olivia(m) => {
            // Counted in symbol periods rather than in `block_secs`, because a
            // block is not readable at the end of its 64 of them. Two more are
            // wanted: one because the symbols overlap by half, so the last one
            // starts at period 63 and its raised cosine runs on past the
            // block's nominal end; and one more because the decoder's analysis
            // window has to sit wholly inside the audio it is handed. Dividing
            // by the nominal block length claims a block a receiver cannot
            // read, which is what this was caught doing — see
            // `what_a_cut_burst_delivered_is_what_a_receiver_copies`, which
            // measures this against the decoder rather than trusting it.
            let periods = secs * RATE as f64 / m.symbol_separ() as f64;
            let blocks =
                ((periods - 2.0) / SYMBOLS_PER_BLOCK as f64).floor().max(0.0) as usize;
            (blocks * m.chars_per_block()).min(chars)
        }
        ModeId::Psk(m) => {
            // The burst opens with idle, and a symbol's energy is spread over
            // two symbol periods by the shaping — so a character is on the air
            // once the symbol *after* its last one has finished.
            let sps = m.symbol_samples() as f64 / RATE as f64;
            let mut done = 0;
            for (n, _) in text.char_indices().skip(1).chain([(text.len(), ' ')]) {
                let bits = psk::varicode::encode(&text[..n]).len();
                if (psk::modem::IDLE_SYMBOLS + bits + 1) as f64 * sps > secs {
                    break;
                }
                done = text[..n].chars().count();
            }
            done
        }
    }
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
    let frames = modulate_frames(text, hz, mode);
    let mut out: Vec<f32> = Vec::new();
    let mut spans: Vec<(f64, f64)> = Vec::new();
    for f in &frames {
        let start = (f.at_s * RATE as f64).round() as usize;
        if out.len() < start + f.samples.len() {
            out.resize(start + f.samples.len(), 0.0);
        }
        out[start..start + f.samples.len()].copy_from_slice(&f.samples);
        spans.push((f.at_s, f.at_s + f.secs()));
    }
    (out, spans)
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

/// How long the audio takes to fade when a transmission is stopped part-way.
///
/// A buffer that simply stops mid-symbol is a step in the waveform, and a step
/// is a click across the whole passband — which is the one thing an abort must
/// not put on the air. Five milliseconds is inaudible as a fade and is a good
/// forty dB down by the time anything of it reaches the band edges.
const ABORT_FADE_S: f32 = 0.005;

/// One queued transmission.
struct Burst {
    /// What whoever queued this knows it by, so one transmission can be taken
    /// back without disturbing the others in the queue.
    id: u64,
    /// UTC second to begin on.
    start_unix: f64,
    samples: Vec<f32>,
}

/// What [`Tx`] remembers about a transmission it has queued, once the samples
/// themselves have gone to the player: enough to say when the card runs dry
/// without reaching into the audio callback for it.
struct Sched {
    id: u64,
    start_unix: f64,
    end_unix: f64,
}

/// What an abort caught, and where.
pub struct Aborted {
    /// Transmissions that had not begun. Nothing of their text went out.
    pub never_began: Vec<u64>,
    /// The transmission that was on the air, and how many seconds of it had
    /// played when the key came up.
    ///
    /// Taken from the clock rather than from the audio callback's read
    /// position, which is a few milliseconds ahead of the antenna and behind a
    /// lock this must not wait on. The error is small and it is on the safe
    /// side: [`delivered_chars`] rounds down to a whole frame or block, so a
    /// couple of milliseconds of doubt can only ever hand back a unit that in
    /// fact went out — which the operator sees — rather than swallow one that
    /// did not.
    pub in_flight: Option<(u64, f64)>,
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
    /// reaching into the audio callback for it. Derived from [`Tx::sched`]
    /// rather than only ever climbing, so that taking a transmission back also
    /// takes back the countdown it put on screen.
    busy_until: f64,
    /// Every transmission queued and not yet finished, in the order sent.
    sched: Vec<Sched>,
    next_id: u64,
    /// Raised to tell the player to let go of the burst it is playing. Consumed
    /// by the callback, which fades rather than cuts — see [`ABORT_FADE_S`].
    stop: Arc<AtomicBool>,
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
        let stop = Arc::new(AtomicBool::new(false));
        let err_fn = |e| eprintln!("output stream error: {e}");

        let stream = match fmt {
            cpal::SampleFormat::F32 => {
                let mut p = Player::new(out_rate, channels, queue.clone(), stop.clone());
                device.build_output_stream(
                    config,
                    move |data: &mut [f32], _: &_| p.fill(data, |v| v),
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let mut p = Player::new(out_rate, channels, queue.clone(), stop.clone());
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
                let mut p = Player::new(out_rate, channels, queue.clone(), stop.clone());
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
        Ok(Tx {
            queue,
            device_name,
            out_rate,
            busy_until: 0.0,
            sched: Vec::new(),
            next_id: 0,
            stop,
            _stream: stream,
        })
    }

    /// Queue 12 kHz samples to begin at `start_unix`. Returns the id this
    /// transmission is known by afterwards, and when it finishes.
    pub fn send(&mut self, samples: Vec<f32>, start_unix: f64) -> (u64, f64) {
        let end = start_unix + samples.len() as f64 / RATE as f64;
        let id = self.next_id;
        self.next_id += 1;
        self.sched.retain(|s| s.end_unix > unix_now());
        self.sched.push(Sched { id, start_unix, end_unix: end });
        self.busy_until = self.busy_until.max(end);
        self.queue.lock().unwrap().push_back(Burst { id, start_unix, samples });
        (id, end)
    }

    /// Seconds until the queue runs dry: 0 when nothing is going out.
    pub fn busy_for(&self) -> f64 {
        (self.busy_until - unix_now()).max(0.0)
    }

    /// Drop the transmissions named in `ids` that have not begun, and say which
    /// were actually dropped.
    ///
    /// "Has not begun" is decided under the queue's own lock, and that is what
    /// makes the answer safe to act on: the player takes a burst out of the
    /// queue only once its moment has come, so one still sitting here with its
    /// start time in the future has not put a sample anywhere near the card.
    /// A burst whose moment has passed is left alone even if the callback has
    /// not reached it yet — half a millisecond of doubt is not worth telling an
    /// operator their text never went out when it may have.
    pub fn cancel_pending(&mut self, ids: &[u64]) -> Vec<u64> {
        let now = unix_now();
        let dropped = drop_pending(&mut self.queue.lock().unwrap(), ids, now);
        self.sched.retain(|s| s.end_unix > now && !dropped.contains(&s.id));
        self.busy_until = self.sched.iter().fold(0.0, |a, s| f64::max(a, s.end_unix));
        dropped
    }

    /// Stop everything: the queue, and the burst already on its way out of the
    /// card. Returns the ids of the transmissions that had not begun, which are
    /// the ones whose text did not go out at all.
    ///
    /// The burst in flight used to be left to finish — `abort` cleared the
    /// queue behind it and set the countdown to zero, so the interface said
    /// idle while the card played on. It is now faded out over
    /// [`ABORT_FADE_S`]. What has already gone to the card, a few milliseconds
    /// of it, is still beyond recall.
    pub fn abort(&mut self) -> Aborted {
        let now = unix_now();
        // From the schedule rather than from the queue: a burst that has begun
        // is no longer *in* the queue — the player took it — and it is the one
        // whose text has to be divided rather than handed back whole.
        let never_began =
            self.sched.iter().filter(|s| s.start_unix > now).map(|s| s.id).collect();
        let in_flight = self
            .sched
            .iter()
            .find(|s| s.start_unix <= now && now < s.end_unix)
            .map(|s| (s.id, now - s.start_unix));
        self.queue.lock().unwrap().clear();
        self.stop.store(true, Ordering::Relaxed);
        self.sched.clear();
        self.busy_until = 0.0;
        Aborted { never_began, in_flight }
    }
}

/// Take the named transmissions out of `queue`, so long as they have not begun
/// by `now`, and say which went. Split out from [`Tx::cancel_pending`] so the
/// rule can be tested without an audio device to hang a `Tx` on.
fn drop_pending(queue: &mut VecDeque<Burst>, ids: &[u64], now: f64) -> Vec<u64> {
    let mut dropped = Vec::new();
    queue.retain(|b| {
        let take_back = b.start_unix > now && ids.contains(&b.id);
        if take_back {
            dropped.push(b.id);
        }
        !take_back
    });
    dropped
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
    /// Raised by [`Tx::abort`], consumed here.
    stop: Arc<AtomicBool>,
    /// How much of the abort fade is left, 1 down to 0. `None` when the burst
    /// is playing normally.
    fade: Option<f32>,
    /// What one output sample takes off that.
    fade_step: f32,
}

impl Player {
    fn new(
        out_rate: u32,
        channels: usize,
        queue: Arc<Mutex<VecDeque<Burst>>>,
        stop: Arc<AtomicBool>,
    ) -> Player {
        Player {
            channels: channels.max(1),
            step: RATE as f64 / out_rate as f64,
            queue,
            cur: None,
            stop,
            fade: None,
            fade_step: 1.0 / (ABORT_FADE_S * out_rate as f32).max(1.0),
        }
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
        // Taken as a swap so the flag is spent the moment it is seen: an abort
        // arriving between callbacks fades one burst, not every burst after it.
        // With nothing playing there is nothing to fade, and the queue behind
        // it has already been cleared by whoever raised the flag.
        if self.stop.swap(false, Ordering::Relaxed) && self.cur.is_some() {
            self.fade = Some(1.0);
        }
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
            if let Some(g) = self.fade.as_mut() {
                v *= *g;
                *g -= self.fade_step;
                if *g <= 0.0 {
                    self.cur = None;
                    self.fade = None;
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

    /// What an Olivia burst is cut off at, decoded by the receiver it was cut
    /// off from.
    ///
    /// The claim [`delivered_chars`] makes is not about arithmetic, it is about
    /// the air: everything past the count it returns did *not* reach the far
    /// end, and is therefore the operator's to have back. So it is checked the
    /// only way that means anything — modulate, truncate the audio where the
    /// abort would have, hand what is left to this crate's own decoder, and see
    /// what a receiving station would have copied.
    ///
    /// It has already earned its place. The first version of the Olivia count
    /// divided by the nominal block length and claimed a block that a receiver
    /// handed the same audio could not read, because Olivia's symbols overlap
    /// by half and the last one runs past the block it belongs to.
    #[test]
    fn what_a_cut_olivia_burst_delivered_is_what_a_receiver_copies() {
        let hz = 1500.0;
        let text = "CQ CQ DE W1AW W1AW FN31 PSE K ";

        for m in [
            ragchew::olivia::OL_8_250,
            ragchew::olivia::OL_16_500,
            ragchew::olivia::OL_32_1000,
        ] {
            let mode = ModeId::Olivia(m);
            let audio = modulate(text, hz, mode);
            let whole = audio.len() as f64 / RATE as f64;
            // Every eighth of the burst, so the cut lands inside a block as
            // often as it lands near the end of one.
            for eighth in 1..=8 {
                let secs = whole * eighth as f64 / 8.0;
                let cut = ((secs * RATE as f64) as usize).min(audio.len());
                let claimed = delivered_chars(text, mode, secs);

                // A narrow search band: a wide one lets the scan report the
                // same signal at more than one candidate centre, and the
                // concatenation would not be the message.
                let got: String =
                    ragchew::protocol::decode_all(&audio[..cut], hz - 200.0, hz + 200.0, &[mode])
                        .iter()
                        .map(|d| d.text.clone())
                        .collect();

                // Nothing is claimed that a receiver did not get. Compared as a
                // prefix rather than by length: the point is that these are the
                // *same characters*, in the same order.
                //
                // A receiver that copied *nothing* is passed over rather than
                // failed. Below about two blocks this decoder cannot find the
                // signal at all — it is scanning a band for a carrier, not
                // tracking one it is already synchronised to — and that is a
                // statement about acquisition, not about what was radiated. The
                // test keeps its teeth everywhere copying succeeded, which is
                // where the block-length error was caught.
                let claimed_text: String = text.chars().take(claimed).collect();
                assert!(
                    got.is_empty() || got.starts_with(&claimed_text),
                    "{}: at {eighth}/8 claimed {claimed_text:?} but the receiver copied {got:?}",
                    m.name()
                );
                // And it is not so cautious as to be useless: by seven eighths
                // of the way through, something has certainly landed.
                if eighth >= 7 {
                    assert!(
                        claimed > 0,
                        "{}: {eighth}/8 of the burst went out and it claimed nothing",
                        m.name()
                    );
                }
            }
            assert_eq!(
                delivered_chars(text, mode, whole),
                text.chars().count(),
                "{} did not deliver its own message in full",
                m.name()
            );
        }
    }

    /// A PSK31 burst delivers whole characters, and the claimed ones are
    /// sample-for-sample what was on the air.
    ///
    /// Checked against the modulator rather than the demodulator, and
    /// deliberately. PSK31 has no block on the air — a receiving station prints
    /// each character as its varicode symbol completes — but this crate's
    /// decoder is a batch one that emits per 256-symbol block and drops a
    /// partial trailing block, so it cannot answer a question about single
    /// characters. What it *can* be asked is whether the audio of the claimed
    /// characters is literally a leading piece of the audio that went out, and
    /// whether that piece fits inside the part that was played. It is, and it
    /// does, and that is the whole guarantee.
    #[test]
    fn a_psk_burst_delivers_whole_characters() {
        let hz = 1500.0;
        let text = "CQ CQ DE W1AW W1AW FN31 PSE K ";
        let m = ragchew::psk::PSK31;
        let mode = ModeId::Psk(m);
        let sps = m.symbol_samples();
        let audio = modulate(text, hz, mode);
        let whole = audio.len() as f64 / RATE as f64;

        // Every symbol boundary, not a handful of sample points: the claim
        // turns over one symbol at a time, and a coarser sweep steps clean over
        // the window where getting it wrong shows.
        let mut seen = 0;
        for symbol in 1..=(audio.len() / sps) {
            let played = symbol * sps;
            let secs = played as f64 / RATE as f64;
            let claimed = delivered_chars(text, mode, secs);
            assert!(claimed >= seen, "the delivered count went backwards");
            seen = claimed;
            if claimed == 0 {
                continue;
            }

            let prefix: String = text.chars().take(claimed).collect();
            let idle = ragchew::psk::modem::IDLE_SYMBOLS;
            let bits = ragchew::psk::varicode::encode(&prefix).len();
            let alone = ragchew::psk::encode(&prefix, hz, mode_of(mode));
            // The shape of a burst, checked against the encoder rather than
            // assumed: idle either side of the text, and one symbol of run-out
            // because the shaping is two symbols long.
            assert_eq!(alone.len(), (2 * idle + bits + 2) * sps, "the burst is not shaped as assumed");

            // Modulating just the claimed characters gives the same samples, up
            // to where the two can still agree. They part company one symbol
            // before the end of the text: the shaping spreads each symbol over
            // two periods, so the last symbol of the prefix overlaps idle here
            // and the next character there.
            let agree = (idle + bits) * sps;
            assert_eq!(
                &alone[..agree],
                &audio[..agree],
                "the claimed characters are not the ones that went out"
            );
            // And the whole of that last symbol — overlap included, which is
            // what a receiver integrates — was inside what played.
            let ends = (idle + bits + 1) * sps;
            assert!(
                ends <= played,
                "claimed {claimed} characters ending at {ends} with only {played} played"
            );
        }
        assert_eq!(
            delivered_chars(text, mode, whole),
            text.chars().count(),
            "PSK31 did not deliver its own message in full"
        );
    }

    /// The PSK mode inside a [`ModeId`], for the tests that need the encoder
    /// directly.
    fn mode_of(mode: ModeId) -> ragchew::psk::Mode {
        match mode {
            ModeId::Psk(m) => m,
            other => panic!("{} is not a PSK mode", other.name()),
        }
    }

    /// A JS8 frame is all of its characters or none of them.
    ///
    /// Not a stream: the 79 symbols are one LDPC codeword under three Costas
    /// arrays, so a frame cut anywhere leaves a receiver with no message rather
    /// than a short one. Treating it as a progress bar — which is what the
    /// composer's strike-through does, and says it does — would hand back text
    /// that had in fact gone, or keep text that had not.
    #[test]
    fn a_js8_frame_delivers_all_of_itself_or_none() {
        let hz = 1200.0;
        let sm = submode::NORMAL;
        let mode = ModeId::Js8(sm.mode);
        let text = "CQ DE W1AW";
        let frames = modulate_frames(text, hz, mode);
        assert_eq!(frames.len(), 1, "the test message should be one frame");
        let whole = frames[0].secs();

        // Cut anywhere inside it and nothing has been delivered...
        for part in [0.1, 0.5, 0.9, 0.99] {
            let secs = whole * part;
            assert_eq!(
                delivered_chars(text, mode, secs),
                0,
                "claimed characters from a frame that was {part} of the way through"
            );
            // ...which is exactly what the decoder makes of it.
            let cut = (secs * RATE as f64) as usize;
            let got = ragchew::js8::modem::decode_all_sm(
                &frames[0].samples[..cut],
                hz - 200.0,
                hz + 200.0,
                30,
                &sm,
            );
            assert!(got.is_empty(), "a frame cut at {part} decoded anyway");
        }
        assert_eq!(delivered_chars(text, mode, whole), text.chars().count());
    }

    /// A JS8 over goes out as one burst per frame, each carrying the characters
    /// it was packed with — and the frames together are the message.
    ///
    /// One buffer with the silence baked in would be a single burst the queue
    /// could only take back whole. These are what make the tail of an over
    /// abortable, and what make the text that comes back exact.
    #[test]
    fn a_js8_over_is_one_burst_per_frame_and_the_frames_are_the_message() {
        let mode = ModeId::Js8(ragchew::js8::Mode::Normal);
        let text = "CQ CQ DE W1AW W1AW FN31 PSE K TNX FER THE CALL AND 73 GL OM ";
        let frames = modulate_frames(text, 1500.0, mode);
        assert!(frames.len() > 1, "the test message fits in one frame; it proves nothing");

        // Nothing is added, dropped or reordered in the division.
        let joined: String = frames.iter().map(|f| f.text.clone()).collect();
        assert_eq!(joined, text, "the frames are not the message");

        let period = mode.period_s().unwrap() as f64;
        for (i, f) in frames.iter().enumerate() {
            assert!(!f.text.is_empty(), "frame {i} carries no text");
            assert!((f.at_s - (i as f64 * period + JS8_CYCLE_OFFSET_S)).abs() < 1e-9,
                "frame {i} starts at {}, not on its own cycle", f.at_s);
            // Each burst is the frame alone — no silence carried along with it,
            // which is what the queue would otherwise have to hold the key down
            // across.
            assert!(f.secs() < period, "frame {i} is {}s of a {period}s cycle", f.secs());
        }

        // And assembled back into one buffer it is what it always was.
        let (audio, spans) = modulate_spans(text, 1500.0, mode);
        assert_eq!(spans.len(), frames.len());
        let got: String = ragchew::protocol::decode_all(&audio, 1000.0, 2000.0, &[mode])
            .iter()
            .map(|d| d.text.clone())
            .collect();
        assert_eq!(got.replace("<>", "").trim(), text.trim(), "the reassembled message changed");
    }

    /// Only what has not begun can be taken back, and only what was asked for.
    ///
    /// The first half is the whole safety of the feature: a burst still in the
    /// queue with its start time ahead of it has not reached the card, so its
    /// text can be handed back to the operator as never sent. One whose moment
    /// has passed stays where it is even though the callback may not have
    /// picked it up yet — the cost of guessing wrong there is telling somebody
    /// their message did not go out when it did.
    #[test]
    fn only_a_transmission_that_has_not_begun_comes_back() {
        let now = 1_000_000.0;
        let burst = |id, start| Burst { id, start_unix: start, samples: vec![0.0; 12] };
        let mut q: VecDeque<Burst> =
            [burst(1, now - 3.0), burst(2, now + 5.0), burst(3, now + 20.0)].into();

        // The one on the air is not on offer, whoever asks for it.
        assert!(drop_pending(&mut q, &[1], now).is_empty(), "took back a burst already going");
        assert_eq!(q.len(), 3);

        // A queued one is, and its neighbours are left alone.
        assert_eq!(drop_pending(&mut q, &[2], now), vec![2]);
        assert_eq!(q.iter().map(|b| b.id).collect::<Vec<_>>(), vec![1, 3]);

        // Asking for everything gets back only what qualifies.
        assert_eq!(drop_pending(&mut q, &[1, 2, 3], now), vec![3]);
        assert_eq!(q.iter().map(|b| b.id).collect::<Vec<_>>(), vec![1]);

        // And an id nobody queued is not an error, it is nothing.
        assert!(drop_pending(&mut q, &[99], now).is_empty());
    }

    /// Stopping a transmission fades it out instead of cutting it off.
    ///
    /// A buffer that stops mid-symbol is a step in the waveform, and a step is
    /// a click clear across the passband — the abort would be louder on the
    /// band than the message it aborted. So the last thing the card is given
    /// has to walk down to zero, and this asserts on the step between one
    /// output sample and the next rather than on the shape of the ramp.
    #[test]
    fn an_aborted_burst_is_faded_rather_than_cut() {
        let rate = 48_000u32;
        let stop = Arc::new(AtomicBool::new(false));
        let queue: Arc<Mutex<VecDeque<Burst>>> = Arc::new(Mutex::new(VecDeque::new()));
        // Full scale throughout, so any step in the output is the abort's doing
        // and not the signal's.
        queue.lock().unwrap().push_back(Burst {
            id: 0,
            start_unix: unix_now() - 0.05,
            samples: vec![1.0; RATE as usize],
        });
        let mut p = Player::new(rate, 1, queue.clone(), stop.clone());

        let mut buf = vec![0.0f32; 64];
        p.fill(&mut buf, |v| v);
        assert!(buf.iter().all(|&v| v > 0.99), "the burst was not playing to begin with");

        stop.store(true, Ordering::Relaxed);
        let mut after = vec![0.0f32; 4096];
        p.fill(&mut after, |v| v);

        // It gets to silence, and stays there.
        assert!(after.last().copied().unwrap().abs() < 1e-6, "still playing after the fade");
        let fade_samples = (ABORT_FADE_S * rate as f32) as usize;
        assert!(
            after[fade_samples + 8..].iter().all(|&v| v.abs() < 1e-6),
            "the fade outran its {ABORT_FADE_S} s"
        );
        // Nowhere does it jump. One full-scale sample to zero would be a step
        // of 1.0; a fade over five milliseconds steps by about 1/240.
        // Seeded with the seam between the two fills, which is where a cut
        // would land if the flag were acted on by dropping the burst outright.
        let seam = (after[0] - buf[buf.len() - 1]).abs();
        let biggest = after.windows(2).map(|w| (w[1] - w[0]).abs()).fold(seam, f32::max);
        assert!(biggest < 0.02, "the audio stepped by {biggest} on the way down");
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
