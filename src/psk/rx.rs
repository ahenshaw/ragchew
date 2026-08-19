//! A PSK receiver that runs continuously: audio in as it arrives, characters
//! out as they finish.
//!
//! [`modem`](super::modem) decodes in blocks — transform 8.2 seconds, slice out
//! a candidate, demodulate 256 symbols at once. That is the right shape for
//! *finding* a station, because the gate that decides whether a PSK signal is
//! really there is a statistical test over a whole block and cannot be applied
//! to one character. It is the wrong shape for *reading* one: the text of a
//! whole block appears at once, so a live monitor delivers a dozen characters
//! in one instant and then nothing for four seconds.
//!
//! This is the other half. Given a carrier that the block gate has already
//! vouched for, an [`Rx`] locks to it and emits each character as its symbol
//! completes — which for PSK31 is about five a second, the rate they were
//! typed at.
//!
//! ```text
//! audio ─NCO─▶ ─decimating FIR─▶ ─matched filter─▶ ─Gardner timing─▶
//!       ──differential detect──▶ ──varicode──▶ characters
//! ```
//!
//! Nothing here decides whether a signal is *present*. An [`Rx`] places its
//! symbol clock on whatever it is first given, so pointed at noise it locks to
//! noise and prints it — exactly as the block demodulator would without its
//! gate. The three gate stages in [`modem`](super::modem) are what decide, and
//! a caller must run them first: acquire on a block, then track with one of
//! these.

use rustfft::num_complex::Complex32;

use super::mode::Mode;
use super::modem::CARRIER_TC;
use super::varicode;
use crate::{dsp, SAMPLE_RATE};

/// Baseband samples per symbol the timing loop runs on.
///
/// Sixteen, matching what the block demodulator interpolates its slice up to,
/// so the two recover timing on the same grid and a character lands at the same
/// sample whichever path decoded it.
pub const SPS: usize = 16;

/// Symbols gathered before the symbol clock is placed.
///
/// Acquisition and tracking are different problems and a single loop cannot do
/// both: a gain slow enough to ride out a run of like symbols — which carries
/// no timing information at all — needs some four hundred symbols to walk in
/// from half a symbol out, which is thirteen seconds of PSK31 read at the wrong
/// instant. So the clock is *placed* rather than walked to, by the same search
/// the block demodulator uses — try every phase, keep the one whose phase steps
/// cluster tightest — and the loop only has to hold it there afterwards.
const ACQUIRE_SYMBOLS: usize = 24;

/// Gardner loop gains, for tracking only. See [`ACQUIRE_SYMBOLS`] for why they
/// do not have to acquire.
///
/// A proportional term alone leaves a standing error under any difference
/// between the transmitter's sampling clock and ours; the integral term absorbs
/// that, and is what a receiver left running for hours actually needs.
const TIMING_KP: f64 = 0.02;
const TIMING_KI: f64 = 0.0008;

/// Smoothing on the residual carrier rotation, in symbols.
///
/// The angle is measured off `d²`, which folds the ± data away, so this is
/// averaging a quantity that is constant for a stationary signal. Long enough
/// to be quiet, short enough to follow a drifting transmitter.
const PHASE_TC: f64 = 48.0;

/// One character, and where in the input its last symbol fell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Char {
    /// Sample offset into the audio stream, counted from the receiver's first.
    pub offset: usize,
    pub ch: char,
    /// How strongly the carrier looked like PSK at the moment this character
    /// completed — [`Rx::carrier`], read there rather than at the end of
    /// whatever buffer the caller happened to hand over.
    ///
    /// **A caller that prints these must test it.** Past the end of a
    /// transmission there is nothing on this carrier but noise, and noise read
    /// as differential BPSK is a stream of perfectly good varicode: it frames,
    /// it decodes, and it prints as words. The receiver goes on demodulating
    /// because that is its job and because a station that pauses mid-over comes
    /// back on the same clock; deciding that the over has *ended* is the
    /// caller's, and `modem::CARRIER_THRESHOLD` is where.
    pub carrier: f32,
}

/// A receiver locked to one carrier.
pub struct Rx {
    mode: Mode,
    hz: f64,
    front: Front,
    /// Matched filter for the transmit pulse, at baseband rate.
    matched: Vec<f32>,
    /// Decimated baseband, before the matched filter.
    hist: Ring,
    /// After it — what the symbol detector reads.
    mf: Ring,
    /// Matched-filter outputs produced, and the position of the next symbol
    /// instant among them. Fractional: our sampling clock is not the
    /// transmitter's.
    seen: f64,
    next_sym: f64,
    freq: f64,
    prev_sym: Option<Complex32>,
    /// Held until the clock is placed; see [`ACQUIRE_SYMBOLS`].
    acquiring: Option<Vec<Complex32>>,
    /// Running mean of the unit phasor of `d²`; its half-angle is the residual
    /// carrier rotation. The same quantity `modem::concentration` reports.
    rot: Complex32,
    /// The same mean again over the shorter [`CARRIER_TC`] window, which
    /// the block decoder keeps the identical statistic over — the threshold is
    /// meaningless unless both average across the same number of symbols. Its
    /// magnitude is the concentration statistic, which is what says the station
    /// is still transmitting.
    carrier: Complex32,
    symbols: usize,
    bits: varicode::Stream,
    /// Input samples consumed, for stamping characters.
    consumed: usize,
    /// Input offset of the symbol most recently detected.
    last_sym_offset: usize,
}

/// Input samples a receiver needs before it reports anything: the symbols it
/// places its clock over, plus the filters' warm-up.
///
/// A caller that has to know where a receiver becomes authoritative — because
/// something else is covering the audio until then — asks for this rather than
/// assuming. It must be fed signal throughout, not silence: the clock is placed
/// on whatever is there.
pub fn lock_samples(mode: Mode) -> usize {
    (ACQUIRE_SYMBOLS + 4) * mode.symbol_samples()
}

impl Rx {
    /// Open a receiver on `hz`. `at` is the input sample offset the first
    /// sample handed to [`feed`](Self::feed) will have.
    pub fn new(hz: f64, mode: Mode, at: usize) -> Rx {
        let decim = mode.symbol_samples() / SPS;
        Rx {
            mode,
            hz,
            front: Front::new(hz, decim, mode),
            matched: dsp::hann(2 * SPS),
            hist: Ring::new(4 * SPS),
            mf: Ring::new(4 * SPS),
            seen: 0.0,
            next_sym: 0.0,
            freq: 0.0,
            prev_sym: None,
            acquiring: Some(Vec::with_capacity((ACQUIRE_SYMBOLS + 2) * SPS)),
            rot: Complex32::new(1.0, 0.0),
            // Starts asserting the carrier is there, rather than having to be
            // convinced over the first dozen symbols. A receiver is only opened
            // where the gate has already vouched for a station, so "present" is
            // the true answer at symbol zero — and being wrong the other way
            // would eat the opening characters of every transmission, which is
            // the trap this whole area keeps setting.
            carrier: Complex32::new(1.0, 0.0),
            symbols: 0,
            bits: varicode::Stream::new(),
            consumed: at,
            last_sym_offset: at,
        }
    }

    pub fn hz(&self) -> f64 {
        self.hz
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// How the bits have framed so far — the gate's stage three, kept running
    /// so a receiver whose station has gone can be told from one still reading.
    pub fn framing(&self) -> varicode::Framing {
        self.bits.framing
    }

    /// Symbols detected since the receiver opened.
    pub fn symbols(&self) -> usize {
        self.symbols
    }

    /// How strongly this carrier still looks like PSK, from 0 to 1.
    ///
    /// The concentration statistic of `modem::concentration`, kept running over
    /// [`CARRIER_TC`] symbols instead of measured over a block. Near 1 while a
    /// station is transmitting and near 0 on noise, so it is what says an over
    /// has ended — some seconds before the next acquisition could, which is the
    /// whole reason it is here. Each [`Char`] carries its own reading, taken
    /// when it completed; this is the receiver's current one.
    pub fn carrier(&self) -> f64 {
        self.carrier.norm() as f64
    }

    /// Feed contiguous audio; get back whatever characters completed inside it.
    ///
    /// The audio must continue where the last call left off — the receiver's
    /// loops carry across calls, which is the whole point of it, so a gap would
    /// be read as a phase and timing step.
    pub fn feed(&mut self, samples: &[f32]) -> Vec<Char> {
        let mut out = Vec::new();
        for &x in samples {
            self.consumed += 1;
            let Some(bb) = self.front.push(x) else { continue };
            self.hist.push(bb);
            // The matched filter is evaluated once per baseband sample, so both
            // the acquisition search and the tracking loop read the same
            // stream and a symbol means the same thing to each.
            let y = self.matched_now();
            self.mf.push(y);
            self.seen += 1.0;

            if let Some(buf) = &mut self.acquiring {
                buf.push(y);
                if buf.len() >= ACQUIRE_SYMBOLS * SPS {
                    self.place_clock();
                }
                continue;
            }
            // A symbol is due once there is a sample past it to interpolate to.
            while self.seen >= self.next_sym + 1.0 {
                self.symbol(&mut out);
            }
        }
        out
    }

    /// Put the symbol clock where the signal says it is.
    ///
    /// The same search the block demodulator runs: of the [`SPS`] phases the
    /// baseband allows, take the one whose symbol-to-symbol phase steps cluster
    /// tightest, which is the one inter-symbol interference is not spoiling.
    /// Its residual rotation seeds the carrier estimate, so the first symbol
    /// after acquisition is already detected against the right axis.
    fn place_clock(&mut self) {
        let buf = self.acquiring.take().unwrap_or_default();
        let mut best = (f64::NEG_INFINITY, 0usize, 0.0f64);
        for off in 0..SPS {
            let syms: Vec<Complex32> = (off..buf.len()).step_by(SPS).map(|i| buf[i]).collect();
            let (c, theta) = super::modem::concentration(&syms);
            if c > best.0 {
                best = (c, off, theta);
            }
        }
        let (_, off, theta) = best;
        self.rot = Complex32::from_polar(1.0, 2.0 * theta as f32);

        // `buf` holds positions 0..seen. Resume at the first symbol instant at
        // or after what has already gone past, with the one before it as the
        // reference the differential detector needs.
        let last = off + ((buf.len() - 1 - off) / SPS) * SPS;
        self.prev_sym = Some(buf[last]);
        self.next_sym = (last + SPS) as f64;
        self.symbols = 1;
    }

    /// Detect the symbol at `next_sym`, advance the loops, and push the bit.
    fn symbol(&mut self, out: &mut Vec<Char>) {
        let now = self.filtered(self.next_sym);
        let mid = self.filtered(self.next_sym - SPS as f64 / 2.0);

        // Gardner: the mid-symbol sample is where a transition crosses, so its
        // correlation with the change either side of it is zero only when the
        // symbol instants straddle the transition evenly. It needs no carrier
        // phase, which is why it suits a signal whose phase is the data.
        if let Some(prev) = self.prev_sym {
            // Normalized by the symbol power, or the loop gain would scale with
            // how loud the station is: the same error would be corrected ten
            // times faster on a strong signal than a weak one, and the loop
            // would be tuned for exactly one signal level.
            let p = ((now.norm_sqr() + prev.norm_sqr()) * 0.5) as f64 + 1e-12;
            let e = ((mid.conj() * (now - prev)).re as f64 / p).clamp(-1.0, 1.0);
            // Sampling late puts the mid-symbol sample *past* the zero crossing,
            // so it carries the incoming symbol's sign and `e` comes out
            // positive — whichever way the transition went. The correction is
            // therefore its negation: late means shorten the next interval.
            // With the sign the other way the loop is positive feedback, and it
            // walks a perfectly good clock off the symbol a bit at a time; that
            // cost six characters in a hundred and twenty at 10 dB, where the
            // block demodulator lost none.
            let c = -e;
            self.freq += TIMING_KI * c;
            self.next_sym += SPS as f64 + TIMING_KP * c + self.freq;
        } else {
            self.next_sym += SPS as f64;
        }
        self.last_sym_offset = self.consumed;

        // Differential detection, with the residual carrier rotation taken off.
        if let Some(prev) = self.prev_sym {
            let d = now * prev.conj();
            let m = d.norm_sqr();
            if m > 0.0 {
                let u = (d * d) / m; // unit phasor of d², data folded away
                let a = (1.0 / PHASE_TC) as f32;
                self.rot = self.rot * (1.0 - a) + u * a;
                let b = (1.0 / CARRIER_TC) as f32;
                self.carrier = self.carrier * (1.0 - b) + u * b;
            }
            self.symbols += 1;
            let theta = self.rot.arg() / 2.0;
            let bit = u8::from((d * Complex32::from_polar(1.0, -theta)).re > 0.0);
            // Every character is reported, with the carrier reading taken here
            // rather than at the end of the caller's buffer — a sound card
            // hands over a few symbols at a time and the statistic moves within
            // one. What to do with a character whose carrier has collapsed is
            // the caller's; see [`Char::carrier`].
            if let Some(got) = self.bits.push(bit) {
                out.push(Char {
                    offset: self.last_sym_offset,
                    ch: got.ch,
                    carrier: self.carrier.norm(),
                });
            }
        }
        self.prev_sym = Some(now);
    }

    /// The matched-filter output at a fractional position, linearly
    /// interpolated between the two nearest whole ones.
    fn filtered(&self, at: f64) -> Complex32 {
        let i = at.floor();
        let frac = (at - i) as f32;
        let a = self.mf_at(i);
        let b = self.mf_at(i + 1.0);
        a * (1.0 - frac) + b * frac
    }

    /// A stored matched-filter output, by absolute position in that stream.
    fn mf_at(&self, at: f64) -> Complex32 {
        let back = self.seen - 1.0 - at;
        if back < 0.0 {
            return Complex32::new(0.0, 0.0);
        }
        self.mf.back(back as usize)
    }

    /// The matched filter evaluated on the newest decimated baseband sample.
    fn matched_now(&self) -> Complex32 {
        let mut acc = Complex32::new(0.0, 0.0);
        for (k, &t) in self.matched.iter().enumerate() {
            // y[p] = sum_k h[k] x[p-k], and back(0) is the newest x.
            acc += self.hist.back(k) * t;
        }
        acc
    }
}

// ---- front end ----

/// Mixer and decimating low-pass: real audio at the crate rate in, complex
/// baseband at [`SPS`] samples per symbol out.
struct Front {
    /// The local oscillator, advanced by multiplying rather than by evaluating
    /// a sine: a trigonometric call per input sample per carrier is the one
    /// cost here that a band full of stations would actually notice.
    osc: Complex32,
    inc: Complex32,
    /// Multiplying a unit phasor twelve thousand times a second walks its
    /// magnitude off 1.0, so it is pulled back periodically.
    since_norm: usize,
    taps: Vec<f32>,
    hist: Ring,
    decim: usize,
    since: usize,
}

/// Input samples between renormalizations of the oscillator.
const NORM_EVERY: usize = 1024;

impl Front {
    fn new(hz: f64, decim: usize, mode: Mode) -> Front {
        // Enough taps that everything above the decimated Nyquist is gone
        // before it can fold. The cost is the tap count divided by the
        // decimation, because the dot product is only evaluated on output
        // samples: a dozen multiplies per input sample per carrier.
        let n = 12 * decim + 1;
        // Midway between the signal's skirts and the decimated Nyquist at
        // `SPS/2` times the baud, which is where the transition band fits.
        let fc = 4.6 * mode.baud() / SAMPLE_RATE as f64;
        let w = -std::f64::consts::TAU * hz / SAMPLE_RATE as f64;
        Front {
            osc: Complex32::new(1.0, 0.0),
            inc: Complex32::from_polar(1.0, w as f32),
            since_norm: 0,
            hist: Ring::new(n),
            taps: dsp::low_pass(n, fc),
            decim,
            since: 0,
        }
    }

    /// Take one input sample; yields a baseband sample every `decim`th.
    fn push(&mut self, x: f32) -> Option<Complex32> {
        let z = self.osc * x;
        self.osc *= self.inc;
        self.since_norm += 1;
        if self.since_norm >= NORM_EVERY {
            self.since_norm = 0;
            let m = self.osc.norm();
            if m > 0.0 {
                self.osc /= m;
            }
        }
        self.hist.push(z);
        self.since += 1;
        if self.since < self.decim {
            return None;
        }
        self.since = 0;
        let mut acc = Complex32::new(0.0, 0.0);
        for (k, &t) in self.taps.iter().enumerate() {
            acc += self.hist.back(k) * t;
        }
        Some(acc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::psk::{self, PSK31, PSK63};

    /// A transmission at `hz` in a quiet band, with `lead_s` seconds of noise
    /// in front of it, and the sample the signal starts at.
    ///
    /// The noise matters: see the module docs on the gate.
    fn band(text: &str, hz: f64, mode: Mode, lead_s: f64, snr: f32) -> (Vec<f32>, usize) {
        let sig = psk::encode(text, hz, mode);
        let lead = (lead_s * SAMPLE_RATE as f64) as usize;
        let mut seed = 0x5EEDu64;
        let mut out: Vec<f32> = (0..lead + sig.len() + SAMPLE_RATE as usize)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                ((seed >> 40) as f32 / (1u32 << 24) as f32 - 0.5) * 0.02
            })
            .collect();
        for (i, s) in sig.iter().enumerate() {
            out[lead + i] += s * snr;
        }
        (out, lead)
    }

    /// A receiver opened the way a caller must open one: pointed at a carrier
    /// the block gate has already vouched for, and started on audio that has
    /// the signal in it rather than on the silence before it. An `Rx` does not
    /// decide whether a signal is there — it places its clock on whatever it is
    /// first given, so given noise it locks to noise, which is exactly why the
    /// gate comes first.
    fn on_signal(hz: f64, mode: Mode, at: usize) -> Rx {
        Rx::new(hz, mode, at)
    }

    /// Feed a receiver in small chunks, the way a sound card delivers audio,
    /// and collect what it says and when it said it.
    fn stream(rx: &mut Rx, samples: &[f32], chunk: usize) -> Vec<(usize, Char)> {
        let mut out = Vec::new();
        for (i, block) in samples.chunks(chunk).enumerate() {
            for c in rx.feed(block) {
                out.push((i * chunk + block.len(), c));
            }
        }
        out
    }

    /// The whole point: text arrives as it is sent, not in blocks.
    ///
    /// A block decoder hands over 256 symbols' worth at once. This checks that
    /// the streaming receiver reads the same message *and* spreads it across
    /// the audio — no chunk of the sound card's carrying an implausible share
    /// of a message that took twenty seconds to send.
    #[test]
    fn reads_a_transmission_as_it_arrives() {
        let text = "cq cq de w1aw w1aw k  the quick brown fox jumps over the lazy dog  ";
        let (audio, at) = band(text, 1500.0, PSK31, 1.0, 0.5);

        let mut rx = on_signal(1500.0, PSK31, at);
        // 512 samples is a typical capture callback: 43 ms, a seventh of a
        // PSK31 character.
        let got = stream(&mut rx, &audio[at..], 512);
        let read: String = got.iter().map(|(_, c)| c.ch).collect();

        assert!(
            read.contains("the quick brown fox jumps over the lazy dog"),
            "read {read:?}"
        );

        // Delivered as it was sent: no single callback carried more than a
        // couple of characters, where a block decoder would have handed over
        // the lot in one.
        let mut per_call = std::collections::HashMap::new();
        for (call, _) in &got {
            *per_call.entry(call).or_insert(0usize) += 1;
        }
        let worst = per_call.values().copied().max().unwrap_or(0);
        assert!(worst <= 2, "one callback produced {worst} characters at once");

        // And the timestamps are in order and inside the audio.
        let mut last = 0;
        for (_, c) in &got {
            assert!(c.offset >= last, "offsets went backwards");
            assert!(c.offset <= audio.len(), "offset past the end of the audio");
            last = c.offset;
        }
    }

    /// A character is stamped where it actually was on the air, so a live
    /// monitor and a recording of it agree about when things were said.
    #[test]
    fn characters_are_stamped_where_they_were_sent() {
        let text = "cq de g4abc k  ";
        let lead_s = 2.0;
        let (audio, start) = band(text, 1200.0, PSK31, lead_s, 0.5);

        let mut rx = on_signal(1200.0, PSK31, start);
        let got = rx.feed(&audio[start..]);
        let first = got.first().expect("something decoded");

        // Offsets are into the whole recording, not into what the receiver was
        // handed, so a monitor and a recording of it agree about when a thing
        // was said. The transmission opens with 32 symbols of idle, and the
        // clock is placed over the first 24 of them.
        let rate = SAMPLE_RATE as f64;
        let idle_s = 32.0 / PSK31.baud();
        let at = first.offset as f64 / rate;
        assert!(at >= lead_s, "first character at {at:.2}s, before the signal at {lead_s}s");
        assert!(
            at < lead_s + idle_s + 1.0,
            "first character at {at:.2}s, long after the signal started at {lead_s}s"
        );
    }

    /// Twice the baud, same receiver: the parameters all follow from the mode.
    #[test]
    fn psk63_reads_too() {
        let text = "cq cq de w1aw w1aw k  the quick brown fox jumps over the lazy dog  ";
        let (audio, at) = band(text, 1700.0, PSK63, 1.0, 0.5);
        let mut rx = on_signal(1700.0, PSK63, at);
        let read: String = rx.feed(&audio[at..]).iter().map(|c| c.ch).collect();
        assert!(read.contains("quick brown fox"), "read {read:?}");
    }

    /// Resample by `ratio`, standing in for a capture clock that is not the
    /// transmitter's. Linear interpolation is well below the errors under test.
    fn restretch(x: &[f32], ratio: f64) -> Vec<f32> {
        let n = (x.len() as f64 / ratio) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 * ratio;
                let (j, f) = (t.floor() as usize, (t - t.floor()) as f32);
                let a = x.get(j).copied().unwrap_or(0.0);
                let b = x.get(j + 1).copied().unwrap_or(0.0);
                a * (1.0 - f) + b * f
            })
            .collect()
    }

    /// The symbol clock is tracked, not merely placed.
    ///
    /// Two sound cards are never quite the same rate, and a monitor is left
    /// running for hours: at 200 ppm the symbol instant walks a whole symbol in
    /// under three minutes, and everything after that is noise. This sends a
    /// long message through a capture clock that is off by that much, which the
    /// placement alone cannot survive — with the loop disabled it reads
    /// nothing like the message, and the assertion below is what says the loop
    /// is doing the work rather than the placement carrying it.
    #[test]
    fn a_capture_clock_that_is_not_the_transmitters_is_tracked() {
        // Long enough that an untracked clock is a symbol out by the end.
        let text = "cq de w1aw  ".repeat(50);
        let (audio, at) = band(&text, 1500.0, PSK31, 1.0, 0.5);
        let sent = audio.len() as f64 / SAMPLE_RATE as f64;
        assert!(sent > 120.0, "only {sent:.0}s of audio; too short to drift");

        for ppm in [-200.0, 200.0] {
            let skewed = restretch(&audio, 1.0 + ppm / 1e6);
            let start = (at as f64 / (1.0 + ppm / 1e6)) as usize;
            let mut rx = on_signal(1500.0, PSK31, start);
            let read: String = rx.feed(&skewed[start..]).iter().map(|c| c.ch).collect();

            // Right to the end, not just at the start where it was placed.
            let hits = read.matches("cq de w1aw").count();
            assert!(hits >= 45, "{ppm} ppm: only {hits} of 50 overs survived: {read:?}");
        }
    }

    /// Chunking is the sound card's business, not the receiver's: the same
    /// audio must decode to the same text however it is handed over.
    #[test]
    fn the_callback_size_makes_no_difference() {
        let text = "cq de w1aw  test test test  ";
        let (audio, at) = band(text, 1500.0, PSK31, 1.0, 0.5);
        let audio = &audio[at..];

        let mut whole = on_signal(1500.0, PSK31, at);
        let want: Vec<Char> = whole.feed(audio);
        assert!(!want.is_empty(), "nothing decoded at all");

        for chunk in [1, 7, 128, 512, 4096] {
            let mut rx = on_signal(1500.0, PSK31, at);
            let got: Vec<Char> =
                audio.chunks(chunk).flat_map(|b| rx.feed(b)).collect();
            assert_eq!(got, want, "chunk size {chunk} changed the result");
        }
    }

    /// The carrier reading separates the text from the noise after it.
    ///
    /// A receiver does not stop at the end of an over — it cannot, since a
    /// station that pauses mid-over has to keep its symbol clock — so it goes
    /// on turning noise into varicode, and varicode from noise frames and
    /// decodes and prints as words. Before there was a reading to test, that
    /// reached the screen until the next acquisition gave up on the station,
    /// three scans and a dozen seconds later.
    ///
    /// What is pinned here is only that the statistic *separates*: every
    /// character of the message is well above the threshold and everything
    /// after the carrier stops is below it. Where exactly the threshold sits
    /// between them is `examples/psk_gate.rs`'s to say, not this test's.
    #[test]
    fn the_carrier_reading_tells_the_message_from_the_noise_after_it() {
        let text = "cq de w1aw  test test test  ";
        let (mut audio, at) = band(text, 1500.0, PSK31, 1.0, 0.5);
        // `band` leaves a second of noise at the end; give it eight, which is
        // long enough for the old behavior to fill a line.
        let ends_at = audio.len() - SAMPLE_RATE as usize;
        let mut seed = 0xDEADu64;
        audio.extend((0..7 * SAMPLE_RATE as usize).map(|_| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((seed >> 40) as f32 / (1u32 << 24) as f32 - 0.5) * 0.02
        }));

        let mut rx = on_signal(1500.0, PSK31, at);
        let got = rx.feed(&audio[at..]);
        let t = crate::psk::modem::CARRIER_THRESHOLD as f32;

        // The reading is a mean over [`CARRIER_TC`] symbols, so for that long
        // after the last real symbol it is legitimately mid-fall: a character
        // completing there was mostly modulated by signal and reads high, and
        // counting it as noise would be asking the statistic to see backwards.
        // That window is the irreducible tail — bounded, and the thing being
        // bounded is the whole point.
        let settle = (CARRIER_TC as usize + 1) * PSK31.symbol_samples();
        let during: Vec<Char> = got.iter().copied().filter(|c| c.offset <= ends_at).collect();
        let after: Vec<Char> =
            got.iter().copied().filter(|c| c.offset > ends_at + settle).collect();
        assert!(during.len() > 10, "decoded almost nothing: {during:?}");
        assert!(!after.is_empty(), "no noise decoded at all — the tail proves nothing");

        let weakest_text = during.iter().map(|c| c.carrier).fold(f32::MAX, f32::min);
        let strongest_noise = after.iter().map(|c| c.carrier).fold(0.0, f32::max);
        assert!(
            weakest_text > strongest_noise,
            "text down to {weakest_text:.3} and noise up to {strongest_noise:.3} — \
             the reading does not separate them"
        );
        assert!(
            (strongest_noise..weakest_text).contains(&t),
            "threshold {t:.3} is outside the gap {strongest_noise:.3}..{weakest_text:.3}"
        );

        // And what an operator would actually see: the whole tail, settling
        // window included, has to come to almost nothing. Before the reading
        // existed this was every character the noise framed — some dozens.
        let printed = got
            .iter()
            .filter(|c| c.offset > ends_at && c.carrier >= t)
            .count();
        assert!(printed <= 3, "{printed} characters of noise would still be printed");
    }
}

/// A fixed-size ring of the most recent complex samples, indexed backwards from
/// the newest.
struct Ring {
    buf: Vec<Complex32>,
    at: usize,
}

impl Ring {
    fn new(n: usize) -> Ring {
        Ring { buf: vec![Complex32::new(0.0, 0.0); n.max(1)], at: 0 }
    }
    fn push(&mut self, z: Complex32) {
        self.buf[self.at] = z;
        self.at = (self.at + 1) % self.buf.len();
    }
    /// `back(0)` is the newest sample, `back(1)` the one before it.
    fn back(&self, n: usize) -> Complex32 {
        if n >= self.buf.len() {
            return Complex32::new(0.0, 0.0);
        }
        self.buf[(self.at + self.buf.len() - 1 - n) % self.buf.len()]
    }
}
