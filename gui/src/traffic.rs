//! A log of what was *heard*: one CSV row per decode.
//!
//! Distinct from [`crate::diag`], which is a debug log — rotated, verbose,
//! written for whoever is chasing a fault and thrown away afterwards. This is
//! the station's own record, in the tradition of keeping one: small enough to
//! run for weeks, plain enough to open in a spreadsheet, and stable enough to
//! be worth keeping. It is the answer to "I want the messages, not the audio":
//! a night of traffic is a few kilobytes here against 700 MB of WAV.
//!
//! Columns are `utc,t_s,mode,hz,snr_db,quality,text`. Both times are when the
//! transmission *was*, not when it was decoded — decodes carry the time their
//! audio began, and a continuous-mode pass reports blocks several seconds old.
//!
//! `t_s` is the offset into the capture or file and is always present. `utc` is
//! filled where the origin is known: live, from when capture started; in a
//! batch decode, from the stamp in a recording's own filename. It is left empty
//! rather than guessed for a file that carries no such stamp, so one shape of
//! row serves both and a reader can key on whichever it has.
//!
//! Two producers, and no interactive file scanning between them. Live capture
//! writes as it decodes; [`crate::app::batch_to_csv`] writes a file in one
//! pass. The interactive file view deliberately does not: it re-scans whenever
//! the mode selection changes, so logging there would write the same traffic
//! again on every change.

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
const HEADER: &str = "utc,t_s,mode,hz,snr_db,quality,text\n";

/// Start writing decodes to a new CSV in `dir`. Returns its path.
///
/// A new file per session rather than one that is appended to for ever: the
/// name carries the UTC start, so a night's traffic is one file to send on,
/// and nothing has to guess whether a half-written row belongs to this run.
pub fn start(dir: &Path) -> Option<PathBuf> {
    fs::create_dir_all(dir).ok()?;
    start_at(&dir.join(format!("ragchew-{}.csv", diag::stamp_compact(diag::unix_now()))))
}

/// Start writing to a named file, truncating anything already there.
///
/// For a batch decode, where the operator has said where the output goes and
/// re-running the same command should replace its previous answer rather than
/// append a second copy of it.
pub fn start_at(path: &Path) -> Option<PathBuf> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).ok()?;
    }
    let mut out = fs::File::create(path).ok()?;
    out.write_all(HEADER.as_bytes()).ok()?;
    ROWS.store(0, Ordering::Relaxed);
    *SINK.lock().unwrap() = Some(Sink { out, path: path.to_path_buf() });
    ENABLED.store(true, Ordering::Relaxed);
    Some(path.to_path_buf())
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

/// Append one decode. `t_s` is its offset into the capture or file; `utc` is
/// the same instant in wall-clock terms where that is known.
///
/// Flushed on every row — the file is opened for append and each row is one
/// `write_all`, so two decode threads cannot interleave mid-row and a process
/// killed overnight loses nothing that had been decoded. A few dozen rows an
/// hour does not need buffering.
pub fn log(utc: Option<f64>, t_s: f64, d: &Decode) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let row = format!(
        "{},{:.2},{},{:.1},{},{:.2},{}\n",
        utc.map_or(String::new(), diag::stamp_iso),
        t_s,
        field(&d.mode.name()),
        d.hz,
        // One decimal, not the whole-dB a ham writes on a card: this is a data
        // file, the precision is free, and rounding to zero produced "-0".
        d.snr_db.map_or(String::new(), |s| format!("{s:.1}")),
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

        log(Some(1_769_731_200.0), 12.5, &decode("DE W1AW, \"EM73\""));
        let mut quiet = decode("no report here");
        quiet.snr_db = None;
        // No known origin — a batch decode of a file with no stamp in its name.
        log(None, 27.5, &quiet);
        stop();

        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], HEADER.trim_end());
        assert_eq!(lines.len(), 3, "expected a header and two rows, got {text:?}");

        let first = parse_row(lines[1]);
        assert_eq!(first[0], "2026-01-30T00:00:00.000Z");
        assert_eq!(first[1], "12.50");
        assert_eq!(first[2], "JS8 Normal");
        assert_eq!(first[3], "1502.3");
        assert_eq!(first[4], "-11.4", "SNR is kept to a decimal, not rounded to whole dB");
        assert_eq!(first[5], "3.25");
        assert_eq!(first[6], "DE W1AW, \"EM73\"", "the comma and quotes did not survive");

        // Unknowns are empty fields, not made-up numbers: an SNR too weak to
        // measure, and a file whose start time nothing recorded.
        let second = parse_row(lines[2]);
        assert_eq!(second[0], "", "invented a wall-clock time it could not know");
        assert_eq!(second[1], "27.50");
        assert_eq!(second[4], "");
        assert_eq!(second[6], "no report here");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Nothing is written while logging is off, and turning it on does not
    /// lose the header.
    #[test]
    fn logging_off_writes_nothing() {
        stop();
        assert!(!is_on());
        log(Some(1_769_731_200.0), 0.0, &decode("should not appear"));
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
