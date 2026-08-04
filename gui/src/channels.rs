//! Channel tracking: associate individual decodes into persistent "channels"
//! (one per station/conversation) by carrier frequency, tolerating drift, so a
//! station's text stays on one row over time.
//!
//! A "decode" is one JS8 frame, one Olivia FEC block, or — for PSK — a single
//! character. The tracker does not care which, only where it landed and when.
//!
//! It does care about *when* on one further count. A decode's own time is known
//! only here, as it arrives; downstream the text is one string with the
//! silences squeezed out of it. So this is where a station going quiet is
//! noticed and written down: see [`Channel::breaks`].

use ragchew::protocol::{Decode, ModeId};

/// How much decoded text a channel keeps.
///
/// A monitor left running overnight would otherwise hold every character it
/// ever decoded, per channel, and the whole set is cloned once a frame. Only
/// the tail is ever read — the text panel shows what fits on a row, and a QSO
/// copies what it wants into its own log as it arrives — so the rest is weight
/// with no reader. Generous enough that a QSO would have to go unread for tens
/// of thousands of characters to miss any: see [`Channel::text_dropped`].
const MAX_TEXT_CHARS: usize = 4096;

/// How many (time, frequency) and (time, SNR) samples a channel keeps, beyond
/// the first.
pub const MAX_HISTORY: usize = 256;

/// A tracked signal accumulating text across transmissions.
#[derive(Clone, Debug)]
pub struct Channel {
    pub id: u64,
    /// Latest carrier frequency (tracks drift).
    pub hz: f64,
    /// Carrier when the channel was first heard, kept apart from
    /// [`Channel::hz_history`] because that is trimmed and this is what drift
    /// is measured against.
    pub first_hz: f64,
    /// Recent (time_s, hz) samples, for drawing the drifting streak / leader
    /// endpoint. Bounded to [`MAX_HISTORY`]; the oldest are dropped.
    pub hz_history: Vec<(f64, f64)>,
    /// Accumulated decoded text, oldest first, bounded to [`MAX_TEXT_CHARS`].
    pub text: String,
    /// Characters dropped off the front of `text` to keep it bounded.
    ///
    /// Readers that count from the beginning of the conversation — a QSO
    /// tracking how much of this channel it has logged — add this to an index
    /// into `text` to get a position that does not shift under trimming.
    pub text_dropped: usize,
    /// Mode of the most recent decode.
    pub mode: ModeId,
    /// Where this station stopped and started again: `(character index, the
    /// time it resumed)`, indexed from the start of the conversation the way
    /// [`Channel::text_dropped`] counts, and trimmed with the text.
    ///
    /// A continuous mode carries no mark for the end of an over — there is no
    /// frame and no schedule, only text that stops coming for a while — and
    /// this is the only place that can see it, because it is the only place
    /// that sees each decode's own time. By the time a reader has the channel,
    /// the text is one string and the silences in it are gone.
    ///
    /// Empty for cycle-aligned modes: a JS8 frame is a whole utterance already.
    pub breaks: Vec<(usize, f64)>,
    /// Character count of the most recently appended chunk (for the scroll-in
    /// animation).
    pub last_chunk_chars: usize,
    /// Audio time (s) of the most recent decode.
    pub last_heard_s: f64,
    /// Audio time (s) of the first decode.
    pub first_heard_s: f64,
    /// Confidence of the most recent decode, in multiples of the protocol's
    /// noise floor — see [`ragchew::protocol::Decode::quality`].
    pub quality: f32,
    /// Best confidence seen on this channel, which is a fairer summary than the
    /// last one when a station is fading in and out.
    pub best_quality: f32,
    /// Signal-to-noise of the most recent decode, in dB against
    /// [`ragchew::dsp::SNR_REF_BW_HZ`] — see [`ragchew::protocol::Decode::snr_db`].
    ///
    /// Kept apart from `quality` because they answer different questions.
    /// `quality` is how sure the decoder is and is comparable only against its
    /// own protocol's threshold; this is how loud the station is, and means the
    /// same thing on every row of the band.
    ///
    /// `None` until a decode arrives that could be measured.
    pub snr_db: Option<f32>,
    /// Best SNR seen on this channel, for the same reason `best_quality` is
    /// kept: a station fading in and out is better summarised by its peak than
    /// by wherever it happened to be on the last block.
    pub best_snr_db: Option<f32>,
    /// Recent (time_s, snr_db) samples, for drawing how the station has been
    /// fading. Bounded to [`MAX_HISTORY`]; the oldest are dropped.
    ///
    /// Recorded here for the reason [`Channel::breaks`] is: this is the only
    /// place a decode still has its own instant. A reader sampling `snr_db`
    /// once a frame would be sampling a series it cannot see the clock of —
    /// too slow for PSK, where several characters land between frames, and
    /// aliased against the cycle on JS8.
    ///
    /// Decodes that could not be measured leave no sample rather than a zero:
    /// see [`ragchew::protocol::Decode::snr_db`]. The gaps that matter — a
    /// station that stopped transmitting — are read off the times, since a
    /// silence is a stretch with no samples in it either way.
    pub snr_history: Vec<(f64, f32)>,
    /// Number of decodes accumulated.
    pub chunks: usize,
}

impl Channel {
    /// How far the carrier has moved since the channel was first heard.
    ///
    /// Real signals wander: the off-air JS8 recording in this repository drifts
    /// about 10 Hz over two minutes.
    pub fn drift_hz(&self) -> f64 {
        self.hz - self.first_hz
    }

    /// Take the newest text on, and drop as much off the front as that costs
    /// once the channel is at its limit.
    fn push_text(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        let len = self.text.chars().count();
        if len <= MAX_TEXT_CHARS {
            return;
        }
        let drop = len - MAX_TEXT_CHARS;
        // By character, not by byte: a callsign is ASCII but decoded text need
        // not be, and splitting a multi-byte character would panic.
        let cut = self.text.char_indices().nth(drop).map_or(self.text.len(), |(i, _)| i);
        self.text.drain(..cut);
        self.text_dropped += drop;
        // A break in text nobody can read any more says nothing.
        self.breaks.retain(|&(i, _)| i >= self.text_dropped);
    }

    /// Absolute index of the next character this channel will take.
    fn next_index(&self) -> usize {
        self.text_dropped + self.text.chars().count()
    }
}

/// How long a station must go quiet before what it sends next is a new over —
/// or `None` for a mode whose decodes are whole utterances already.
///
/// Five seconds is long enough to type a word badly and short enough to be a
/// pause rather than a hesitation. Fast modes are held to it as a floor:
/// PSK63's own timing would end an over after two thirds of a second, which is
/// well inside a hunt-and-peck typist's reach for a key. Slower ones are given
/// room to be slow — Olivia sends a block every two seconds, and two of those
/// in a row are not a pause.
///
/// Cycle-aligned modes are exempt rather than merely slow. A JS8 frame is a
/// packed, self-contained message sent on a UTC boundary, so one frame is one
/// thing said. The split is the same one the decode threads make
/// ([`audio::spawn_decoder`](crate::audio::spawn_decoder)), for the same
/// reason: a schedule is either there to be used or it is not.
pub fn over_gap_s(mode: ModeId) -> Option<f64> {
    if mode.period_s().is_some() {
        return None;
    }
    Some((4.0 * mode.chunk_secs()).max(MIN_OVER_GAP_S))
}

/// Shortest silence that ends an over, however fast the mode is.
pub const MIN_OVER_GAP_S: f64 = 5.0;

/// Tracks channels as decodes arrive (in time order).
#[derive(Clone)]
pub struct ChannelSet {
    channels: Vec<Channel>,
    next_id: u64,
    /// Baseline frequency association tolerance (Hz). Modes wider than this ask
    /// for more; see [`ModeId::assoc_tol_hz`].
    pub tol_hz: f64,
}

impl ChannelSet {
    pub fn new(tol_hz: f64) -> ChannelSet {
        ChannelSet { channels: Vec::new(), next_id: 0, tol_hz }
    }

    /// Ingest a decode: append to the nearest channel within tolerance, else
    /// start a new channel. Decodes are expected in non-decreasing `time_s`.
    pub fn add(&mut self, d: Decode) {
        // Tolerance is at least what the mode itself needs: a wide-spacing mode
        // (JS8 Turbo's 20 Hz bins, an Olivia signal 1 kHz across) whose
        // per-decode frequency estimate lands a bin over still tracks as one
        // channel. Decodes only ever join a channel of the same protocol, so a
        // wide Olivia signal cannot swallow a JS8 station sitting inside it.
        let tol = self.tol_hz.max(d.mode.assoc_tol_hz());
        let mut best: Option<(usize, f64)> = None;
        for (i, ch) in self.channels.iter().enumerate() {
            if ch.mode.protocol() != d.mode.protocol() {
                continue;
            }
            let dhz = (ch.hz - d.hz).abs();
            if dhz <= tol && best.map_or(true, |(_, bd)| dhz < bd) {
                best = Some((i, dhz));
            }
        }
        match best {
            Some((i, _)) => {
                let ch = &mut self.channels[i];
                ch.hz = d.hz;
                ch.mode = d.mode;
                ch.hz_history.push((d.time_s, d.hz));
                if ch.hz_history.len() > MAX_HISTORY {
                    ch.hz_history.remove(0);
                }
                ch.last_chunk_chars = d.text.chars().count();
                // Measured here because here is the only place the times are
                // still separate: one decode, one instant. Recorded before the
                // text goes on, so the index is where the new over starts.
                if let Some(limit) = over_gap_s(d.mode) {
                    if d.time_s - ch.last_heard_s > limit {
                        ch.breaks.push((ch.next_index(), d.time_s));
                    }
                }
                ch.push_text(&d.text);
                ch.last_heard_s = d.time_s;
                ch.quality = d.quality;
                ch.best_quality = ch.best_quality.max(d.quality);
                if let Some(snr) = d.snr_db {
                    ch.snr_db = Some(snr);
                    ch.best_snr_db = Some(ch.best_snr_db.map_or(snr, |b: f32| b.max(snr)));
                    ch.snr_history.push((d.time_s, snr));
                    if ch.snr_history.len() > MAX_HISTORY {
                        ch.snr_history.remove(0);
                    }
                }
                ch.chunks += 1;
            }
            None => {
                let id = self.next_id;
                self.next_id += 1;
                self.channels.push(Channel {
                    id,
                    hz: d.hz,
                    first_hz: d.hz,
                    mode: d.mode,
                    hz_history: vec![(d.time_s, d.hz)],
                    last_chunk_chars: d.text.chars().count(),
                    text_dropped: 0,
                    text: String::new(),
                    last_heard_s: d.time_s,
                    first_heard_s: d.time_s,
                    quality: d.quality,
                    best_quality: d.quality,
                    snr_db: d.snr_db,
                    best_snr_db: d.snr_db,
                    snr_history: d.snr_db.map_or_else(Vec::new, |s| vec![(d.time_s, s)]),
                    chunks: 1,
                    breaks: Vec::new(),
                });
                // Through `push_text`, so a single outsized decode cannot put
                // a brand-new channel over the limit either.
                let ch = self.channels.last_mut().expect("just pushed");
                ch.push_text(&d.text);
            }
        }
    }

    pub fn channels(&self) -> &[Channel] {
        &self.channels
    }

    /// The channel with this id, if it is still in the set. A QSO holds an id
    /// rather than a reference because the file view rebuilds its channels from
    /// scratch on every frame.
    pub fn by_id(&self, id: u64) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    /// Build a channel set from a batch of decodes (sorted by time first).
    pub fn from_decodes(decodes: impl IntoIterator<Item = Decode>, tol_hz: f64) -> ChannelSet {
        let mut ds: Vec<Decode> = decodes.into_iter().collect();
        ds.sort_by(|a, b| a.time_s.partial_cmp(&b.time_s).unwrap());
        let mut set = ChannelSet::new(tol_hz);
        for d in ds {
            set.add(d);
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ragchew::{js8, olivia};

    fn d(hz: f64, t: f64, s: &str) -> Decode {
        Decode {
            snr_db: None,
            hz,
            time_s: t,
            text: s.to_string(),
            mode: ModeId::Js8(js8::Mode::Normal),
            quality: 1.0,
        }
    }

    /// A hovering operator wants to know how well a station is being heard and
    /// how far it has wandered, so the channel has to carry both.
    #[test]
    fn tracks_quality_and_drift() {
        let q = |hz: f64, t: f64, quality: f32| Decode {
            snr_db: None,
            hz,
            time_s: t,
            text: "x".to_string(),
            mode: ModeId::Js8(js8::Mode::Normal),
            quality,
        };
        let set = ChannelSet::from_decodes(
            [q(1000.0, 0.0, 3.0), q(1004.0, 15.0, 9.5), q(1006.0, 30.0, 5.0)],
            15.0,
        );
        let c = &set.channels()[0];
        assert_eq!(c.chunks, 3);
        assert_eq!(c.quality, 5.0, "quality is the most recent");
        assert_eq!(c.best_quality, 9.5, "best survives a fade");
        assert_eq!(c.drift_hz(), 6.0, "drift is measured from where it started");
    }

    #[test]
    fn associates_by_frequency_and_accumulates_text() {
        let set = ChannelSet::from_decodes(
            [d(1000.0, 0.0, "A"), d(1005.0, 15.0, "B"), d(1500.0, 15.0, "X")],
            15.0,
        );
        assert_eq!(set.channels().len(), 2);
        let c0 = &set.channels()[0];
        assert_eq!(c0.text, "AB");
        assert_eq!(c0.hz, 1005.0); // tracks to latest
        assert_eq!(set.channels()[1].text, "X");
    }

    #[test]
    fn drift_beyond_tolerance_splits_into_two() {
        // 30 Hz apart with a 15 Hz tolerance -> two channels
        let set = ChannelSet::from_decodes([d(1000.0, 0.0, "A"), d(1030.0, 15.0, "B")], 15.0);
        assert_eq!(set.channels().len(), 2);
    }

    /// A JS8 station transmitting inside a wide Olivia signal's bandwidth is
    /// still its own conversation.
    #[test]
    fn protocols_never_share_a_channel() {
        let ol = Decode {
            snr_db: None,
            hz: 1000.0,
            time_s: 0.0,
            text: "OL".to_string(),
            mode: ModeId::Olivia(olivia::OL_32_1000),
            quality: 4.0,
        };
        let set = ChannelSet::from_decodes([ol, d(1010.0, 1.0, "JS")], 15.0);
        assert_eq!(set.channels().len(), 2);
    }

    /// Successive Olivia blocks of one transmission join up, even though the
    /// mode's frequency estimate is only good to a tone spacing.
    #[test]
    fn olivia_blocks_accumulate_into_one_channel() {
        let block = |hz: f64, t: f64, s: &str| Decode {
            snr_db: None,
            hz,
            time_s: t,
            text: s.to_string(),
            mode: ModeId::Olivia(olivia::OL_16_500),
            quality: 5.0,
        };
        let set = ChannelSet::from_decodes(
            [block(1500.0, 0.0, "CQ C"), block(1508.0, 2.0, "Q DE"), block(1492.0, 4.0, " W1AW")],
            15.0,
        );
        assert_eq!(set.channels().len(), 1);
        assert_eq!(set.channels()[0].text, "CQ CQ DE W1AW");
    }

    /// A channel left running must not accumulate text without limit: the whole
    /// set is deep-cloned once a frame, so what a monitor holds after a night on
    /// the air is what it pays for on every one of them.
    #[test]
    fn a_channels_text_stops_growing() {
        let mut set = ChannelSet::new(15.0);
        let chunk = "CQ CQ DE K2N FN20 ";
        let n = 4000;
        for i in 0..n {
            set.add(Decode {
                snr_db: None,
                hz: 1000.0,
                time_s: i as f64,
                text: chunk.to_string(),
                mode: ModeId::Js8(js8::Mode::Normal),
                quality: 4.0,
            });
        }
        let ch = &set.channels()[0];
        let produced = n * chunk.chars().count();
        assert_eq!(ch.text.chars().count(), MAX_TEXT_CHARS, "text is not at its bound");
        assert_eq!(
            ch.text_dropped + ch.text.chars().count(),
            produced,
            "what was dropped plus what is kept must be everything decoded"
        );
        // The tail is what survives, and it is intact.
        assert!(ch.text.ends_with(chunk), "the newest text was trimmed instead of the oldest");
        assert!(ch.hz_history.len() <= MAX_HISTORY, "history is not bounded");
    }

    /// Drift is measured from where a station was first heard, which outlives
    /// the frequency history being trimmed.
    #[test]
    fn drift_survives_a_trimmed_history() {
        let mut set = ChannelSet::new(15.0);
        for i in 0..(MAX_HISTORY * 3) {
            set.add(Decode {
                snr_db: None,
                // creeping up 0.01 Hz a decode, well inside the tolerance
                hz: 1000.0 + i as f64 * 0.01,
                time_s: i as f64,
                text: "x".to_string(),
                mode: ModeId::Js8(js8::Mode::Normal),
                quality: 4.0,
            });
        }
        let ch = &set.channels()[0];
        let expected = (MAX_HISTORY * 3 - 1) as f64 * 0.01;
        assert!(
            (ch.drift_hz() - expected).abs() < 1e-6,
            "drift {} should be {expected} from where it started",
            ch.drift_hz()
        );
    }

    /// The SNR series is recorded against each decode's own time, keeps only
    /// what was measured, and is bounded like the frequency history beside it.
    ///
    /// The gap in the middle is what a station going quiet leaves behind: no
    /// samples at all, rather than samples at nothing. A reader that drew a
    /// zero there would be drawing a collapse into the noise that never
    /// happened — the station simply stopped talking.
    #[test]
    fn snr_history_records_only_what_was_measured() {
        let mut set = ChannelSet::new(15.0);
        let snr = |i: usize| match i {
            10..=20 => None,
            _ => Some(-6.0 - i as f32 * 0.1),
        };
        for i in 0..(MAX_HISTORY * 2) {
            set.add(Decode {
                snr_db: snr(i),
                hz: 1000.0,
                time_s: i as f64 * 15.0,
                text: "x".to_string(),
                mode: ModeId::Js8(js8::Mode::Normal),
                quality: 4.0,
            });
        }
        let ch = &set.channels()[0];
        assert_eq!(ch.snr_history.len(), MAX_HISTORY, "the series is not at its bound");
        // The newest sample is the one kept, and it is the one reported.
        let &(t, db) = ch.snr_history.last().expect("something was recorded");
        assert_eq!(t, (MAX_HISTORY * 2 - 1) as f64 * 15.0, "the wrong end was trimmed");
        assert_eq!(Some(db), ch.snr_db);
        // Times are strictly increasing and each carries its own decode's
        // value — no interpolation, no filler for the unmeasured ones.
        assert!(
            ch.snr_history.windows(2).all(|w| w[1].0 > w[0].0),
            "samples are out of order"
        );
        for &(t, db) in &ch.snr_history {
            let i = (t / 15.0).round() as usize;
            assert_eq!(Some(db), snr(i), "sample at {t} s does not match its decode");
        }
    }

    /// A channel that has never been measured has no series at all, rather than
    /// a series of nothing.
    #[test]
    fn an_unmeasured_channel_has_no_series() {
        let mut set = ChannelSet::new(15.0);
        set.add(Decode {
            snr_db: None,
            hz: 1000.0,
            time_s: 0.0,
            text: "x".to_string(),
            mode: ModeId::Js8(js8::Mode::Normal),
            quality: 4.0,
        });
        assert!(set.channels()[0].snr_history.is_empty());
    }

    /// Text longer than the bound in a single decode still leaves the channel
    /// at its limit rather than over it.
    #[test]
    fn one_outsized_decode_does_not_break_the_bound() {
        let mut set = ChannelSet::new(15.0);
        set.add(Decode {
            snr_db: None,
            hz: 1000.0,
            time_s: 0.0,
            text: "y".repeat(MAX_TEXT_CHARS * 2),
            mode: ModeId::Js8(js8::Mode::Normal),
            quality: 4.0,
        });
        assert_eq!(set.channels()[0].text.chars().count(), MAX_TEXT_CHARS);
    }

    /// Where a station stopped and started again is recorded as it arrives,
    /// because it cannot be recovered afterwards: by the time a reader has the
    /// channel, the silence is just the space between two characters in a
    /// string.
    #[test]
    fn a_pause_in_a_continuous_mode_is_marked() {
        let psk = |t: f64, c: char| Decode {
            snr_db: None,
            hz: 620.0,
            time_s: t,
            text: c.to_string(),
            mode: ModeId::Psk(ragchew::psk::PSK31),
            quality: 4.0,
        };
        let step = ModeId::Psk(ragchew::psk::PSK31).chunk_secs();
        let mut set = ChannelSet::new(15.0);

        let mut t = 0.0;
        for c in "hi om ".chars() {
            set.add(psk(t, c));
            t += step;
        }
        assert!(set.channels()[0].breaks.is_empty(), "text arriving steadily is one over");

        // The station stops, then comes back.
        let resumed = t + MIN_OVER_GAP_S + 1.0;
        t = resumed;
        for c in "back ".chars() {
            set.add(psk(t, c));
            t += step;
        }
        let ch = &set.channels()[0];
        assert_eq!(ch.breaks.len(), 1, "breaks: {:?}", ch.breaks);
        assert_eq!(ch.breaks[0].0, "hi om ".chars().count(), "marked at the wrong character");
        assert!((ch.breaks[0].1 - resumed).abs() < 1e-9, "marked at the wrong time");
        assert_eq!(ch.text, "hi om back ", "the text itself is untouched");
    }

    /// A cycle-aligned mode records none: every frame is a whole utterance, so
    /// there is nothing for a gap to tell anyone.
    #[test]
    fn a_cycle_aligned_mode_marks_nothing() {
        let mut set = ChannelSet::new(15.0);
        for i in 0..4 {
            set.add(d(1000.0, i as f64 * 15.0, "DE K2N "));
        }
        assert!(set.channels()[0].breaks.is_empty());
        assert!(over_gap_s(ModeId::Js8(js8::Mode::Normal)).is_none());
    }

    /// Breaks are indexed from the start of the conversation, like the text
    /// they point into, and go when the text they point at does.
    #[test]
    fn breaks_are_trimmed_with_the_text() {
        let olivia = |t: f64, s: &str| Decode {
            snr_db: None,
            hz: 1000.0,
            time_s: t,
            text: s.to_string(),
            mode: ModeId::Olivia(olivia::OL_8_250),
            quality: 4.0,
        };
        let mut set = ChannelSet::new(15.0);
        let mut t = 0.0;
        // Pause between every block, so every one of them starts an over.
        for _ in 0..(MAX_TEXT_CHARS / 8 + 40) {
            set.add(olivia(t, "12345678"));
            t += 60.0;
        }
        let ch = &set.channels()[0];
        assert!(ch.text_dropped > 0, "the channel never trimmed; this proves nothing");
        assert!(
            ch.breaks.iter().all(|&(i, _)| i >= ch.text_dropped),
            "a break points into text that is gone"
        );
        assert!(!ch.breaks.is_empty(), "every break was dropped");
    }
}
