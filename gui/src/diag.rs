//! Diagnostics for faults that only show up hours in: a log file, counters the
//! decode threads keep as they run, and a panic hook so a thread that dies
//! leaves a note.
//!
//! The motivating report is "left it running overnight; the waterfall was still
//! scrolling but nothing had decoded since about two in the morning". Nothing in
//! the app could answer that afterwards — the decode threads are detached, they
//! report only by sending decodes, and a thread that has stopped sending looks
//! exactly like a band that has gone quiet. So this records enough, cheaply
//! enough to leave on all night, to tell those apart:
//!
//! * [`Health`] counts *passes* as well as decodes, so "scanned and heard
//!   nothing" is distinguishable from "stopped scanning".
//! * The heartbeat logs [`AudioBuf::lag_s`](crate::audio::AudioBuf::lag_s) — how
//!   far the captured audio has fallen behind the wall clock. Every decoder
//!   slices the buffer by UTC while the waterfall walks it by sample index, so
//!   drift or dropped samples stop decoding while leaving the waterfall perfect.
//!   That is the reported symptom exactly, and nothing else in the app measures
//!   it.
//! * [`install_panic_hook`] catches the case where a decode thread simply died.
//!
//! No dependency on the windowing layer, so the tests run headless.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the epoch, as a float.
pub fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

// ---- the log file ----

struct Sink {
    out: fs::File,
    path: PathBuf,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Whether anything is listening. Read on every log call, so it is an atomic
/// rather than a lock — the point is that a disabled log costs nothing.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Log files kept in the directory. An overnight run writes a few hundred
/// kilobytes, so this is about not leaving litter, not about space.
const KEEP_LOGS: usize = 10;

/// Overrides [`default_dir`] once [`start`] has been given somewhere to write.
/// Recordings go wherever the log went, so `--log-dir` moves both and the two
/// halves of a fault report stay together.
static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Where logs and recordings go: whatever [`start`] was given, else
/// `$XDG_DATA_HOME/ragchew`, else `~/.local/share/ragchew`, else the temp
/// directory.
pub fn data_dir() -> PathBuf {
    if let Some(d) = DIR.lock().ok().and_then(|g| g.clone()) {
        return d;
    }
    default_dir()
}

fn default_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(x).join("ragchew");
    }
    if let Some(h) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(h).join(".local/share/ragchew");
    }
    std::env::temp_dir().join("ragchew")
}

/// Open a log file in `dir` (default [`data_dir`]) and start recording to it.
///
/// Returns the path, or `None` if the directory could not be made — in which
/// case logging stays off and the app runs exactly as before. A monitor must
/// not refuse to monitor because it could not open its log.
pub fn start(dir: Option<&Path>) -> Option<PathBuf> {
    let dir = dir.map(|d| d.to_path_buf()).unwrap_or_else(default_dir);
    fs::create_dir_all(&dir).ok()?;
    *DIR.lock().unwrap() = Some(dir.clone());
    let path = dir.join(format!("ragchew-{}.log", stamp_compact(unix_now())));
    let out = fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
    *SINK.lock().unwrap() = Some(Sink { out, path: path.clone() });
    ENABLED.store(true, Ordering::Relaxed);
    prune(&dir, "ragchew-", ".log", KEEP_LOGS);
    Some(path)
}

/// The log file in use, if any.
pub fn log_path() -> Option<PathBuf> {
    SINK.lock().ok()?.as_ref().map(|s| s.path.clone())
}

/// Delete all but the `keep` newest matching files, oldest first.
///
/// The names carry a sortable UTC stamp, so lexical order is chronological and
/// no file metadata has to be consulted.
fn prune(dir: &Path, prefix: &str, suffix: &str, keep: usize) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut names: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with(prefix) && n.ends_with(suffix)
            })
        })
        .collect();
    names.sort();
    let excess = names.len().saturating_sub(keep);
    for p in names.iter().take(excess) {
        let _ = fs::remove_file(p);
    }
}

/// Write one line. Called by the [`diag_info!`] family rather than directly.
///
/// A failed write is dropped: the log is an observer, and an observer that can
/// take the program down with it is worse than no log. Each line is one
/// `write_all` to a file opened for append, so lines from the capture callback,
/// two decode threads, the heartbeat and the UI do not interleave mid-line.
pub fn write_line(level: &str, target: &str, args: fmt::Arguments) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let line = format!("{} {:<5} {:<10} {}\n", stamp_iso(unix_now()), level, target, args);
    if let Ok(mut g) = SINK.lock() {
        if let Some(s) = g.as_mut() {
            let _ = s.out.write_all(line.as_bytes());
        }
    }
}

#[macro_export]
macro_rules! diag_info {
    ($target:expr, $($arg:tt)*) => {
        $crate::diag::write_line("INFO", $target, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! diag_warn {
    ($target:expr, $($arg:tt)*) => {
        $crate::diag::write_line("WARN", $target, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! diag_error {
    ($target:expr, $($arg:tt)*) => {
        $crate::diag::write_line("ERROR", $target, format_args!($($arg)*))
    };
}

// ---- UTC formatting ----
//
// Two formats off one conversion, rather than a date-time dependency for what
// a log line and a filename need.

/// Civil date from a count of days since 1970-01-01. Hinnant's algorithm.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn parts(unix: f64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let secs = unix.floor() as i64;
    let millis = ((unix - secs as f64) * 1000.0) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    let (y, mo, d) = civil(days);
    (y, mo, d, rem / 3600, (rem / 60) % 60, rem % 60, millis)
}

/// `2026-07-30T02:14:07.123Z` — for log lines.
pub fn stamp_iso(unix: f64) -> String {
    let (y, mo, d, h, mi, s, ms) = parts(unix);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

/// `20260730-021407` — for filenames, and sortable.
pub fn stamp_compact(unix: f64) -> String {
    let (y, mo, d, h, mi, s, _) = parts(unix);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Days since 1970-01-01 for a civil date. The inverse of [`civil`].
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m as i64 - 3 } else { m as i64 + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Find a [`stamp_compact`] anywhere in a filename and read it back as UTC.
///
/// A recording made by this app is named for the moment it started, which is
/// the only record of when its audio *was*. Decoding one offline can therefore
/// date its traffic properly instead of reporting bare offsets into the file.
///
/// Deliberately a search rather than a strict parse of `ragchew-<stamp>.wav`:
/// a file that has been renamed or moved keeps its stamp, and a name with no
/// stamp in it simply yields `None` and the times stay relative.
pub fn stamp_in_name(name: &str) -> Option<f64> {
    let b = name.as_bytes();
    let digits = |i: usize, n: usize| {
        b.get(i..i + n).filter(|s| s.iter().all(|c| c.is_ascii_digit()))
    };
    for i in 0..b.len() {
        // YYYYMMDD-HHMMSS
        let (Some(date), Some(b'-'), Some(time)) =
            (digits(i, 8), b.get(i + 8).copied(), digits(i + 9, 6))
        else {
            continue;
        };
        let num = |s: &[u8], a: usize, n: usize| -> i64 {
            std::str::from_utf8(&s[a..a + n]).unwrap().parse().unwrap()
        };
        let (y, mo, d) = (num(date, 0, 4), num(date, 4, 2) as u32, num(date, 6, 2) as u32);
        let (h, mi, s) = (num(time, 0, 2), num(time, 2, 2), num(time, 4, 2));
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
            continue;
        }
        return Some((days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s) as f64);
    }
    None
}

// ---- panic hook ----

/// Log panics, then hand on to whatever hook was already installed.
///
/// A decode thread is detached and nothing joins it, so a panic in one is
/// invisible: the thread vanishes, decodes stop, and the waterfall — which is
/// driven from the UI thread off the audio buffer — carries on as if nothing
/// had happened. That is indistinguishable, from the outside, from a band that
/// went quiet. This is what makes it distinguishable afterwards.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>").to_string();
        let at = info.location().map_or_else(String::new, |l| format!(" at {l}"));
        // The payload alone: `PanicHookInfo`'s own Display already carries the
        // location and a newline, which would put the message on a second line
        // and repeat what `at` just said.
        let msg = info.payload_as_str().unwrap_or("<non-string panic payload>");
        diag_error!("panic", "thread '{name}' panicked{at}: {msg}");
        diag_error!("panic", "backtrace:\n{}", std::backtrace::Backtrace::force_capture());
        prev(info);
    }));
}

// ---- health counters ----

/// An `f64` kept in an atomic, for timestamps written by one thread and read by
/// another. Ordinary `Relaxed` loads: a heartbeat that reads a value one pass
/// out of date reports a number a second stale, which does not matter, and
/// paying for ordering on every decode pass would.
#[derive(Default)]
pub struct AtomicF64(AtomicU64);

impl AtomicF64 {
    pub fn set(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// What one decode thread has been doing.
///
/// `passes` is the field that matters and the one nothing else in the app had:
/// a thread that is scanning a quiet band still counts passes, so a stalled or
/// dead thread no longer looks like silence on the air.
#[derive(Default)]
pub struct ThreadHealth {
    /// Decode passes begun.
    pub passes: AtomicU64,
    /// Decodes emitted to the UI.
    pub decodes: AtomicU64,
    /// Passes abandoned because the buffer did not hold enough audio for the
    /// window asked for. A count that climbs here means the decoder is asking
    /// for audio the capture has not got — see the heartbeat's `lag`.
    pub short: AtomicU64,
    /// Wide searches for where a delayed stream's frames are actually landing
    /// (JS8 only). Zero here while `short` or `passes` climb and `decodes`
    /// stays flat says the decoder never went looking, which is a fault in the
    /// decoder rather than a quiet band.
    pub searches: AtomicU64,
    /// Cycles that came due and were never decoded, because the thread was
    /// still working through older ones (JS8 only). Anything but zero here is
    /// coverage lost — whatever was transmitted in those cycles was never
    /// looked at, and cannot be recovered.
    pub skipped: AtomicU64,
    /// When the last pass started.
    pub last_pass: AtomicF64,
    /// What the last pass cost, in milliseconds.
    pub last_ms: AtomicU64,
    /// Cleared when the thread returns, by any route.
    pub running: AtomicBool,
    /// Whether this thread was ever meant to exist.
    ///
    /// A thread is only spawned for modes that are actually enabled, so an
    /// operator listening for JS8 alone has no continuous thread and never
    /// had one. Warning that it is "no longer running" every thirty seconds
    /// buries the log in an alarm about a deliberate choice — and the first
    /// thing done with a night's log is to grep for exactly that.
    pub wanted: AtomicBool,
}

impl ThreadHealth {
    pub fn pass_begun(&self) {
        self.passes.fetch_add(1, Ordering::Relaxed);
        self.last_pass.set(unix_now());
    }
    pub fn pass_done(&self, ms: f64) {
        self.last_ms.store(ms as u64, Ordering::Relaxed);
    }
    /// Seconds since the last pass began, or `None` if there has never been one.
    pub fn since_pass(&self) -> Option<f64> {
        let t = self.last_pass.get();
        (t > 0.0).then(|| unix_now() - t)
    }
}

/// Everything the heartbeat reports on, shared between the capture callback,
/// the decode threads, the UI thread and the heartbeat itself.
#[derive(Default)]
pub struct Health {
    /// The cycle-aligned (JS8) decode thread.
    pub js8: ThreadHealth,
    /// The rolling-window (Olivia, PSK) decode thread.
    pub continuous: ThreadHealth,
    /// The band scanner behind it, which is where that thread's cost went when
    /// it was one thread. It is reported separately rather than folded back in:
    /// a scan too expensive for the machine no longer delays anything, so the
    /// only place it can now be seen is its own cost.
    pub acquire: ThreadHealth,
    /// Spectrogram columns the UI has consumed — the waterfall's own pulse.
    /// The reported fault had this climbing while decodes stopped, so the log
    /// has to carry both or it cannot confirm the report.
    pub ui_columns: AtomicU64,
    /// Frames the UI has painted.
    pub ui_frames: AtomicU64,
    /// Errors reported by the capture stream.
    pub stream_errors: AtomicU64,
    /// Sample blocks the recorder could not keep up with.
    pub record_dropped: AtomicU64,
}

impl Health {
    pub fn bump(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }
}

/// Marks one decode thread as running for as long as this is alive.
///
/// A drop guard rather than a flag set and cleared by hand, because the exit
/// worth catching is the one that does not run to the bottom of the function.
/// A panicking thread unwinds through this and the flag clears, so the log says
/// the thread is gone rather than leaving it apparently running for ever with a
/// pass count that never moves.
pub struct RunningGuard {
    health: std::sync::Arc<Health>,
    pick: fn(&Health) -> &ThreadHealth,
}

impl RunningGuard {
    pub fn new(health: std::sync::Arc<Health>, pick: fn(&Health) -> &ThreadHealth) -> RunningGuard {
        pick(&health).running.store(true, Ordering::Relaxed);
        RunningGuard { health, pick }
    }
    /// The counters this guard is watching over.
    pub fn stats(&self) -> &ThreadHealth {
        (self.pick)(&self.health)
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.stats().running.store(false, Ordering::Relaxed);
    }
}

/// Read a counter.
pub fn get(c: &AtomicU64) -> u64 {
    c.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stamp has to be right, because every conclusion drawn from a log is
    /// drawn from when things happened. Checked against dates whose weekday and
    /// leap-year behaviour are known independently.
    #[test]
    fn utc_stamps_are_correct() {
        assert_eq!(stamp_iso(0.0), "1970-01-01T00:00:00.000Z");
        // A leap day, and the second after it.
        assert_eq!(stamp_iso(951_782_400.0), "2000-02-29T00:00:00.000Z");
        assert_eq!(stamp_iso(951_868_799.0), "2000-02-29T23:59:59.000Z");
        assert_eq!(stamp_iso(951_868_800.0), "2000-03-01T00:00:00.000Z");
        // 2100 is not a leap year, which the every-four-years rule gets wrong.
        assert_eq!(stamp_iso(4_107_542_400.0), "2100-03-01T00:00:00.000Z");
        assert_eq!(stamp_iso(1_769_731_200.0), "2026-01-30T00:00:00.000Z");
        assert_eq!(stamp_compact(1_769_731_200.0), "20260130-000000");
        // Milliseconds survive, since decode timing is argued in fractions.
        assert_eq!(stamp_iso(1_769_731_200.456), "2026-01-30T00:00:00.456Z");
    }

    /// Filenames sort chronologically, which is what [`prune`] relies on to
    /// decide what is old without asking the filesystem.
    #[test]
    fn compact_stamps_sort_chronologically() {
        // Deliberately out of order, and spanning a century boundary and a
        // year rollover — the places a fixed-width format could lose ordering.
        let times = [1_769_731_200.0, 951_782_400.0, 4_107_542_400.0, 0.0, 1_769_817_599.0];
        let mut sorted = times;
        sorted.sort_by(f64::total_cmp);

        let lexical = {
            let mut s: Vec<String> = times.iter().map(|&t| stamp_compact(t)).collect();
            s.sort();
            s
        };
        let chronological: Vec<String> = sorted.iter().map(|&t| stamp_compact(t)).collect();
        assert_eq!(lexical, chronological);
    }

    /// Only the newest are kept, and unrelated files are left alone.
    #[test]
    fn pruning_keeps_the_newest() {
        let dir = std::env::temp_dir().join(format!("ragchew-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for i in 0..8 {
            fs::write(dir.join(format!("ragchew-2026073{i}-000000.log")), "x").unwrap();
        }
        fs::write(dir.join("notes.txt"), "keep me").unwrap();
        prune(&dir, "ragchew-", ".log", 3);

        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "notes.txt".to_string(),
                "ragchew-20260735-000000.log".to_string(),
                "ragchew-20260736-000000.log".to_string(),
                "ragchew-20260737-000000.log".to_string(),
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A recording's own name dates its audio, so decoding one offline can
    /// report real times. Round-tripped against [`stamp_compact`] rather than
    /// against hand-computed epochs, and checked on the names this app writes.
    #[test]
    fn a_stamp_in_a_filename_reads_back() {
        for t in [0.0, 951_782_400.0, 1_769_731_200.0, 4_107_542_400.0, 1_769_817_599.0] {
            let name = format!("ragchew-{}.wav", stamp_compact(t));
            assert_eq!(stamp_in_name(&name), Some(t), "{name}");
        }
        // Renamed, moved, or with the stamp buried: still found.
        assert_eq!(stamp_in_name("20260130-000000"), Some(1_769_731_200.0));
        assert_eq!(stamp_in_name("night2/ragchew-20260130-000000.wav"), Some(1_769_731_200.0));
        assert_eq!(stamp_in_name("copy-of-ragchew-20260130-000000-final.wav"),
                   Some(1_769_731_200.0));

        // No stamp, or not a real date: times stay relative rather than wrong.
        assert_eq!(stamp_in_name("recording.wav"), None);
        assert_eq!(stamp_in_name("ragchew.wav"), None);
        assert_eq!(stamp_in_name("ragchew-20261330-000000.wav"), None, "month 13");
        assert_eq!(stamp_in_name("ragchew-20260130-250000.wav"), None, "hour 25");
        assert_eq!(stamp_in_name("1234567-123456.wav"), None, "too few date digits");
    }

    /// `--log-dir` moves the recordings too, so a fault report is one folder
    /// rather than a log here and the audio it describes somewhere else.
    #[test]
    fn the_log_directory_is_where_recordings_go() {
        let dir = std::env::temp_dir().join(format!("ragchew-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log = start(Some(&dir)).expect("a writable directory");
        assert_eq!(log.parent(), Some(dir.as_path()));
        assert_eq!(data_dir(), dir);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A thread that has never run is distinguishable from one that ran a
    /// moment ago — the whole basis of "is it still scanning?".
    #[test]
    fn a_thread_that_never_ran_reports_nothing() {
        let h = ThreadHealth::default();
        assert!(h.since_pass().is_none());
        h.pass_begun();
        assert!(h.since_pass().is_some_and(|s| s < 1.0));
        assert_eq!(get(&h.passes), 1);
    }
}
