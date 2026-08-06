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

/// How finely a channel times the text it took on: a new mark is written only
/// once this much has passed since the last one.
///
/// A second is far below any silence worth noticing — see [`MIN_OVER_GAP_S`] —
/// and far above the resolution anything reading these needs. What it buys is
/// a bound on the fastest mode: PSK decodes a character at a time, and one mark
/// per character would be sixteen bytes of bookkeeping for every byte of text.
pub const ARRIVAL_QUANTUM_S: f64 = 1.0;

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
    /// A cycle-aligned mode gets them too, and for the same purpose, though it
    /// arrives at them differently: JS8 frames carry a flag for the end of an
    /// over, so most breaks here are the station's own word for it rather than
    /// a timing. See [`Channel::over_ended`].
    pub breaks: Vec<(usize, f64)>,
    /// Where each frame of a cycle-aligned mode begins: `(character index, the
    /// time it started)`, indexed and trimmed like [`Channel::breaks`].
    ///
    /// A JS8 frame is a fixed-size packet, and a sentence is several of them a
    /// cycle apart. Joined into a paragraph the seams are invisible, which is
    /// how it should read — but the seams are also where the timing is, and on
    /// this mode the timing is diagnostic: a station that took four cycles to
    /// say six words was being repeated by its own software. So they are kept,
    /// for the log to mark rather than to break on.
    ///
    /// Empty for the continuous modes, where a "frame" is a character or a
    /// two-second block and marking them would be marking nothing.
    pub frames: Vec<(usize, f64)>,
    /// When each stretch of the text arrived: `(character index, the time it
    /// was decoded)`, indexed and trimmed like [`Channel::breaks`], and never
    /// written more often than [`ARRIVAL_QUANTUM_S`].
    ///
    /// This is what lets old text be aged off a row a piece at a time. A reader
    /// holding the channel has one string and one [`Channel::last_heard_s`],
    /// which says when the *end* of it arrived and nothing about the rest: a
    /// station that talked for a minute an hour ago and has been quiet since
    /// would have to go all at once or not at all.
    ///
    /// Coalescing cannot span a pause, which is the property that matters: a
    /// mark is written whenever a decode lands a quantum or more after the last
    /// one, so text from after a silence never inherits the age of the text
    /// before it.
    ///
    /// On a cycle-aligned mode this holds the same indices as
    /// [`Channel::frames`], frames being further apart than the quantum, but it
    /// is not the same fact: that one is where the seams are and exists only
    /// where there are packets, this one is how old the text is and exists on
    /// every mode.
    pub arrivals: Vec<(usize, f64)>,
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
    /// Whether the last decode said it finished what the station was saying.
    ///
    /// The next decode to arrive therefore starts a new over, whatever the
    /// clock says — see [`ragchew::protocol::Decode::ends_over`]. Held rather
    /// than acted on at once because an over ends where the *following* frame
    /// begins, and that index does not exist until the following frame does.
    pub over_ended: bool,
    /// Who is talking, if a frame in this over said so.
    ///
    /// Two stations working each other on JS8 usually share one offset — the
    /// answering station replies where it heard the call — so a channel is a
    /// frequency, not a speaker. Joining frames into a paragraph without
    /// watching this would put both halves of the exchange in one paragraph.
    pub over_sender: Option<String>,
}

/// The callsign a JS8 frame names as its sender, if it names one.
///
/// Two of the four frame types this crate parses carry one where a reader can
/// see it: a heartbeat or CQ renders as `KN4CRD: CQ CQ CQ EM73`, and a directed
/// message as `[KN4CRD JY1 SNR -12]`. Free text and dense frames carry no
/// callsign at all, which is why this returns an option and why nothing may
/// conclude from `None` that the speaker changed.
///
/// The shape test is deliberately loose — something with both a letter and a
/// digit in it — rather than the ITU pattern [`crate::qso::callsign_in`]
/// applies. It is guarding against a free-text frame that happens to open with
/// a word and a colon, and the cost of being wrong either way is small: a
/// missed split reads as one long over, and a spurious one reads as the log
/// already read before any of this.
pub fn sender_in(text: &str) -> Option<&str> {
    let token = match text.strip_prefix('[') {
        Some(rest) => rest.split([' ', ']']).next()?,
        None => text.split_whitespace().next()?.strip_suffix(':')?,
    };
    let call_ish = (3..=11).contains(&token.len())
        && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '/')
        && token.chars().any(|c| c.is_ascii_digit())
        && token.chars().any(|c| c.is_ascii_alphabetic());
    call_ish.then_some(token)
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
        if len > MAX_TEXT_CHARS {
            self.drop_front(len - MAX_TEXT_CHARS);
        }
    }

    /// Drop `n` characters off the front of the text, and with them everything
    /// indexed into what has gone.
    fn drop_front(&mut self, n: usize) {
        let n = n.min(self.text.chars().count());
        // By character, not by byte: a callsign is ASCII but decoded text need
        // not be, and splitting a multi-byte character would panic.
        let cut = self.text.char_indices().nth(n).map_or(self.text.len(), |(i, _)| i);
        self.text.drain(..cut);
        self.text_dropped += n;
        // A break in text nobody can read any more says nothing, and neither
        // does a seam between two frames that have both gone.
        self.breaks.retain(|&(i, _)| i >= self.text_dropped);
        self.frames.retain(|&(i, _)| i >= self.text_dropped);
        // Arrivals are the exception: the cut usually lands *inside* the
        // stretch the oldest surviving mark covers, and text with no mark at or
        // before it has no age at all — it would read as older than anything on
        // record and be forgotten on the next pass, which is text disappearing
        // for want of bookkeeping. So that mark is carried up to the new front
        // rather than dropped with the characters it used to point at.
        let head = self.arrivals.iter().rev().find(|&&(i, _)| i <= self.text_dropped);
        let head = head.map(|&(_, t)| (self.text_dropped, t));
        self.arrivals.retain(|&(i, _)| i > self.text_dropped);
        if let (Some(mark), false) = (head, self.text.is_empty()) {
            self.arrivals.insert(0, mark);
        }
    }

    /// Absolute index of the next character this channel will take.
    fn next_index(&self) -> usize {
        self.text_dropped + self.text.chars().count()
    }

    /// How the tail of the text divides by age: `(how many characters, when
    /// they arrived)`, oldest first, covering the last `n` characters — or all
    /// of them, if the channel holds fewer.
    ///
    /// The time is `None` where the text is older than the oldest arrival still
    /// on record. That can only be the very front of the buffer, and it is by
    /// construction the stalest text there is.
    pub fn tail_ages(&self, n: usize) -> Vec<(usize, Option<f64>)> {
        let have = self.next_index();
        let held = self.text.chars().count();
        let mut out = Vec::new();
        let mut i = have - held.min(n);
        while i < have {
            // The mark covering `i`, and the index at which the next one takes
            // over from it.
            let k = self.arrivals.partition_point(|&(j, _)| j <= i);
            let at = k.checked_sub(1).map(|k| self.arrivals[k].1);
            let end = self.arrivals.get(k).map_or(have, |&(j, _)| j.min(have));
            out.push((end - i, at));
            i = end;
        }
        out
    }

    /// Forget text that arrived before `at`.
    ///
    /// Whole marks at a time, so up to [`ARRIVAL_QUANTUM_S`] of text either side
    /// of the cut goes with the stretch it was recorded in. Nothing reading this
    /// is aiming at a character — a timeout is measured in minutes and a clear
    /// is aimed at everything on a row — so the extra precision would cost more
    /// bookkeeping than it could ever be worth.
    pub fn forget_before(&mut self, at: f64) {
        let k = self.arrivals.partition_point(|&(_, t)| t < at);
        // Nothing on record is new enough, so all of it goes — including any
        // text past the last mark, which arrived inside that mark's stretch and
        // is therefore no newer than a quantum after it.
        let cut = self.arrivals.get(k).map_or(self.next_index(), |&(i, _)| i);
        if cut > self.text_dropped {
            self.drop_front(cut - self.text_dropped);
        }
    }
}

/// How long a station must go quiet before what it sends next is a new over.
///
/// Five seconds is long enough to type a word badly and short enough to be a
/// pause rather than a hesitation. Fast modes are held to it as a floor:
/// PSK63's own timing would end an over after two thirds of a second, which is
/// well inside a hunt-and-peck typist's reach for a key. Slower ones are given
/// room to be slow — Olivia sends a block every two seconds, and two of those
/// in a row are not a pause.
///
/// A cycle-aligned mode is measured against its *period* rather than the length
/// of its frame, and given only half a cycle of slack: consecutive frames start
/// exactly one period apart, and JS8 Turbo spends under four seconds of its six
/// transmitting, so anything looser than this would count a frame that failed
/// to decode as continued speech. This is the weakest of the three things that
/// end an over there — the station's own end-of-over flag is the first, and a
/// change of sender the second — and it exists for the case those cannot cover:
/// a final frame lost to the noise, which in this mode is an ordinary evening.
pub fn over_gap_s(mode: ModeId) -> f64 {
    match mode.period_s() {
        Some(p) => p as f64 * 1.5,
        None => (4.0 * mode.chunk_secs()).max(MIN_OVER_GAP_S),
    }
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
                // Decided here because here is the only place the times are
                // still separate: one decode, one instant. Recorded before the
                // text goes on, so the index is where the new over starts.
                let sender = sender_in(&d.text).map(str::to_string);
                let changed_hands = match (&ch.over_sender, &sender) {
                    (Some(was), Some(now)) => was != now,
                    // A frame with no callsign in it — free text, which is most
                    // of a JS8 ragchew — is nobody new. Claiming otherwise would
                    // split every over that started with a directed frame and
                    // carried on in free text, which is the shape of an ordinary
                    // reply.
                    _ => false,
                };
                let quiet = d.time_s - ch.last_heard_s > over_gap_s(d.mode);
                if ch.over_ended || changed_hands || quiet {
                    ch.breaks.push((ch.next_index(), d.time_s));
                    ch.over_sender = sender;
                } else if ch.over_sender.is_none() {
                    // An over that began without a name can still acquire one:
                    // free text followed by a directed frame is one station, and
                    // the second frame is the one that says which.
                    ch.over_sender = sender;
                }
                if d.mode.period_s().is_some() {
                    ch.frames.push((ch.next_index(), d.time_s));
                }
                // Written before the text goes on, like the marks above it, so
                // the index is where this decode's text starts.
                if ch.arrivals.last().is_none_or(|&(_, t)| d.time_s - t >= ARRIVAL_QUANTUM_S) {
                    ch.arrivals.push((ch.next_index(), d.time_s));
                }
                ch.over_ended = d.ends_over;
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
                    frames: if d.mode.period_s().is_some() { vec![(0, d.time_s)] } else { Vec::new() },
                    arrivals: vec![(0, d.time_s)],
                    over_ended: d.ends_over,
                    over_sender: sender_in(&d.text).map(str::to_string),
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

    /// Forget text that arrived before the instant `cut` names for the channel
    /// it is asked about, and with it any channel left with nothing to show.
    ///
    /// A channel is dropped rather than kept empty because an empty row is not
    /// a station: there is no text to read and its leader would point at a
    /// signal the waterfall has long since carried away. `-∞` is "keep
    /// everything", and is the only way a channel that has never said anything
    /// survives this.
    pub fn forget(&mut self, cut: impl Fn(&Channel) -> f64) {
        self.channels.retain_mut(|ch| {
            let at = cut(ch);
            if !at.is_finite() {
                return true;
            }
            ch.forget_before(at);
            !ch.text.is_empty()
        });
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
            ends_over: false,
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
            ends_over: false,
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
            ends_over: false,
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
            ends_over: false,
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
                ends_over: false,
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
                ends_over: false,
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
                ends_over: false,
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
            ends_over: false,
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
            ends_over: false,
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
            ends_over: false,
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

    /// A cycle-aligned mode runs its frames together and marks each one, so a
    /// reader can join them into a paragraph and still see how it was carried.
    #[test]
    fn consecutive_frames_are_one_over_with_the_seams_kept() {
        let mut set = ChannelSet::new(15.0);
        let period = ModeId::Js8(js8::Mode::Normal).period_s().expect("cycle-aligned") as f64;
        for i in 0..4 {
            set.add(d(1000.0, i as f64 * period, "DE K2N "));
        }
        let ch = &set.channels()[0];
        assert!(ch.breaks.is_empty(), "an unbroken over was split");
        assert_eq!(ch.frames.len(), 4, "four frames left {} marks", ch.frames.len());
        assert_eq!(ch.frames[0], (0, 0.0));
        assert_eq!(ch.frames[1].0, "DE K2N ".chars().count());
        assert_eq!(ch.frames[3].1, 3.0 * period);
    }

    /// The station's own flag ends the over, whatever the clock says — and the
    /// break lands where the next frame starts, so the frame that carried the
    /// flag stays in the over it finished.
    #[test]
    fn an_end_of_over_flag_breaks_where_the_next_frame_starts() {
        let mut set = ChannelSet::new(15.0);
        let mut first = d(1000.0, 0.0, "DE K2N ");
        first.ends_over = true;
        set.add(first);
        set.add(d(1000.0, 15.0, "HI OM "));
        let ch = &set.channels()[0];
        assert_eq!(ch.breaks.len(), 1, "the flag was not acted on");
        assert_eq!(ch.breaks[0], ("DE K2N ".chars().count(), 15.0));
    }

    /// Two stations on one carrier are two overs, which is the ordinary shape
    /// of a JS8 QSO: the answering station replies on the offset it heard.
    #[test]
    fn a_change_of_sender_breaks_the_over() {
        let mut set = ChannelSet::new(15.0);
        set.add(d(1000.0, 0.0, "K2N: CQ CQ CQ "));
        // Free text names nobody, and nobody is not somebody else.
        set.add(d(1000.0, 15.0, "FN20 "));
        assert!(set.channels()[0].breaks.is_empty(), "free text was read as a new speaker");
        set.add(d(1000.0, 30.0, "[W1AW K2N SNR -12]"));
        assert_eq!(set.channels()[0].breaks.len(), 1, "the second station was not separated");
    }

    /// The two shapes a JS8 frame names its sender in, and the ones that name
    /// nobody.
    #[test]
    fn a_sender_is_read_only_where_a_frame_gives_one() {
        assert_eq!(sender_in("KN4CRD: CQ CQ CQ EM73"), Some("KN4CRD"));
        assert_eq!(sender_in("[KN4CRD JY1 SNR -12]"), Some("KN4CRD"));
        assert_eq!(sender_in("HELLO OM HW CPY"), None, "free text named a sender");
        assert_eq!(sender_in("TNX: FB"), None, "a word and a colon is not a call");
        assert_eq!(sender_in(""), None);
    }

    /// Text from before a cut is forgotten, and what is left keeps its place in
    /// the conversation: everything indexed into a channel counts from the
    /// start of it, so a reader that has already logged some of this must not
    /// find the rest shifted under it.
    #[test]
    fn text_from_before_a_cut_is_forgotten() {
        let mut set = ChannelSet::new(15.0);
        for i in 0..4 {
            set.add(d(1000.0, i as f64 * 15.0, "DE K2N "));
        }
        set.forget(|_| 20.0);
        let ch = &set.channels()[0];
        assert_eq!(ch.text, "DE K2N DE K2N ", "the wrong end was forgotten");
        assert_eq!(ch.text_dropped, 14, "what is left is at the wrong index");
        for (what, marks) in [("frame", &ch.frames), ("arrival", &ch.arrivals)] {
            assert!(
                marks.iter().all(|&(i, _)| i >= ch.text_dropped),
                "an {what} mark points into text that is gone: {marks:?}"
            );
        }
        assert_eq!(ch.arrivals.first().map(|&(_, t)| t), Some(30.0), "{:?}", ch.arrivals);
    }

    /// A channel with nothing left to show is not a channel any more: an empty
    /// row is a leader pointing at a signal that has scrolled away.
    #[test]
    fn a_channel_forgotten_entirely_leaves_the_set() {
        let mut set = ChannelSet::from_decodes([d(1000.0, 0.0, "A"), d(1500.0, 30.0, "B")], 15.0);
        set.forget(|_| 15.0);
        assert_eq!(set.channels().len(), 1, "the quiet station is still here");
        assert_eq!(set.channels()[0].text, "B");
    }

    /// No cut is no change — not even to a channel that has never managed to
    /// say anything.
    #[test]
    fn nothing_is_forgotten_without_a_cut() {
        let mut set = ChannelSet::from_decodes([d(1000.0, 0.0, ""), d(1500.0, 0.0, "B")], 15.0);
        set.forget(|_| f64::NEG_INFINITY);
        assert_eq!(set.channels().len(), 2);
        assert_eq!(set.channels()[1].text, "B", "text went missing with no cut set");
    }

    /// Text that arrived after a silence is never aged with the text before it.
    ///
    /// This is the property the arrival marks exist for. On a per-character
    /// mode their times are coalesced, and if the coalescing could span a pause
    /// then a station that came back after ten minutes would have what it just
    /// said forgotten along with what it said before.
    #[test]
    fn a_silence_always_gets_its_own_mark() {
        let psk = |t: f64, c: char| Decode {
            snr_db: None,
            hz: 620.0,
            time_s: t,
            text: c.to_string(),
            ends_over: false,
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
        let resumed = t + 600.0;
        t = resumed;
        for c in "back".chars() {
            set.add(psk(t, c));
            t += step;
        }
        // Forget anything older than a minute before the station came back.
        set.forget(|_| resumed - 60.0);
        assert_eq!(set.channels()[0].text, "back", "the new over was aged with the old one");
    }

    /// The marks stay bounded on a mode that decodes one character at a time:
    /// no closer together than the quantum, whatever the character rate is.
    #[test]
    fn arrival_marks_are_no_finer_than_the_quantum() {
        let step = ModeId::Psk(ragchew::psk::PSK31).chunk_secs();
        let mut set = ChannelSet::new(15.0);
        for i in 0..600 {
            set.add(Decode {
                snr_db: None,
                hz: 620.0,
                time_s: i as f64 * step,
                text: "x".to_string(),
                ends_over: false,
                mode: ModeId::Psk(ragchew::psk::PSK31),
                quality: 4.0,
            });
        }
        let ch = &set.channels()[0];
        assert!(
            ch.arrivals.windows(2).all(|w| w[1].1 - w[0].1 >= ARRIVAL_QUANTUM_S),
            "two marks landed inside one quantum: {:?}",
            ch.arrivals
        );
        // PSK31 carries about five characters a second, so one mark a second is
        // a fifth of what a mark per decode would have cost.
        assert!(
            ch.arrivals.len() * 3 < ch.text.chars().count(),
            "{} marks for {} characters",
            ch.arrivals.len(),
            ch.text.chars().count()
        );
    }

    /// The tail divides exactly: every character a row would show is accounted
    /// for once, and the newest stretch is the last one.
    #[test]
    fn the_tail_divides_by_age_without_gaps() {
        let mut set = ChannelSet::new(15.0);
        for i in 0..4 {
            set.add(d(1000.0, i as f64 * 15.0, "DE K2N "));
        }
        let ch = &set.channels()[0];
        let held = ch.text.chars().count();

        let runs = ch.tail_ages(10);
        assert_eq!(runs.iter().map(|&(n, _)| n).sum::<usize>(), 10, "{runs:?}");
        assert_eq!(runs.last().and_then(|&(_, t)| t), Some(45.0), "the last stretch is the newest");

        let all = ch.tail_ages(10_000);
        assert_eq!(all.iter().map(|&(n, _)| n).sum::<usize>(), held, "asking for more lost some");
        assert_eq!(all.len(), 4, "four frames should be four stretches: {all:?}");
        assert_eq!(all[0], (7, Some(0.0)));
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
            ends_over: false,
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
