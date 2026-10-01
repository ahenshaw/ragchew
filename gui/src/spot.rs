//! Reporting who was heard to PSKReporter.
//!
//! PSKReporter collects reception reports from receivers all over the world and
//! puts them on a map, which is how an operator finds out where their signal is
//! getting to. A report says *this station heard that callsign, on this
//! frequency, in this mode, at this time* — and that is all; nothing of what
//! was said is sent.
//!
//! Three pieces, each testable without the others:
//!
//! - [`Spotter`] watches the channel set and turns finished overs into
//!   [`Spot`]s, once per callsign per band per hour.
//! - [`datagram`] encodes them in PSKReporter's wire format, an IPFIX
//!   (RFC 7011) profile over UDP, from the spec at
//!   <https://pskreporter.info/pskdev.html>.
//! - [`Reporter`] owns the socket on a thread of its own and paces the sends
//!   the way the spec asks: one datagram per five minutes or so, the timer not
//!   lined up with the clock.
//!
//! Who is heard comes from [`crate::qso::callsign_in`] over the whole of each
//! over — the `DE` rule first, then the first call-shaped token. That is a
//! heuristic over free text for Olivia and PSK, and it will sometimes name a
//! station that was only mentioned. It was chosen knowing that, over spotting
//! JS8 alone.

use std::collections::HashMap;
use std::net::{ToSocketAddrs, UdpSocket};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ragchew::protocol::{ModeId, Protocol};

use crate::channels::{over_gap_s, Channel};
use crate::qso::callsign_in;

/// Where reports go. The spec asks that this name be used rather than the bare
/// `pskreporter.info`, which only takes the test port now.
pub const SERVER: &str = "report.pskreporter.info:4739";

/// How often a datagram may go: no more than one per five minutes, unless one
/// fills.
pub const INTERVAL: Duration = Duration::from_secs(300);

/// Spread added to the interval, as a fraction of it, drawn afresh for every
/// send: up to half a minute on five. The spec asks for it explicitly — a timer
/// that is not randomized lines a population of receivers up on one second.
const JITTER: f64 = 0.1;

/// How long before the same callsign on the same band is reported again.
///
/// The spec's floor is five minutes and its preference an hour "if it has not
/// changed". A station still on the band an hour later is worth saying so; one
/// heard every over for an hour is not worth twelve rows.
pub const REPEAT_AFTER_S: f64 = 3600.0;

/// Silence added to a mode's own over gap before a quiet station's last over
/// is taken as finished.
///
/// The gap alone is measured on the decodes' own times, and decodes arrive late
/// against the audio clock — a JS8 frame up to a cycle after it began, an
/// Olivia block after a window of up to 24 s has been searched. Without this,
/// an over would close before its last frame had been decoded, and a call cut
/// by the frame packing would be read in half.
const DECODE_LAG_S: f64 = 30.0;

/// Largest datagram built. Well inside the 1500-byte Ethernet MTU with room for
/// the IP and UDP headers and a tunnel or two, so it never fragments.
const MAX_DATAGRAM: usize = 1400;

/// Template IDs. Arbitrary in 256–65535; the receiver one is the spec's own,
/// since the template is too. The sender template is not one of the spec's
/// four — it has the SNR but not the iMD, which nothing here measures — so it
/// takes an ID of its own rather than reuse one the server may have cached
/// with a different shape.
const RECEIVER_TEMPLATE: u16 = 0x9992;
const SENDER_TEMPLATE: u16 = 0x5243;

/// The IANA enterprise number PSKReporter's fields are registered under.
const ENTERPRISE: u32 = 30351;

/// `informationSource`: automatically extracted from decoded text. The only
/// value the server counts as a reception report.
const AUTOMATIC: u8 = 1;

/// One station heard.
#[derive(Clone, Debug, PartialEq)]
pub struct Spot {
    /// As decoded, uppercased, with any `/P` or `DL/` decoration kept: the spec
    /// asks for "the entire string that is repeated after the DE".
    pub call: String,
    /// The signal's own frequency on the air, in hertz: the dial plus or minus
    /// the audio offset, by sideband.
    pub hz: u32,
    pub snr_db: i8,
    /// ADIF mode or submode.
    pub mode: String,
    /// When it was heard, in UTC seconds.
    pub at_unix: u32,
}

/// The receiving station: this one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Station {
    pub call: String,
    pub grid: String,
}

impl Station {
    /// Whether this is enough to report under. A callsign is the whole of the
    /// receiver's identity on the map, so without one nothing goes. The grid is
    /// checked for shape and not for existence, because a typo in one is the
    /// operator's to see on the map.
    pub fn is_complete(&self) -> bool {
        callsign_in(&self.call).is_some_and(|c| c.eq_ignore_ascii_case(self.call.trim()))
            && is_grid(self.grid.trim())
    }
}

/// A Maidenhead locator of four, six or eight characters: field letters A–R,
/// square digits, subsquare letters A–X, extended-square digits.
pub fn is_grid(g: &str) -> bool {
    let b = g.as_bytes();
    let ok = |i: usize| match i {
        0 | 1 => b[i].to_ascii_uppercase().is_ascii_uppercase() && b[i].to_ascii_uppercase() <= b'R',
        2 | 3 | 6 | 7 => b[i].is_ascii_digit(),
        _ => b[i].to_ascii_uppercase().is_ascii_uppercase() && b[i].to_ascii_uppercase() <= b'X',
    };
    matches!(b.len(), 4 | 6 | 8) && (0..b.len()).all(ok)
}

/// The ADIF name for a mode, which is what the spec asks for: `JS8` is a
/// submode of MFSK, the Olivia ones are submodes of OLIVIA written
/// `OLIVIA 8/500`, and `PSK31`/`PSK63` are submodes of PSK.
pub fn adif_mode(mode: ModeId) -> String {
    match mode {
        ModeId::Js8(_) => "JS8".to_string(),
        ModeId::Olivia(m) => format!("OLIVIA {}", m.short_name()),
        ModeId::Psk(m) => m.name().to_ascii_uppercase(),
    }
}

/// The amateur band a frequency is in, as its lower edge in kilohertz — the
/// unit a callsign is suppressed in. Outside every band, the megahertz it is
/// in, so a repeat is still held off somewhere a band plan does not cover.
fn band_of(hz: u32) -> u32 {
    // Edges wide enough for every region's allocation.
    const BANDS: [(u32, u32); 14] = [
        (1_800, 2_000),
        (3_500, 4_000),
        (5_250, 5_450),
        (7_000, 7_300),
        (10_100, 10_150),
        (14_000, 14_350),
        (18_068, 18_168),
        (21_000, 21_450),
        (24_890, 24_990),
        (28_000, 29_700),
        (50_000, 54_000),
        (144_000, 148_000),
        (420_000, 450_000),
        (472, 479),
    ];
    let khz = hz / 1000;
    BANDS.iter().find(|(lo, hi)| (*lo..=*hi).contains(&khz)).map_or(hz / 1_000_000 * 1000, |b| b.0)
}

/// Turns what the channel set has heard into spots.
///
/// Reads each over once, when it is finished, rather than every time it grows.
/// Text arrives in pieces — a JS8 frame packs thirteen characters and cuts
/// whatever word it reaches, PSK comes a character at a time — and the front of
/// a call is a call: `K2L` passes every test `K2LOW` does. An over is finished
/// when the channel has a break after it, when the station flagged it as its
/// last frame, or when the station has been quiet for its mode's over gap plus
/// [`DECODE_LAG_S`].
#[derive(Debug, Default)]
pub struct Spotter {
    /// Per channel, how far into its text the overs have been read: an absolute
    /// index, counted the way [`Channel::next_index`] counts.
    read_to: HashMap<u64, usize>,
    /// The audio time this spotter started at. A channel already on the air by
    /// then has its text so far treated as read, so turning reporting on does
    /// not report the last quarter of an hour at the present time.
    since_s: Option<f64>,
    /// When each (call, band) was last reported, in UTC seconds.
    reported: HashMap<(String, u32), f64>,
}

impl Spotter {
    pub fn new() -> Spotter {
        Spotter::default()
    }

    /// Read every finished over not read yet, and return a spot for each that
    /// names somebody not reported recently.
    ///
    /// `audio_now` is the clock the channels are timed on, and `now_unix` the
    /// wall clock to stamp spots with. `on_air` maps an audio offset to the
    /// frequency on the air, or `None` when the dial or its sideband is not
    /// known — an over finished while it is `None` is read and dropped, since
    /// where it was heard is half of a spot. `wanted` says which protocols to
    /// report. `me` is never reported: hearing yourself is not news.
    pub fn observe(
        &mut self,
        channels: &[Channel],
        audio_now: f64,
        now_unix: f64,
        on_air: impl Fn(f64) -> Option<f64>,
        wanted: impl Fn(Protocol) -> bool,
        me: &str,
    ) -> Vec<Spot> {
        let since = *self.since_s.get_or_insert(audio_now);
        let mut out = Vec::new();
        for ch in channels {
            let read = self.read_to.entry(ch.id).or_insert_with(|| {
                if ch.first_heard_s < since {
                    ch.next_index()
                } else {
                    0
                }
            });
            // Over boundaries, as absolute indexes. The first over starts
            // wherever the text does.
            let end = ch.next_index();
            let mut starts: Vec<usize> = vec![ch.text_dropped];
            starts.extend(ch.breaks.iter().map(|&(i, _)| i).filter(|&i| i > ch.text_dropped));
            let quiet = audio_now - ch.last_heard_s > over_gap_s(ch.mode) + DECODE_LAG_S;
            for (k, &from) in starts.iter().enumerate() {
                let to = match starts.get(k + 1) {
                    Some(&next) => next,
                    None if ch.over_ended || quiet => end,
                    None => break,
                };
                if from < *read || to <= from {
                    // Begun before reporting was on, or before the text was
                    // trimmed under it: either way, not all of it is here.
                    *read = (*read).max(to);
                    continue;
                }
                *read = to;
                if !wanted(ch.mode.protocol()) {
                    continue;
                }
                let over: String =
                    ch.text.chars().skip(from - ch.text_dropped).take(to - from).collect();
                let Some(call) = callsign_in(&over) else { continue };
                if same_station(&call, me) {
                    continue;
                }
                let Some(hz) = on_air(ch.hz).filter(|hz| *hz > 0.0 && *hz < u32::MAX as f64) else {
                    continue;
                };
                // Heard at the end of the over, on the wall clock: the over's
                // last decode, carried back from now by how long ago in audio
                // that was.
                let at = now_unix - (audio_now - ch.last_heard_s).max(0.0);
                let hz = hz.round() as u32;
                let key = (call.clone(), band_of(hz));
                if self.reported.get(&key).is_some_and(|&t| now_unix - t < REPEAT_AFTER_S) {
                    continue;
                }
                // The latest measurement, or failing that the best: an over
                // whose last block was too near the buffer's edge to measure
                // still had a signal.
                let Some(snr) = ch.snr_db.or(ch.best_snr_db) else { continue };
                self.reported.insert(key, now_unix);
                out.push(Spot {
                    call,
                    hz,
                    snr_db: snr.round().clamp(-128.0, 127.0) as i8,
                    mode: adif_mode(ch.mode),
                    at_unix: at.max(0.0).round() as u32,
                });
            }
        }
        // Channels forgotten by the set take their bookkeeping with them.
        self.read_to.retain(|id, _| channels.iter().any(|c| c.id == *id));
        self.reported.retain(|_, t| now_unix - *t < REPEAT_AFTER_S);
        out
    }
}

/// Whether two callsigns are one station, decoration aside: `W1AW/P` is
/// `W1AW`, and so is `VE/W1AW`.
fn same_station(a: &str, b: &str) -> bool {
    let core = |c: &str| -> String {
        let c = c.trim().to_ascii_uppercase();
        c.split('/').max_by_key(|p| p.len()).unwrap_or("").to_string()
    };
    let a = core(a);
    !a.is_empty() && a == core(b)
}

/// One IPFIX field specifier: an enterprise element (the high bit of the ID
/// set, then the enterprise number) or a standard one.
fn field(out: &mut Vec<u8>, id: u16, len: u16, enterprise: bool) {
    if enterprise {
        out.extend((0x8000 | id).to_be_bytes());
        out.extend(len.to_be_bytes());
        out.extend(ENTERPRISE.to_be_bytes());
    } else {
        out.extend(id.to_be_bytes());
        out.extend(len.to_be_bytes());
    }
}

/// Variable length, in an IPFIX field specifier.
const VAR: u16 = 0xFFFF;

/// The two templates, as one block of bytes: the receiver's as an options
/// template (set 3), the sender's as a plain one (set 2).
fn templates() -> Vec<u8> {
    let mut out = Vec::new();
    // receiverCallsign, receiverLocator, decoderSoftware — the spec's first
    // receiver descriptor byte for byte, scope count of one and all.
    let mut set = Vec::new();
    set.extend(RECEIVER_TEMPLATE.to_be_bytes());
    set.extend(3u16.to_be_bytes());
    set.extend(1u16.to_be_bytes());
    field(&mut set, 2, VAR, true);
    field(&mut set, 4, VAR, true);
    field(&mut set, 8, VAR, true);
    push_set(&mut out, 3, &set);
    // senderCallsign, frequency, sNR, mode, informationSource,
    // flowStartSeconds.
    let mut set = Vec::new();
    set.extend(SENDER_TEMPLATE.to_be_bytes());
    set.extend(6u16.to_be_bytes());
    field(&mut set, 1, VAR, true);
    field(&mut set, 5, 4, true);
    field(&mut set, 6, 1, true);
    field(&mut set, 10, VAR, true);
    field(&mut set, 11, 1, true);
    field(&mut set, 150, 4, false);
    push_set(&mut out, 2, &set);
    out
}

/// A set: its ID, its length including this header and the padding, the body,
/// then nulls out to a multiple of four.
fn push_set(out: &mut Vec<u8>, id: u16, body: &[u8]) {
    let padded = (4 + body.len()).next_multiple_of(4);
    out.extend(id.to_be_bytes());
    out.extend((padded as u16).to_be_bytes());
    out.extend(body);
    out.resize(out.len() + padded - 4 - body.len(), 0);
}

/// A string field: one length byte, then the UTF-8. The spec caps the length
/// code at 254, since 255 would mean a three-byte length follows.
fn string(out: &mut Vec<u8>, s: &str) {
    let mut n = s.len().min(254);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    out.push(n as u8);
    out.extend(&s.as_bytes()[..n]);
}

/// The receiver record set for a station.
fn receiver(me: &Station, software: &str) -> Vec<u8> {
    let mut body = Vec::new();
    string(&mut body, &me.call.trim().to_ascii_uppercase());
    string(&mut body, me.grid.trim());
    string(&mut body, software);
    let mut out = Vec::new();
    push_set(&mut out, RECEIVER_TEMPLATE, &body);
    out
}

/// One sender record, in the template's field order.
fn sender(out: &mut Vec<u8>, s: &Spot) {
    string(out, &s.call);
    out.extend(s.hz.to_be_bytes());
    out.push(s.snr_db as u8);
    string(out, &s.mode);
    out.push(AUTOMATIC);
    out.extend(s.at_unix.to_be_bytes());
}

/// Encode spots as datagrams: as many as fit in each, and as many datagrams as
/// that takes. Every one carries the receiver record — the spec asks for it in
/// each — and, when `with_templates`, the templates ahead of it.
///
/// `seq` is the sequence number of the first; the spec counts reports, not
/// packets, so each datagram's is the previous one's plus the spots it held.
pub fn datagrams(
    me: &Station,
    software: &str,
    spots: &[Spot],
    mut seq: u32,
    stream_id: u32,
    now_unix: u32,
    with_templates: bool,
) -> Vec<Vec<u8>> {
    let head = {
        let mut h = if with_templates { templates() } else { Vec::new() };
        h.extend(receiver(me, software));
        h
    };
    let mut out = Vec::new();
    let mut rest = spots;
    while !rest.is_empty() {
        let mut records = Vec::new();
        let mut n = 0;
        for s in rest {
            let mut r = Vec::new();
            sender(&mut r, s);
            // Header, what is already here, the set header and up to three
            // bytes of padding.
            if n > 0 && 16 + head.len() + 4 + records.len() + r.len() + 3 > MAX_DATAGRAM {
                break;
            }
            records.extend(r);
            n += 1;
        }
        let mut d = Vec::with_capacity(MAX_DATAGRAM);
        d.extend(10u16.to_be_bytes());
        d.extend(0u16.to_be_bytes()); // length, filled in below
        d.extend(now_unix.to_be_bytes());
        d.extend(seq.to_be_bytes());
        d.extend(stream_id.to_be_bytes());
        d.extend(&head);
        push_set(&mut d, SENDER_TEMPLATE, &records);
        let len = d.len() as u16;
        d[2..4].copy_from_slice(&len.to_be_bytes());
        out.push(d);
        seq = seq.wrapping_add(n as u32);
        rest = &rest[n..];
    }
    out
}

/// What the reporting thread last did, for the settings menu to show.
#[derive(Clone, Debug, Default)]
pub struct Status {
    /// Spots waiting for the next send.
    pub queued: usize,
    /// Spots sent since the thread started.
    pub sent: u64,
    /// When the last datagram went.
    pub last_sent: Option<SystemTime>,
    /// Why the last send failed, cleared by the next one that works.
    pub fault: Option<String>,
}

enum Msg {
    Spots(Vec<Spot>),
    Station(Station),
}

/// The socket, on a thread of its own: resolving the server's name can take
/// seconds, and the interface must not wait on it.
///
/// Dropping this sends whatever is queued and waits for it to go, so spots
/// heard are not lost because the app closed before the timer ran — but not
/// for long: a server that cannot be reached is not a reason to hang the
/// window. It happens when reporting is turned off or the app closes, never in
/// the course of a frame.
pub struct Reporter {
    tx: Option<Sender<Msg>>,
    status: Arc<Mutex<Status>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Reporter {
    /// Start reporting to [`SERVER`] as `me`.
    pub fn start(me: Station) -> Reporter {
        Reporter::to(SERVER.to_string(), me, INTERVAL)
    }

    /// Start reporting to `server` every `interval` or so. For a test, a
    /// socket on loopback and a short interval.
    pub fn to(server: String, me: Station, interval: Duration) -> Reporter {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let st = status.clone();
        let thread = std::thread::Builder::new()
            .name("pskreporter".into())
            .spawn(move || run(rx, st, server, me, interval))
            .ok();
        Reporter { tx: Some(tx), status, thread }
    }

    pub fn spots(&self, spots: Vec<Spot>) {
        if let (Some(tx), false) = (&self.tx, spots.is_empty()) {
            let _ = tx.send(Msg::Spots(spots));
        }
    }

    /// Change who is reporting. Takes effect from the next datagram.
    pub fn station(&self, me: Station) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Station(me));
        }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        // Closing the channel is the signal; the thread flushes and ends.
        self.tx = None;
        let Some(t) = self.thread.take() else { return };
        let give_up = Instant::now() + Duration::from_secs(2);
        while !t.is_finished() && Instant::now() < give_up {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// A number nobody else will pick, without a random-number crate: the clock's
/// nanoseconds and the process ID through a mixing function. It needs to
/// differ between receivers, not to resist anyone guessing it.
fn scramble(salt: u64) -> u64 {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let mut x = nanos ^ (u64::from(std::process::id()) << 32) ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // splitmix64's finalizer.
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn run(rx: Receiver<Msg>, status: Arc<Mutex<Status>>, server: String, mut me: Station, interval: Duration) {
    let software = format!("ragchew {}", env!("CARGO_PKG_VERSION"));
    let stream_id = scramble(0) as u32;
    // One socket for the whole session: the spec asks that the source port
    // stay the same across every datagram from one sender.
    let socket = UdpSocket::bind("0.0.0.0:0");
    let mut queue: Vec<Spot> = Vec::new();
    let mut seq: u32 = 0;
    let mut sends = 0u32;
    let mut templates_at: Option<Instant> = None;
    let jitter = |n: u32| interval.mul_f64(JITTER * (scramble(u64::from(n) + 1) % 1001) as f64 / 1000.0);
    let mut due = Instant::now() + interval + jitter(0);
    loop {
        let closing = match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Ok(Msg::Spots(s)) => {
                queue.extend(s);
                false
            }
            Ok(Msg::Station(s)) => {
                me = s;
                false
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => true,
        };
        status.lock().unwrap().queued = queue.len();
        if !closing && Instant::now() < due {
            continue;
        }
        if !queue.is_empty() {
            // In the first three datagrams, and hourly after: the server
            // caches them, but not forever.
            let with_templates =
                sends < 3 || templates_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(3600));
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32);
            let result = socket.as_ref().map_err(|e| e.to_string()).and_then(|sock| {
                let addr = server
                    .to_socket_addrs()
                    .map_err(|e| format!("cannot find {server}: {e}"))?
                    .find(|a| a.is_ipv4())
                    .ok_or_else(|| format!("{server} has no IPv4 address"))?;
                for d in datagrams(&me, &software, &queue, seq, stream_id, now, with_templates) {
                    sock.send_to(&d, addr).map_err(|e| format!("sending to {server}: {e}"))?;
                }
                Ok(())
            });
            let mut st = status.lock().unwrap();
            match result {
                Ok(()) => {
                    seq = seq.wrapping_add(queue.len() as u32);
                    sends += 1;
                    if with_templates {
                        templates_at = Some(Instant::now());
                    }
                    st.sent += queue.len() as u64;
                    st.last_sent = Some(SystemTime::now());
                    st.fault = None;
                    queue.clear();
                }
                Err(e) => {
                    crate::diag_warn!("spot", "{e}");
                    st.fault = Some(e);
                    // Kept for the next attempt, but not without bound: a
                    // machine off the network all evening is not owed every
                    // station it heard once it comes back.
                    let excess = queue.len().saturating_sub(500);
                    queue.drain(..excess);
                }
            }
            st.queued = queue.len();
        }
        if closing {
            return;
        }
        due = Instant::now() + interval + jitter(sends);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::ChannelSet;
    use ragchew::protocol::Decode;
    use ragchew::{js8, olivia, psk};

    fn n1dq() -> Station {
        Station { call: "N1DQ".into(), grid: "FN42hn".into() }
    }

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }

    /// The spec's own worked example of the receiver descriptor and record,
    /// byte for byte.
    #[test]
    fn the_receiver_matches_the_spec() {
        let t = templates();
        let want = hex(
            "00 03 00 24 99 92 00 03 00 01 80 02 FF FF 00 00 76 8F \
             80 04 FF FF 00 00 76 8F 80 08 FF FF 00 00 76 8F 00 00",
        );
        assert_eq!(&t[..want.len()], &want[..]);
        let want = hex(
            "99 92 00 20 04 4E 31 44 51 06 46 4E 34 32 68 6E \
             0D 48 6F 6D 65 62 72 65 77 20 76 35 2E 36 00 00",
        );
        assert_eq!(receiver(&n1dq(), "Homebrew v5.6"), want);
    }

    /// The sender descriptor is the spec's seven-field one with the iMD taken
    /// out, so it is checked against that one with the same line removed.
    #[test]
    fn the_sender_template_is_the_spec_s_less_imd() {
        let t = templates();
        let sender = &t[36..];
        let want = hex(
            "00 02 00 34 52 43 00 06 80 01 FF FF 00 00 76 8F 80 05 00 04 00 00 76 8F \
             80 06 00 01 00 00 76 8F 80 0A FF FF 00 00 76 8F 80 0B 00 01 00 00 76 8F 00 96 00 04",
        );
        assert_eq!(sender, &want[..]);
    }

    fn spot(call: &str, hz: u32) -> Spot {
        Spot { call: call.into(), hz, snr_db: -12, mode: "PSK31".into(), at_unix: 1_200_960_084 }
    }

    /// A datagram read back by a parser that knows only IPFIX: the templates
    /// in it say how to read the records in it. If the two disagree anywhere,
    /// this is where it shows.
    fn parse(d: &[u8]) -> (u32, Vec<Vec<Vec<u8>>>) {
        assert_eq!(&d[..2], &[0, 10], "version");
        assert_eq!(u16::from_be_bytes([d[2], d[3]]) as usize, d.len(), "length");
        let seq = u32::from_be_bytes(d[8..12].try_into().unwrap());
        let mut templates: HashMap<u16, Vec<u16>> = HashMap::new();
        let mut records = Vec::new();
        let mut at = 16;
        while at < d.len() {
            let id = u16::from_be_bytes([d[at], d[at + 1]]);
            let len = u16::from_be_bytes([d[at + 2], d[at + 3]]) as usize;
            assert_eq!(len % 4, 0, "set {id:#x} is not padded to four");
            let set = &d[at + 4..at + len];
            match id {
                2 | 3 => {
                    let tid = u16::from_be_bytes([set[0], set[1]]);
                    let n = u16::from_be_bytes([set[2], set[3]]) as usize;
                    let mut p = if id == 3 { 6 } else { 4 };
                    let mut lens = Vec::new();
                    for _ in 0..n {
                        let fid = u16::from_be_bytes([set[p], set[p + 1]]);
                        lens.push(u16::from_be_bytes([set[p + 2], set[p + 3]]));
                        p += if fid & 0x8000 != 0 { 8 } else { 4 };
                    }
                    assert!(set[p..].iter().all(|&b| b == 0), "template {tid:#x} trails junk");
                    templates.insert(tid, lens);
                }
                tid => {
                    let lens = templates.get(&tid).expect("a record before its template");
                    let mut p = 0;
                    // Stop when what is left could only be padding.
                    while set.len() - p >= 4 || set[p..].iter().any(|&b| b != 0) {
                        let mut rec = Vec::new();
                        for &l in lens {
                            let n = if l == VAR {
                                p += 1;
                                set[p - 1] as usize
                            } else {
                                l as usize
                            };
                            rec.push(set[p..p + n].to_vec());
                            p += n;
                        }
                        records.push(rec);
                    }
                }
            }
            at += len;
        }
        (seq, records)
    }

    #[test]
    fn a_datagram_reads_back_through_its_own_templates() {
        let spots = [spot("W1AW", 14_070_567), spot("DL/K2LOW/P", 7_040_123)];
        let d = datagrams(&n1dq(), "ragchew test", &spots, 7, 0xDEAD_BEEF, 1_200_960_114, true);
        assert_eq!(d.len(), 1);
        let (seq, recs) = parse(&d[0]);
        assert_eq!(seq, 7);
        assert_eq!(recs.len(), 3, "the receiver and two senders");
        assert_eq!(recs[0], vec![b"N1DQ".to_vec(), b"FN42hn".to_vec(), b"ragchew test".to_vec()]);
        assert_eq!(recs[2][0], b"DL/K2LOW/P");
        assert_eq!(recs[2][1], 7_040_123u32.to_be_bytes());
        assert_eq!(recs[2][2], [(-12i8) as u8]);
        assert_eq!(recs[2][3], b"PSK31");
        assert_eq!(recs[2][4], [1], "informationSource must be 1 to count");
        assert_eq!(recs[2][5], 1_200_960_084u32.to_be_bytes());
    }

    /// A full evening splits across datagrams under the MTU, every one
    /// readable on its own, sequence numbers counting reports across them.
    #[test]
    fn many_spots_split_into_datagrams_that_each_stand_alone() {
        let spots: Vec<Spot> = (0..300).map(|i| spot(&format!("W{}AB", i % 10), 14_070_000 + i)).collect();
        let d = datagrams(&n1dq(), "ragchew", &spots, 0, 1, 0, false);
        assert!(d.len() > 1);
        let mut total = 0;
        for g in &d {
            assert!(g.len() <= MAX_DATAGRAM, "{} bytes", g.len());
            // Without templates in the datagram the parser cannot read it, so
            // put them in front the way the server's cache would.
            let mut with = g[..16].to_vec();
            with.extend(templates());
            with.extend(&g[16..]);
            let len = with.len() as u16;
            with[2..4].copy_from_slice(&len.to_be_bytes());
            let (seq, recs) = parse(&with);
            assert_eq!(seq as usize, total, "sequence counts reports");
            total += recs.len() - 1;
        }
        assert_eq!(total, 300);
    }

    #[test]
    fn grids_and_stations() {
        for g in ["FN42", "fn42hn", "EM73tu", "JO22ab12", "RR99XX"] {
            assert!(is_grid(g), "{g}");
        }
        for g in ["", "FN4", "SN42", "FN42yz", "FN42hn1", "4242", "FNAA"] {
            assert!(!is_grid(g), "{g}");
        }
        assert!(n1dq().is_complete());
        assert!(!Station { call: "".into(), grid: "FN42".into() }.is_complete());
        assert!(!Station { call: "N1DQ".into(), grid: "".into() }.is_complete());
        assert!(!Station { call: "N1DQ X".into(), grid: "FN42".into() }.is_complete());
        assert!(same_station("w1aw", "W1AW/P"));
        assert!(same_station("VE/W1AW", "W1AW"));
        assert!(!same_station("W1AW", "W1AX"));
        assert!(!same_station("W1AW", ""));
    }

    #[test]
    fn modes_by_their_adif_names() {
        assert_eq!(adif_mode(ModeId::Js8(js8::Mode::Normal)), "JS8");
        assert_eq!(adif_mode(ModeId::Psk(psk::PSK31)), "PSK31");
        assert_eq!(adif_mode(ModeId::Psk(psk::PSK63)), "PSK63");
        assert_eq!(adif_mode(ModeId::Olivia(olivia::Mode { tones: 8, bandwidth: 500 })), "OLIVIA 8/500");
    }

    fn decode(mode: ModeId, hz: f64, t: f64, text: &str, ends_over: bool) -> Decode {
        Decode { mode, hz, time_s: t, quality: 5.0, snr_db: Some(-7.4), text: text.into(), ends_over }
    }

    const PSK: ModeId = ModeId::Psk(psk::PSK31);
    const JS8: ModeId = ModeId::Js8(js8::Mode::Normal);

    /// USB at 14.070 MHz.
    fn usb(audio: f64) -> Option<f64> {
        Some(14_070_000.0 + audio)
    }

    /// A PSK station typing a character at a time: nothing is reported while
    /// it is still sending, and when it stops, the station after the DE is —
    /// not the one it was calling, and not the front of its own call.
    #[test]
    fn a_psk_over_is_read_whole_once_it_ends() {
        let mut set = ChannelSet::new(15.0);
        let mut sp = Spotter::new();
        let text = "K2N K2N DE W1AW/QRP W1AW/QRP K ";
        let mut t = 10.0;
        let mut got = sp.observe(set.channels(), 0.0, 1000.0, usb, |_| true, "N1DQ");
        for (i, c) in text.chars().enumerate() {
            set.add(decode(PSK, 1500.0, t, &c.to_string(), false));
            t += 0.2;
            // Partway through, the text so far holds "K2N K2N DE W1A".
            if i == 13 {
                got.extend(sp.observe(set.channels(), t, 1000.0 + t, usb, |_| true, "N1DQ"));
            }
        }
        assert!(got.is_empty(), "spotted mid-over: {got:?}");
        let later = t + over_gap_s(PSK) + DECODE_LAG_S + 1.0;
        let got = sp.observe(set.channels(), later, 1000.0 + later, usb, |_| true, "N1DQ");
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].call, "W1AW/QRP");
        assert_eq!(got[0].hz, 14_071_500);
        assert_eq!(got[0].snr_db, -7);
        assert_eq!(got[0].mode, "PSK31");
        // Stamped when the station stopped, not when this noticed.
        assert_eq!(got[0].at_unix, (1000.0 + set.channels()[0].last_heard_s).round() as u32);
        // And once is enough.
        let again = sp.observe(set.channels(), later + 60.0, 1060.0 + later, usb, |_| true, "N1DQ");
        assert!(again.is_empty());
    }

    /// A JS8 over cut by the frame packing mid-call: the station's own
    /// end-of-over flag finishes it, and the whole call is reported.
    #[test]
    fn a_js8_call_split_across_frames_is_read_whole() {
        let mut set = ChannelSet::new(15.0);
        let mut sp = Spotter::new();
        let _ = sp.observe(set.channels(), 0.0, 0.0, usb, |_| true, "N1DQ");
        set.add(decode(JS8, 1000.0, 15.0, "W1JSK DE K2L", false));
        // Past the mode's over gap, 22.5 s after the first frame began, but
        // short of when the next — begun at 30 s, fifteen seconds long — can
        // have been decoded. Nothing heard yet is not the same as finished.
        let got = sp.observe(set.channels(), 44.0, 44.0, usb, |_| true, "N1DQ");
        assert!(got.is_empty(), "{got:?}");
        set.add(decode(JS8, 1000.0, 30.0, "OW 2W ONLY ", true));
        let got = sp.observe(set.channels(), 46.0, 46.0, usb, |_| true, "N1DQ");
        assert_eq!(got.iter().map(|s| s.call.as_str()).collect::<Vec<_>>(), ["K2LOW"]);
    }

    #[test]
    fn what_is_not_reported() {
        let mut set = ChannelSet::new(15.0);
        let mut sp = Spotter::new();
        let _ = sp.observe(set.channels(), 0.0, 0.0, usb, |_| true, "N1DQ");
        set.add(decode(JS8, 1000.0, 15.0, "CQ CQ DE N1DQ/P FN42 ", true));
        set.add(decode(JS8, 1500.0, 15.0, "CQ CQ DE W1AW FN31 ", true));
        set.add(decode(PSK, 2000.0, 15.0, "CQ CQ DE K2N K2N K ", true));
        let wanted = |p: Protocol| p == Protocol::Js8;
        // No dial: heard and read, but nowhere to put it.
        let got = sp.observe(set.channels(), 50.0, 50.0, |_| None, wanted, "N1DQ");
        assert!(got.is_empty(), "{got:?}");
        // The dial comes back: those overs are gone, the next one is not.
        set.add(decode(JS8, 1500.0, 60.0, "CQ CQ DE W1AW FN31 ", true));
        set.add(decode(PSK, 2000.0, 60.0, "CQ CQ DE K2N K2N K ", true));
        let got = sp.observe(set.channels(), 76.0, 76.0, usb, wanted, "N1DQ");
        // Not N1DQ, who is me; not K2N, whose protocol is not wanted.
        assert_eq!(got.iter().map(|s| s.call.as_str()).collect::<Vec<_>>(), ["W1AW"]);
        // W1AW again within the hour: not again.
        set.add(decode(JS8, 1500.0, 90.0, "CQ CQ DE W1AW FN31 ", true));
        assert!(sp.observe(set.channels(), 106.0, 106.0, usb, wanted, "N1DQ").is_empty());
        // An hour on, it is news again.
        set.add(decode(JS8, 1500.0, 4000.0, "CQ CQ DE W1AW FN31 ", true));
        let got = sp.observe(set.channels(), 4016.0, 4016.0, usb, wanted, "N1DQ");
        assert_eq!(got.len(), 1);
    }

    /// Reporting turned on part way through an evening reports from then, not
    /// the backlog stamped as now.
    #[test]
    fn turning_it_on_does_not_report_the_past() {
        let mut set = ChannelSet::new(15.0);
        set.add(decode(JS8, 1500.0, 15.0, "CQ CQ DE W1AW FN31 ", true));
        let mut sp = Spotter::new();
        assert!(sp.observe(set.channels(), 100.0, 100.0, usb, |_| true, "N1DQ").is_empty());
        set.add(decode(JS8, 1500.0, 120.0, "CQ CQ DE W1AW FN31 ", true));
        assert_eq!(sp.observe(set.channels(), 136.0, 136.0, usb, |_| true, "N1DQ").len(), 1);
    }

    /// The thread end to end, over loopback: the timer sends, the datagram
    /// reads back, and dropping the reporter flushes what is still queued.
    #[test]
    fn the_reporter_sends_on_its_timer_and_flushes_on_drop() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let addr = server.local_addr().unwrap().to_string();
        let r = Reporter::to(addr, n1dq(), Duration::from_millis(50));
        r.spots(vec![spot("W1AW", 14_070_567)]);
        let mut buf = [0u8; 2048];
        let (n, from) = server.recv_from(&mut buf).unwrap();
        let (_, recs) = parse(&buf[..n]);
        assert_eq!(recs[1][0], b"W1AW");
        // A long timer now, so only the end can send the next one.
        drop(r);
        let r = Reporter::to(server.local_addr().unwrap().to_string(), n1dq(), Duration::from_secs(3600));
        r.spots(vec![spot("K2N", 7_040_000)]);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(r.status().queued, 1);
        drop(r);
        let (n, from2) = server.recv_from(&mut buf).unwrap();
        let (seq, recs) = parse(&buf[..n]);
        assert_eq!(seq, 0);
        assert_eq!(recs[1][0], b"K2N");
        assert_ne!(from, from2, "two reporters, two sockets");
    }
}
