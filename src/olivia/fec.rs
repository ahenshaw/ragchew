//! Olivia's forward error correction: 64-bit Walsh codes, scrambling and
//! interleaving.
//!
//! One FEC block carries `bits_per_symbol` characters and always occupies 64
//! symbols, whatever the mode. Each 7-bit character becomes a whole 64-chip
//! Walsh function (6 bits pick the function, the 7th inverts it), so a
//! character survives even when most of its chips are wrong — this, not any
//! parity check, is where Olivia's famous weak-signal performance comes from.
//!
//! Per character (indexed by `freq_bit`, its bit position within a symbol):
//!
//! 1. **Spread** — inverse Hadamard transform of a delta at the character's
//!    value, giving 64 chips of ±1.
//! 2. **Scramble** — XOR against [`SCRAMBLING_CODE`], rotated by
//!    `freq_bit * CODE_SHIFT`, so the characters of one block do not share a
//!    pattern.
//! 3. **Interleave** — chip `t` is placed in bit `(freq_bit + t) mod
//!    bits_per_symbol` of symbol `t`, so each character is spread across every
//!    bit position and a lost tone damages all characters a little rather than
//!    one character fatally.
//!
//! Transcribed from Pawel Jalocha's reference implementation (`MFSK_Encoder` /
//! `MFSK_SoftDecoder` in fldigi's `pj_mfsk.h`) and cross-checked against it;
//! see `tools/ref/olivia`.

use super::fht::{fht, ifht};
use super::mode::{Mode, SYMBOLS_PER_BLOCK};

/// The fixed scrambling sequence, applied to every character's Walsh code.
pub const SCRAMBLING_CODE: u64 = 0xE257_E6D0_2915_74EC;

/// How far the scrambling code is rotated between successive characters of a
/// block. (Contestia, the same modem with 6-bit characters, uses 5.)
pub const CODE_SHIFT: usize = 13;

/// Characters are 7-bit; the top bit inverts the Walsh function.
const CHAR_MASK: u8 = 0x7F;

/// Spread one character into 64 ±1 chips, scrambled for character slot
/// `freq_bit`.
fn spread(ch: u8, freq_bit: usize) -> [i32; SYMBOLS_PER_BLOCK] {
    let ch = (ch & CHAR_MASK) as usize;
    let mut buf = [0i32; SYMBOLS_PER_BLOCK];
    // Low 6 bits choose the Walsh function; bit 6 flips its sign.
    if ch < SYMBOLS_PER_BLOCK {
        buf[ch] = 1;
    } else {
        buf[ch - SYMBOLS_PER_BLOCK] = -1;
    }
    ifht(&mut buf);
    scramble(&mut buf, freq_bit * CODE_SHIFT);
    buf
}

/// Negate the chips selected by the scrambling code, starting `offset` bits in.
fn scramble<T: Copy + std::ops::Neg<Output = T>>(chips: &mut [T], offset: usize) {
    let wrap = SYMBOLS_PER_BLOCK - 1;
    let mut code_bit = offset & wrap;
    for c in chips.iter_mut() {
        if SCRAMBLING_CODE & (1u64 << code_bit) != 0 {
            *c = -*c;
        }
        code_bit = (code_bit + 1) & wrap;
    }
}

/// Encode `chars_per_block` characters into 64 symbol values.
///
/// Short input is padded with NUL, which Olivia uses as its idle character.
/// The values returned are symbol *values*, not tone indices — the modulator
/// Gray-codes them before choosing a tone.
pub fn encode_block(chars: &[u8], mode: Mode) -> [u8; SYMBOLS_PER_BLOCK] {
    let bps = mode.bits_per_symbol();
    let mut out = [0u8; SYMBOLS_PER_BLOCK];
    for freq_bit in 0..bps {
        let chips = spread(chars.get(freq_bit).copied().unwrap_or(0), freq_bit);
        for (time_bit, &chip) in chips.iter().enumerate() {
            if chip < 0 {
                out[time_bit] |= 1 << ((freq_bit + time_bit) % bps);
            }
        }
    }
    out
}

/// What the decoder recovered from one block.
#[derive(Clone, Debug)]
pub struct Block {
    /// The `chars_per_block` decoded characters, in order.
    pub chars: Vec<u8>,
    /// Mean Walsh correlation peak.
    pub signal: f64,
    /// Mean energy in the 63 non-peak Walsh positions.
    pub noise: f64,
}

impl Block {
    /// Peak-to-noise ratio of the Walsh correlation — the quantity Olivia
    /// receivers sync on.
    ///
    /// Correlating *noise* against 64 Walsh functions and keeping the largest
    /// still yields about 2.6, so that, not zero, is the floor to judge this
    /// against. A noise-free block has no competing correlation at all and
    /// scores infinity.
    pub fn snr(&self) -> f64 {
        if self.noise > 0.0 {
            self.signal / self.noise.sqrt()
        } else if self.signal > 0.0 {
            f64::INFINITY
        } else {
            0.0
        }
    }

    /// Decoded characters as displayable text.
    ///
    /// NUL is Olivia's idle character and disappears; line breaks become
    /// spaces, since a monitor shows one running line per station.
    pub fn text(&self) -> String {
        self.chars
            .iter()
            .filter_map(|&c| match c {
                b'\r' | b'\n' | b'\t' => Some(' '),
                0x20..=0x7E => Some(c as char),
                _ => None,
            })
            .collect()
    }
}

/// Soft-decode one block out of a flat grid of per-bit soft values.
///
/// `soft[slot * bits_per_symbol + bit]` is the receiver's confidence in bit
/// `bit` of the symbol in time slot `slot`: positive for a 0, negative for a 1,
/// magnitude proportional to certainty. (That sign convention is the
/// reference's, and it is why the correlation peak's sign carries the
/// character's top bit.) The block's 64 symbols are taken from `start_slot`,
/// every `stride` slots — a stride above 1 lets the caller keep a finer time
/// grid than one slot per symbol and search block timing within it.
pub fn decode_block(soft: &[f32], start_slot: usize, stride: usize, mode: Mode) -> Block {
    let bps = mode.bits_per_symbol();
    let mut chars = Vec::with_capacity(bps);
    let mut signal = 0.0f64;
    let mut noise = 0.0f64;
    for freq_bit in 0..bps {
        let (ch, peak, n) = correlate(soft, start_slot, stride, mode, freq_bit);
        chars.push(ch);
        signal += peak;
        noise += n;
    }
    Block { chars, signal: signal / bps as f64, noise: noise / bps as f64 }
}

/// Peak-to-noise ratio of a block's *first* character alone.
///
/// A receiver hunting for signals tests enormous numbers of frequency/timing
/// hypotheses and rejects almost all of them, so it pays to ask the cheapest
/// question first: one character costs `1/bits_per_symbol` of a whole block and
/// already separates a real signal from noise well enough to throw out the
/// hopeless cases. Survivors get [`decode_block`].
pub fn screen_snr(soft: &[f32], start_slot: usize, stride: usize, mode: Mode) -> f64 {
    let (_, peak, noise) = correlate(soft, start_slot, stride, mode, 0);
    if noise > 0.0 {
        peak / noise.sqrt()
    } else if peak > 0.0 {
        f64::INFINITY
    } else {
        0.0
    }
}

/// Correlate one character of a block against all 64 Walsh functions.
///
/// Returns the character, the size of the winning correlation, and the mean
/// energy of the 63 losing ones.
fn correlate(
    soft: &[f32],
    start_slot: usize,
    stride: usize,
    mode: Mode,
    freq_bit: usize,
) -> (u8, f64, f64) {
    let bps = mode.bits_per_symbol();

    // Undo the interleave: chip `t` of this character rode in bit
    // `(freq_bit + t) mod bps` of symbol `t`.
    let mut buf = [0f64; SYMBOLS_PER_BLOCK];
    let mut bit = freq_bit % bps;
    let mut slot = start_slot;
    for b in buf.iter_mut() {
        *b = soft[slot * bps + bit] as f64;
        bit += 1;
        if bit >= bps {
            bit -= bps;
        }
        slot += stride;
    }
    scramble(&mut buf, freq_bit * CODE_SHIFT);
    fht(&mut buf);

    // The Walsh function with the largest |correlation| is the character;
    // a negative peak means its top bit is set.
    let mut peak = 0.0f64;
    let mut peak_pos = 0usize;
    let mut sqr_sum = 0.0f64;
    for (i, &v) in buf.iter().enumerate() {
        sqr_sum += v * v;
        if v.abs() > peak.abs() {
            peak = v;
            peak_pos = i;
        }
    }
    let mut ch = peak_pos as u8;
    if peak < 0.0 {
        ch += SYMBOLS_PER_BLOCK as u8;
    }
    (ch, peak.abs(), (sqr_sum - peak * peak) / (SYMBOLS_PER_BLOCK - 1) as f64)
}

/// Split text into blocks of `chars_per_block` 7-bit characters, NUL-padded.
///
/// Anything outside 7-bit ASCII is replaced with `?`, since a character *is* a
/// 7-bit value in this protocol.
pub fn text_to_blocks(text: &str, mode: Mode) -> Vec<Vec<u8>> {
    let n = mode.chars_per_block();
    let bytes: Vec<u8> = text.chars().map(|c| if c.is_ascii() { c as u8 } else { b'?' }).collect();
    bytes
        .chunks(n)
        .map(|c| {
            let mut v = c.to_vec();
            v.resize(n, 0);
            v
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::olivia::mode::{OL_16_500, OL_32_1000, OL_8_250};

    /// Turn hard symbol values back into the flat soft grid a perfect receiver
    /// would produce (+1 for a 0 bit, -1 for a 1 bit).
    fn ideal_soft(symbols: &[u8], mode: Mode) -> Vec<f32> {
        symbols
            .iter()
            .flat_map(|&s| {
                (0..mode.bits_per_symbol()).map(move |b| if s >> b & 1 == 1 { -1.0 } else { 1.0 })
            })
            .collect()
    }

    #[test]
    fn block_round_trip_is_exact() {
        for mode in [OL_8_250, OL_16_500, OL_32_1000] {
            let chars: Vec<u8> =
                "RAGCHEW!".bytes().take(mode.chars_per_block()).collect();
            let syms = encode_block(&chars, mode);
            let block = decode_block(&ideal_soft(&syms, mode), 0, 1, mode);
            assert_eq!(block.chars, chars, "{}", mode.name());
            assert!(block.snr() > 100.0, "a noise-free block should correlate perfectly");
        }
    }

    /// Every 7-bit value survives the round trip, including the top-bit-set
    /// half that rides on an inverted Walsh function.
    #[test]
    fn all_7_bit_characters_round_trip() {
        let mode = OL_16_500;
        for c in 0..=127u8 {
            let chars = vec![c; mode.chars_per_block()];
            let syms = encode_block(&chars, mode);
            let block = decode_block(&ideal_soft(&syms, mode), 0, 1, mode);
            assert_eq!(block.chars, chars, "character {c}");
        }
    }

    /// A block is 64 symbols regardless of mode, and each symbol fits in
    /// `bits_per_symbol` bits.
    #[test]
    fn symbols_stay_in_range() {
        for mode in [OL_8_250, OL_16_500, OL_32_1000] {
            let syms = encode_block(b"HELLO", mode);
            assert_eq!(syms.len(), SYMBOLS_PER_BLOCK);
            assert!(syms.iter().all(|&s| (s as u16) < mode.tones));
        }
    }

    /// Losing a quarter of the symbols to interference still decodes — this is
    /// the whole point of spreading each character over 64 chips.
    #[test]
    fn survives_a_quarter_of_symbols_being_wrong() {
        let mode = OL_32_1000;
        let chars = b"HELLO".to_vec();
        let syms = encode_block(&chars, mode);
        let mut soft = ideal_soft(&syms, mode);
        let bps = mode.bits_per_symbol();
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        for sym in 0..SYMBOLS_PER_BLOCK {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            if rng >> 62 == 0 {
                // this symbol was received as a random other tone
                let wrong = (rng >> 32) as u8 % mode.tones as u8;
                for b in 0..bps {
                    soft[sym * bps + b] = if wrong >> b & 1 == 1 { -1.0 } else { 1.0 };
                }
            }
        }
        let block = decode_block(&soft, 0, 1, mode);
        assert_eq!(block.chars, chars);
        assert!(block.snr() > 3.0, "SNR was {}", block.snr());
    }

    #[test]
    fn text_splits_into_padded_blocks() {
        let b = text_to_blocks("ABCDEFG", OL_32_1000); // 5 chars per block
        assert_eq!(b, vec![b"ABCDE".to_vec(), vec![b'F', b'G', 0, 0, 0]]);
    }
}
