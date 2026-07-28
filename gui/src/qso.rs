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
    /// Audio-clock time this landed, in the same seconds the playback clock
    /// counts. The log shows it as an offset from the start of the QSO, which
    /// is what tells you the rhythm of an exchange.
    pub at_s: f64,
    pub dir: Dir,
    pub text: String,
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
    /// How many characters of the bound channel's accumulated text are already
    /// in the log.
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
        self.log.push(Entry { at_s, dir: Dir::Tx, text: text.trim().to_string() });
        self.last_s = at_s.max(self.last_s);
    }

    /// Take whatever the followed channel has decoded since last time.
    ///
    /// The channel accumulates one long string, so the new tail is the whole of
    /// what arrived — one entry per decode in practice, since this runs every
    /// frame and a frame is far shorter than a JS8 cycle or an Olivia block.
    fn absorb(&mut self, ch: &Channel) {
        self.hz = ch.hz;
        self.mode = ch.mode;
        self.drift_hz = ch.drift_hz();
        self.quality = ch.quality;

        let len = ch.text.chars().count();
        // Scrubbing the playback clock backwards rebuilds the channel with less
        // text than we have already logged. Nothing to take, and nothing to
        // panic about either.
        if len <= self.absorbed {
            return;
        }
        let new: String = ch.text.chars().skip(self.absorbed).collect();
        self.absorbed = len;
        self.last_s = ch.last_heard_s;
        if self.call.trim().is_empty() {
            if let Some(c) = callsign_in(&new) {
                self.call = c;
            }
        }
        self.log.push(Entry { at_s: ch.last_heard_s, dir: Dir::Rx, text: new });
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
            started_s: ch.first_heard_s,
            last_s: ch.last_heard_s,
            started_utc: backdated(now_s - ch.first_heard_s),
            log: Vec::new(),
            draft: String::new(),
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
            started_s: now_s,
            last_s: now_s,
            started_utc: SystemTime::now(),
            log: Vec::new(),
            draft: String::new(),
            absorbed: 0,
        });
        self.active = self.qsos.len() - 1;
        self.active
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
    use ragchew::protocol::Decode;
    use ragchew::{js8, olivia};

    fn js8_decode(hz: f64, t: f64, text: &str) -> Decode {
        Decode {
            mode: ModeId::Js8(js8::Mode::Normal),
            hz,
            time_s: t,
            quality: 4.0,
            text: text.to_string(),
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

        // and the next decode on that channel arrives as its own line
        set.add(js8_decode(1000.0, 35.0, "73 GL "));
        qsos.absorb(&set);
        let q = &qsos.qsos()[0];
        assert_eq!(q.log.len(), 2);
        assert_eq!(q.log[1].text, "73 GL ");
        assert_eq!(q.log[1].at_s, 35.0);
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
