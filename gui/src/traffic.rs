//! A log of what was *heard*: one CSV row per decode.
//!
//! Distinct from [`crate::diag`], which is a debug log — rotated, verbose,
//! written for whoever is chasing a fault and thrown away afterwards. This is
//! the station's own record, in the tradition of keeping one: small enough to
//! run for weeks, plain enough to open in a spreadsheet, and stable enough to
//! be worth keeping. It is the answer to "I want the messages, not the audio":
//! a night of traffic is a few kilobytes here against 700 MB of WAV.
//!
//! Columns are `utc,mode,hz,snr_db,quality,text`. The timestamp is when the
//! transmission was, not when it was decoded — decodes carry the time their
//! audio began, and a continuous-mode pass reports blocks several seconds old.
//!
//! Live capture only. A file being scanned is re-scanned whenever the mode
//! selection changes, so logging that would write the same traffic again every
//! time; a file already *is* a record of itself.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use ragchew::protocol::Decode;

use crate::diag;

struct Sink {
    out: fs::File,
    path: PathBuf,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);
static ENABLED: AtomicBool = AtomicBool::new(false);
static ROWS: AtomicU64 = AtomicU64::new(0);

/// The header row, and the definition of the format.
const HEADER: &str = "utc,mode,hz,snr_db,quality,text\n";

/// Start writing decodes to a new CSV in `dir`. Returns its path.
///
/// A new file per session rather than one that is appended to for ever: the
/// name carries the UTC start, so a night's traffic is one file to send on,
/// and nothing has to guess whether a half-written row belongs to this run.
pub fn start(dir: &Path) -> Option<PathBuf> {
    fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("ragchew-{}.csv", diag::stamp_compact(diag::unix_now())));
    let mut out = fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
    if out.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
        out.write_all(HEADER.as_bytes()).ok()?;
    }
    ROWS.store(0, Ordering::Relaxed);
    *SINK.lock().unwrap() = Some(Sink { out, path: path.clone() });
    ENABLED.store(true, Ordering::Relaxed);
    Some(path)
}

/// Stop logging and close the file.
pub fn stop() {
    ENABLED.store(false, Ordering::Relaxed);
    *SINK.lock().unwrap() = None;
}

pub fn is_on() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The file being written, if any.
pub fn path() -> Option<PathBuf> {
    SINK.lock().ok()?.as_ref().map(|s| s.path.clone())
}

/// Rows written this session.
pub fn rows() -> u64 {
    ROWS.load(Ordering::Relaxed)
}

/// Append one decode, stamped with the UTC time its audio began.
///
/// Flushed on every row — the file is opened for append and each row is one
/// `write_all`, so two decode threads cannot interleave mid-row and a process
/// killed overnight loses nothing that had been decoded. A few dozen rows an
/// hour does not need buffering.
pub fn log(utc: f64, d: &Decode) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let row = format!(
        "{},{},{:.1},{},{:.2},{}\n",
        diag::stamp_iso(utc),
        field(&d.mode.name()),
        d.hz,
        d.snr_db.map_or(String::new(), |s| format!("{s:.0}")),
        d.quality,
        field(&d.text)
    );
    if let Ok(mut g) = SINK.lock() {
        if let Some(s) = g.as_mut() {
            if s.out.write_all(row.as_bytes()).is_ok() {
                ROWS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// One CSV field, quoted per RFC 4180.
///
/// Always quoted rather than only when it has to be: decoded text is arbitrary
/// off-air characters, and a rule that is applied unconditionally cannot be got
/// wrong by an input nobody thought of.
///
/// Control characters are replaced rather than escaped. RFC 4180 permits a
/// newline inside a quoted field, but a traffic log's whole use is `grep`,
/// `tail` and `wc -l` — one record per line is worth more here than being able
/// to round-trip a stray CR out of a noisy Olivia block.
fn field(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\"\""),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ragchew::protocol::ModeId;

    fn decode(text: &str) -> Decode {
        Decode {
            mode: ModeId::Js8(ragchew::js8::Mode::Normal),
            hz: 1502.34,
            time_s: 12.5,
            quality: 3.25,
            snr_db: Some(-11.4),
            text: text.to_string(),
        }
    }

    /// Text off the air is arbitrary, and a log that cannot be parsed is not a
    /// log. Every character CSV cares about, in one message.
    #[test]
    fn text_that_would_break_a_csv_is_escaped() {
        assert_eq!(field("hello"), "\"hello\"");
        assert_eq!(field("W1AW, EM73"), "\"W1AW, EM73\"");
        assert_eq!(field("say \"hi\""), "\"say \"\"hi\"\"\"");
        // One record per line, whatever the band delivered.
        assert_eq!(field("two\nlines\r\n"), "\"two lines  \"");
        assert_eq!(field("bell\x07here"), "\"bell here\"");
        assert_eq!(field(""), "\"\"");
    }

    /// A written row reads back as the fields that went in — checked by
    /// parsing, not by comparing against a string this test also wrote.
    #[test]
    fn a_row_round_trips_through_a_csv_reader() {
        let dir = std::env::temp_dir().join(format!("ragchew-csv-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = start(&dir).unwrap();

        log(1_769_731_200.0, &decode("DE W1AW, \"EM73\""));
        let mut quiet = decode("no report here");
        quiet.snr_db = None;
        log(1_769_731_215.0, &quiet);
        stop();

        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], HEADER.trim_end());
        assert_eq!(lines.len(), 3, "expected a header and two rows, got {text:?}");

        let first = parse_row(lines[1]);
        assert_eq!(first[0], "2026-01-30T00:00:00.000Z");
        assert_eq!(first[1], "JS8 Normal");
        assert_eq!(first[2], "1502.3");
        assert_eq!(first[3], "-11");
        assert_eq!(first[4], "3.25");
        assert_eq!(first[5], "DE W1AW, \"EM73\"", "the comma and quotes did not survive");

        // An unmeasurable SNR is an empty field, not a made-up number.
        let second = parse_row(lines[2]);
        assert_eq!(second[3], "");
        assert_eq!(second[5], "no report here");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Nothing is written while logging is off, and turning it on does not
    /// lose the header.
    #[test]
    fn logging_off_writes_nothing() {
        stop();
        assert!(!is_on());
        log(1_769_731_200.0, &decode("should not appear"));
        assert!(path().is_none());
    }

    /// A minimal RFC 4180 reader, so the test parses the file rather than
    /// asserting against a format it assumed.
    fn parse_row(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' if in_quotes && chars.peek() == Some(&'"') => {
                    cur.push('"');
                    chars.next();
                }
                '"' => in_quotes = !in_quotes,
                ',' if !in_quotes => out.push(std::mem::take(&mut cur)),
                c => cur.push(c),
            }
        }
        out.push(cur);
        out
    }
}
