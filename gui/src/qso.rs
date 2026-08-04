//! Conversations: one [`Qso`] per station worked, each with its own log, its
//! own half-written reply, and the notes an operator fills in as they come over
//! the air.
//!
//! A QSO is opened from a tracked channel and then *follows* it: whatever that
//! channel decodes next is appended to the log as received. That binding is by
//! channel id rather than by reference, because the file view rebuilds its
//! channel set from the decodes on every frame — see [`QsoSet::absorb`].
//!
//! Nothing here draws anything or touches the radio; the panel is in `app`, the
//! modulator in `tx`.

use std::time::{Duration, SystemTime};

use ragchew::protocol::ModeId;

use crate::channels::{Channel, ChannelSet};

/// Which way a log entry went.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    /// Decoded off the air.
    Rx,
    /// Sent, or queued to send, by us.
    Tx,
}

impl Dir {
    /// Two characters for the log's direction column.
    pub fn tag(self) -> &'static str {
        match self {
            Dir::Rx => "RX",
            Dir::Tx => "TX",
        }
    }
}

/// One line of the conversation.
#[derive(Clone, Debug)]
pub struct Entry {
    /// Audio-clock time this *began*, in the same seconds the playback clock
    /// counts. The log shows it as an offset from the start of the QSO, which
    /// is what tells you the rhythm of an exchange.
    ///
    /// An entry that grew over several decodes keeps the time of the first, so
    /// the column reads as when the station started saying this rather than
    /// when it stopped.
    pub at_s: f64,
    pub dir: Dir,
    pub text: String,
    /// Seams inside `text`: `(character offset, the time the frame there
    /// started)`, for an entry built out of several packets.
    ///
    /// One JS8 frame is a packet, not a sentence, so the log joins consecutive
    /// frames of an over into one entry and marks where they met. The offsets
    /// are into this entry's own text, and never zero — the start of an entry
    /// needs no mark, since the entry is one.
    ///
    /// Kept out of the text itself so that the log, the traffic CSV and
    /// anything copied out of the panel stay what the station said. This is how
    /// it was *carried*, which is the interface's business alone.
    pub marks: Vec<(usize, f64)>,
}

/// How many (time, SNR) samples a QSO keeps.
///
/// Twice what a channel holds, because this is the record of one conversation
/// rather than a rolling window over the band, and a long ragchew on PSK — a
/// sample per character — would otherwise lose its first half hour. At the far
/// end it is still bounded: the set is cloned once a frame like everything else
/// here.
pub const MAX_SNR_POINTS: usize = 512;

/// The signal report this mode's exchange is made of, for a station measured
/// at `snr_db` — or `None` where a measurement does not determine one.
///
/// JS8 exchanges the SNR itself, in whole dB with a sign, and that is precisely
/// what [`ragchew::protocol::Decode::snr_db`] measures. Filling it in is
/// transcription, not invention, which is the whole test for whether this
/// should happen at all.
///
/// The keyboard modes get nothing, and this is settled rather than pending.
/// RST and RSQ are not determined by an SNR: readability and tone quality are
/// separate judgements, and the strength digit is conventionally read off a
/// meter rather than off the audio. An operator sending a courtesy 599 is
/// making a choice, and a log has to record the choice they made rather than a
/// number this crate invented for them. The measured figure is on the panel
/// beside the field for them to use as they see fit.
///
/// So do not add a derived RSQ here. It was considered and ruled out.
pub fn suggested_report(mode: ModeId, snr_db: f32) -> Option<String> {
    match mode {
        // Two digits and a sign, as JS8Call and WSJT-X write it: "-05", "+13".
        ModeId::Js8(_) => Some(format!("{:+03}", snr_db.round() as i32)),
        ModeId::Olivia(_) | ModeId::Psk(_) => None,
    }
}

/// The part of a received entry whose last word is certainly finished.
///
/// A continuous mode arrives a character at a time, so the token on the end of
/// an entry is usually still being sent — and a partial call sign is call
/// *shaped* long before it is right: `kb9p` passes every test `kb9psk` does,
/// and a QSO that grabbed it would carry the wrong call into the log. So
/// everything past the last space is left until the next character settles it.
///
/// A frame from a cycle-aligned mode arrives whole and is returned whole; its
/// last token is finished by definition, and trimming it would lose the call in
/// a frame that ends on one.
fn settled_text(text: &str, mode: ModeId) -> &str {
    if mode.period_s().is_some() {
        return text;
    }
    match text.char_indices().rfind(|(_, c)| c.is_whitespace()) {
        Some((i, c)) => &text[..i + c.len_utf8()],
        None => "",
    }
}

/// Text on the air.
///
/// It stays in the composer while it goes out, struck through as far as it has
/// got, and is cleared when the transmission ends — so the operator can see
/// what is leaving and how much of it is left.
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub text: String,
    /// UTC seconds the burst begins and ends. Before it begins — waiting for
    /// the next cycle boundary, which is most of a JS8 send — nothing is
    /// struck through yet, which is itself worth seeing.
    pub start_unix: f64,
    pub end_unix: f64,
}

impl Outgoing {
    /// How many characters have gone out by `now_unix`.
    ///
    /// From elapsed time against the length of the burst. For a character
    /// stream like Olivia that is very nearly the truth; for a JS8 frame,
    /// where the whole message is packed into one fixed-length burst, it is a
    /// progress bar wearing the text's clothes.
    pub fn sent_chars(&self, now_unix: f64) -> usize {
        let span = self.end_unix - self.start_unix;
        if span <= 0.0 || !span.is_finite() {
            return self.text.chars().count();
        }
        let done = ((now_unix - self.start_unix) / span).clamp(0.0, 1.0);
        (done * self.text.chars().count() as f64).round() as usize
    }

    pub fn finished(&self, now_unix: f64) -> bool {
        now_unix >= self.end_unix
    }
}

/// A conversation with one station.
#[derive(Clone, Debug)]
pub struct Qso {
    pub id: u64,
    /// The other station's call, read out of the text where it can be — see
    /// [`callsign_in`] — and editable, because a heuristic over decoded text
    /// gets it wrong sometimes and the operator is the authority.
    pub call: String,
    pub name: String,
    pub qth: String,
    pub grid: String,
    pub rst_sent: String,
    pub rst_rcvd: String,
    /// Whether `rst_sent` is still this crate's to keep current.
    ///
    /// Set false the moment the operator types in the field, and never set back
    /// — what goes in the log has to be what they chose to send. Until then the
    /// report tracks the station, so a QSO opened on a fade and answered ten
    /// minutes later reports what was last copied rather than what was first.
    pub rst_sent_auto: bool,
    /// Channel being followed, if this QSO was opened from one.
    pub channel: Option<u64>,
    /// Carrier and mode: taken from the bound channel as it drifts, or set by
    /// hand on a QSO opened blank.
    pub hz: f64,
    pub mode: ModeId,
    /// Live from the bound channel: how far the carrier has wandered, and the
    /// confidence of the last decode in multiples of the noise floor.
    pub drift_hz: f64,
    pub quality: f32,
    /// Live from the bound channel: how loud the station is, in dB against
    /// [`ragchew::dsp::SNR_REF_BW_HZ`], and the best it has been. Unlike
    /// `quality` this is a measurement and means the same thing in every mode,
    /// which is what makes it worth offering as a signal report.
    pub snr_db: Option<f32>,
    pub best_snr_db: Option<f32>,
    /// Live from the bound channel: (time_s, snr_db) for every decode measured
    /// since the QSO opened, oldest first, bounded to [`MAX_SNR_POINTS`].
    ///
    /// Kept here rather than read off the channel because a QSO outlives what
    /// the channel holds — [`Channel::snr_history`] is trimmed to its own
    /// bound, and a conversation is entitled to the shape of the whole path,
    /// not to the last few minutes of it.
    pub snr_history: Vec<(f64, f32)>,
    /// Audio-clock time the station was first heard (or the tab was opened, for
    /// a blank one), and when it was last heard from.
    pub started_s: f64,
    pub last_s: f64,
    /// Wall-clock instant of `started_s`, for the log: a contact is logged in
    /// UTC, and the audio clock counts from the start of a capture or a file.
    ///
    /// Live, the audio clock runs at wall-clock rate, so backdating by the age
    /// of the channel puts this at the moment the station was actually first
    /// heard. Reviewing a recording there is nothing to be faithful to — a WAV
    /// carries no start time — so a QSO opened there is stamped with the time
    /// it was opened.
    pub started_utc: SystemTime,
    pub log: Vec<Entry>,
    /// The reply being composed. Per QSO, so switching tabs and coming back
    /// finds it as you left it.
    pub draft: String,
    /// What is on the air from this QSO and what is queued behind it, oldest
    /// first. Kept until the last of it has gone, so the operator can watch it
    /// leave while typing the next one.
    pub outgoing: Vec<Outgoing>,
    /// Set when a transmission ends, so the composer takes the keyboard back
    /// the moment it returns: the operator's hands are already there.
    pub want_focus: bool,
    /// How many characters the bound channel has ever produced that are already
    /// in the log — counted from the start of the conversation, not from the
    /// start of what the channel still holds, since that is trimmed.
    absorbed: usize,
}

impl Qso {
    /// What the tab says: the call once it is known, and the carrier until then
    /// — never an empty tab, and never a bare number once there is a call.
    pub fn label(&self) -> String {
        if self.call.trim().is_empty() {
            format!("{:.0} Hz", self.hz)
        } else {
            self.call.trim().to_string()
        }
    }

    /// How long the QSO has been running at audio-clock time `now_s`.
    pub fn elapsed_s(&self, now_s: f64) -> f64 {
        (now_s - self.started_s).max(0.0)
    }

    /// Add something we sent (or queued to send).
    pub fn push_tx(&mut self, text: &str, at_s: f64) {
        self.log.push(Entry {
            at_s,
            dir: Dir::Tx,
            text: text.trim().to_string(),
            marks: Vec::new(),
        });
        self.last_s = at_s.max(self.last_s);
    }

    /// Add received characters: onto the entry already open, or as an entry of
    /// their own when `began` says they start an over.
    ///
    /// Our own transmission always ends the entry it interrupts, whatever the
    /// timing — the last thing in the log being a TX is exactly what "the other
    /// station started a new over" looks like, so nothing joins across it.
    ///
    /// `frames` are the frame starts of the whole stretch being absorbed, at
    /// their conversation indices, and `base` is the conversation index of
    /// `chars[0]` — which is what turns one into the other. A frame starting
    /// exactly where the entry does is not a seam inside it: the entry's own
    /// beginning is already the mark.
    fn push_rx(
        &mut self,
        chars: &[char],
        began: Option<f64>,
        now_s: f64,
        frames: &[(usize, f64)],
        base: usize,
    ) {
        if chars.is_empty() {
            return;
        }
        let text: String = chars.iter().collect();
        let open = match self.log.last() {
            Some(e) if began.is_none() && e.dir == Dir::Rx => Some(self.log.len() - 1),
            _ => None,
        };
        let i = match open {
            Some(i) => {
                self.log[i].text.push_str(&text);
                i
            }
            None => {
                self.log.push(Entry {
                    at_s: began.unwrap_or(now_s),
                    dir: Dir::Rx,
                    text,
                    marks: Vec::new(),
                });
                self.log.len() - 1
            }
        };
        let start = self.log[i].text.chars().count() - chars.len();
        for &(at, t) in frames {
            if at >= base && at < base + chars.len() {
                let off = start + (at - base);
                if off > 0 {
                    self.log[i].marks.push((off, t));
                }
            }
        }
    }

    /// Take the SNR samples the channel has recorded since last time.
    ///
    /// Separate from the text, and taken by time rather than by count, because
    /// the two series are trimmed against different bounds: a channel that has
    /// dropped the front of its history is not a channel that has decoded
    /// anything new.
    fn take_snr(&mut self, ch: &Channel) {
        // Scrubbing the playback clock backwards rebuilds the channel out of
        // less audio, and samples we hold from beyond where it now ends are
        // from a future that has not been decoded yet. Dropping them is the
        // same rewind `absorb` guards against for the text, and it is why this
        // sits above that guard rather than after it.
        self.snr_history.retain(|&(t, _)| t <= ch.last_heard_s);
        let last = self.snr_history.last().map_or(f64::NEG_INFINITY, |&(t, _)| t);
        self.snr_history.extend(ch.snr_history.iter().filter(|&&(t, _)| t > last));
        if self.snr_history.len() > MAX_SNR_POINTS {
            let over = self.snr_history.len() - MAX_SNR_POINTS;
            self.snr_history.drain(..over);
        }
    }

    /// Take whatever the followed channel has decoded since last time.
    ///
    /// The channel accumulates one long string, so the new tail is the whole of
    /// what arrived. This runs every frame, so on a continuous mode that tail
    /// is usually a single character — which is why it joins the entry already
    /// open rather than starting one.
    ///
    /// What breaks the run is either of the two things that mean the other
    /// station finished: we transmitted, or it stopped. The second is not
    /// something this can time for itself. A QSO absorbs whatever arrived since
    /// it last looked, which is one character during playback and a whole scan
    /// pass — several seconds of text — from a live radio, so the interval
    /// between calls measures the decoder's cadence and not the operator's.
    /// [`Channel::breaks`] holds the real silences, recorded where the decodes
    /// still had their own times, and this splits on those.
    fn absorb(&mut self, ch: &Channel) {
        self.hz = ch.hz;
        self.mode = ch.mode;
        self.drift_hz = ch.drift_hz();
        self.quality = ch.quality;
        self.snr_db = ch.snr_db;
        self.best_snr_db = ch.best_snr_db;
        self.take_snr(ch);
        // The report follows the most recent decode rather than the best one:
        // what you send is what you just copied, which is also what WSJT-X and
        // JS8Call report.
        if self.rst_sent_auto {
            if let Some(report) = ch.snr_db.and_then(|db| suggested_report(ch.mode, db)) {
                self.rst_sent = report;
            }
        }

        // `absorbed` counts from the start of the conversation rather than
        // from the start of what the channel still holds, so trimming the front
        // of that text does not shift what this has already taken.
        let have = ch.text_dropped + ch.text.chars().count();
        // Scrubbing the playback clock backwards rebuilds the channel with less
        // text than we have already logged. Nothing to take, and nothing to
        // panic about either.
        if have <= self.absorbed {
            return;
        }
        // Text trimmed away before this QSO reached it is gone; start from the
        // oldest character the channel still has rather than from behind it.
        let seen = self.absorbed.max(ch.text_dropped);
        let new: Vec<char> = ch.text.chars().skip(seen - ch.text_dropped).collect();
        self.absorbed = have;
        self.last_s = ch.last_heard_s;

        // Every over that starts inside what we just took. Usually none: text
        // still arriving is the same over as the character before it.
        let starts = ch.breaks.iter().filter(|&&(i, _)| i >= seen && i < have);

        // Where each frame of this stretch starts, for the log to mark the
        // seams between them; empty on a continuous mode, which has none.
        let frames: Vec<(usize, f64)> =
            ch.frames.iter().copied().filter(|&(i, _)| i >= seen && i < have).collect();

        let mut from = 0usize;
        // `None` continues the entry already open; `Some` is a fresh over, and
        // carries the time it began so the log stamps it with that rather than
        // with whenever this happened to be noticed.
        //
        // JS8 continues an entry too. A frame is a packet rather than an
        // utterance — a sentence is three of them a cycle apart — so a line
        // apiece read as a station talking in fragments. What ends the entry
        // now is what ends an over: the flag the station sets on its last
        // frame, a change of speaker, or a real silence, all of them recorded
        // as breaks by `channels`.
        let mut began: Option<f64> = None;
        for &(i, at_s) in starts {
            let cut = i - seen;
            if cut > from {
                self.push_rx(&new[from..cut], began, ch.last_heard_s, &frames, seen + from);
            }
            from = cut;
            began = Some(at_s);
        }
        self.push_rx(&new[from..], began, ch.last_heard_s, &frames, seen + from);

        // Looked for in the whole entry rather than in what just arrived: on a
        // continuous mode what just arrived is one character, and a call sign
        // spread over twenty of them would never be seen. Rescanning costs
        // nothing worth counting, and only happens while the call is unknown.
        if self.call.trim().is_empty() {
            let entry = self.log.last().expect("an entry was just written");
            if let Some(c) = callsign_in(settled_text(&entry.text, ch.mode)) {
                self.call = c;
            }
        }
    }
}

/// Every open conversation, and which tab is on top.
#[derive(Clone, Debug, Default)]
pub struct QsoSet {
    qsos: Vec<Qso>,
    active: usize,
    next_id: u64,
}

impl QsoSet {
    pub fn new() -> QsoSet {
        QsoSet::default()
    }

    pub fn is_empty(&self) -> bool {
        self.qsos.is_empty()
    }

    pub fn len(&self) -> usize {
        self.qsos.len()
    }

    pub fn qsos(&self) -> &[Qso] {
        &self.qsos
    }

    #[cfg(test)]
    pub fn qsos_mut(&mut self) -> &mut [Qso] {
        &mut self.qsos
    }

    /// Index of the tab on top. Meaningless when there are no QSOs; callers
    /// check [`QsoSet::is_empty`] first.
    pub fn active(&self) -> usize {
        self.active.min(self.qsos.len().saturating_sub(1))
    }

    pub fn set_active(&mut self, i: usize) {
        if i < self.qsos.len() {
            self.active = i;
        }
    }

    pub fn active_qso_mut(&mut self) -> Option<&mut Qso> {
        let i = self.active();
        self.qsos.get_mut(i)
    }

    /// Open a QSO on this channel, or bring the existing one to the front.
    ///
    /// The QSO starts with everything already heard on the channel as its first
    /// received line, and counts its offsets from when the station was first
    /// heard rather than from the click — the exchange did not begin when you
    /// happened to notice it. Its UTC stamp is backdated the same way, from the
    /// audio clock's `now_s`.
    pub fn open_for_channel(&mut self, ch: &Channel, now_s: f64) -> usize {
        if let Some(i) = self.qsos.iter().position(|q| q.channel == Some(ch.id)) {
            self.active = i;
            return i;
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut q = Qso {
            rst_sent_auto: true,
            id,
            call: String::new(),
            name: String::new(),
            qth: String::new(),
            grid: String::new(),
            rst_sent: String::new(),
            rst_rcvd: String::new(),
            channel: Some(ch.id),
            hz: ch.hz,
            mode: ch.mode,
            drift_hz: ch.drift_hz(),
            quality: ch.quality,
            snr_db: ch.snr_db,
            best_snr_db: ch.best_snr_db,
            snr_history: Vec::new(),
            started_s: ch.first_heard_s,
            last_s: ch.last_heard_s,
            started_utc: backdated(now_s - ch.first_heard_s),
            log: Vec::new(),
            draft: String::new(),
            outgoing: Vec::new(),
            want_focus: false,
            absorbed: 0,
        };
        q.absorb(ch);
        // The seeded line arrived over the whole time the station has been
        // heard, so it belongs at the start of the log rather than at the
        // moment of the last decode.
        if let Some(e) = q.log.first_mut() {
            e.at_s = ch.first_heard_s;
        }
        self.qsos.push(q);
        self.active = self.qsos.len() - 1;
        self.active
    }

    /// Open an empty QSO, to be filled in by hand — a station worked off the
    /// air, or one you mean to call before it has been decoded.
    pub fn open_blank(&mut self, hz: f64, mode: ModeId, now_s: f64) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.qsos.push(Qso {
            rst_sent_auto: true,
            id,
            call: String::new(),
            name: String::new(),
            qth: String::new(),
            grid: String::new(),
            rst_sent: String::new(),
            rst_rcvd: String::new(),
            channel: None,
            hz,
            mode,
            drift_hz: 0.0,
            quality: 0.0,
            snr_db: None,
            best_snr_db: None,
            snr_history: Vec::new(),
            started_s: now_s,
            last_s: now_s,
            started_utc: SystemTime::now(),
            log: Vec::new(),
            draft: String::new(),
            outgoing: Vec::new(),
            want_focus: false,
            absorbed: 0,
        });
        self.active = self.qsos.len() - 1;
        self.active
    }

    /// Clear the composer of any QSO whose transmission has finished.
    ///
    /// Across all of them, not just the one on top: a send does not pause
    /// because its tab is not the one being looked at.
    pub fn retire_sent(&mut self, now_unix: f64) {
        for q in &mut self.qsos {
            if q.outgoing.is_empty() {
                continue;
            }
            // Each message goes as it finishes rather than the buffer emptying
            // in one go at the end: over a long exchange, holding every
            // completed transmission until the last one had gone would leave
            // the pane growing all session. What is left is what is still to
            // go, and the one going out now.
            q.outgoing.retain(|o| !o.finished(now_unix));
            if q.outgoing.is_empty() {
                q.want_focus = true;
            }
        }
    }

    pub fn close(&mut self, i: usize) {
        if i < self.qsos.len() {
            self.qsos.remove(i);
            self.active = self.active.min(self.qsos.len().saturating_sub(1));
        }
    }

    /// Pull new text from every followed channel into its QSO.
    pub fn absorb(&mut self, channels: &ChannelSet) {
        for q in self.qsos.iter_mut() {
            if let Some(ch) = q.channel.and_then(|id| channels.by_id(id)) {
                q.absorb(ch);
            }
        }
    }
}

/// The other station's call sign, if the text plainly contains one.
///
/// Amateur text is telegraphic and its conventions are strong: a call follows
/// `DE`, and that is the case worth getting right, so it is tried first. Failing
/// that, the first token shaped like a call is taken.
///
/// "Shaped like a call" is the ITU pattern — a one or two character prefix with
/// a letter in it, one digit, then one to four letters — which is deliberately
/// stricter than "has a digit in it". Amateur text is full of tokens that would
/// pass a loose test and are not calls: grids (`EM73`), power (`400W`, `5W`),
/// reports (`579`), and the whole of the Q code.
/// Now, less `age_s` seconds of audio clock. A negative age (a channel heard
/// "after" now, which the file view can produce mid-seek) and a clock too near
/// the epoch to subtract from both fall back to now.
fn backdated(age_s: f64) -> SystemTime {
    let now = SystemTime::now();
    if !age_s.is_finite() || age_s <= 0.0 {
        return now;
    }
    // from_secs_f64 panics on anything it cannot represent, so cap the age at
    // a century rather than trust an audio clock that has gone strange.
    now.checked_sub(Duration::from_secs_f64(age_s.min(3.2e9))).unwrap_or(now)
}

pub fn callsign_in(text: &str) -> Option<String> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    fn clean(t: &str) -> &str {
        t.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/')
    }

    // After DE is where a station says who it is.
    for (i, t) in tokens.iter().enumerate() {
        if clean(t).eq_ignore_ascii_case("DE") {
            if let Some(c) = tokens.get(i + 1).and_then(|t| as_callsign(clean(t))) {
                return Some(c);
            }
        }
    }
    tokens.iter().find_map(|t| as_callsign(clean(t)))
}

/// `Some(call)` if this token is a call sign, uppercased.
///
/// A portable or reciprocal suffix (`W1AW/P`, `DL/W1AW`) is one call with
/// decoration, so the whole token is kept when any part of it is a call.
fn as_callsign(token: &str) -> Option<String> {
    if token.is_empty() || token.len() > 12 {
        return None;
    }
    let up = token.to_ascii_uppercase();
    if up.split('/').any(is_call_core) {
        Some(up)
    } else {
        None
    }
}

/// The ITU call pattern: prefix of one or two characters including at least one
/// letter, then a single digit, then one to four letters.
fn is_call_core(s: &str) -> bool {
    let b = s.as_bytes();
    if !b.iter().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    // The digit that separates prefix from suffix is the *first* one, since the
    // suffix is letters only.
    let Some(d) = b.iter().position(|c| c.is_ascii_digit()) else { return false };
    let (prefix, suffix) = (&b[..d], &b[d + 1..]);
    (1..=2).contains(&prefix.len())
        && prefix.iter().any(|c| c.is_ascii_alphabetic())
        && (1..=4).contains(&suffix.len())
        && suffix.iter().all(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::MIN_OVER_GAP_S;
    use ragchew::protocol::Decode;
    use ragchew::{js8, olivia};

    fn js8_decode(hz: f64, t: f64, text: &str) -> Decode {
        Decode {
            snr_db: None,
            mode: ModeId::Js8(js8::Mode::Normal),
            hz,
            time_s: t,
            quality: 4.0,
            text: text.to_string(),
            ends_over: false,
        }
    }

    /// One character, which is all a PSK decode ever is.
    fn psk_decode(hz: f64, t: f64, ch: char) -> Decode {
        Decode {
            snr_db: None,
            mode: ModeId::Psk(ragchew::psk::PSK31),
            hz,
            time_s: t,
            quality: 4.0,
            text: ch.to_string(),
            ends_over: false,
        }
    }

    /// Feed a PSK station's text in one character at a time, at the rate the
    /// mode sends them, absorbing after each — which is what the app does, a
    /// frame at a time, as the playback clock passes each character.
    fn psk_over(set: &mut ChannelSet, qsos: &mut QsoSet, from_s: f64, text: &str) -> f64 {
        let step = ModeId::Psk(ragchew::psk::PSK31).chunk_secs();
        let mut t = from_s;
        for c in text.chars() {
            set.add(psk_decode(620.0, t, c));
            qsos.absorb(set);
            t += step;
        }
        t
    }


    fn measured(t: f64, snr: f32) -> Decode {
        Decode {
            snr_db: Some(snr),
            mode: ModeId::Js8(js8::Mode::Normal),
            hz: 1000.0,
            time_s: t,
            quality: 4.0,
            text: "DE K2N ".to_string(),
            ends_over: false,
        }
    }

    /// A QSO takes the whole series the channel has, keeps it in time order,
    /// and takes each sample once however often it looks.
    ///
    /// Absorbing runs every frame — hundreds of times between two JS8 frames —
    /// so "once" is the property that matters: a series that grew by a sample
    /// per frame would draw its own refresh rate rather than the path.
    #[test]
    fn a_qso_takes_each_snr_sample_once() {
        let mut set = ChannelSet::from_decodes([measured(0.0, -12.0)], 15.0);
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);
        assert_eq!(qsos.qsos()[0].snr_history, vec![(0.0, -12.0)], "the QSO opened empty");

        for (i, snr) in [-9.0f32, -14.5, -3.0].iter().enumerate() {
            set.add(measured((i + 1) as f64 * 15.0, *snr));
            // Several times over, as the app does between one decode and the next.
            for _ in 0..5 {
                qsos.absorb(&set);
            }
        }
        assert_eq!(
            qsos.qsos()[0].snr_history,
            vec![(0.0, -12.0), (15.0, -9.0), (30.0, -14.5), (45.0, -3.0)]
        );
    }

    /// Scrubbing the playback clock backwards un-hears what was heard after it.
    ///
    /// The file view rebuilds its channels from the decodes up to the clock, so
    /// a rewind hands the QSO a channel that ends earlier than the series it is
    /// holding. Those samples are of audio that has not been played yet.
    #[test]
    fn rewinding_drops_snr_from_the_future() {
        let all = [
            measured(0.0, -12.0),
            measured(15.0, -9.0),
            measured(30.0, -14.5),
            measured(45.0, -3.0),
        ];
        let mut qsos = QsoSet::new();
        let set = ChannelSet::from_decodes(all.clone(), 15.0);
        qsos.open_for_channel(&set.channels()[0], 45.0);
        assert_eq!(qsos.qsos()[0].snr_history.len(), 4);

        // Back to 20 s: the channel is rebuilt from the first two decodes only.
        let rewound = ChannelSet::from_decodes(all[..2].to_vec(), 15.0);
        qsos.absorb(&rewound);
        assert_eq!(qsos.qsos()[0].snr_history, vec![(0.0, -12.0), (15.0, -9.0)]);

        // And playing forward again re-takes them, once each.
        qsos.absorb(&set);
        assert_eq!(qsos.qsos()[0].snr_history.len(), 4, "the series did not come back");
    }

    /// A long conversation stays bounded, and it is the oldest that goes.
    #[test]
    fn the_snr_series_is_bounded() {
        let mut set = ChannelSet::from_decodes([measured(0.0, -12.0)], 15.0);
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);
        for i in 1..(MAX_SNR_POINTS * 2) {
            set.add(measured(i as f64 * 15.0, -6.0));
            qsos.absorb(&set);
        }
        let h = &qsos.qsos()[0].snr_history;
        assert_eq!(h.len(), MAX_SNR_POINTS, "the series is not at its bound");
        assert_eq!(h.last().expect("bounded, not empty").0, (MAX_SNR_POINTS * 2 - 1) as f64 * 15.0);
    }

    /// A JS8 report is transcription: the exchange *is* the SNR, so the field
    /// is filled from the measurement and follows the station while it is
    /// still ours.
    #[test]
    fn a_js8_report_is_filled_from_the_measurement() {
        let decode = |t: f64, snr: f32| Decode {
            snr_db: Some(snr),
            mode: ModeId::Js8(js8::Mode::Normal),
            hz: 1000.0,
            time_s: t,
            quality: 4.0,
            text: "DE K2N ".to_string(),
            ends_over: false,
        };
        let mut set = ChannelSet::from_decodes([decode(0.0, -12.4)], 15.0);
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);
        assert_eq!(qsos.qsos()[0].rst_sent, "-12", "not filled from the first decode");

        // The station comes up; what we would send comes up with it, because a
        // report is of the transmission you are answering.
        set.add(decode(15.0, 6.5));
        qsos.absorb(&set);
        assert_eq!(qsos.qsos()[0].rst_sent, "+07");
        assert_eq!(qsos.qsos()[0].snr_db, Some(6.5));
    }

    /// Once the operator types, the field is theirs for good — a log records
    /// what was sent, not what was measured after the fact.
    #[test]
    fn typing_a_report_stops_it_being_overwritten() {
        let decode = |t: f64, snr: f32| Decode {
            snr_db: Some(snr),
            mode: ModeId::Js8(js8::Mode::Normal),
            hz: 1000.0,
            time_s: t,
            quality: 4.0,
            text: "DE K2N ".to_string(),
            ends_over: false,
        };
        let mut set = ChannelSet::from_decodes([decode(0.0, -12.0)], 15.0);
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);

        // What the panel does when the field is edited.
        qsos.qsos_mut()[0].rst_sent = "599".to_string();
        qsos.qsos_mut()[0].rst_sent_auto = false;

        set.add(decode(15.0, 9.0));
        qsos.absorb(&set);
        assert_eq!(qsos.qsos()[0].rst_sent, "599", "the operator's report was overwritten");
        // The measurement itself still tracks; it is only the report that is
        // handed over.
        assert_eq!(qsos.qsos()[0].snr_db, Some(9.0));
    }

    /// The keyboard modes are left alone: RST and RSQ are judgements, and an
    /// SNR does not determine them.
    #[test]
    fn a_keyboard_mode_report_is_left_to_the_operator() {
        for mode in [ModeId::Psk(ragchew::psk::PSK31), ModeId::Olivia(olivia::OL_8_250)] {
            assert_eq!(suggested_report(mode, 3.0), None, "{} was given a report", mode.name());

            let set = ChannelSet::from_decodes(
                [Decode {
                    snr_db: Some(3.0),
                    mode,
                    hz: 1000.0,
                    time_s: 0.0,
                    quality: 4.0,
                    text: "cq de w1aw ".to_string(),
                    ends_over: false,
                }],
                15.0,
            );
            let mut qsos = QsoSet::new();
            qsos.open_for_channel(&set.channels()[0], 0.0);
            assert!(
                qsos.qsos()[0].rst_sent.is_empty(),
                "{} filled in a report: {:?}",
                mode.name(),
                qsos.qsos()[0].rst_sent
            );
        }
    }

    /// The wire format JS8Call and WSJT-X use: a sign and two digits.
    #[test]
    fn a_report_is_written_the_way_the_mode_writes_it() {
        let js8 = ModeId::Js8(js8::Mode::Normal);
        for (snr, want) in
            [(-12.4f32, "-12"), (-12.6, "-13"), (0.4, "+00"), (6.5, "+07"), (-5.0, "-05")]
        {
            assert_eq!(suggested_report(js8, snr).as_deref(), Some(want), "for {snr} dB");
        }
    }

    /// The calls in this repository's own demo band, which are the ones any
    /// change here is going to be judged against.
    #[test]
    fn reads_the_call_out_of_demo_traffic() {
        let cases = [
            ("CQ CQ DE KN4RAG KN4RAG K ", "KN4RAG"),
            ("CQ DE W1SLW ", "W1SLW"),
            ("DE VE3XYZ ", "VE3XYZ"),
            ("DE G4ABC ", "G4ABC"),
            ("DE ZL2T ", "ZL2T"),
            ("CQ DE N0FST ", "N0FST"),
            ("TEST DE W9T ", "W9T"),
            ("DE VE7OLV QRP 5W ", "VE7OLV"),
            ("DE W6GONE 8/500 -12.5 DB SAME LEVEL BUT TOO FAST ", "W6GONE"),
            ("DE W2QRM ", "W2QRM"),
        ];
        for (text, want) in cases {
            assert_eq!(callsign_in(text).as_deref(), Some(want), "in {text:?}");
        }
    }

    /// Amateur text is full of tokens that a "has a digit in it" test would
    /// call a call sign. None of these are one.
    #[test]
    fn does_not_mistake_ham_shorthand_for_a_call() {
        for text in [
            "EM73 PSE K ",       // grid
            "FN20 5W ",          // grid and power
            "QRO 400W ",         // power
            "GM OM UR RST 579 ", // a signal report
            "RRR TU ",           //
            "SK E E ",           //
            "IN THE MUD ",       //
            "73 GL ",            //
            "FB 73 GL ",         //
            "QRZ? ",             //
            "DN70 ",             // grid
            "IO91 QRP ",         // grid
        ] {
            assert_eq!(callsign_in(text), None, "in {text:?}");
        }
    }

    /// A call after DE wins over anything earlier in the line, and decoration
    /// comes along with it.
    #[test]
    fn de_names_the_sender() {
        assert_eq!(callsign_in("W1AW DE K2N ").as_deref(), Some("K2N"));
        assert_eq!(callsign_in("K2N/P DE W1AW/QRP ").as_deref(), Some("W1AW/QRP"));
        assert_eq!(callsign_in("DL/W1AW K ").as_deref(), Some("DL/W1AW"));
        // no DE at all, so the first call-shaped token stands
        assert_eq!(callsign_in("K2N THIS IS ME ").as_deref(), Some("K2N"));
    }

    /// Opening a QSO on a channel picks up what has already been said, and
    /// counts time from when the station was first heard.
    #[test]
    fn opening_a_qso_takes_the_history_with_it() {
        let mut set = ChannelSet::from_decodes(
            [js8_decode(1000.0, 5.0, "CQ CQ DE K2N "), js8_decode(1000.0, 20.0, "FN20 5W ")],
            15.0,
        );
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 60.0);
        let q = &qsos.qsos()[0];
        assert_eq!(q.call, "K2N");
        assert_eq!(q.started_s, 5.0);
        assert_eq!(q.log.len(), 1);
        assert_eq!(q.log[0].dir, Dir::Rx);
        assert_eq!(q.log[0].text, "CQ CQ DE K2N FN20 5W ");
        assert_eq!(q.log[0].at_s, 5.0);

        // and the next frame joins the over it continues, seam and all
        set.add(js8_decode(1000.0, 35.0, "73 GL "));
        qsos.absorb(&set);
        let q = &qsos.qsos()[0];
        assert_eq!(q.log.len(), 1, "the over came out as {} entries", q.log.len());
        assert_eq!(q.log[0].text, "CQ CQ DE K2N FN20 5W 73 GL ");
        assert_eq!(q.log[0].at_s, 5.0, "the entry is stamped when the over began");
        assert_eq!(
            q.log[0].marks.last().map(|&(_, t)| t),
            Some(35.0),
            "the newest seam does not carry its frame's time"
        );
    }

    /// A PSK over is one line in the log, not one line per character.
    ///
    /// PSK decodes arrive a character at a time — there is no frame and no
    /// block — so a log that started an entry per decode gave every letter its
    /// own timestamped row. What has to come back is the over, whole, stamped
    /// when the station started sending it.
    #[test]
    fn a_psk_over_is_one_entry() {
        let text = "cq cq de kb9psk pse k ";
        let mut set = ChannelSet::new(15.0);
        set.add(psk_decode(620.0, 0.0, 'c'));
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);
        psk_over(&mut set, &mut qsos, ModeId::Psk(ragchew::psk::PSK31).chunk_secs(), &text[1..]);

        let q = &qsos.qsos()[0];
        assert_eq!(q.log.len(), 1, "one over, {} entries: {:?}", q.log.len(), q.log);
        assert_eq!(q.log[0].text, text);
        assert_eq!(q.log[0].at_s, 0.0, "stamped when the over ended, not when it began");
        assert_eq!(q.call, "KB9PSK");
    }

    /// Live, a scan pass hands over several seconds of text at once — longer
    /// than the silence that ends an entry. The over must not split at those
    /// boundaries: they are the decoder's cadence, not the operator's.
    #[test]
    fn an_over_absorbed_in_batches_is_still_one_entry() {
        let step = ModeId::Psk(ragchew::psk::PSK31).chunk_secs();
        const BATCH: usize = 40;
        assert!(
            BATCH as f64 * step > MIN_OVER_GAP_S,
            "a batch shorter than the gap would prove nothing"
        );

        let text: Vec<char> = "cq cq de kb9psk kb9psk pse k  ".repeat(4).chars().collect();
        let mut set = ChannelSet::new(15.0);
        let mut qsos = QsoSet::new();
        let mut t = 0.0;
        for (i, batch) in text.chunks(BATCH).enumerate() {
            for c in batch {
                set.add(psk_decode(620.0, t, *c));
                t += step;
            }
            if i == 0 {
                qsos.open_for_channel(&set.channels()[0], 0.0);
            }
            qsos.absorb(&set);
        }

        let q = &qsos.qsos()[0];
        assert_eq!(q.log.len(), 1, "the over split on a batch boundary: {:?}", q.log);
        assert_eq!(q.log[0].text.chars().count(), text.len());
    }

    /// A distinguishable gap in reception is what ends an over, since nothing
    /// in a continuous mode's signal says so.
    #[test]
    fn silence_starts_a_new_psk_entry() {
        let mut set = ChannelSet::new(15.0);
        set.add(psk_decode(620.0, 0.0, 'h'));
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);
        let end = psk_over(&mut set, &mut qsos, 0.3, "i om ");

        // A pause too short to mean anything: still the same over.
        let end = psk_over(&mut set, &mut qsos, end + MIN_OVER_GAP_S - 1.0, "es tnx ");
        assert_eq!(qsos.qsos()[0].log.len(), 1, "a two-second pause split the over");

        // Long enough that the station plainly stopped.
        psk_over(&mut set, &mut qsos, end + MIN_OVER_GAP_S + 1.0, "back again ");
        let q = &qsos.qsos()[0];
        assert_eq!(q.log.len(), 2, "the station came back and the log did not notice");
        assert_eq!(q.log[0].text, "hi om es tnx ");
        assert_eq!(q.log[1].text, "back again ");
    }

    /// Sending ends the over we were copying, whatever the timing — which is
    /// the other half of "start a new entry when we send".
    #[test]
    fn our_own_transmission_ends_the_received_entry() {
        let mut set = ChannelSet::new(15.0);
        set.add(psk_decode(620.0, 0.0, 'k'));
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);
        let end = psk_over(&mut set, &mut qsos, 0.3, " de w1aw ");

        qsos.qsos_mut()[0].push_tx("r r ur 599", end);
        // Straight back to us, well inside the gap that would otherwise join.
        psk_over(&mut set, &mut qsos, end + 0.5, "tnx 73 ");

        let q = &qsos.qsos()[0];
        let dirs: Vec<Dir> = q.log.iter().map(|e| e.dir).collect();
        assert_eq!(dirs, [Dir::Rx, Dir::Tx, Dir::Rx], "logged {:?}", q.log);
        assert_eq!(q.log[2].text, "tnx 73 ");
    }

    /// Olivia is continuous too, so its blocks join into an over — but a block
    /// takes two seconds, and two in a row are not a pause.
    #[test]
    fn olivia_blocks_join_but_a_real_pause_does_not() {
        let block = |t: f64, text: &str| Decode {
            snr_db: None,
            mode: ModeId::Olivia(olivia::OL_8_250),
            hz: 1000.0,
            time_s: t,
            quality: 4.0,
            text: text.to_string(),
            ends_over: false,
        };
        let mut set = ChannelSet::new(15.0);
        set.add(block(0.0, "gm om "));
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);

        for (i, text) in ["ur rst ", "579 in ", "em73 "].iter().enumerate() {
            set.add(block(2.05 * (i + 1) as f64, text));
            qsos.absorb(&set);
        }
        assert_eq!(qsos.qsos()[0].log.len(), 1, "back-to-back blocks split");
        assert_eq!(qsos.qsos()[0].log[0].text, "gm om ur rst 579 in em73 ");

        set.add(block(60.0, "btu "));
        qsos.absorb(&set);
        assert_eq!(qsos.qsos()[0].log.len(), 2, "a minute of silence did not split");
    }

    /// JS8 frames of one over read as one thing said, with the seams kept.
    ///
    /// A frame is a packet, not a sentence: three of them a cycle apart are a
    /// station saying one thing slowly. They used to take a line each, which
    /// read as a station talking in fragments. What joins them is the log, and
    /// what still separates them is a mark carrying the time each frame
    /// started — the log's text is what was said, and the marks are how it
    /// came.
    #[test]
    fn js8_frames_of_one_over_join_up() {
        let period = ModeId::Js8(js8::Mode::Normal).period_s().expect("cycle-aligned") as f64;
        let mut set = ChannelSet::new(15.0);
        set.add(js8_decode(1000.0, 0.0, "K2N: HW CPY "));
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);
        for (i, text) in ["FN20 5W ", "PSE K "].iter().enumerate() {
            set.add(js8_decode(1000.0, (i + 1) as f64 * period, text));
            qsos.absorb(&set);
        }

        let q = &qsos.qsos()[0];
        assert_eq!(q.log.len(), 1, "one over came out as {} entries", q.log.len());
        assert_eq!(q.log[0].text, "K2N: HW CPY FN20 5W PSE K ");
        // Two seams for three frames, at the joins, timed by their own cycles.
        let marks: Vec<(usize, f64)> = q.log[0].marks.clone();
        assert_eq!(marks.len(), 2, "three frames left {} seams", marks.len());
        assert_eq!(marks[0].1, period);
        assert_eq!(marks[1].1, 2.0 * period);
        for &(at, _) in &marks {
            assert!(at > 0 && at < q.log[0].text.chars().count(), "seam at {at} is outside");
        }
        // And they are where the text actually joins.
        let cut: String = q.log[0].text.chars().skip(marks[0].0).collect();
        assert!(cut.starts_with("FN20"), "the first seam fell mid-frame: {cut:?}");
    }

    /// The station's own end-of-over flag closes the entry, even when the next
    /// frame follows in the very next cycle.
    ///
    /// This is the case a timing rule cannot get right: a reply that comes back
    /// immediately is exactly what a good QSO looks like, and joining it to the
    /// call it answers would merge the two halves of the exchange.
    #[test]
    fn an_over_ends_where_the_station_says_it_does() {
        let period = ModeId::Js8(js8::Mode::Normal).period_s().expect("cycle-aligned") as f64;
        let mut done = js8_decode(1000.0, 0.0, "K2N: CQ CQ ");
        done.ends_over = true;
        let mut set = ChannelSet::from_decodes([done], 15.0);
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);
        set.add(js8_decode(1000.0, period, "W1AW: K2N SNR -12 "));
        qsos.absorb(&set);

        let q = &qsos.qsos()[0];
        assert_eq!(q.log.len(), 2, "the answer joined the call it was answering");
        assert_eq!(q.log[1].at_s, period, "the new entry is stamped with its own cycle");
        assert!(q.log[0].marks.is_empty() && q.log[1].marks.is_empty());
    }

    /// Two stations sharing one offset — which is how JS8 replies work — keep
    /// their own entries even with no flag between them.
    #[test]
    fn a_change_of_speaker_starts_a_new_entry() {
        let period = ModeId::Js8(js8::Mode::Normal).period_s().expect("cycle-aligned") as f64;
        let mut set = ChannelSet::new(15.0);
        set.add(js8_decode(1000.0, 0.0, "K2N: CQ CQ CQ "));
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);
        // Free text in between carries no callsign and is not somebody new.
        set.add(js8_decode(1000.0, period, "FN20 "));
        set.add(js8_decode(1000.0, 2.0 * period, "W1AW: K2N HW CPY "));
        qsos.absorb(&set);

        let q = &qsos.qsos()[0];
        assert_eq!(q.log.len(), 2, "expected the two stations to be two entries");
        assert_eq!(q.log[0].text, "K2N: CQ CQ CQ FN20 ");
        assert_eq!(q.log[1].text, "W1AW: K2N HW CPY ");
    }

    /// A lost last frame is the case the flag cannot cover, and the clock
    /// covers it: in this mode, losing one is an ordinary evening.
    #[test]
    fn a_silence_still_ends_an_over_without_a_flag() {
        let mut set = ChannelSet::new(15.0);
        set.add(js8_decode(1000.0, 0.0, "K2N: HW CPY "));
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);
        set.add(js8_decode(1000.0, 120.0, "W1AW DE K2N "));
        qsos.absorb(&set);
        assert_eq!(qsos.qsos()[0].log.len(), 2, "two minutes of silence did not split");
    }

    /// The UTC stamp is the moment the station was first heard, not the moment
    /// the tab was opened: a QSO on a channel heard 55 s ago is stamped 55 s
    /// ago, and one opened by hand is stamped now.
    #[test]
    fn a_qso_is_stamped_when_the_station_was_first_heard() {
        let set = ChannelSet::from_decodes([js8_decode(1000.0, 5.0, "CQ DE K2N ")], 15.0);
        let mut qsos = QsoSet::new();
        let before = SystemTime::now();
        qsos.open_for_channel(&set.channels()[0], 60.0); // first heard at 5 s, now 60 s
        qsos.open_blank(1500.0, ModeId::Js8(ragchew::js8::Mode::Normal), 60.0);
        let after = SystemTime::now();

        let age = |t: SystemTime| before.duration_since(t).unwrap_or_default().as_secs_f64();
        assert!((age(qsos.qsos()[0].started_utc) - 55.0).abs() < 1.0, "not backdated 55 s");
        let blank = qsos.qsos()[1].started_utc;
        assert!(blank >= before && blank <= after, "a blank QSO is stamped now");
    }

    /// A QSO following a channel keeps logging correctly once that channel
    /// starts trimming its text.
    ///
    /// The channel only keeps a tail, so the position a QSO has reached cannot
    /// be an index into what is left — it counts from the start of the
    /// conversation instead. Get that wrong and the log either replays text it
    /// already has or silently stops growing.
    #[test]
    fn a_qso_keeps_up_with_a_channel_that_trims_its_text() {
        let mut set = ChannelSet::new(15.0);
        let chunk = "DE K2N ";
        let decode = |i: usize| js8_decode(1000.0, i as f64, chunk);

        set.add(decode(0));
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 1.0);

        // Enough traffic to push the channel well past its bound.
        let n = 2000;
        for i in 1..n {
            set.add(decode(i));
            qsos.absorb(&set);
        }

        let ch = &set.channels()[0];
        assert!(ch.text_dropped > 0, "the channel never trimmed; this proves nothing");

        let q = &qsos.qsos()[0];
        let logged: String = q.log.iter().filter(|e| e.dir == Dir::Rx).map(|e| e.text.as_str()).collect();
        assert_eq!(
            logged.chars().count(),
            n * chunk.chars().count(),
            "the log should hold every character the channel produced, once"
        );
        assert_eq!(logged, chunk.repeat(n), "the log is not what was decoded, in order");
    }

    /// A QSO that falls further behind than the channel keeps loses the text in
    /// between — but it must lose it cleanly, taking up from the oldest
    /// character still held rather than subtracting its way past zero.
    #[test]
    fn a_qso_that_falls_behind_a_trim_resumes_cleanly() {
        let mut set = ChannelSet::new(15.0);
        let mut qsos = QsoSet::new();
        set.add(js8_decode(1000.0, 0.0, "start "));
        qsos.open_for_channel(&set.channels()[0], 0.0);
        let logged_first = qsos.qsos()[0].log[0].text.chars().count();

        // A long stretch with nobody reading it: the channel trims text this
        // QSO has never seen.
        for i in 1..3000 {
            set.add(js8_decode(1000.0, i as f64, "middle "));
        }
        set.add(js8_decode(1000.0, 3000.0, "end "));
        qsos.absorb(&set);

        let q = &qsos.qsos()[0];
        let last = q.log.last().expect("a log entry");
        assert!(last.text.chars().count() > logged_first, "nothing was absorbed after the gap");
        assert!(last.text.ends_with("end "), "resumed at the wrong place: {:?}", last.text);
        // What it took is exactly what the channel still had, on the end of
        // what it had already logged: nothing invented to bridge the hole, and
        // nothing double-counted across it.
        let ch = &set.channels()[0];
        assert_eq!(last.text, format!("start {}", ch.text));
    }

    /// Text is struck through as it goes out: nothing while the burst is still
    /// waiting for its cycle boundary, all of it once the burst has ended.
    #[test]
    fn outgoing_text_is_struck_through_as_it_is_sent() {
        let out = Outgoing {
            text: "CQ CQ DE K2N".to_string(), // 12 characters
            start_unix: 1000.0,
            end_unix: 1012.0,
        };
        // Queued but not started — a JS8 send waits out most of its cycle here.
        assert_eq!(out.sent_chars(900.0), 0);
        assert_eq!(out.sent_chars(1000.0), 0);
        // Halfway through the burst is halfway through the text.
        assert_eq!(out.sent_chars(1006.0), 6);
        // And it neither overruns nor stops short at the end.
        assert_eq!(out.sent_chars(1012.0), 12);
        assert_eq!(out.sent_chars(9999.0), 12);
        assert!(!out.finished(1011.9));
        assert!(out.finished(1012.0));

        // A zero-length burst is finished, not divided by.
        let instant = Outgoing { text: "73".to_string(), start_unix: 5.0, end_unix: 5.0 };
        assert_eq!(instant.sent_chars(5.0), 2);
        assert!(instant.finished(5.0));
    }

    /// A finished transmission clears the composer of every QSO holding one,
    /// not just the tab being looked at.
    #[test]
    fn finished_transmissions_are_retired_on_every_tab() {
        let set = ChannelSet::from_decodes([js8_decode(1000.0, 0.0, "CQ ")], 15.0);
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);
        qsos.open_blank(1500.0, ModeId::Js8(ragchew::js8::Mode::Normal), 0.0);
        for q in qsos.qsos_mut() {
            q.outgoing
                .push(Outgoing { text: "TEST".to_string(), start_unix: 0.0, end_unix: 10.0 });
        }
        qsos.set_active(0);

        qsos.retire_sent(9.0);
        assert!(qsos.qsos().iter().all(|q| !q.outgoing.is_empty()), "retired mid-burst");
        qsos.retire_sent(10.0);
        assert!(
            qsos.qsos().iter().all(|q| q.outgoing.is_empty()),
            "a finished send should clear on the tabs in the background too"
        );
    }

    /// A finished transmission asks for the keyboard back, so the composer can
    /// take it the moment it returns.
    #[test]
    fn a_finished_transmission_asks_for_the_composer_to_be_focused() {
        let set = ChannelSet::from_decodes([js8_decode(1000.0, 0.0, "CQ ")], 15.0);
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);
        qsos.qsos_mut()[0]
            .outgoing
            .push(Outgoing { text: "TEST".to_string(), start_unix: 0.0, end_unix: 10.0 });

        qsos.retire_sent(9.0);
        assert!(!qsos.qsos()[0].want_focus, "asked for focus mid-burst");
        qsos.retire_sent(10.0);
        assert!(qsos.qsos()[0].want_focus, "the composer should take the keyboard back");
    }

    /// Each message leaves the buffer as it finishes, so a long exchange does
    /// not accumulate everything it has ever sent.
    #[test]
    fn each_message_clears_as_it_finishes() {
        let set = ChannelSet::from_decodes([js8_decode(1000.0, 0.0, "CQ ")], 15.0);
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&set.channels()[0], 0.0);
        let q = &mut qsos.qsos_mut()[0];
        q.outgoing.push(Outgoing { text: "FIRST ".into(), start_unix: 0.0, end_unix: 10.0 });
        q.outgoing.push(Outgoing { text: "SECOND".into(), start_unix: 15.0, end_unix: 25.0 });

        // The first has gone, the second has not started: all of the first is
        // struck through, none of the second.
        let q = &qsos.qsos()[0];
        assert_eq!(q.outgoing[0].sent_chars(12.0), 6);
        assert_eq!(q.outgoing[1].sent_chars(12.0), 0);

        // The finished one goes; the queued one stays, and is still whole.
        qsos.retire_sent(12.0);
        let left = &qsos.qsos()[0].outgoing;
        assert_eq!(left.len(), 1, "the finished message should have gone");
        assert_eq!(left[0].text, "SECOND", "the wrong one was dropped");
        assert!(!qsos.qsos()[0].want_focus, "asked for focus with a send still queued");

        // and the keyboard comes back once there is nothing left to go
        qsos.retire_sent(25.0);
        assert!(qsos.qsos()[0].outgoing.is_empty());
        assert!(qsos.qsos()[0].want_focus);
    }

    /// Clicking the same channel again returns to its tab instead of opening a
    /// second one; a different channel gets its own.
    #[test]
    fn one_tab_per_channel() {
        let set = ChannelSet::from_decodes(
            [js8_decode(1000.0, 1.0, "DE K2N "), js8_decode(2000.0, 2.0, "DE W1AW ")],
            15.0,
        );
        let mut qsos = QsoSet::new();
        let a = qsos.open_for_channel(&set.channels()[0], 60.0);
        let b = qsos.open_for_channel(&set.channels()[1], 60.0);
        assert_ne!(a, b);
        assert_eq!(qsos.len(), 2);
        assert_eq!(qsos.open_for_channel(&set.channels()[0], 60.0), a);
        assert_eq!(qsos.len(), 2);
        assert_eq!(qsos.active(), a);
    }

    /// Scrubbing playback backwards leaves the channel shorter than the log.
    /// Nothing is duplicated, and nothing panics.
    #[test]
    fn rewinding_the_clock_does_not_re_log_text() {
        let full = ChannelSet::from_decodes(
            [js8_decode(1000.0, 1.0, "DE K2N "), js8_decode(1000.0, 16.0, "FN20 ")],
            15.0,
        );
        let mut qsos = QsoSet::new();
        qsos.open_for_channel(&full.channels()[0], 60.0);
        let rewound = ChannelSet::from_decodes([js8_decode(1000.0, 1.0, "DE K2N ")], 15.0);
        qsos.absorb(&rewound);
        assert_eq!(qsos.qsos()[0].log.len(), 1);
    }

    /// A blank QSO has no channel to take a carrier from, so it keeps the one
    /// it was opened with — and its tab says the frequency until a call is
    /// typed in.
    #[test]
    fn a_blank_qso_labels_itself_by_frequency() {
        let mut qsos = QsoSet::new();
        qsos.open_blank(1234.0, ModeId::Olivia(olivia::OL_8_250), 10.0);
        assert_eq!(qsos.qsos()[0].label(), "1234 Hz");
        qsos.active_qso_mut().unwrap().call = "W1AW".into();
        assert_eq!(qsos.qsos()[0].label(), "W1AW");
    }
}
