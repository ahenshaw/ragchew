//! BPSK31 modulator, three-stage detection gate, and demodulator.
//!
//! # Transmit
//!
//! One symbol = one bit, differentially encoded: a `1` holds the carrier phase,
//! a `0` reverses it. Each symbol is shaped by a raised cosine two symbols long
//! at one-symbol spacing, so two like-signed symbols sum to a constant envelope
//! and two opposite-signed ones cross through zero. That is what keeps the
//! spectrum inside 60 Hz, and — see below — it is also what makes the mode
//! findable.
//!
//! # Receive
//!
//! BPSK31 has no FEC, no CRC and no sync pattern. Nothing in the demodulator
//! can say "that was a signal": feed it noise and it produces bits, and varicode
//! will turn some of them into letters. So the scan is not "demodulate the band
//! and print what comes out" — it is a gate, and only what survives it is
//! demodulated at all.
//!
//! **Stage one — the envelope line.** Every phase reversal drives the amplitude
//! through zero, so a narrowband slice of a real signal has a spectral line in
//! its *envelope* at exactly the symbol rate. Noise has none. [`screen`] scores
//! that line against the median of its neighbours.
//!
//! **Stage two — phase concentration.** A line at 31.25 Hz only says "amplitude
//! modulated at 31.25 baud", which several Olivia modes also are — they run at
//! the same symbol rate, all of them descended from the same 2000/64. So the
//! survivors are differentially detected and asked whether the symbol-to-symbol
//! phase steps cluster on two antipodal points, which is what makes a signal
//! *BPSK*. Squaring `z[k]·conj(z[k-1])` folds the ± data away and leaves unit
//! phasors that align only for real BPSK.
//!
//! **Stage three — does it frame?** The first two find a BPSK signal at the
//! right baud. This one asks whether what comes out of it is language, chiefly
//! by the structure of its gaps: a varicode word begins and ends with `1`, so
//! during text every gap is exactly the two-zero separator and three zeros in a
//! row cannot happen. See [`believable`].
//!
//! Measured, from `examples/psk_gate.rs` and the tests below:
//!
//! | | noise | other modes, strong | real signal |
//! |---|---|---|---|
//! | stage 1+2 | 0 of 110,640 candidates | often passes | 98% at −10 dB |
//! | stage 3 | — | 0 of 34 blocks | every block to −10 dB |
//!
//! −10 dB in 2.5 kHz is about where BPSK31 stops being copyable at all, so the
//! gate costs no sensitivity worth having.
//!
//! One collision the gate cannot settle on its own is Olivia 4/125, which
//! shares the symbol rate *and* is narrow enough to sit inside the analysis
//! slice. That is settled a level up, in [`protocol`](crate::protocol), where
//! Olivia's Walsh correlation can prove itself and PSK31 cannot.
//!
//! Note that all three stages are ratios — against the local noise floor, or
//! against what random bits frame at. A band with no noise in it has no floor
//! to measure against, and leakage from a distant station will score as well as
//! the station does. Real audio always obliges; synthetic bands must be asked.

use rustfft::num_complex::Complex32;

use super::mode::{Mode, SLOTS_PER_SYMBOL};
use super::varicode;
use crate::dsp;
use crate::SAMPLE_RATE;

/// Bins cut out of the band spectrum around a candidate — the analysis slice.
///
/// Fixed rather than derived, so a block holds the same number of symbols
/// whatever the baud and the gate's thresholds mean the same thing in every
/// mode. At 31.25 baud it is a 125 Hz slice of 8.192 s.
pub const SLICE_BINS: usize = 1024;

/// Symbols in one analysis block.
pub const SYMBOLS_PER_BLOCK: usize = SLICE_BINS / SLOTS_PER_SYMBOL;

/// The envelope line's bin in a [`SLICE_BINS`]-point transform of the baseband
/// envelope. The baseband runs at four samples per symbol, so the symbol rate
/// sits at a quarter of its sample rate — a quarter of the way to the top bin.
const LINE_BIN: usize = SLICE_BINS / SLOTS_PER_SYMBOL;

/// Bins either side of [`LINE_BIN`] the line is looked for in, for clock error
/// and drift.
const LINE_SLACK: usize = 2;

/// Bins either side of [`LINE_BIN`] excluded from the background estimate, so a
/// strong line's own skirts do not raise the floor it is measured against.
const LINE_GUARD: usize = 6;

/// Envelope-spectrum bins the background median is taken over: clear of DC and
/// of the slice's own edge.
const BG: std::ops::Range<usize> = 80..480;

/// What the envelope-line score reads on noise — the median over a band of it.
/// Quality is quoted in multiples of this, so 1.0 is "no better than noise".
pub const NOISE_FLOOR: f64 = 2.6;

/// Stage one. A whole sweep of a noise-only band clears this about half the
/// time, which is the point: it is a cheap screen, set low enough to cost no
/// sensitivity, and the stages after it are what decide.
pub const SCREEN_THRESHOLD: f64 = 11.2;

/// Stage two: phase concentration a candidate must reach to be demodulated.
/// The highest any of 110,640 noise candidates reached was 0.225, and this sits
/// at the 99.99th percentile of that distribution.
pub const CONC_THRESHOLD: f64 = 0.204;

/// Symbols the carrier statistic is averaged over, wherever it is computed.
///
/// Here rather than beside either user, because the block decoder and the
/// streaming receiver both keep it, and a threshold measured over one window is
/// meaningless against another. The two must average over the same number of
/// symbols or [`CARRIER_THRESHOLD`] means two different things.
///
/// Short on purpose, and separate from the phase estimate's own smoothing: that
/// is tracking something which barely moves and wants a long average, while
/// this is watching for something to stop, where every symbol of averaging is a
/// symbol of noise printed after it did.
pub const CARRIER_TC: f64 = 12.0;

/// The same statistic as [`CONC_THRESHOLD`], applied to decide that a station
/// being read has stopped transmitting rather than that one is there at all.
/// Tested per character, in both the block decoder and
/// [`crate::psk::rx::Rx::carrier`].
///
/// Higher than `CONC_THRESHOLD` and it has to be, which is the thing to
/// understand before touching it. That figure is calibrated over a 256-symbol
/// block; this one runs over twelve, so its noise distribution is far broader —
/// a twelve-symbol mean of random phasors sits around 0.29 and reaches past 0.6
/// often enough to print a word. Reusing 0.204 here would gate nothing at all.
///
/// The two are also answering different questions, and the costs are not
/// symmetric. Stage two asks "is there a station here", where a false alarm
/// invents one out of noise and is expensive. This asks "is the station I am
/// already reading still there", where being slightly too eager costs a
/// character at the end of an over and being too slow costs a line of rubbish
/// after every one.
///
/// Measured — section G of `examples/psk_gate.rs`, and the number is a knee
/// rather than a midpoint. Across both modes, noise after a transmission
/// reaches 0.56 at its worst; text at every SNR the gate can actually find a
/// station at reaches down to 0.51 for PSK63 at its own −6 dB floor. The two
/// distributions therefore *touch*, and there is no threshold that is free.
///
/// | | text lost at the weakest usable signal | noise still printed |
/// |---|---|---|
/// | 0.50 | 0.0% | 0.21 / 0.28% |
/// | **0.55** | **0.0 / 0.9%** | **0.05 / 0.03%** |
/// | 0.60 | 0.0 / 3.1% | 0.00% |
/// | 0.70 | 0.0 / 13.3% | 0.00% |
///
/// 0.55 is where the noise falls by most of an order of magnitude for under one
/// per cent of the weakest text. Going on to 0.60 buys the last twentieth of a
/// per cent of noise for three per cent of a weak PSK63 station, and 0.70 —
/// which is where an eyeballed midpoint of the two distributions would have put
/// it — costs one character in seven of it. Nothing above 0.55 is worth its
/// price, which is not what looking at the histogram suggests.
pub const CARRIER_THRESHOLD: f64 = 0.55;

/// How much finer the demodulator's baseband is than the gate's.
///
/// Zero-padding the same slice interpolates it exactly, giving 16 samples per
/// symbol to recover timing on without widening the filter or re-reading the
/// input.
const DEMOD_OVERSAMPLE: usize = 4;

const DEMOD_LEN: usize = SLICE_BINS * DEMOD_OVERSAMPLE;
const SLOTS_PER_SYMBOL_DEMOD: usize = SLOTS_PER_SYMBOL * DEMOD_OVERSAMPLE;

/// Symbols of idle a transmission is topped and tailed with.
///
/// Idle is an unbroken run of reversals — the strongest envelope line the mode
/// can produce — so it is both what operators send between overs and what gives
/// a receiver the best chance of finding the signal before there is any text.
///
/// Public because it is the offset of the first character in the burst, which
/// is what anyone timing a transmission against its text has to start from.
pub const IDLE_SYMBOLS: usize = 32;

// ---- modulation ----

/// Modulate a differential bit stream at `center_hz`.
///
/// A `1` holds the phase, a `0` reverses it.
pub fn synthesize(bits: &[u8], center_hz: f64, mode: Mode) -> Vec<f32> {
    let sps = mode.symbol_samples();

    let mut pol = 1.0f64;
    let syms: Vec<f64> = bits
        .iter()
        .map(|&b| {
            if b == 0 {
                pol = -pol;
            }
            pol
        })
        .collect();

    // A raised cosine two symbols long at one-symbol spacing: like-signed
    // neighbours sum to a constant, opposite-signed ones cross through zero.
    let shape = dsp::hann(2 * sps);

    let mut base = vec![0.0f64; syms.len() * sps + 2 * sps];
    for (k, &p) in syms.iter().enumerate() {
        for (i, &s) in shape.iter().enumerate() {
            base[k * sps + i] += p * s as f64;
        }
    }

    let w = 2.0 * std::f64::consts::PI * center_hz / SAMPLE_RATE as f64;
    base.iter().enumerate().map(|(n, &x)| (x * (w * n as f64).cos()) as f32).collect()
}

/// Encode text into a BPSK31 transmission centred on `center_hz`, with idle
/// either side of it.
pub fn encode(text: &str, center_hz: f64, mode: Mode) -> Vec<f32> {
    let mut bits = vec![0u8; IDLE_SYMBOLS];
    bits.extend(varicode::encode(text));
    bits.resize(bits.len() + IDLE_SYMBOLS, 0);
    synthesize(&bits, center_hz, mode)
}

// ---- demodulation ----

/// One decoded character, at a frequency and a time.
#[derive(Clone, Debug)]
pub struct DecodeResult {
    pub mode: Mode,
    /// Carrier frequency in Hz, refined from the candidate grid.
    pub hz: f64,
    /// Sample offset into the input of the symbol that completed the character.
    pub offset: usize,
    /// Stage-one envelope-line score, in the same units as [`NOISE_FLOOR`].
    pub score: f64,
    /// Stage-two phase concentration, 0..1.
    pub conc: f64,
    /// The character.
    pub text: String,
}

/// One analysis block: the input transformed once, then screened and
/// demodulated at as many candidate frequencies as the caller likes.
///
/// The transform is the expensive part and the slice is cheap, which is what
/// makes screening a whole band affordable — 58 candidates cost under two
/// milliseconds on top of one forward FFT.
pub struct Block {
    spec: Vec<Complex32>,
    n: usize,
    mode: Mode,
}

impl Block {
    /// Transform one block of `samples` starting at `start`.
    ///
    /// Samples past the end count as zero, so a block running off the end of
    /// the input is zero-padded rather than refused.
    pub fn new(samples: &[f32], start: usize, mode: Mode) -> Block {
        let n = mode.block_samples();
        let mut spec: Vec<Complex32> = (0..n)
            .map(|i| Complex32::new(samples.get(start + i).copied().unwrap_or(0.0), 0.0))
            .collect();
        dsp::plan(n).process(&mut spec);
        Block { spec, n, mode }
    }

    /// Stages one and two at one candidate: `(envelope score, concentration)`.
    ///
    /// Public because the gate's thresholds are empirical, and anything
    /// empirical has to stay measurable — `examples/psk_gate.rs` is built on
    /// this and on [`framing`](Self::framing).
    pub fn screen(&self, hz: f64) -> (f64, f64) {
        let bb = baseband(&self.spec, self.n, hz, SLICE_BINS);
        let score = envelope_score(&bb);
        let conc = (0..SLOTS_PER_SYMBOL)
            .map(|off| concentration(&symbols_at(&bb, off, SLOTS_PER_SYMBOL)).0)
            .fold(0.0, f64::max);
        (score, conc)
    }

    /// Stage three's input: demodulate at `hz` and report how the bits frame.
    pub fn framing(&self, hz: f64) -> varicode::Framing {
        self.demod(hz).framing
    }
}

/// Complex baseband at `hz`: [`SLICE_BINS`] positive-frequency bins, inverse
/// transformed into `out_len` samples.
///
/// Keeping only positive frequencies makes this the analytic signal, so `|z|`
/// is the envelope directly. `out_len` longer than [`SLICE_BINS`] zero-pads the
/// spectrum, which interpolates the baseband exactly — same filter, finer grid.
fn baseband(spec: &[Complex32], n: usize, hz: f64, out_len: usize) -> Vec<Complex32> {
    let center = (hz * n as f64 / SAMPLE_RATE as f64).round() as isize;
    let lo = center - (SLICE_BINS / 2) as isize;
    let mut buf = vec![Complex32::new(0.0, 0.0); out_len];
    for i in 0..SLICE_BINS {
        let src = lo + i as isize;
        if src < 0 || src as usize >= spec.len() {
            continue;
        }
        // The slice's centre bin becomes DC; everything below it wraps to the
        // top of the buffer, which is where negative frequencies belong.
        let dst = if i >= SLICE_BINS / 2 {
            i - SLICE_BINS / 2
        } else {
            out_len - SLICE_BINS / 2 + i
        };
        buf[dst] = spec[src as usize];
    }
    dsp::plan_inverse(out_len).process(&mut buf);
    buf
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() {
        0.0
    } else {
        v[v.len() / 2]
    }
}

/// Stage one: the envelope's symbol-rate line against the median of its
/// neighbours. 1.0 is "no line at all"; see [`NOISE_FLOOR`] for what noise
/// scores.
fn envelope_score(bb: &[Complex32]) -> f64 {
    let k = bb.len();
    let env: Vec<f32> = bb.iter().map(|z| z.norm()).collect();
    let mean = env.iter().sum::<f32>() / k as f32;

    let mut buf: Vec<Complex32> = env
        .iter()
        .enumerate()
        .map(|(i, &e)| {
            let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / k as f64).cos();
            Complex32::new((e - mean) * w as f32, 0.0)
        })
        .collect();
    dsp::plan(k).process(&mut buf);
    let p: Vec<f64> = buf.iter().take(k / 2).map(|z| z.norm_sqr() as f64).collect();

    let line = (LINE_BIN - LINE_SLACK..=LINE_BIN + LINE_SLACK)
        .map(|i| p[i])
        .fold(0.0, f64::max);
    let mut bg: Vec<f64> = BG
        .filter(|i| !(LINE_BIN - LINE_GUARD..=LINE_BIN + LINE_GUARD).contains(i))
        .map(|i| p[i])
        .collect();
    let floor = median(&mut bg);

    if floor > 0.0 {
        line / floor
    } else {
        0.0
    }
}

/// Stage two, for one timing offset: how tightly the symbol-to-symbol phase
/// steps cluster on two antipodal points, and the residual rotation between
/// them.
///
/// `d = z[k]·conj(z[k-1])` is the differential detector's output. Squaring it
/// folds the ± data away, so clean BPSK becomes unit phasors all pointing one
/// way and the mean magnitude goes to 1, while random phase averages towards
/// zero as `1/sqrt(symbols)`. The angle that survives is twice the per-symbol
/// phase step, which is the residual carrier error.
pub(super) fn concentration(syms: &[Complex32]) -> (f64, f64) {
    let mut sum = Complex32::new(0.0, 0.0);
    let mut n = 0.0f64;
    for k in 1..syms.len() {
        let d = syms[k] * syms[k - 1].conj();
        let m = d.norm_sqr();
        if m > 0.0 {
            sum += (d * d) / m;
            n += 1.0;
        }
    }
    if n > 0.0 {
        // arg is in (-pi, pi], so halving it lands in (-pi/2, pi/2] — the
        // branch nearest zero, which is the one a small frequency error is on.
        (sum.norm() as f64 / n, sum.arg() as f64 / 2.0)
    } else {
        (0.0, 0.0)
    }
}

fn symbols_at(bb: &[Complex32], offset: usize, stride: usize) -> Vec<Complex32> {
    (offset..bb.len()).step_by(stride).map(|i| bb[i]).collect()
}

/// One block's demodulation.
struct Demod {
    chars: Vec<varicode::Decoded>,
    conc: f64,
    offset: usize,
    framing: varicode::Framing,
    /// The running carrier statistic after each bit's symbol, indexed as
    /// `bits` is. See [`Block::demod`].
    carrier: Vec<f32>,
}

impl Block {
    /// Refine a candidate's carrier from the grid to the signal, by the power
    /// centroid of its spectrum with the local noise floor taken off.
    ///
    /// Without the subtraction the centroid is dragged towards the middle of
    /// whatever window it is taken over; with it, only the signal votes.
    ///
    /// It has to *iterate to convergence* rather than run a fixed couple of
    /// passes. A candidate at the far edge of the screen's tolerance overlaps
    /// only the signal's lower skirt, so the first centroid lands part of the
    /// way there and each pass closes the remaining gap. Stopping early leaves
    /// the carrier tens of hertz out — which does not stop the block decoding,
    /// but reports one station as two.
    fn refine_hz(&self, hz: f64) -> f64 {
        /// Passes at which convergence has either happened or is not going to.
        const MAX_PASSES: usize = 8;
        /// Movement below which a pass has stopped telling us anything.
        const SETTLED_HZ: f64 = 0.25;

        let baud = self.mode.baud();
        let bin_hz = SAMPLE_RATE as f64 / self.n as f64;
        let bin = |f: f64| (f / bin_hz).round().max(0.0) as usize;
        let mut f = hz;

        for _ in 0..MAX_PASSES {
            // Wide enough to hold the whole signal even when the estimate is
            // still most of a slice away from it.
            let (lo, hi) = (bin(f - 1.8 * baud), bin(f + 1.8 * baud));
            // The floor is measured clear of that, or the signal's own skirts
            // would raise the level its centre is weighed against.
            let (wlo, whi) = (bin(f - 6.0 * baud), bin(f + 6.0 * baud));
            if hi >= self.spec.len() || lo >= hi {
                break;
            }
            let mut ring: Vec<f64> = (wlo..=whi.min(self.spec.len() - 1))
                .filter(|&b| b < lo || b > hi)
                .map(|b| self.spec[b].norm_sqr() as f64)
                .collect();
            let floor = median(&mut ring);

            let mut num = 0.0;
            let mut den = 0.0;
            for b in lo..=hi {
                let p = (self.spec[b].norm_sqr() as f64 - floor).max(0.0);
                num += p * b as f64 * bin_hz;
                den += p;
            }
            if den <= 0.0 {
                break;
            }
            let next = num / den;
            let moved = (next - f).abs();
            f = next;
            if moved < SETTLED_HZ {
                break;
            }
        }
        f
    }

    /// Demodulate one candidate: recover timing, differentially detect, and
    /// hand the bits to varicode.
    ///
    /// Timing is the offset, of the sixteen the interpolated baseband allows,
    /// that maximises phase concentration — which inter-symbol interference
    /// spoils, so it peaks where the symbol clock actually is.
    fn demod(&self, hz: f64) -> Demod {
        let bb = baseband(&self.spec, self.n, hz, DEMOD_LEN);

        let mut best = (0.0f64, 0usize, 0.0f64);
        for off in 0..SLOTS_PER_SYMBOL_DEMOD {
            let (c, theta) = concentration(&symbols_at(&bb, off, SLOTS_PER_SYMBOL_DEMOD));
            if c > best.0 {
                best = (c, off, theta);
            }
        }
        let (conc, off, theta) = best;

        let syms = symbols_at(&bb, off, SLOTS_PER_SYMBOL_DEMOD);
        let rot = Complex32::from_polar(1.0, -theta as f32);
        // The block's own concentration is a single figure over all 256
        // symbols, which is what stage two wants and is no use for saying
        // *when* within the block the station was transmitting. A block is 8.2
        // seconds of PSK31 and an over rarely starts or ends on its boundary,
        // so the same statistic is kept running alongside — see
        // [`CARRIER_TC`] — and the characters outside the transmission are
        // dropped by it in [`decode_all`].
        //
        // Unlike the receiver's, this starts at zero rather than at one. An
        // `Rx` is opened where the gate has already vouched for a station, so
        // "present" is true at its first symbol; a block has no such warrant
        // and begins wherever it begins. The cost of making it earn the first
        // dozen symbols is nothing, because blocks overlap by half and a
        // character suppressed at one block's edge sits in the middle of its
        // neighbour.
        let b = (1.0 / CARRIER_TC) as f32;
        let mut running = Complex32::new(0.0, 0.0);
        let mut carrier: Vec<f32> = Vec::with_capacity(syms.len());
        let bits: Vec<u8> = (1..syms.len())
            .map(|k| {
                let d = syms[k] * syms[k - 1].conj() * rot;
                let m = d.norm_sqr();
                if m > 0.0 {
                    running = running * (1.0 - b) + (d * d) / m * b;
                }
                carrier.push(running.norm());
                u8::from(d.re > 0.0)
            })
            .collect();

        let (chars, framing) = varicode::decode_framed(&bits);
        Demod { chars, conc, offset: off, framing, carrier }
    }
}

/// Fewest words a block must hold before its framing means anything.
///
/// Both ratios below are means over `words` draws; too few and they are
/// dominated by their own scatter. Real text puts thirty-odd in a block.
///
/// It is also the one condition a steady carrier cannot satisfy. Read as
/// differential BPSK, an unmodulated tone gives a *periodic* bit stream, and a
/// periodic stream frames every bit as tidily as language does — a JS8 carrier
/// falling inside a PSK63 slice of the demo band cleared both ratios and printed
/// `ttttt`. What separated it was that it reached only thirteen words, where
/// real text yields 28–34 per block whatever the baud, the block being a fixed
/// 256 symbols. Hence 24: clear of both.
pub const MIN_WORDS: usize = 24;

/// Fraction of a block's gaps that must be exactly the two-zero separator.
///
/// This is the sharp one, and it is structural rather than statistical: a
/// varicode word begins and ends with `1`, so while a station is sending text
/// every gap is exactly `00` and three zeros in a row cannot occur. Arbitrary
/// bits have geometric gaps, so only two thirds of theirs are two long — gaps
/// long enough to be idle are not counted either way.
///
/// Measured: the demo band in this repository, which contains no PSK31, spans
/// 0.000–0.833 over 34 blocks; real BPSK31 spans 0.900–1.000 at −10 dB and
/// 0.941–1.000 at −8 dB. A station *idling* — the case that matters, because
/// idle is what tops and tails every transmission — reads exactly 1.000.
pub const TIGHT_RATIO_MIN: f64 = 0.85;

/// Fraction of a block's words that must be characters in the table.
///
/// Weaker than the gap structure and deliberately not relied on alone. Varicode
/// covers 95 of the 143 possible words of ten bits or fewer, and the short ones
/// arbitrary bits stumble into are all in it, so 83% of random words are
/// "valid" — see [`varicode`](super::varicode)'s tests. It earns its place as a
/// second, partly independent condition: garbage that happens to frame tightly
/// rarely also decodes.
pub const VALID_RATIO_MIN: f64 = 0.85;

/// Stage three: does the bit stream frame the way varicode frames?
///
/// The first two stages find a BPSK signal at 31.25 baud. This asks whether
/// what comes out of it is *language* — and it is what catches what the first
/// two cannot. Stages one and two were measured against other modes at −6 dB,
/// where they separate cleanly, but that does not generalise upwards: a strong
/// Olivia signal puts enough symbol-rate structure into a narrow slice to clear
/// both, and the demo band, which holds no PSK31 at all, produced eleven phantom
/// stations on two stages alone.
///
/// Framing is what stops it, because a signal's framing does not improve with
/// its strength when there is no varicode in it to find. On the two conditions
/// above together: 0 of 34 demo-band blocks pass, against every block of a real
/// signal at −10 dB and better.
fn believable(d: &Demod) -> bool {
    d.framing.words >= MIN_WORDS
        && d.framing.tight_ratio() >= TIGHT_RATIO_MIN
        && d.framing.valid_ratio() >= VALID_RATIO_MIN
}

/// Decode every PSK signal in `samples` between `hz_lo` and `hz_hi`.
///
/// The band is screened block by block; blocks overlap by half so a character
/// straddling a boundary is not lost, and the duplicates that costs are removed
/// on the way out.
pub fn decode_all(samples: &[f32], hz_lo: f64, hz_hi: f64, modes: &[Mode]) -> Vec<DecodeResult> {
    let mut out: Vec<DecodeResult> = Vec::new();

    for &mode in modes {
        let n = mode.block_samples();
        if samples.len() < n {
            continue;
        }
        let step = mode.candidate_step_hz();

        let mut start = 0;
        while start + n <= samples.len() {
            let block = Block::new(samples, start, mode);

            // Screen the grid, and refine whatever survives both stages.
            let mut kept: Vec<(f64, f64, f64)> = Vec::new(); // (hz, score, conc)
            let mut hz = hz_lo;
            while hz <= hz_hi {
                let (score, conc) = block.screen(hz);
                if score >= SCREEN_THRESHOLD && conc >= CONC_THRESHOLD {
                    kept.push((block.refine_hz(hz), score, conc));
                }
                hz += step;
            }

            // The grid is finer than the screen's tolerance, so one signal can
            // survive at two neighbouring candidates. After refinement both
            // land on the same carrier: keep the stronger.
            kept.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            let mut stations: Vec<(f64, f64, f64)> = Vec::new();
            for c in kept {
                if stations.iter().any(|s| (s.0 - c.0).abs() < mode.baud() / 2.0) {
                    continue;
                }
                stations.push(c);
            }

            for (fhz, score, _) in stations {
                let d = block.demod(fhz);
                if d.conc < CONC_THRESHOLD || !believable(&d) {
                    continue;
                }
                let (conc, off) = (d.conc, d.offset);
                let carrier = d.carrier;
                for d in d.chars {
                    // Where in the block the station actually was. A block
                    // spans 8.2 seconds of PSK31 and an over rarely fills it,
                    // so the symbols before it keyed up and after it stopped
                    // are noise — and noise read as differential BPSK frames
                    // and decodes and prints as words.
                    if carrier.get(d.bit).is_none_or(|&c| c < CARRIER_THRESHOLD as f32) {
                        continue;
                    }
                    // Bit k came from symbol k+1 of the interpolated baseband,
                    // which maps back to the input by the same ratio the slice
                    // decimated it.
                    let slot = off + (d.bit + 1) * SLOTS_PER_SYMBOL_DEMOD;
                    let offset = start + slot * n / DEMOD_LEN;
                    out.push(DecodeResult {
                        mode,
                        hz: fhz,
                        offset,
                        score,
                        conc,
                        text: d.ch.to_string(),
                    });
                }
            }

            start += n / 2;
        }
    }

    dedup(&mut out);
    out
}

/// Drop characters that overlapping blocks decoded twice.
///
/// The symbol clock is recovered from the signal, not from the block, so the
/// same character comes back at the same time and frequency however the block
/// happened to fall across it. Tolerance is one symbol, and the shortest
/// character is three, so this cannot merge two real ones.
fn dedup(out: &mut Vec<DecodeResult>) {
    out.sort_by_key(|d| d.offset);
    let mut kept: Vec<DecodeResult> = Vec::with_capacity(out.len());
    for d in out.drain(..) {
        let tol = d.mode.symbol_samples();
        let dup = kept.iter().any(|k| {
            k.mode == d.mode
                && k.text == d.text
                && (k.hz - d.hz).abs() < d.mode.baud() / 2.0
                && d.offset.abs_diff(k.offset) < tol
        });
        if !dup {
            kept.push(d);
        }
    }
    *out = kept;
}

impl Mode {
    /// Samples in one analysis block.
    pub fn block_samples(&self) -> usize {
        SLICE_BINS * self.decim()
    }

    /// Duration of one analysis block, in seconds. A signal must be present for
    /// a good part of one to be found.
    pub fn block_secs(&self) -> f64 {
        self.block_samples() as f64 / SAMPLE_RATE as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::psk::mode::PSK31;

    /// The block geometry the gate's thresholds were measured on.
    #[test]
    fn block_geometry() {
        assert_eq!(PSK31.block_samples(), 98_304);
        assert!((PSK31.block_secs() - 8.192).abs() < 1e-6);
        assert_eq!(SYMBOLS_PER_BLOCK, 256);
        assert_eq!(PSK31.block_samples() / PSK31.symbol_samples(), SYMBOLS_PER_BLOCK);
        // The interpolated baseband must map back to whole input samples.
        assert_eq!(PSK31.block_samples() % DEMOD_LEN, 0);
    }

    /// A station sending `text` over and over for long enough to fill several
    /// analysis blocks.
    ///
    /// The text is repeated and encoded once, not encoded and concatenated: a
    /// transmission joined end to end restarts the carrier phase every time,
    /// which is a click, and a click is broadband energy this decoder has every
    /// right to be confused by.
    fn transmission(text: &str, hz: f64) -> Vec<f32> {
        let mut s = String::new();
        while s.chars().count() < 300 {
            s.push_str(text);
        }
        encode(&s, hz, PSK31)
    }

    /// Deterministic white noise, as a fraction of a full-scale signal.
    ///
    /// Every test below adds some, and not for realism's sake: both stages of
    /// the gate are *ratios against the local noise floor*, and a band with no
    /// noise in it has no floor to measure against. Give the detector pure
    /// arithmetic dust as a background and the leakage from a station a
    /// kilohertz away scores as well as the station does.
    fn noise(n: usize, rms: f32, seed: u64) -> Vec<f32> {
        let mut s = seed;
        let mut next = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 11) as f64 + 1.0) / (1u64 << 53) as f64
        };
        (0..n)
            .map(|_| {
                let (u1, u2) = (next(), next());
                (rms as f64 * (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos())
                    as f32
            })
            .collect()
    }

    /// A band holding `stations`, each at its own frequency, over noise.
    fn band(stations: &[(&str, f64)], noise_rms: f32, seed: u64) -> Vec<f32> {
        let sigs: Vec<Vec<f32>> = stations.iter().map(|(t, hz)| transmission(t, *hz)).collect();
        let len = sigs.iter().map(|s| s.len()).max().unwrap_or(0);
        let mut buf = noise(len, noise_rms, seed);
        for sig in &sigs {
            for (i, s) in sig.iter().enumerate() {
                buf[i] += s * 0.5;
            }
        }
        buf
    }

    #[test]
    fn round_trips() {
        let want = "cq cq de w1aw k ";
        let audio = band(&[(want, 1500.0)], 0.05, 1);
        let out = decode_all(&audio, 300.0, 2600.0, &[PSK31]);
        let text: String = out.iter().map(|d| d.text.clone()).collect();
        assert!(text.contains(want), "decoded {text:?}");
    }

    /// The carrier is found from a 40 Hz grid wherever it actually sits, and
    /// one station is reported as one station.
    #[test]
    fn finds_a_carrier_between_grid_points() {
        for (i, hz) in [777.0, 1237.5, 1502.0, 2111.0].into_iter().enumerate() {
            let audio = band(&[("the quick brown fox ", hz)], 0.05, 20 + i as u64);
            let out = decode_all(&audio, 300.0, 2600.0, &[PSK31]);
            assert!(!out.is_empty(), "nothing at {hz} Hz");

            let mut carriers: Vec<f64> = out.iter().map(|d| d.hz).collect();
            carriers.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let (lo, hi) = (carriers[0], carriers[carriers.len() - 1]);
            assert!(
                (lo - hz).abs() < 3.0 && (hi - hz).abs() < 3.0,
                "one station at {hz} Hz came back as carriers {lo}..{hi}"
            );

            let text: String = out.iter().map(|d| d.text.clone()).collect();
            assert!(text.contains("quick brown fox"), "at {hz} Hz decoded {text:?}");
        }
    }

    #[test]
    fn two_stations_in_one_band() {
        let audio = band(
            &[("hello from station one ", 800.0), ("and this is station two ", 1900.0)],
            0.05,
            3,
        );
        let out = decode_all(&audio, 300.0, 2600.0, &[PSK31]);
        let low: String = out.iter().filter(|d| d.hz < 1400.0).map(|d| d.text.clone()).collect();
        let high: String = out.iter().filter(|d| d.hz >= 1400.0).map(|d| d.text.clone()).collect();
        assert!(low.contains("station one"), "low decoded {low:?}");
        assert!(high.contains("station two"), "high decoded {high:?}");
    }

    /// Overlapping blocks decode the same characters twice; the dedup has to
    /// remove them without eating repeated letters in the text itself.
    #[test]
    fn keeps_doubled_letters() {
        let audio = band(&[("aabbcc happened ", 1400.0)], 0.05, 4);
        let out = decode_all(&audio, 300.0, 2600.0, &[PSK31]);
        let text: String = out.iter().map(|d| d.text.clone()).collect();
        assert!(text.contains("aabbcc happened"), "decoded {text:?}");
    }

    /// The whole reason the gate exists: an empty band must stay empty. A
    /// demodulator pointed at noise produces bits, and five in six of the words
    /// they frame into are valid varicode characters.
    #[test]
    fn prints_nothing_from_pure_noise() {
        for seed in 0..8 {
            let audio = noise(PSK31.block_samples() * 3, 0.1, 0x0154 + seed);
            let out = decode_all(&audio, 300.0, 2600.0, &[PSK31]);
            let text: String = out.iter().map(|d| d.text.clone()).collect();
            assert!(out.is_empty(), "noise seed {seed} printed {text:?}");
        }
    }
}
