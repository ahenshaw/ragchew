//! Recording the live capture to disk, so a fault that takes hours to appear
//! can be replayed in seconds.
//!
//! What is written is the resampled 12 kHz mono stream — *exactly* the samples
//! the decoders see, not the sound card's own format. That is the point: a
//! recording made here reproduces a decode failure when it is opened again,
//! because nothing is left between the file and the decoder to behave
//! differently the second time. It is also, deliberately, the format the file
//! view already reads, so debugging an overnight run is
//! `ragchew ~/.local/share/ragchew/ragchew-20260730-013000.wav` and nothing else.
//!
//! Two things follow from "overnight":
//!
//! * The header is rewritten as the recording grows, so a file whose process
//!   was killed — or which is copied while still recording — is a valid, fully
//!   playable WAV of everything written up to that point. A WAV finalised only
//!   on a clean exit is exactly the file you do not have after a crash.
//! * Nothing blocks the capture callback. Samples go down a channel to a writer
//!   thread; if the disk cannot keep up, blocks are dropped and counted rather
//!   than stalling the audio thread.

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::diag::{self, Health};
use crate::{diag_info, diag_warn};

/// Bytes per sample on disk. 16-bit PCM: 86 MB an hour at 12 kHz, so a long
/// night costs under a gigabyte, and it is what [`ragchew::wav`] reads.
const BYTES_PER_SAMPLE: u64 = 2;

/// Blocks of samples the writer may fall behind by before blocks are dropped.
///
/// The capture callback delivers a few hundred samples at a time, so this is
/// seconds of slack — far more than a disk write needs, and bounded so a
/// wedged disk cannot grow the queue until the process is killed for it.
const QUEUE_BLOCKS: usize = 2048;

/// How much audio is written between header rewrites. The rewrite is two seeks
/// and eight bytes; at this cadence a killed process loses at most this much
/// from the end of the file.
const SYNC_EVERY_S: f64 = 5.0;

/// A recording in progress.
///
/// Dropping it closes the channel, which finishes the file and retires the
/// writer thread; the drop waits for that, so a recording stopped from the
/// interface is complete by the time the button comes back up.
pub struct Recorder {
    path: PathBuf,
    tx: SyncSender<Vec<f32>>,
    /// Samples committed to disk, for the readout on the bar.
    written: Arc<AtomicU64>,
    started_unix: f64,
    rate: u32,
    health: Arc<Health>,
    /// Tells the writer to finish. Closing the channel is not enough on its
    /// own: a [`Tap`] left attached to a capture keeps a sender alive, and the
    /// writer would block on `recv` for ever with the drop below waiting on it.
    /// A stop flag makes shutdown independent of who else is still holding one.
    stop: Arc<AtomicBool>,
    writer: Option<thread::JoinHandle<()>>,
}

impl Recorder {
    /// Start recording `rate` Hz mono into a new file in `dir`, named for the
    /// UTC time it started.
    pub fn start(dir: &Path, rate: u32, health: Arc<Health>) -> std::io::Result<Recorder> {
        std::fs::create_dir_all(dir)?;
        let started_unix = diag::unix_now();
        let path = dir.join(format!("ragchew-{}.wav", diag::stamp_compact(started_unix)));
        let file = std::fs::File::create(&path)?;

        let (tx, rx) = sync_channel::<Vec<f32>>(QUEUE_BLOCKS);
        let written = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let w = written.clone();
        let p = path.clone();
        let h = health.clone();
        let s = stop.clone();
        let writer = thread::Builder::new()
            .name("ragchew-record".into())
            .spawn(move || run(file, p, rate, rx, w, h, s))?;

        diag_info!("record", "started {} at {rate} Hz mono 16-bit", path.display());
        Ok(Recorder {
            path,
            tx,
            written,
            started_unix,
            rate,
            health,
            stop,
            writer: Some(writer),
        })
    }

    /// A handle the capture buffer can push samples into. Cloneable, so a
    /// recording survives the operator changing audio device mid-run: the new
    /// stream is simply given another one of these.
    pub fn tap(&self) -> Tap {
        Tap { tx: self.tx.clone(), health: self.health.clone() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Seconds of audio committed to the file.
    pub fn seconds(&self) -> f64 {
        self.written.load(Ordering::Relaxed) as f64 / self.rate as f64
    }

    /// Size of the file so far, in bytes.
    pub fn bytes(&self) -> u64 {
        HEADER_LEN + self.written.load(Ordering::Relaxed) * BYTES_PER_SAMPLE
    }

    /// When the recording started (UTC seconds), so a decode's capture-relative
    /// time can be turned back into a wall-clock one.
    pub fn started_unix(&self) -> f64 {
        self.started_unix
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Then wait for it: the last few blocks and the final header rewrite
        // happen in that thread, and a recording that is one flush short of
        // valid is not worth having.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
        diag_info!(
            "record",
            "stopped {} — {:.1} s, {} bytes",
            self.path.display(),
            self.seconds(),
            self.bytes()
        );
    }
}

/// The capture side of a recording: cheap to clone, and dropping every clone
/// is what tells the writer the audio has stopped.
#[derive(Clone)]
pub struct Tap {
    tx: SyncSender<Vec<f32>>,
    health: Arc<Health>,
}

impl Tap {
    /// Queue a block of samples. Never blocks and never fails: a full queue
    /// drops the block, because this is called from the audio callback and
    /// stalling there costs the capture itself — the thing being recorded.
    /// Dropped blocks are counted, so a recording with gaps says so in the log
    /// rather than quietly being shorter than the night it covers.
    pub fn push(&self, samples: &[f32]) -> bool {
        match self.tx.try_send(samples.to_vec()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                Health::bump(&self.health.record_dropped, 1);
                false
            }
        }
    }
}

// ---- the writer thread ----

/// Bytes before the sample data: the canonical 44-byte RIFF/WAVE header.
const HEADER_LEN: u64 = 44;

/// How long the writer waits on the channel before looking at the stop flag.
/// Short enough that stopping a recording feels immediate, long enough that an
/// idle recorder is not spinning.
const POLL: Duration = Duration::from_millis(100);

fn run(
    file: std::fs::File,
    path: PathBuf,
    rate: u32,
    rx: Receiver<Vec<f32>>,
    written: Arc<AtomicU64>,
    health: Arc<Health>,
    stop: Arc<AtomicBool>,
) {
    let mut w = std::io::BufWriter::new(file);
    if write_header(&mut w, rate, 0).is_err() {
        diag_warn!("record", "could not write header to {}", path.display());
        return;
    }

    let sync_every = (SYNC_EVERY_S * rate as f64) as u64;
    let mut n: u64 = 0;
    let mut since_sync: u64 = 0;
    // Blocks are dropped by `Tap::push`, on the audio thread, which must not
    // stop to log. They are reported from here instead, on the sync tick, and
    // only when the count has moved.
    let mut dropped_seen = 0u64;

    // Set once a stop has been asked for. From then on the queue is drained
    // without waiting: everything already accepted is still written — the queue
    // holds a minute and more of audio, which a stop must not throw away — but
    // nothing new is waited for.
    let mut stopping = false;

    loop {
        // Not a plain `recv`: shutdown must not depend on every sender having
        // been dropped, because a `Tap` left attached to a capture is one, and
        // waiting on it would hang the interface that is trying to stop.
        let block = if stopping {
            match rx.try_recv() {
                Ok(b) => b,
                Err(_) => break, // drained
            }
        } else {
            match rx.recv_timeout(POLL) {
                Ok(b) => b,
                Err(RecvTimeoutError::Timeout) if stop.load(Ordering::Relaxed) => break,
                // Idle: nothing arriving, nobody asking to stop. Commit what is
                // there, so a capture that has gone silent still leaves a file
                // whose header matches its contents.
                Err(RecvTimeoutError::Timeout) => {
                    if since_sync > 0 {
                        since_sync = 0;
                        let _ = sync(&mut w, rate, n);
                        written.store(n, Ordering::Relaxed);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        };
        for &s in &block {
            // Same scaling as `ragchew::wav::write`, so a recording and a file
            // written by the library quantise identically.
            let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            if w.write_all(&v.to_le_bytes()).is_err() {
                diag_warn!("record", "write failed after {n} samples; stopping");
                finish(&mut w, rate, n);
                return;
            }
        }
        n += block.len() as u64;
        since_sync += block.len() as u64;

        if since_sync >= sync_every {
            since_sync = 0;
            // Committed and self-describing: from here the file on disk is a
            // valid WAV of everything so far, whatever happens to the process.
            if sync(&mut w, rate, n).is_err() {
                diag_warn!("record", "could not update header of {}", path.display());
            }
            written.store(n, Ordering::Relaxed);
            let now = diag::get(&health.record_dropped);
            if now > dropped_seen {
                diag_warn!("record", "{} sample blocks dropped in all", now);
                dropped_seen = now;
            }
        }

        stopping |= stop.load(Ordering::Relaxed);
    }
    finish(&mut w, rate, n);
    written.store(n, Ordering::Relaxed);
}

fn finish(w: &mut std::io::BufWriter<std::fs::File>, rate: u32, n: u64) {
    let _ = sync(w, rate, n);
}

/// Flush the samples, then rewrite the two length fields to match.
fn sync(w: &mut std::io::BufWriter<std::fs::File>, rate: u32, n: u64) -> std::io::Result<()> {
    w.flush()?;
    let f = w.get_mut();
    let end = f.stream_position()?;
    f.seek(SeekFrom::Start(0))?;
    let mut head = Vec::with_capacity(HEADER_LEN as usize);
    write_header(&mut head, rate, n)?;
    f.write_all(&head)?;
    f.seek(SeekFrom::Start(end))?;
    f.sync_data()
}

/// The 44-byte canonical header for `n` mono 16-bit samples.
///
/// A recording longer than about 49 hours overflows the 32-bit length fields
/// that RIFF has and nothing can be done about; the fields saturate, so the
/// file stays readable and simply understates its length rather than wrapping
/// to a small number and being truncated by every reader that opens it.
fn write_header<W: Write>(w: &mut W, rate: u32, n: u64) -> std::io::Result<()> {
    let data_len = u32::try_from(n * BYTES_PER_SAMPLE).unwrap_or(u32::MAX);
    w.write_all(b"RIFF")?;
    w.write_all(&data_len.saturating_add(36).to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&1u16.to_le_bytes())?; // mono
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(&(rate * 2).to_le_bytes())?; // byte rate
    w.write_all(&2u16.to_le_bytes())?; // block align
    w.write_all(&16u16.to_le_bytes())?; // bits
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("ragchew-rec-{tag}-{}-{:?}", std::process::id(), thread::current().id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A tone in, the same tone out — through the channel, the writer thread,
    /// the header and `wav::read`. If the round trip does not hold, a recording
    /// is not evidence of anything.
    ///
    /// Also the guard on *stopping*: the samples go in far faster than the
    /// writer drains them, so most of them are still queued when the recorder
    /// is dropped. A stop that abandoned the queue instead of draining it would
    /// silently lose up to a minute of audio off the end of every recording,
    /// and it is the length assertion here that says so.
    #[test]
    fn what_is_recorded_reads_back() {
        let dir = tmp("roundtrip");
        let health = Arc::new(Health::default());
        let rec = Recorder::start(&dir, 12_000, health).unwrap();
        let tap = rec.tap();

        let sent: Vec<f32> =
            (0..24_000).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        for block in sent.chunks(512) {
            assert!(tap.push(block));
        }
        let path = rec.path().to_path_buf();
        drop(tap);
        drop(rec); // finalises and joins

        let (got, rate) = ragchew::wav::read(path.to_str().unwrap()).unwrap();
        assert_eq!(rate, 12_000);
        assert_eq!(got.len(), sent.len());
        // 16-bit quantisation is the only difference allowed: rounding to the
        // grid, plus the 32767/32768 asymmetry between how the library writes
        // and how it reads. Two LSBs covers both and nothing else.
        let worst =
            sent.iter().zip(&got).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 2.0 / 32_768.0, "round trip changed samples by {worst}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The file on disk is a valid, complete WAV *while still recording* — which
    /// is the whole reason the header is rewritten as it goes. An overnight run
    /// that is killed at 3 a.m. must still leave something openable.
    #[test]
    fn a_recording_killed_mid_run_is_still_readable() {
        let dir = tmp("killed");
        let health = Arc::new(Health::default());
        let rec = Recorder::start(&dir, 12_000, health).unwrap();
        let tap = rec.tap();

        // Comfortably past one sync boundary, so there is a committed header
        // to find, and then some more that has not been synced yet.
        let n = (SYNC_EVERY_S * 12_000.0) as usize * 2 + 3_000;
        for block in vec![0.25f32; n].chunks(512) {
            tap.push(block);
        }
        // Wait for the writer to have committed at least one sync, without
        // stopping it: this is a live file, mid-recording.
        let path = rec.path().to_path_buf();
        for _ in 0..200 {
            if rec.seconds() >= SYNC_EVERY_S {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(25));
        }

        // Copy it the way a person would, while it is still being written.
        let copy = dir.join("snapshot.wav");
        std::fs::copy(&path, &copy).unwrap();
        let (got, rate) = ragchew::wav::read(copy.to_str().unwrap()).unwrap();
        assert_eq!(rate, 12_000);
        assert!(
            got.len() >= (SYNC_EVERY_S * 12_000.0) as usize,
            "only {} samples readable from a live recording",
            got.len()
        );
        assert!(got.iter().all(|&s| (s - 0.25).abs() < 1e-3));

        drop(tap);
        drop(rec);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Stopping a recording must not wait on whoever else is holding a tap.
    ///
    /// The tap is deliberately cloneable and lives on the audio buffer, so a
    /// stray one outliving the recorder is an ordinary state, not a bug. If
    /// shutdown depended on every sender being dropped, the writer would sit in
    /// `recv` for ever and the join in `Drop` would take the interface down
    /// with it — a diagnostic feature hanging the app it exists to debug.
    #[test]
    fn stopping_does_not_wait_for_a_stray_tap() {
        let dir = tmp("stray");
        let health = Arc::new(Health::default());
        let rec = Recorder::start(&dir, 12_000, health).unwrap();

        let stray = rec.tap();
        stray.push(&[0.1; 4_096]);
        let path = rec.path().to_path_buf();

        let t = std::time::Instant::now();
        drop(rec); // joins the writer
        let took = t.elapsed();
        assert!(took.as_secs_f64() < 2.0, "stopping took {took:?} with a tap still attached");

        // And it still finalised properly on the way out.
        let (got, _) = ragchew::wav::read(path.to_str().unwrap()).unwrap();
        assert_eq!(got.len(), 4_096);

        // The stray outlives it harmlessly: pushing into a retired recorder is
        // a dropped block, not a panic.
        stray.push(&[0.1; 16]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A full queue drops blocks instead of blocking, because the caller is the
    /// audio callback. The recording loses audio; the capture does not stall.
    #[test]
    fn a_full_queue_drops_rather_than_blocks() {
        let (tx, rx) = sync_channel::<Vec<f32>>(2);
        let health = Arc::new(Health::default());
        let tap = Tap { tx, health: health.clone() };
        assert!(tap.push(&[0.0; 4]));
        assert!(tap.push(&[0.0; 4]));
        // Third has nowhere to go, and must come straight back.
        let t = std::time::Instant::now();
        assert!(!tap.push(&[0.0; 4]));
        assert!(t.elapsed().as_millis() < 50, "push blocked on a full queue");
        drop(rx);
        // A dead writer is also not something to block on.
        assert!(!tap.push(&[0.0; 4]));
        // ...and both losses are on the record.
        assert_eq!(diag::get(&health.record_dropped), 2);
    }
}
