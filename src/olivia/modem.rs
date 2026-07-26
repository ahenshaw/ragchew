//! Olivia modulator and demodulator.
//!
//! # Transmit
//!
//! One symbol = one tone, held for `symbol_separ` samples but shaped by a
//! raised cosine twice that long, so consecutive symbols overlap by half. Two
//! overlapping raised cosines sum to a constant, giving a steady envelope with
//! no keying clicks. The phase steps by ±90° between symbols so that two
//! neighbouring symbols on the *same* tone cannot cancel in their overlap.
//!
//! Symbol values are Gray-coded before choosing a tone, so mistaking a tone for
//! its neighbour — by far the likeliest error — corrupts only one bit.
//!
//! # Receive
//!
//! The demodulator is non-coherent: a raised-cosine-windowed FFT the length of
//! one symbol shape, taken every half symbol. Tones then land exactly two bins
//! apart, and the window's nulls put a tone's leakage into all *other* tone
//! bins at zero. Each symbol yields one soft value per bit ([`fec`]'s input);
//! the FEC layer does the rest.
//!
//! Finding a signal means searching three things at once — centre frequency,
//! block timing, and mode — so [`decode_all_mode`] builds the tone grid for a
//! whole band once, then slides a 64-symbol block over it at every plausible
//! frequency and timing, keeping what correlates.
//!
//! [`fec`]: super::fec

use super::fec::{self, Block};
use super::mode::{Mode, SYMBOLS_PER_BLOCK};
use crate::dsp;
use crate::SAMPLE_RATE;

/// Time slots per symbol in the receiver's tone grid: block timing is searched
/// to half a symbol, which is what an Olivia receiver conventionally does.
const SLOTS_PER_SYMBOL: usize = 2;

/// What noise scores on the Walsh correlation: pick the largest of 64
/// correlations against pure noise and it still stands about 2.6 times the
/// remaining ones. The decision thresholds sit above *this*, not above zero.
pub const NOISE_FLOOR: f64 = 2.6;

/// Walsh peak-to-noise ratio a signal must reach to be believed — both to sync
/// in the first place and, block by block, to be worth printing.
///
/// It has to scale with the mode. A block's score is the mean over its
/// `bits_per_symbol` characters, so a 4-tone mode averages two correlations
/// where a 32-tone mode averages five: the same noise floor, but far more spread
/// around it, and a band scan takes the maximum over millions of hypotheses.
/// Hence the `1/sqrt` shape — enough headroom above [`NOISE_FLOOR`] that a band
/// of pure noise stays silent in every mode.
pub fn threshold(mode: Mode) -> f64 {
    NOISE_FLOOR + 6.0 / ((SYNC_BLOCKS * mode.bits_per_symbol()) as f64).sqrt()
}

/// Consecutive blocks a candidate's sync is averaged over. Longer averaging
/// separates signal from noise more sharply but needs a longer transmission.
const SYNC_BLOCKS: usize = 4;

/// Single-character sync a frequency/timing hypothesis must reach before it is
/// worth decoding in full. Low enough to let anything plausible through — its
/// only job is to throw out the hopeless majority cheaply.
const SCREEN_THRESHOLD: f64 = 3.2;

/// Gray-code a symbol value to its tone index.
fn gray(v: u8) -> u8 {
    v ^ (v >> 1)
}

/// Inverse of [`gray`]: tone index back to symbol value.
fn ungray(mut g: u8) -> u8 {
    g ^= g >> 4;
    g ^= g >> 2;
    g ^= g >> 1;
    g
}

// ---- modulation ----

/// Modulate a sequence of symbol values (not tone indices) at `center_hz`.
///
/// The result is `(symbols.len() + 1) * symbol_separ` samples long: every
/// symbol's shape is two `symbol_separ` periods wide, so the last one runs half
/// a symbol past the end of the last slot.
pub fn synthesize(symbols: &[u8], center_hz: f64, mode: Mode) -> Vec<f32> {
    let separ = mode.symbol_separ();
    let len = mode.symbol_len();
    let shape = dsp::hann(len);
    let mut out = vec![0.0f32; symbols.len() * separ + len];

    // ±90° per symbol, from a fixed generator so encoding is reproducible.
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut phase = 0.0f64;

    for (k, &sym) in symbols.iter().enumerate() {
        let hz = mode.tone_hz(center_hz, gray(sym) as usize);
        let w = 2.0 * std::f64::consts::PI * hz / SAMPLE_RATE as f64;
        let start = k * separ;
        for i in 0..len {
            out[start + i] += (phase + w * i as f64).cos() as f32 * shape[i];
        }
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        phase += if rng >> 63 == 0 { 0.5 } else { -0.5 } * std::f64::consts::PI;
    }
    out
}

/// Encode text into an Olivia transmission centred on `center_hz`.
///
/// The text is padded with idle characters to a whole number of FEC blocks.
pub fn encode(text: &str, center_hz: f64, mode: Mode) -> Vec<f32> {
    let symbols: Vec<u8> = fec::text_to_blocks(text, mode)
        .iter()
        .flat_map(|chars| fec::encode_block(chars, mode))
        .collect();
    synthesize(&symbols, center_hz, mode)
}

// ---- demodulation ----

/// Tone magnitudes over a band, on a grid of half-symbol time slots and
/// half-tone-spacing frequency bins.
///
/// One of these covers every candidate centre frequency on the bin grid at
/// once: a signal whose lowest tone sits in bin `b` has its tones in bins `b`,
/// `b+2`, `b+4`, …, so testing a different centre is just a different starting
/// bin, not a new transform.
struct ToneGrid {
    bin_lo: usize,
    n_bins: usize,
    n_slots: usize,
    /// Frequency the input was mixed down by before transforming, to reach
    /// centres that fall between bins.
    delta_hz: f64,
    bin_hz: f64,
    mags: Vec<f32>,
}

impl ToneGrid {
    fn compute(samples: &[f32], mode: Mode, bin_lo: usize, bin_hi: usize, delta_hz: f64) -> ToneGrid {
        let len = mode.symbol_len();
        let hop = mode.symbol_separ() / SLOTS_PER_SYMBOL;
        let window = dsp::hann(len);
        let n_slots = if samples.len() >= len { (samples.len() - len) / hop + 1 } else { 0 };
        let n_bins = bin_hi + 1 - bin_lo;
        let mut mags = vec![0.0f32; n_slots * n_bins];
        for s in 0..n_slots {
            let spec = dsp::fft_window(samples, s * hop, delta_hz, SAMPLE_RATE, len, Some(&window));
            let row = &mut mags[s * n_bins..(s + 1) * n_bins];
            for (j, m) in row.iter_mut().enumerate() {
                *m = spec[bin_lo + j].norm();
            }
        }
        ToneGrid {
            bin_lo,
            n_bins,
            n_slots,
            delta_hz,
            bin_hz: SAMPLE_RATE as f64 / len as f64,
            mags,
        }
    }

    /// Frequency of the signal whose lowest tone sits in bin `b`.
    fn center_of(&self, b: usize, mode: Mode) -> f64 {
        let first_tone = b as f64 * self.bin_hz + self.delta_hz;
        first_tone + mode.bandwidth as f64 / 2.0 - mode.spacing() / 2.0
    }

    /// Per-bit soft values for a signal whose lowest tone is in bin `b0`,
    /// flattened as `[slot * bits_per_symbol + bit]` for [`fec::decode_block`].
    ///
    /// Positive means the bit is more likely 0. Values are normalised by the
    /// symbol's total energy, so a fading signal weights its strong symbols
    /// more heavily than its weak ones.
    fn soft_bits(&self, b0: usize, mode: Mode) -> Vec<f32> {
        let bps = mode.bits_per_symbol();
        let tones = mode.tones as usize;
        // Symbol value of each tone position, precomputed.
        let values: Vec<u8> = (0..tones).map(|t| ungray(t as u8)).collect();
        let mut out = vec![0.0f32; self.n_slots * bps];
        for s in 0..self.n_slots {
            let row = &self.mags[s * self.n_bins..(s + 1) * self.n_bins];
            let bits = &mut out[s * bps..(s + 1) * bps];
            let mut total = 0.0f32;
            for (t, &v) in values.iter().enumerate() {
                let m = row[b0 - self.bin_lo + 2 * t];
                let energy = m * m;
                total += energy;
                for (b, bit) in bits.iter_mut().enumerate() {
                    if v >> b & 1 == 1 {
                        *bit -= energy;
                    } else {
                        *bit += energy;
                    }
                }
            }
            if total > 0.0 {
                for bit in bits.iter_mut() {
                    *bit /= total;
                }
            }
        }
        out
    }
}

/// One decoded Olivia FEC block.
pub struct DecodeResult {
    /// The mode it decoded as.
    pub mode: Mode,
    /// Centre frequency (Hz).
    pub hz: f64,
    /// Sample offset of the block's first symbol.
    pub offset: usize,
    /// Walsh peak-to-noise ratio of this block (see [`Block::snr`]).
    pub snr: f64,
    /// Mean block SNR of the signal this block belongs to — how confident the
    /// decoder is that there is a signal here at all.
    pub sync: f64,
    /// The decoded block.
    pub block: Block,
}

impl DecodeResult {
    /// The block's text (idle characters removed).
    pub fn text(&self) -> String {
        self.block.text()
    }
}

/// Decode every Olivia block of one mode in `samples` within `hz_lo..hz_hi`.
///
/// Three unknowns are searched at once. **Frequency**: candidate centres sit on
/// a half-tone-spacing grid across the whole band, plus a quarter-spacing offset
/// pass, so any real centre is covered to within an eighth of a tone spacing.
/// **Timing**: a block boundary can fall on any of the 128 half-symbol slots in
/// a block. **Presence**: the Walsh correlation is itself the detector, since
/// Olivia carries no sync pattern to look for.
///
/// Every frequency is tried rather than pre-selected by how much energy sits
/// there. Spreading each character over 64 chips is what lets Olivia work at
/// signal levels an energy detector cannot see, and being one bin out loses the
/// signal completely — so guessing where to look is exactly the wrong economy.
/// What makes the exhaustive search affordable instead is [`fec::screen_snr`]:
/// hypotheses are first judged on a single character, and only the few
/// survivors cost a whole block.
pub fn decode_all_mode(samples: &[f32], hz_lo: f64, hz_hi: f64, mode: Mode) -> Vec<DecodeResult> {
    if !mode.is_valid() {
        return Vec::new();
    }
    let len = mode.symbol_len();
    let block_slots = SYMBOLS_PER_BLOCK * SLOTS_PER_SYMBOL;
    if samples.len() < len * SYMBOLS_PER_BLOCK {
        return Vec::new();
    }

    let bin_hz = SAMPLE_RATE as f64 / len as f64; // = spacing / 2
    let tone_bins = 2 * (mode.tones as usize - 1);
    // Lowest tone of the lowest centre we will try, and of the highest. A wide
    // mode near the bottom of the band would want tones below 0 Hz; clamping
    // rather than giving up just means its lowest centres are out of reach.
    let first_lo = hz_lo - mode.bandwidth as f64 / 2.0 + mode.spacing() / 2.0;
    let first_hi = hz_hi - mode.bandwidth as f64 / 2.0 + mode.spacing() / 2.0;
    let bin_first = (first_lo / bin_hz).floor().max(1.0) as usize;
    let bin_last = (first_hi / bin_hz).ceil().max(1.0) as usize;
    let bin_hi = (bin_last + tone_bins).min(len / 2 - 1);
    if bin_hi <= bin_first + tone_bins {
        return Vec::new();
    }

    let hop = mode.symbol_separ() / SLOTS_PER_SYMBOL;
    let mut hits: Vec<DecodeResult> = Vec::new();

    // Two mixing offsets, half a bin apart: together they put any centre within
    // a quarter of a bin of a grid point.
    for delta in [0.0, bin_hz / 2.0] {
        let grid = ToneGrid::compute(samples, mode, bin_first, bin_hi, delta);
        if grid.n_slots < block_slots {
            continue;
        }
        let last_start = grid.n_slots - block_slots;

        for b0 in bin_first..=(bin_hi - tone_bins) {
            let soft = grid.soft_bits(b0, mode);
            let hz = grid.center_of(b0, mode);

            // One cheap screening score per start slot covers every block phase
            // and position at once: phase p's blocks are the entries at p,
            // p + block_slots, p + 2 * block_slots, …
            let screen: Vec<f64> = (0..=last_start)
                .map(|s| fec::screen_snr(&soft, s, SLOTS_PER_SYMBOL, mode))
                .collect();

            // Every timing that syncs is kept, not just the best one. Two
            // stations working each other sit on the *same* frequency and take
            // turns, and each keys up on its own block boundary — so one
            // frequency routinely carries several block timings over a
            // recording, and picking a single winner would copy one side of the
            // QSO and drop the other.
            for phase in 0..block_slots.min(last_start + 1) {
                let starts: Vec<usize> = (phase..=last_start).step_by(block_slots).collect();
                let screened: Vec<f64> = starts.iter().map(|&s| screen[s]).collect();
                if sustained(&screened) < SCREEN_THRESHOLD {
                    continue;
                }
                let snr: Vec<f64> = starts
                    .iter()
                    .map(|&s| fec::decode_block(&soft, s, SLOTS_PER_SYMBOL, mode).snr())
                    .collect();
                let sync = sustained(&snr);
                if sync < threshold(mode) {
                    continue;
                }
                for (i, &start) in starts.iter().enumerate() {
                    // A lone strong block in an otherwise quiet stretch is noise
                    // getting lucky; a real transmission is blocks on end.
                    let neighbour_ok = [i.wrapping_sub(1), i + 1]
                        .iter()
                        .any(|&j| snr.get(j).map_or(false, |&s| s >= threshold(mode)));
                    if snr[i] < threshold(mode) || !neighbour_ok {
                        continue;
                    }
                    let block = fec::decode_block(&soft, start, SLOTS_PER_SYMBOL, mode);
                    if block.text().is_empty() {
                        continue;
                    }
                    hits.push(DecodeResult {
                        mode,
                        hz,
                        offset: start * hop,
                        snr: snr[i],
                        sync,
                        block,
                    });
                }
            }
        }
    }

    // Resolve the overlaps. One real transmission lights up neighbouring centre
    // bins, both mixing passes and several near-miss timings, all claiming the
    // same moment — so take the blocks in order of confidence and drop any that
    // collide with one already taken. Blocks that merely share a frequency
    // without sharing a moment are different overs, and both survive.
    //
    // Confidence is the *timing's* sync first and the individual block second.
    // A near-miss timing can produce the odd block that scores well by luck, and
    // ranking on that alone would let it displace a block of the transmission it
    // is a near miss of.
    hits.sort_by(|a, b| {
        b.sync.partial_cmp(&a.sync).unwrap().then(b.snr.partial_cmp(&a.snr).unwrap())
    });
    let block_samples = block_slots * hop;
    let mut out: Vec<DecodeResult> = Vec::new();
    for h in hits {
        let collides = out.iter().any(|o| {
            (o.hz - h.hz).abs() < mode.bandwidth as f64
                && o.offset.abs_diff(h.offset) < block_samples
        });
        if !collides {
            out.push(h);
        }
    }

    out.sort_by_key(|r| r.offset);
    out
}

/// How well a candidate holds up over time: the mean of its strongest run of
/// [`SYNC_BLOCKS`] consecutive blocks.
///
/// Scoring the best run rather than the average means a station that transmits
/// for ten seconds of a two-minute recording still syncs, while a timing that
/// merely got lucky on one block does not.
fn sustained(blocks: &[f64]) -> f64 {
    if blocks.is_empty() {
        return 0.0;
    }
    let run = SYNC_BLOCKS.min(blocks.len());
    let mut sum: f64 = blocks[..run].iter().sum();
    let mut best = sum / run as f64;
    for i in run..blocks.len() {
        sum += blocks[i] - blocks[i - run];
        best = best.max(sum / run as f64);
    }
    best
}

/// Decode every Olivia block of any of `modes` in `samples`.
///
/// One transmission can sync weakly as a *neighbouring* mode — a narrow mode
/// will happily lock onto a slice of a wide one — so where two modes claim the
/// same stretch of band the better-synced signal wins outright, taking all its
/// blocks with it. Judging block by block would interleave the two into
/// nonsense.
pub fn decode_all(samples: &[f32], hz_lo: f64, hz_hi: f64, modes: &[Mode]) -> Vec<DecodeResult> {
    // Every block from one synced candidate shares its exact centre and sync,
    // which is what makes them groupable.
    let mut signals: Vec<Vec<DecodeResult>> = Vec::new();
    for &mode in modes {
        for r in decode_all_mode(samples, hz_lo, hz_hi, mode) {
            match signals
                .iter_mut()
                .find(|g| g[0].mode == r.mode && g[0].hz == r.hz)
            {
                Some(g) => g.push(r),
                None => signals.push(vec![r]),
            }
        }
    }

    // Strongest first, so a weaker overlapping interpretation is the one dropped.
    signals.sort_by(|a, b| b[0].sync.partial_cmp(&a[0].sync).unwrap());
    let mut kept: Vec<Vec<DecodeResult>> = Vec::new();
    for g in signals {
        if !kept.iter().any(|k| signals_overlap(k, &g)) {
            kept.push(g);
        }
    }

    let mut out: Vec<DecodeResult> = kept.into_iter().flatten().collect();
    out.sort_by_key(|r| r.offset);
    out
}

/// Whether two synced signals occupy the same band at the same time, and so
/// cannot both be real.
fn signals_overlap(a: &[DecodeResult], b: &[DecodeResult]) -> bool {
    let bw = a[0].mode.bandwidth.max(b[0].mode.bandwidth) as f64 / 2.0;
    if (a[0].hz - b[0].hz).abs() >= bw {
        return false;
    }
    let span = |g: &[DecodeResult]| {
        let block = (g[0].mode.block_secs() * SAMPLE_RATE as f64) as usize;
        (g.first().unwrap().offset, g.last().unwrap().offset + block)
    };
    let (a0, a1) = span(a);
    let (b0, b1) = span(b);
    a0 < b1 && b0 < a1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::olivia::mode::{OL_16_500, OL_32_1000, OL_8_250};

    #[test]
    fn gray_round_trips() {
        for v in 0..=255u8 {
            assert_eq!(ungray(gray(v)), v);
        }
    }

    /// Mistaking a tone for its neighbour — by far the likeliest error —
    /// usually costs a single bit. That is the point of Gray coding.
    ///
    /// The reference maps a symbol *value* to a tone position rather than the
    /// reverse, which leaves a quarter of neighbouring tone pairs costing more
    /// than one bit. We match it, because that is what is on the air.
    #[test]
    fn adjacent_tones_usually_differ_in_one_bit() {
        let n = 31usize;
        let cost: Vec<u32> = (0..n as u8).map(|t| (ungray(t) ^ ungray(t + 1)).count_ones()).collect();
        let single = cost.iter().filter(|&&c| c == 1).count();
        let mean = cost.iter().sum::<u32>() as f64 / n as f64;
        assert!(single * 4 >= n * 3, "only {single}/{n} tone neighbours cost one bit");
        assert!(mean < 1.5, "mean cost of a tone slip is {mean} bits");
    }

    /// The modulator puts energy where the mode says it should, and nowhere
    /// else.
    #[test]
    fn tones_land_on_their_nominal_frequencies() {
        let mode = OL_16_500;
        let center = 1500.0;
        for sym in 0..mode.tones as u8 {
            let audio = synthesize(&[sym; 8], center, mode);
            let spec = dsp::fft_window(&audio, mode.symbol_separ(), 0.0, SAMPLE_RATE, mode.symbol_len(), Some(&dsp::hann(mode.symbol_len())));
            let bin_hz = SAMPLE_RATE as f64 / mode.symbol_len() as f64;
            let peak = (1..mode.symbol_len() / 2)
                .max_by(|&a, &b| spec[a].norm().partial_cmp(&spec[b].norm()).unwrap())
                .unwrap();
            let want = mode.tone_hz(center, gray(sym) as usize);
            assert!(
                (peak as f64 * bin_hz - want).abs() <= bin_hz,
                "symbol {sym}: peak at {} Hz, expected {want} Hz",
                peak as f64 * bin_hz
            );
        }
    }

    /// The envelope is flat in the middle of a transmission: overlapping
    /// raised cosines sum to a constant, so there is nothing to click.
    #[test]
    fn envelope_is_steady() {
        let mode = OL_8_250;
        let audio = synthesize(&[3u8; 32], 1200.0, mode);
        let separ = mode.symbol_separ();
        let rms = |w: &[f32]| (w.iter().map(|x| x * x).sum::<f32>() / w.len() as f32).sqrt();
        let mid = rms(&audio[8 * separ..24 * separ]);
        for k in 8..24 {
            let r = rms(&audio[k * separ..(k + 1) * separ]);
            assert!((r - mid).abs() < 0.25 * mid, "symbol {k} envelope {r} vs {mid}");
        }
    }

    /// A block sitting exactly on the bin grid decodes with the tones where
    /// they belong — the modulator and demodulator agree before any searching
    /// is involved.
    #[test]
    fn aligned_block_decodes_exactly() {
        let mode = OL_8_250;
        let bin_hz = SAMPLE_RATE as f64 / mode.symbol_len() as f64;
        let b0 = 70usize;
        let center = b0 as f64 * bin_hz + mode.bandwidth as f64 / 2.0 - mode.spacing() / 2.0;
        let chars = b"HEL".to_vec();
        let audio = synthesize(&fec::encode_block(&chars, mode), center, mode);

        let grid = ToneGrid::compute(&audio, mode, b0, b0 + 2 * (mode.tones as usize - 1), 0.0);
        assert!((grid.center_of(b0, mode) - center).abs() < 1e-9);

        // every symbol's strongest tone is the one that was transmitted
        for (sym, &want) in fec::encode_block(&chars, mode).iter().enumerate() {
            let row = &grid.mags[sym * SLOTS_PER_SYMBOL * grid.n_bins..][..grid.n_bins];
            let peak = (0..mode.tones as usize)
                .max_by(|&a, &b| row[2 * a].partial_cmp(&row[2 * b]).unwrap())
                .unwrap();
            assert_eq!(ungray(peak as u8), want, "symbol {sym}");
        }

        let block = fec::decode_block(&grid.soft_bits(b0, mode), 0, SLOTS_PER_SYMBOL, mode);
        assert_eq!(block.chars, chars);
    }

    /// A full pass through the modem: text in, same text out, at the right
    /// frequency and mode.
    #[test]
    fn clean_round_trip() {
        for mode in [OL_8_250, OL_16_500, OL_32_1000] {
            let text = "CQ CQ DE W1AW W1AW K ";
            let mut audio = vec![0.0f32; SAMPLE_RATE as usize / 3];
            audio.extend(encode(text, 1200.0, mode));
            audio.extend(vec![0.0f32; SAMPLE_RATE as usize / 3]);

            let res = decode_all_mode(&audio, 900.0, 1500.0, mode);
            let got: String = res.iter().map(|r| r.text()).collect();
            assert!(
                got.contains("CQ CQ DE W1AW"),
                "{}: decoded {got:?} from {} blocks",
                mode.name(),
                res.len()
            );
            let hz = res[0].hz;
            assert!((hz - 1200.0).abs() < mode.spacing(), "{}: centre {hz}", mode.name());
        }
    }
}

