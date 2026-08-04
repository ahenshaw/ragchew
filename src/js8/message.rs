//! JS8 message framing: assemble/parse the 87 message bits (`a87`).
//!
//! Layout of `a87`: bits `0..72` payload, `72..75` the 3 "itype" flag bits,
//! `75..87` the 12-bit CRC. The leading bits of the payload select the frame
//! type (`10` free-text, `000` heartbeat/CQ, `011` directed, `11` dense, …).

use crate::js8::crc::ft8_crc12;
use crate::js8::varicode;

/// The 3 itype flag bits carried at `a87[72..75]`.
///
/// JS8 uses these as first/last markers within an "over"; `Last`/`FirstAndLast`
/// cause the decoder to append `<>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IType {
    /// Neither first nor last (itype 0).
    None = 0,
    /// Last frame (itype 2).
    Last = 2,
    /// First and last frame (itype 3).
    FirstAndLast = 3,
}

/// A decoded JS8 message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// Free-text (Huffman) content.
    FreeText(String),
    /// Heartbeat or CQ, e.g. `KN4CRD: CQ CQ CQ EM73`.
    HeartbeatCq(String),
    /// Directed message, e.g. `[FROM TO CMD extra]`.
    Directed(String),
    /// (s,c)-dense compressed text (word list not embedded; returned raw-ish).
    Dense(String),
    /// Compound-callsign frame (not yet fully parsed).
    Compound,
    /// Compound directed frame (not yet fully parsed).
    CompoundDirected,
    /// Unrecognized leading bits.
    Unknown,
}

impl Message {
    /// Render for a reader: what the station said, or a note that we cannot say.
    ///
    /// The two compound frames are the ones we recognise but cannot yet parse,
    /// and what they render as matters, because this string is what reaches the
    /// conversation log, the traffic CSV and the interface. The reference — and
    /// this, until it was seen on the air — returns the frame type's own name,
    /// so `CompoundDirected` appeared in a QSO as though a station had typed
    /// it, in a panel where every other line is something somebody sent.
    ///
    /// Parenthesised and in lower case, then. JS8 message text is upper case
    /// and unbracketed, so this cannot be read as coming from the air, and the
    /// sighting is still reported — a compound-callsign station is at that
    /// frequency at that moment, which is worth knowing even unread.
    pub fn text(&self) -> String {
        match self {
            Message::FreeText(s)
            | Message::HeartbeatCq(s)
            | Message::Directed(s)
            | Message::Dense(s) => s.clone(),
            Message::Compound => "(compound callsign)".to_string(),
            Message::CompoundDirected => "(compound callsign, directed)".to_string(),
            Message::Unknown => String::new(),
        }
    }

    /// Whether this is something a station said, as against a note from the
    /// decoder about a frame it could not read.
    ///
    /// The distinction the interface needs: a station's own words can be
    /// answered, logged and searched; a note about an unparsed frame cannot,
    /// and should not be counted as traffic.
    pub fn is_from_the_air(&self) -> bool {
        matches!(
            self,
            Message::FreeText(_)
                | Message::HeartbeatCq(_)
                | Message::Directed(_)
                | Message::Dense(_)
        )
    }
}

/// Whether this frame's itype flags say it is the last of an over.
///
/// [`IType::Last`] and [`IType::FirstAndLast`] are 2 and 3, so the flag is the
/// middle bit of the three at `a87[72..75]` — which is the same bit the
/// unpackers test before appending `<>` to their text.
///
/// The frame type does not come into it: the flags sit outside the payload, so
/// a directed frame carries them as plainly as a free-text one even though only
/// two of the unpackers render them.
pub fn ends_over(a87: &[u8; 87]) -> bool {
    a87[73] != 0
}

/// Write `n` bits of `x` (MSB first) into `a[off..off+n]`. Mirrors `setbits()`.
fn setbits(a: &mut [u8], off: usize, n: usize, x: u64) {
    for i in 0..n {
        a[off + n - 1 - i] = ((x >> i) & 1) as u8;
    }
}

/// Read `n` bits (MSB first) from `a[start..start+n]` as an integer.
fn getbits(a: &[u8], start: usize, n: usize) -> u64 {
    let mut x = 0u64;
    for i in 0..n {
        x = (x << 1) | a[start + i] as u64;
    }
    x
}

/// Attach the itype flags and CRC-12 to a 72-bit payload, producing full `a87`.
///
/// The CRC is computed over the first 76 bits (payload + itype + one zero pad),
/// matching the reference `ft8_crc(a87, 76, ...)`, then written to bits 75..87.
pub fn finalize(payload72: &[u8; 72], itype: IType) -> [u8; 87] {
    let mut a = [0u8; 87];
    a[..72].copy_from_slice(payload72);
    setbits(&mut a, 72, 3, itype as u64);
    // bit 75 stays 0 (the pad bit included in the 76-bit CRC input)
    let crc = ft8_crc12(&a[..76]);
    a[75..87].copy_from_slice(&crc);
    a
}

/// Verify the CRC embedded in a full 87-bit message.
///
/// The CRC input is bits `0..75` plus a single zero pad at bit 75 (the same
/// bit that later holds `crc[0]`), so bit 75 must be forced to 0 before
/// recomputing — mirroring the reference `check_crc()`.
pub fn crc_ok(a87: &[u8; 87]) -> bool {
    let mut input = [0u8; 76];
    input[..75].copy_from_slice(&a87[..75]);
    // input[75] stays 0
    let crc = ft8_crc12(&input);
    crc[..] == a87[75..87]
}

/// Build the full `a87` for a free-text message.
///
/// Returns the message bits and the number of characters consumed from `text`
/// (a single frame holds only a limited amount of text).
pub fn freetext(text: &str, itype: IType) -> ([u8; 87], usize) {
    let (payload, consumed) = varicode::pack_freetext(text);
    (finalize(&payload, itype), consumed)
}

/// Parse the 87 message bits into a [`Message`], dispatching on the frame type.
pub fn unpack(a87: &[u8; 87]) -> Message {
    match (a87[0], a87[1], a87[2]) {
        (0, 1, 1) => Message::Directed(unpack_directed(a87)),
        (0, 0, 0) => Message::HeartbeatCq(unpack_heartbeat_cq(a87)),
        (1, 0, _) => Message::FreeText(varicode::unpack_freetext(a87)),
        (1, 1, _) => Message::Dense(unpack_dense(a87)),
        (0, 0, 1) => Message::Compound,
        (0, 1, 0) => Message::CompoundDirected,
        _ => Message::Unknown,
    }
}

// ---- callsign / grid helpers (ports of the reference unpackers) ----

fn unpack_call28(x: u32) -> String {
    let c2 = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let c3 = b"0123456789";
    let c4 = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ ";
    let c1 = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ ";
    let mut x = x;
    let mut s = [b' '; 6];
    s[5] = c4[(x % 27) as usize];
    x /= 27;
    s[4] = c4[(x % 27) as usize];
    x /= 27;
    s[3] = c4[(x % 27) as usize];
    x /= 27;
    s[2] = c3[(x % 10) as usize];
    x /= 10;
    s[1] = c2[(x % 36) as usize];
    x /= 36;
    s[0] = if (x as usize) < c1.len() {
        c1[x as usize]
    } else {
        b'?'
    };
    String::from_utf8_lossy(&s).trim().to_string()
}

fn unpack_50(mut x: u64) -> String {
    let v: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ /@";
    let mut s = [0u8; 11];
    let mut o = 10usize;
    s[o] = v[(x % 38) as usize];
    o -= 1;
    x /= 38;
    s[o] = v[(x % 38) as usize];
    o -= 1;
    x /= 38;
    s[o] = v[(x % 38) as usize];
    o -= 1;
    x /= 38;
    s[o] = if x & 1 != 0 { b'/' } else { b' ' };
    o -= 1;
    x /= 2;
    s[o] = v[(x % 38) as usize];
    o -= 1;
    x /= 38;
    s[o] = v[(x % 38) as usize];
    o -= 1;
    x /= 38;
    s[o] = v[(x % 38) as usize];
    o -= 1;
    x /= 38;
    s[o] = if x & 1 != 0 { b'/' } else { b' ' };
    o -= 1;
    x /= 2;
    s[o] = v[(x % 38) as usize];
    o -= 1;
    x /= 38;
    s[o] = v[(x % 38) as usize];
    o -= 1;
    x /= 38;
    s[o] = v[(x % 39) as usize];
    s.iter().filter(|&&c| c != b' ').map(|&c| c as char).collect()
}

fn unpack_grid(ng: i32) -> String {
    let lat = ng % 180;
    let lng = (ng / 180) * 2 - 180;
    let mut t = [0u8; 4];
    t[0] = b'A' + ((179 - lng) / 20) as u8;
    t[1] = b'A' + (lat / 10) as u8;
    t[2] = b'0' + (((179 - lng) % 20) / 2) as u8;
    t[3] = b'0' + (lat % 10) as u8;
    String::from_utf8_lossy(&t).to_string()
}

const HBS: [&str; 8] = [
    "HB", "HB AUTO", "HB AUTO RELAY", "HB AUTO RELAY SPOT", "HB RELAY", "HB RELAY SPOT", "HB SPOT",
    "HB AUTO SPOT",
];
const CQS: [&str; 8] = [
    "CQ CQ CQ", "CQ DX", "CQ QRP", "CQ CONTEST", "CQ FIELD", "CQ FD", "CQ CQ", "CQ",
];

fn unpack_heartbeat_cq(a87: &[u8; 87]) -> String {
    let call = unpack_50(getbits(a87, 3, 50));
    let num = getbits(a87, 53, 16) as u32;
    let bits3 = getbits(a87, 69, 3) as usize;
    let mut ret = format!("{}: ", call);
    if num & 0x8000 != 0 {
        ret.push_str(CQS[bits3]);
    } else {
        ret.push_str(HBS[bits3]);
    }
    if (num & 0x7fff) != 0x7fff {
        ret.push(' ');
        ret.push_str(&unpack_grid((num & 0x7fff) as i32));
    }
    ret
}

const DIRECTED_CMDS: [&str; 32] = [
    "SNR?", "DIT DIT", "NACK", "HEARING?", "GRID?", ">", "STATUS?", "STATUS", "HEARING", "MSG",
    "MSG TO:", "QUERY", "QUERY MSGS?", "QUERY CALL", "RESERVED", "GRID", "INFO?", "INFO", "FB",
    "HW CPY?", "SK", "RR", "QSL?", "QSL", "CMD", "SNR", "NO", "YES", "73", "ACK", "AGN?", "TEXT",
];

fn unpack_directed(a87: &[u8; 87]) -> String {
    let call1 = unpack_call28(getbits(a87, 3, 28) as u32);
    let call2 = unpack_call28(getbits(a87, 31, 28) as u32);
    let cmd = getbits(a87, 59, 5) as usize;
    let extra = getbits(a87, 66, 6) as i32;
    let cmd_text = DIRECTED_CMDS.get(cmd).copied().unwrap_or("");
    let extra_str = if cmd == 25 {
        (extra - 31).to_string()
    } else {
        extra.to_string()
    };
    let cmd_field = if cmd_text.is_empty() {
        cmd.to_string()
    } else {
        cmd_text.to_string()
    };
    format!("[{} {} {} {}]", call1, call2, cmd_field, extra_str)
}

/// JS8Call's (s,c)-dense word list (`jsc_list.cpp`), one word per line.
static JSC_WORDS_RAW: &str = include_str!("jsc_words.txt");

fn jsc_words() -> &'static Vec<&'static str> {
    use std::sync::OnceLock;
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| JSC_WORDS_RAW.lines().collect())
}

/// (s,c)-dense decompression, from JS8Call's `jsc.cpp` (s=7, c=9), a port of the
/// reference `unpack_dense()`.
fn unpack_dense(a87: &[u8; 87]) -> String {
    let words = jsc_words();
    const S: i64 = 7;
    const C: i64 = 9;
    let mut base = [0i64; 8];
    base[1] = S;
    for k in 2..8 {
        base[k] = base[k - 1] + S * C.pow((k - 1) as u32);
    }

    let mut ret = String::new();
    let mut i = 2usize; // bit index into a87
    let mut index: i64 = 0;
    let mut k = 0usize; // nibble number within the current index
    while i + 4 <= 72 {
        let nibble = ((a87[i] as i64) << 3)
            | ((a87[i + 1] as i64) << 2)
            | ((a87[i + 2] as i64) << 1)
            | (a87[i + 3] as i64);
        i += 4;
        if nibble >= S {
            index = index * C + nibble - S;
            k += 1;
        } else {
            index = index * S + nibble + base[k];
            if index >= 0 && (index as usize) < words.len() {
                ret.push_str(words[index as usize]);
            }
            if i < 72 && a87[i] != 0 {
                ret.push(' ');
            }
            i += 1;
            index = 0;
            k = 0;
        }
    }
    if a87[73] != 0 {
        ret.push_str("<>");
    }
    ret
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame types we recognise but cannot parse must not read as though a
    /// station sent them.
    ///
    /// This string goes into the conversation log, the traffic CSV and the
    /// interface, all of which present it as what was said. `CompoundDirected`
    /// turned up in a QSO panel on 14.078 looking exactly like a word somebody
    /// had typed — the first thing asked about it was what it meant.
    #[test]
    fn an_unparsed_frame_does_not_read_as_something_a_station_said() {
        for m in [Message::Compound, Message::CompoundDirected] {
            let t = m.text();
            assert!(!t.is_empty(), "{m:?} says nothing at all, which hides a real station");
            assert!(t.starts_with('(') && t.ends_with(')'), "{m:?} renders as {t:?}");
            // JS8 message text is upper case and unbracketed, so lower case
            // inside parentheses cannot be mistaken for something off the air.
            assert!(
                t.chars().any(|c| c.is_ascii_lowercase()),
                "{t:?} could pass for a transmission"
            );
            assert!(!m.is_from_the_air(), "{m:?} is the decoder talking, not a station");
        }
    }

    /// And what a station really did send is passed through untouched — the
    /// note above must not become a general wrapper.
    #[test]
    fn what_a_station_said_is_passed_through_as_it_was_sent() {
        let said = "KN4CRD: CQ CQ CQ EM73";
        assert_eq!(Message::HeartbeatCq(said.to_string()).text(), said);
        assert!(Message::HeartbeatCq(said.to_string()).is_from_the_air());
        assert_eq!(Message::FreeText("HELLO WORLD".into()).text(), "HELLO WORLD");
        assert!(Message::Directed("[A B C]".into()).is_from_the_air());
        // A frame whose leading bits match nothing is not a sighting worth
        // reporting, and stays silent.
        assert_eq!(Message::Unknown.text(), "");
        assert!(!Message::Unknown.is_from_the_air());
    }
}
