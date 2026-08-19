//! Live decoding of the modes that run continuously, with PSK read as it
//! arrives rather than a block at a time.
//!
//! Olivia and PSK have no schedule, so there is nothing to align to and the
//! band is simply re-scanned. That is right for Olivia, whose blocks are
//! seconds long anyway. It is wrong for PSK31, which sends five characters a
//! second: a scan every four seconds delivers fifteen of them in one instant
//! and then nothing, and the text jumps instead of flowing.
//!
//! So the two are split here:
//!
//! * **Acquisition**, every [`ACQ_STEP_S`] over the last window, is the
//!   existing block scan and is unchanged. It is what finds stations, and for
//!   PSK it is the only thing entitled to an opinion about whether one is
//!   really there — the gate is a statistical test over 256 symbols and cannot
//!   be applied to a single character. Olivia's decodes come from here.
//! * **Tracking**: a PSK station the scan has vouched for gets a
//!   [`psk::rx::Rx`] locked to it, which emits each character as its symbol
//!   completes. Between acquisitions, that is where PSK text comes from.
//!
//! Acquisition is also where all of the cost is — a band scan across six Olivia
//! modes is seconds of CPU, against a cadence of four — so it does not run in
//! the same call as tracking. [`Continuous::due`] hands the window out as an
//! [`Acquisition`], which the caller runs wherever it likes (a worker thread in
//! the app, [`Continuous::feed_blocking`] in tests), and [`Continuous::apply`]
//! folds the result back in. Only one is ever out at a time, so a scan that
//! overruns its cadence delays the next one instead of queueing another behind
//! it, and characters keep arriving throughout.
//!
//! That is not a refinement. With the scan inline, a machine that could not
//! quite afford it consumed audio more slowly than the audio arrived, and fell
//! behind by a little more on every pass — text from a live band landed half a
//! minute late and getting later. Now the worst a scan too expensive for the
//! machine can do is happen less often: PSK is untouched and Olivia thins out.
//!
//! Arbitration falls out of the split. Deciding whether a carrier is really
//! PSK31 or an Olivia signal being read twice is a question about a *station*,
//! not about a character, so [`protocol::decode_all`] settles it once at
//! acquisition — a PSK carrier that loses to an Olivia signal simply never gets
//! a receiver. Nothing has to arbitrate a character that arrived three seconds
//! before the Olivia block it would have been weighed against.
//!
//! The window is not one length. Olivia's decoder needs several of the slowest
//! mode's FEC blocks in front of it before it can sync at all, which is where
//! [`ACQ_WINDOW_S`] comes from; PSK needs one analysis block, 8.2 s at PSK31
//! and 4.1 at PSK63, because that is the unit its gate is calibrated over. So
//! the window is taken from the modes actually enabled, and — more to the point
//! — the *first* scan runs as soon as the buffer holds the shortest of them.
//! Nothing is emitted before then, so on a PSK-only band that is the whole of
//! the startup delay, and with Olivia alongside it PSK text still starts at
//! 8.2 s rather than waiting out a window sized for something else.
//!
//! The audio handed to [`Continuous::feed`] must be contiguous: a receiver's
//! loops carry across calls, so a gap in the stream is a gap in its lock.

use ragchew::protocol::{self, Decode, ModeId};
use ragchew::psk;
use ragchew::SAMPLE_RATE;

/// Audio an acquisition looks at for a block-scanned mode. It must hold several
/// FEC blocks of the slowest Olivia mode for that decoder to sync at all.
pub const ACQ_WINDOW_S: f64 = 24.0;

/// How often acquisition runs, at most. Olivia text appears this long after it
/// is sent; PSK no longer waits for it. A window shorter than twice this is
/// stepped at half its own length instead, so consecutive scans still overlap
/// — see [`Continuous::new`].
pub const ACQ_STEP_S: f64 = 4.0;

/// Audio a mode needs in front of it before a scan can find it at all.
///
/// For PSK that is one analysis block — 256 symbols, which is what the gate's
/// thresholds are calibrated over, and what [`psk::decode_all`] silently skips
/// a mode for want of. Everything else is scanned over the whole window and
/// needs all of it.
fn acq_samples(m: &ModeId) -> usize {
    match m {
        ModeId::Psk(p) => p.block_samples(),
        _ => (ACQ_WINDOW_S * SAMPLE_RATE as f64) as usize,
    }
}

/// Acquisitions a PSK station may go unconfirmed before its receiver is closed.
///
/// Two, so an over that pauses for a few seconds — which is most overs — keeps
/// its receiver and its symbol clock rather than re-acquiring from nothing.
const MISSES_BEFORE_CLOSE: usize = 2;

/// One character off a streaming receiver, as the rest of the app sees it.
///
/// The quality is the one the acquisition measured, not a fresh figure: the
/// gate's statistic is a property of a station over a block, and a single
/// character is a fifth of a second of it. Same for the SNR, which is left
/// unmeasured rather than computed from too little audio — a station's strength
/// is what the scan that found it reported.
fn streamed(mode: psk::Mode, hz: f64, quality: f32, c: psk::rx::Char, rate: f64) -> Decode {
    Decode {
        mode: ModeId::Psk(mode),
        hz,
        time_s: c.offset as f64 / rate,
        quality,
        snr_db: None,
        text: c.ch.to_string(),
        ends_over: false,
    }
}

/// A PSK station being tracked between acquisitions.
struct Station {
    rx: psk::rx::Rx,
    mode: psk::Mode,
    hz: f64,
    /// Carried from the acquisition that confirmed it, so a streamed character
    /// reports the same quality figure as a block-decoded one rather than a
    /// zero the interface would read as "no better than noise".
    quality: f32,
    /// Absolute sample offset from which this receiver, rather than the block
    /// scan, is the authority on what the station said.
    covered_from: usize,
    /// Absolute sample offset this receiver has been fed up to.
    fed_to: usize,
    /// Consecutive acquisitions that have not confirmed it.
    missed: usize,
}

/// The continuous-mode decoder for one live capture.
pub struct Continuous {
    modes: Vec<ModeId>,
    band: (f64, f64),
    /// The acquisition window: the most recent [`Continuous::window`] samples.
    buf: Vec<f32>,
    /// Absolute offset of `buf[0]`.
    at: usize,
    /// Absolute offset at which the next acquisition is due.
    next_acq: usize,
    /// Where the window of the acquisition currently out for scanning began,
    /// if one is out. It says two things: that no other may be started, and
    /// that the audio from there on must be kept, because a station the scan
    /// finds has to be caught up from where it started transmitting.
    pending: Option<usize>,
    /// Audio one acquisition looks at, in samples: what the most demanding
    /// enabled mode needs.
    window: usize,
    /// What the least demanding one needs. Below this a scan can find nothing,
    /// so there is no point running one; at or above it there is, which is why
    /// the first scan does not wait for a full window.
    least: usize,
    /// How far the window slides between acquisitions.
    step: usize,
    stations: Vec<Station>,
    /// Decodes already emitted, so overlapping windows do not report the same
    /// thing twice.
    seen: Vec<(ModeId, f64, f64)>,
    /// The newest time reported on each channel, so nothing is ever emitted
    /// behind something already sent there. See [`Continuous::feed`].
    high_water: Vec<(ModeId, f64, f64)>,
}

impl Continuous {
    pub fn new(modes: Vec<ModeId>, band: (f64, f64)) -> Continuous {
        let full = (ACQ_WINDOW_S * SAMPLE_RATE as f64) as usize;
        let window = modes.iter().map(acq_samples).max().unwrap_or(full);
        let least = modes.iter().map(acq_samples).min().unwrap_or(full);
        // A character straddling the end of one scan's audio has to be whole
        // inside another's, which within a long window the block scan arranges
        // for itself by overlapping its blocks by half. A window only one block
        // long has no room to do that, so the overlap has to come from the
        // cadence instead: never step by more than half a window. At 24 s this
        // leaves [`ACQ_STEP_S`] alone; at PSK63's 4.1 s it halves it.
        let step = ((ACQ_STEP_S * SAMPLE_RATE as f64) as usize).min(window / 2).max(1);
        Continuous {
            modes,
            band,
            buf: Vec::new(),
            at: 0,
            // The first scan is due as soon as one can find anything, not when
            // the window has filled.
            next_acq: least,
            pending: None,
            window,
            least,
            step,
            stations: Vec::new(),
            seen: Vec::new(),
            high_water: Vec::new(),
        }
    }

    /// PSK stations currently being tracked, as `(mode, Hz)` — for the log.
    pub fn tracking(&self) -> Vec<(psk::Mode, f64)> {
        self.stations.iter().map(|s| (s.mode, s.hz)).collect()
    }

    /// Feed contiguous audio; get back what the tracked receivers read in it.
    ///
    /// This is the cheap half and it runs on every callback: no scanning, just
    /// the open receivers being handed the audio they have not had. New
    /// stations arrive through [`due`](Self::due) and [`apply`](Self::apply).
    ///
    /// Times on the returned decodes are seconds from the first sample ever
    /// fed, so a caller that knows when that was knows when everything was.
    pub fn feed(&mut self, samples: &[f32]) -> Vec<Decode> {
        let rate = SAMPLE_RATE as f64;
        self.buf.extend_from_slice(samples);
        let mut out: Vec<Decode> = Vec::new();

        // Every open receiver gets the audio it has not had.
        let end = self.at + self.buf.len();
        let (at, buf) = (self.at, &self.buf);
        self.stations.retain_mut(|s| {
            // A receiver whose audio has gone cannot be carried across the
            // gap — a jump in phase and timing is not something its loops
            // recover from — so it is dropped and re-acquired rather than fed
            // the wrong samples or, worse, quietly skipped for ever while
            // still counting as tracked. Trimming happens below, after this,
            // so in practice nothing reaches it.
            if s.fed_to < at {
                return false;
            }
            if s.fed_to < end {
                let (mode, hz, q) = (s.mode, s.hz, s.quality);
                out.extend(
                    s.rx
                        .feed(&buf[s.fed_to - at..])
                        // The station has to still be there. A receiver goes on
                        // demodulating after an over ends — deliberately, so a
                        // pause mid-over keeps its symbol clock — and what it
                        // demodulates from the noise left behind is varicode
                        // that frames and decodes and prints as words. Without
                        // this the only thing that ever stopped it was the next
                        // acquisition failing to confirm the station, three
                        // scans and a dozen seconds later, and every one of
                        // those seconds reached the screen.
                        .into_iter()
                        .filter(|c| c.carrier >= psk::modem::CARRIER_THRESHOLD as f32)
                        .map(|c| streamed(mode, hz, q, c, rate)),
                );
                s.fed_to = end;
            }
            true
        });

        self.trim();
        out.sort_by(|a, b| a.time_s.partial_cmp(&b.time_s).unwrap());
        self.in_order(out)
    }

    /// Drop audio no scan can look at any more.
    ///
    /// Which is less than it sounds. A window is kept for the scan due next,
    /// and that window ends where the scan comes *due* rather than at the live
    /// edge, so it reaches further back than a window whenever audio has run
    /// ahead of the cadence. A scan already out pins its own window's start on
    /// top of that: what it finds there has to be caught up from where the
    /// station began transmitting.
    ///
    /// The two-window cap over the top is for a worker that has wedged — the
    /// buffer stops growing, and the scan comes back to a window that is simply
    /// older than what can still be caught up, which [`apply`](Self::apply)
    /// handles by clamping.
    fn trim(&mut self) {
        let live = self.at + self.buf.len();
        let mut from = live.saturating_sub(self.window);
        from = from.min(self.next_acq.saturating_sub(self.window));
        if let Some(pinned) = self.pending {
            from = from.min(pinned);
        }
        from = from.max(live.saturating_sub(2 * self.window));
        if from > self.at {
            self.buf.drain(..from - self.at);
            self.at = from;
        }
    }

    /// The window due to be scanned, if one is due and none is already out.
    ///
    /// A scan looks at a whole window once there is one, and at everything
    /// there is before that. The modes too slow to be found in it are the ones
    /// that decline to decode at all, so an early scan is not a worse scan — it
    /// is the same scan with fewer modes able to answer, and it is what gets
    /// PSK text out at 8.2 s instead of 24.
    ///
    /// The window ends where the scan came *due*, not at the live edge of the
    /// buffer. The cadence is a grid, so that is the same stretch of audio
    /// however the sound card happens to cut the stream up — and a decoder
    /// whose text depended on the callback size would be a decoder nobody could
    /// reason about. Nothing is skipped by it: the next window is a whole
    /// window long and reaches back over everything in between.
    pub fn due(&mut self) -> Option<Acquisition> {
        let live = self.at + self.buf.len();
        // A slot whose audio has already been trimmed away cannot be scanned;
        // the grid catches up to what is still held rather than trying.
        if self.next_acq < self.at + self.least {
            let behind = self.at + self.least - self.next_acq;
            self.next_acq += behind.div_ceil(self.step) * self.step;
        }
        if self.pending.is_some() || live < self.next_acq {
            return None;
        }
        let end = self.next_acq;
        let start = end.saturating_sub(self.window).max(self.at);
        if end - start < self.least {
            return None;
        }
        self.pending = Some(start);
        // The slots that went by while a scan was out are gone, not owed:
        // counting them up would be a backlog that can never be worked off,
        // which is what made a machine that could not quite afford the scan
        // fall a little further behind on every pass.
        self.next_acq += self.step;
        if self.next_acq <= live {
            self.next_acq += ((live - self.next_acq) / self.step + 1) * self.step;
        }
        Some(Acquisition {
            samples: self.buf[start - self.at..end - self.at].to_vec(),
            at: start,
            band: self.band,
            modes: self.modes.clone(),
        })
    }

    /// Give up on the scan that is out, so another can be started.
    ///
    /// For a caller whose worker has gone: without it the decoder waits for a
    /// result that is never coming and never scans again.
    pub fn abandon(&mut self) {
        self.pending = None;
    }

    /// Feed audio and run any due scan here and now — the whole pipeline in one
    /// call, for tests and for any caller that does not want a second thread.
    pub fn feed_blocking(&mut self, samples: &[f32]) -> Vec<Decode> {
        let mut out = self.feed(samples);
        if let Some(job) = self.due() {
            out.extend(self.apply(job.run()));
        }
        out
    }

    /// Drop anything older than what a channel has already been told.
    ///
    /// The two sources report on very different delays. A tracked receiver
    /// reports a character the moment it finishes, while an acquisition reports
    /// a whole window at once and so hands over decodes up to
    /// a whole window old. Left alone those interleave: text from half a
    /// minute ago arrives after text from a second ago, and the channels the
    /// interface keeps are explicitly documented to need decodes in time order
    /// — out of order, `ChannelSet` appends to the wrong place and then reads
    /// the next real decode as a long silence, which is the very jumping this
    /// was all meant to cure.
    ///
    /// Per channel, not globally: an Olivia block legitimately lands well
    /// behind a PSK character, and they are different conversations. What is
    /// dropped is only a *station's own* past — which in practice is the
    /// block scan re-reporting phantoms it found in the noise before the
    /// station started, several acquisitions running.
    fn in_order(&mut self, decodes: Vec<Decode>) -> Vec<Decode> {
        let mut kept = Vec::with_capacity(decodes.len());
        for d in decodes {
            let tol = d.mode.assoc_tol_hz();
            match self
                .high_water
                .iter_mut()
                .find(|(m, hz, _)| *m == d.mode && (*hz - d.hz).abs() <= tol)
            {
                Some(w) if d.time_s < w.2 => continue,
                Some(w) => {
                    w.1 = d.hz;
                    w.2 = d.time_s;
                }
                None => self.high_water.push((d.mode, d.hz, d.time_s)),
            }
            kept.push(d);
        }
        kept
    }

    /// Whether this decode has already been reported.
    ///
    /// Windows overlap by design and a PSK station is read by two things at its
    /// handover, so the same character arrives more than once and must be
    /// recognized.
    ///
    /// The rule differs by protocol, and PSK is the one that needs care. Half a
    /// character either side is right for an Olivia block, but a PSK character
    /// can be a single bit — a space is `1` — so the *next* character completes
    /// thirty milliseconds later, well inside half of an average one. That
    /// tolerance ate every space in a re-read stretch.
    ///
    /// One symbol is the right window instead, and it is deliberately *not*
    /// also matched on the text. Two real characters are never that close: the
    /// shortest occupies three symbols once its separator is counted. What can
    /// land there is the block scan and a receiver reading the same character
    /// differently across their handover, and the point of the check is to
    /// settle that rather than to print both — matching on text would call a
    /// disagreement two characters and interleave them.
    fn already_said(&self, d: &Decode) -> bool {
        self.seen.iter().any(|&(m, hz, t)| {
            m == d.mode
                && (hz - d.hz).abs() < d.mode.bandwidth_hz() / 2.0
                && (t - d.time_s).abs() < d.mode.dup_tol_s()
        })
    }

    /// Fold a finished scan back in: emit what the block-decoded modes found,
    /// and open or confirm a receiver for every PSK station it vouched for.
    ///
    /// The audio has moved on since the scan was handed out — that is the whole
    /// point of handing it out — so a receiver opened here is caught up to the
    /// live edge of the buffer rather than to the end of the window that found
    /// it. Nothing is lost in between.
    pub fn apply(&mut self, done: Acquired) -> Vec<Decode> {
        let rate = SAMPLE_RATE as f64;
        let mut out: Vec<Decode> = Vec::new();
        self.pending = None;
        let live = self.at + self.buf.len();

        let (psk_found, others): (Vec<Decode>, Vec<Decode>) =
            done.found.into_iter().partition(|d| matches!(d.mode, ModeId::Psk(_)));

        // Held until the PSK stations below have been settled, because which
        // PSK characters the block is still responsible for depends on where
        // each receiver takes over. Everything then goes through one dedup:
        // windows overlap by design, so without it every acquisition reports
        // the same block again — which reads as text interleaved with older
        // copies of itself, not as an obvious repeat.
        let mut emit = others;

        // PSK: what matters is which *stations* cleared the gate. Group this
        // window's characters by station, keeping the earliest — that is where
        // the station was transmitting, and so where a receiver can lock.
        for s in &mut self.stations {
            s.missed += 1;
        }
        // mode, hz, earliest t, best quality
        let mut seen_now: Vec<(psk::Mode, f64, f64, f32)> = Vec::new();
        for d in &psk_found {
            let ModeId::Psk(mode) = d.mode else { continue };
            match seen_now
                .iter_mut()
                .find(|(m, hz, _, _)| *m == mode && (*hz - d.hz).abs() < mode.baud() / 2.0)
            {
                Some(g) => {
                    g.2 = g.2.min(d.time_s);
                    g.3 = g.3.max(d.quality);
                }
                None => seen_now.push((mode, d.hz, d.time_s, d.quality)),
            }
        }

        for (mode, hz, first_t, quality) in seen_now {
            if let Some(s) = self
                .stations
                .iter_mut()
                .find(|s| s.mode == mode && (s.hz - hz).abs() < mode.baud() / 2.0)
            {
                s.missed = 0;
                s.hz = hz;
                s.quality = quality;
                continue;
            }
            // Opened where the station is actually transmitting, not at the
            // start of the window: a receiver places its clock on whatever it
            // is first given, so one started on the silence before a station
            // locks to that silence and never reads a word of what follows.
            //
            // Clamped to the buffer, which normally changes nothing: the
            // station was found inside audio still held. It bites only when a
            // scan came back so late that its window has been trimmed away, and
            // then the receiver starts as early as it still can.
            let start = ((first_t * rate) as usize).max(self.at).min(live);
            let mut rx = psk::rx::Rx::new(hz, mode, start);
            // Until a receiver has had this much signal it has not placed its
            // clock, so the block scan stays authoritative that far and the two
            // meet without a gap or a repeat.
            let covered_from = start + psk::rx::lock_samples(mode);

            // Caught up on everything held since, so the receiver is locked and
            // current before any new audio reaches it. What it reads here is
            // *kept*, not discarded: the block stops covering the station at
            // `covered_from` and live tracking only begins now, so throwing
            // this away leaves several seconds of the station's first over
            // decoded by nobody.
            let caught = rx.feed(&self.buf[start - self.at..]);
            emit.extend(
                caught
                    .into_iter()
                    .filter(|c| c.offset >= covered_from)
                    // The same test the live path applies, and needed here for
                    // the mirror-image reason: a scan locates a station to
                    // within a block, so a receiver opened at the start of one
                    // begins on however much of the band came before the
                    // station keyed up — and reads that as varicode too.
                    .filter(|c| c.carrier >= psk::modem::CARRIER_THRESHOLD as f32)
                    .map(|c| streamed(mode, hz, quality, c, rate)),
            );

            self.stations.push(Station {
                rx,
                mode,
                hz,
                quality,
                covered_from,
                fed_to: live,
                missed: 0,
            });
        }

        // Block text for a PSK station, up to the point its receiver takes
        // over. Past that the receiver is saying the same thing, better timed.
        for d in psk_found {
            let ModeId::Psk(mode) = d.mode else { continue };
            let covered = self
                .stations
                .iter()
                .find(|s| s.mode == mode && (s.hz - d.hz).abs() < mode.baud() / 2.0)
                .is_some_and(|s| (d.time_s * rate) as usize >= s.covered_from);
            if !covered {
                emit.push(d);
            }
        }

        for d in emit {
            if !self.already_said(&d) {
                self.seen.push((d.mode, d.hz, d.time_s));
                out.push(d);
            }
        }
        let cutoff = (self.at as f64 - self.window as f64) / rate;
        self.seen.retain(|(_, _, t)| *t >= cutoff);

        self.stations.retain(|s| s.missed <= MISSES_BEFORE_CLOSE);
        out.sort_by(|a, b| a.time_s.partial_cmp(&b.time_s).unwrap());
        self.in_order(out)
    }
}

/// One window of audio, waiting to be scanned.
///
/// It carries a copy of the audio rather than a borrow because it is meant to
/// cross a thread: [`run`](Self::run) is several seconds of arithmetic that
/// wants no lock held and nothing waiting on it.
pub struct Acquisition {
    samples: Vec<f32>,
    /// Absolute offset of `samples[0]`, so times come back on the decoder's
    /// clock rather than the window's.
    at: usize,
    band: (f64, f64),
    modes: Vec<ModeId>,
}

impl Acquisition {
    /// The block scan — the whole gate, arbitration included, exactly as a file
    /// scan runs it. No state and no borrows: run it anywhere.
    pub fn run(self) -> Acquired {
        let win_start = self.at as f64 / SAMPLE_RATE as f64;
        Acquired {
            found: protocol::decode_all(&self.samples, self.band.0, self.band.1, &self.modes)
                .into_iter()
                .map(|mut d| {
                    d.time_s += win_start;
                    d
                })
                .collect(),
        }
    }
}

/// What one scan found, on its way back to the decoder that asked for it.
pub struct Acquired {
    found: Vec<Decode>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Noise to put in front of a station: past the window a PSK31-only
    /// decoder uses, which is one analysis block.
    ///
    /// The lead has to be longer than that window for a test to be about
    /// anything real. Until the buffer has filled, no acquisition has run at
    /// all, and a station present through that fill is found in one gulp by the
    /// very first scan — a startup transient the app sees once, and the subject
    /// of [`the_first_text_does_not_wait_for_a_window_it_does_not_need`] rather
    /// than of the tests below. What a monitor actually does for hours
    /// afterwards is find a station a few seconds after it clears the gate,
    /// which is what a lead past the window length sets up.
    fn lead_s() -> f64 {
        psk::PSK31.block_secs() + 2.0
    }

    /// A band with one PSK31 station starting `lead_s` in.
    fn band_with_psk(text: &str, hz: f64, lead_s: f64) -> Vec<f32> {
        let sig = psk::encode(text, hz, psk::PSK31);
        let lead = (lead_s * SAMPLE_RATE as f64) as usize;
        let mut seed = 0x5EEDu64;
        let mut out: Vec<f32> = (0..lead + sig.len() + SAMPLE_RATE as usize * 4)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                ((seed >> 40) as f32 / (1u32 << 24) as f32 - 0.5) * 0.02
            })
            .collect();
        for (i, s) in sig.iter().enumerate() {
            out[lead + i] += s * 0.5;
        }
        out
    }

    /// Nothing waits for a window sized for a mode that is not enabled.
    ///
    /// A PSK31-only decoder can find a station in one analysis block, so that
    /// — not [`ACQ_WINDOW_S`] — is what the first scan waits for, and on a PSK
    /// band that wait *is* the startup delay: before this, a monitor started on
    /// a busy band said nothing for twenty-four seconds and then said all of it
    /// at once.
    #[test]
    fn the_first_text_does_not_wait_for_a_window_it_does_not_need() {
        let text = "cq cq de w1aw w1aw k  the quick brown fox jumps over the lazy dog  \
                    de w1aw ur rst 599 599 name andy qth london hw cpy? k  ";
        // Transmitting almost from the first sample, as a station already in an
        // over is when the app is started on top of it.
        let audio = band_with_psk(text, 1500.0, 0.25);

        let mut c = Continuous::new(vec![ModeId::Psk(psk::PSK31)], (300.0, 2600.0));
        let mut fed = 0usize;
        let mut first = None;
        for block in audio.chunks(512) {
            fed += block.len();
            let got = c.feed_blocking(block);
            if !got.is_empty() && first.is_none() {
                first = Some(fed as f64 / SAMPLE_RATE as f64);
            }
        }

        let at = first.expect("nothing decoded at all");
        // One block to find the station, and at worst one more step before the
        // scan whose window is all signal rather than part lead-in. It measures
        // 8.19 s — the very first scan finds it — and the headroom is for a
        // station whose over starts further inside the first block.
        let bound = psk::PSK31.block_secs() + ACQ_STEP_S + 1.0;
        assert!(at < bound, "first text after {at:.1}s of audio, wanted under {bound:.1}s");
    }

    /// The window is what the enabled modes need, and the first scan is what
    /// the least demanding of them needs.
    ///
    /// Mixing the two is the case worth pinning: Olivia still gets its full
    /// twenty-four seconds, and PSK still does not wait for them. Handing
    /// Olivia's decoder a short window is safe by the same rule PSK's follows —
    /// a decoder given less than one block of a mode reports nothing for it,
    /// rather than reporting something worse.
    #[test]
    fn the_window_follows_the_modes_and_the_first_scan_follows_the_quickest() {
        let secs = |n: usize| n as f64 / SAMPLE_RATE as f64;
        let psk31 = ModeId::Psk(psk::PSK31);
        let psk63 = ModeId::Psk(psk::PSK63);
        let olivia = ModeId::Olivia(ragchew::olivia::OL_8_250);

        let c = Continuous::new(vec![psk31], (300.0, 2600.0));
        assert_eq!(secs(c.window), psk::PSK31.block_secs());
        assert_eq!(c.next_acq, c.window, "waited for more than it needed");

        let c = Continuous::new(vec![psk31, psk63], (300.0, 2600.0));
        assert_eq!(secs(c.window), psk::PSK31.block_secs(), "the slower mode sets the window");
        assert_eq!(secs(c.next_acq), psk::PSK63.block_secs(), "the quicker one goes first");

        // Half a window at most, so consecutive scans overlap where a single
        // window is too short for the block scan to overlap inside it.
        let c = Continuous::new(vec![psk63], (300.0, 2600.0));
        assert!(secs(c.step) <= psk::PSK63.block_secs() / 2.0, "step {} samples", c.step);

        let c = Continuous::new(vec![psk31, olivia], (300.0, 2600.0));
        assert_eq!(secs(c.window), ACQ_WINDOW_S, "Olivia lost its window");
        assert_eq!(secs(c.next_acq), psk::PSK31.block_secs(), "PSK waited for Olivia's window");
        assert_eq!(secs(c.step), ACQ_STEP_S, "a long window should keep the usual cadence");
    }

    /// The point of the whole exercise: text arrives spread across the capture
    /// instead of a block at a time.
    ///
    /// A callback is 512 samples, 43 ms — a seventh of a PSK31 character. If
    /// the old behavior were still in place, every character would land in one
    /// of the handful of callbacks an acquisition falls in, and the busiest
    /// callback would hold fifteen of them.
    #[test]
    fn psk_text_arrives_spread_out_rather_than_in_blocks() {
        let text = "cq cq de w1aw w1aw k  the quick brown fox jumps over the lazy dog  \
                    de w1aw ur rst 599 599 name andy qth london hw cpy? k  ";
        let audio = band_with_psk(text, 1500.0, lead_s());

        let mut c = Continuous::new(vec![ModeId::Psk(psk::PSK31)], (300.0, 2600.0));
        let mut per_call: Vec<usize> = Vec::new();
        let mut read = String::new();
        for block in audio.chunks(512) {
            let got = c.feed_blocking(block);
            per_call.push(got.len());
            read.extend(got.iter().map(|d| d.text.clone()));
        }

        assert!(
            read.contains("quick brown fox jumps over the lazy dog"),
            "did not read the message: {read:?}"
        );

        // The acquisition block itself still delivers its own text at once —
        // the gate needs a whole block and that is not negotiable. Everything
        // after it should trickle. So: most of the message must arrive outside
        // the few fat callbacks.
        let total: usize = per_call.iter().sum();
        let mut sorted = per_call.clone();
        sorted.sort_unstable();
        sorted.reverse();
        let in_fattest_three: usize = sorted.iter().take(3).sum();
        assert!(
            in_fattest_three * 2 < total,
            "{in_fattest_three} of {total} decodes arrived in just three callbacks"
        );
    }

    /// A channel is never told about its own past.
    ///
    /// [`crate::channels::ChannelSet`] documents that decodes arrive in time
    /// order, and breaks in a specific way when they do not: it appends to the
    /// wrong place and then reads the following decode as a long silence, which
    /// starts a spurious new over. That is the jumping this module exists to
    /// cure, so producing it here would be worse than useless.
    ///
    /// The hazard is real rather than theoretical. Two sources report on very
    /// different delays — a receiver the instant a character finishes, an
    /// acquisition a whole window at once — and the block scan finds phantoms
    /// in the noise *before* a station starts, then reports them again on each
    /// of the next several acquisitions, by which time they are half a minute
    /// stale. Before this was fixed they landed scattered through the text:
    /// `lazy` read as `lazm$y`.
    #[test]
    fn a_channel_is_never_told_about_its_own_past() {
        let text = "cq cq de w1aw w1aw k  the quick brown fox jumps over the lazy dog  \
                    de w1aw ur rst 599 599 name andy qth london hw cpy? k  ";
        let audio = band_with_psk(text, 1500.0, lead_s());

        let mut c = Continuous::new(vec![ModeId::Psk(psk::PSK31)], (300.0, 2600.0));
        let mut newest: Vec<(ModeId, f64, f64)> = Vec::new();
        let mut checked = 0;
        for block in audio.chunks(512) {
            for d in c.feed_blocking(block) {
                let tol = d.mode.assoc_tol_hz();
                match newest
                    .iter_mut()
                    .find(|(m, hz, _)| *m == d.mode && (*hz - d.hz).abs() <= tol)
                {
                    Some(w) => {
                        assert!(
                            d.time_s >= w.2,
                            "{:.1} Hz went backwards: {:?} at {:.2}s after {:.2}s",
                            d.hz,
                            d.text,
                            d.time_s,
                            w.2
                        );
                        w.2 = d.time_s;
                    }
                    None => newest.push((d.mode, d.hz, d.time_s)),
                }
                checked += 1;
            }
        }
        assert!(checked > 100, "only {checked} decodes; the test proved little");
    }

    /// Feeding contiguous audio in different sized pieces must not change what
    /// is decoded — the receiver's lock carries across calls, and a sound card
    /// chooses its own buffer size.
    #[test]
    fn the_callback_size_does_not_change_the_text() {
        let text = "cq de w1aw  test test test  de w1aw k  the quick brown fox  ";
        let audio = band_with_psk(text, 1500.0, lead_s());

        let read = |chunk: usize| -> String {
            let mut c = Continuous::new(vec![ModeId::Psk(psk::PSK31)], (300.0, 2600.0));
            audio
                .chunks(chunk)
                .flat_map(|b| c.feed_blocking(b))
                .map(|d| d.text)
                .collect()
        };
        let want = read(4096);
        assert!(!want.is_empty(), "nothing decoded at all");
        for chunk in [512, 1000, 8192] {
            assert_eq!(read(chunk), want, "chunk size {chunk} changed the text");
        }
    }

    /// A band with nothing on it stays silent. The streaming receiver has no
    /// gate of its own, so this is really a test that the gate is still in
    /// front of it.
    #[test]
    fn noise_alone_opens_no_receivers() {
        let mut seed = 0xC0FFEEu64;
        let noise: Vec<f32> = (0..SAMPLE_RATE as usize * 40)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                ((seed >> 40) as f32 / (1u32 << 24) as f32 - 0.5) * 0.08
            })
            .collect();

        let mut c = Continuous::new(vec![ModeId::Psk(psk::PSK31)], (300.0, 2600.0));
        let mut n = 0;
        for block in noise.chunks(4096) {
            n += c.feed_blocking(block).len();
        }
        assert_eq!(n, 0, "printed {n} characters from a band with nothing on it");
        assert!(c.tracking().is_empty(), "tracking {:?} on noise", c.tracking());
    }

    /// A scan is expensive, so at most one is ever out — and the ones that came
    /// due while it was out do not pile up behind it.
    ///
    /// Both halves matter. Queueing them was the fault behind a live capture
    /// sliding into the past: each scan cost more than the cadence it was
    /// started on, so every pass ended owing more scans than the last, and text
    /// arrived later and later until the audio buffer overran.
    #[test]
    fn a_scan_that_overruns_neither_doubles_up_nor_queues() {
        let audio = band_with_psk("cq cq de w1aw w1aw k  ", 1500.0, lead_s());
        let mut c = Continuous::new(vec![ModeId::Psk(psk::PSK31)], (300.0, 2600.0));

        let block = (psk::PSK31.block_secs() * SAMPLE_RATE as f64) as usize;
        c.feed(&audio[..block + 4096]);
        let job = c.due().expect("nothing came due with a whole block in hand");

        // However much audio arrives while it is out, nothing else starts.
        for chunk in audio[block..].chunks(4096) {
            c.feed(chunk);
            assert!(c.due().is_none(), "started a second scan with one already out");
        }

        // And when it comes back, what is due is the audio there is now: one
        // scan, not one for every step that went by while it was out.
        let _ = c.apply(job.run());
        let mut issued = 0;
        while c.due().is_some() {
            let _ = c.apply(Acquired { found: Vec::new() });
            issued += 1;
        }
        assert_eq!(issued, 1, "{issued} scans were owed for the time one scan took");
    }

    /// Text keeps arriving while a scan is out. This is the whole point of
    /// taking the scan off this path: a station already being tracked is read
    /// by its receiver, which knows nothing about scanning and waits for none.
    #[test]
    fn tracking_keeps_reading_while_a_scan_is_out() {
        let text = "cq cq de w1aw w1aw k  the quick brown fox jumps over the lazy dog  \
                    de w1aw ur rst 599 599 name andy qth london hw cpy? k  ";
        let audio = band_with_psk(text, 1500.0, lead_s());
        let mut c = Continuous::new(vec![ModeId::Psk(psk::PSK31)], (300.0, 2600.0));

        // Far enough in that the station is being tracked and a scan is out.
        let mut chunks = audio.chunks(4096);
        let mut held = None;
        for chunk in chunks.by_ref() {
            if c.tracking().is_empty() {
                c.feed_blocking(chunk);
                continue;
            }
            c.feed(chunk);
            held = c.due();
            if held.is_some() {
                break;
            }
        }
        assert!(held.is_some(), "never got a station tracked with a scan out");

        // The scan is never applied — as if the machine were far too slow for
        // it — and the rest of the capture is fed with only the receivers to
        // read it.
        let mut read = String::new();
        for chunk in chunks {
            read.extend(c.feed(chunk).iter().map(|d| d.text.clone()));
        }
        assert!(
            read.contains("lazy dog"),
            "the receiver stopped when the scanner did: {read:?}"
        );
    }

    /// A receiver whose audio has been trimmed away is closed, not skipped.
    ///
    /// Its loops cannot be carried across a gap, so it could only print
    /// nonsense — but silently skipping it is worse than closing it, because it
    /// still counts as tracked and so the scan that would re-acquire the
    /// station finds it already there and confirms it for ever. The station
    /// goes quiet on screen while everything reports it as being read.
    #[test]
    fn a_receiver_whose_audio_has_gone_is_closed_rather_than_skipped() {
        let mut c = Continuous::new(vec![ModeId::Psk(psk::PSK31)], (300.0, 2600.0));
        c.at = 10_000;
        c.stations.push(Station {
            rx: psk::rx::Rx::new(1500.0, psk::PSK31, 0),
            mode: psk::PSK31,
            hz: 1500.0,
            quality: 50.0,
            covered_from: 0,
            fed_to: 0,
            missed: 0,
        });

        c.feed(&vec![0.0; 1200]);
        assert!(c.tracking().is_empty(), "kept a receiver it can never feed again");
    }
}
