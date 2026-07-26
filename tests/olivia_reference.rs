//! Bit-exact validation of the Olivia FEC against Pawel Jalocha's reference —
//! the same implementation fldigi transmits and receives with, so agreeing with
//! it here is what makes a `ragchew` decode of a real signal mean anything.
//!
//! `tests/vectors/olivia_vectors.txt` is produced by `tools/ref/olivia`. Each
//! line is:
//!
//! ```text
//! bits_per_symbol|chars(hex)|symbols(64)|tones(64)|decoded(hex)
//! ```

use ragchew::olivia::fec;
use ragchew::olivia::Mode;

struct Vector {
    mode: Mode,
    chars: Vec<u8>,
    symbols: Vec<u8>,
    tones: Vec<u8>,
    decoded: Vec<u8>,
}

fn hex(s: &str) -> Vec<u8> {
    s.as_bytes().chunks(2).map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap()).collect()
}

fn nums(s: &str) -> Vec<u8> {
    s.split(' ').map(|x| x.parse().unwrap()).collect()
}

fn load_vectors() -> Vec<Vector> {
    include_str!("vectors/olivia_vectors.txt")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split('|').collect();
            assert_eq!(f.len(), 5, "malformed line: {l}");
            let bits: u32 = f[0].parse().unwrap();
            Vector {
                // Bandwidth does not enter the FEC at all — it only sets timing
                // — so any bandwidth exercises the same coding.
                mode: Mode::new(1 << bits, 500),
                chars: hex(f[1]),
                symbols: nums(f[2]),
                tones: nums(f[3]),
                decoded: hex(f[4]),
            }
        })
        .collect()
}

#[test]
fn vectors_cover_every_common_tone_count() {
    let vs = load_vectors();
    assert!(vs.len() >= 40, "only {} vectors", vs.len());
    for tones in [4u16, 8, 16, 32] {
        assert!(vs.iter().any(|v| v.mode.tones == tones), "no {tones}-tone vectors");
    }
    // the awkward cases are worth having: idle blocks and top-bit-set characters
    assert!(vs.iter().any(|v| v.chars.iter().all(|&c| c == 0)), "no all-idle block");
    assert!(vs.iter().any(|v| v.chars.iter().any(|&c| c >= 0x40)), "no high characters");
}

/// Spreading, scrambling and interleaving produce exactly the reference's 64
/// symbol values.
#[test]
fn block_symbols_bit_exact() {
    for v in load_vectors() {
        let got = fec::encode_block(&v.chars, v.mode);
        assert_eq!(
            got.to_vec(),
            v.symbols,
            "{} symbols differ for chars {:02x?}",
            v.mode.name(),
            v.chars
        );
    }
}

/// The Gray mapping from symbol value to transmitted tone matches the
/// reference's, which is what puts our tones on the same carriers as fldigi's.
#[test]
fn tone_mapping_bit_exact() {
    for v in load_vectors() {
        let got: Vec<u8> = fec::encode_block(&v.chars, v.mode)
            .iter()
            .map(|&s| s ^ (s >> 1)) // GrayCode()
            .collect();
        assert_eq!(got, v.tones, "{} tones differ", v.mode.name());
    }
}

/// Our soft decoder recovers what the reference's soft decoder recovers, from
/// the same ideal input — including the top-bit-set characters that ride on an
/// inverted Walsh function.
#[test]
fn soft_decode_matches_reference() {
    for v in load_vectors() {
        let bps = v.mode.bits_per_symbol();
        let soft: Vec<f32> = v
            .symbols
            .iter()
            .flat_map(|&s| (0..bps).map(move |b| if s >> b & 1 == 1 { -1.0 } else { 1.0 }))
            .collect();
        let block = fec::decode_block(&soft, 0, 1, v.mode);
        assert_eq!(block.chars, v.decoded, "{} decode differs", v.mode.name());
        // and the reference agrees with what went in, 7 bits at a time
        assert_eq!(block.chars, v.chars);
    }
}
