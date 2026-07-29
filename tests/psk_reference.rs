//! The PSK31 varicode table, checked against fldigi's.
//!
//! Varicode is not an algorithm — it is 256 hand-assigned codes, and a single
//! transcribed digit is a character that decodes wrong on the air for ever
//! without any test in this crate noticing. PSK31 carries no FEC and no CRC, so
//! nothing downstream would catch it either: a wrong code produces a plausible
//! character, not an error. Hence a reference.
//!
//! `tests/vectors/psk_varicode.txt` is produced by `tools/ref/psk/extract.py`
//! from fldigi's `pskvaricode.cxx`. Each line is:
//!
//! ```text
//! ascii_code|bits
//! ```

use ragchew::psk::varicode::{self, VARICODE};

fn reference() -> Vec<(u8, &'static str)> {
    let v: Vec<(u8, &str)> = include_str!("vectors/psk_varicode.txt")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (i, code) = l.split_once('|').expect("malformed line");
            (i.parse().expect("bad index"), code)
        })
        .collect();
    assert_eq!(v.len(), 256, "the reference covers the whole byte range");
    v
}

/// Every code this crate implements, digit for digit as fldigi sends it.
#[test]
fn every_code_matches_fldigi() {
    for (i, code) in reference() {
        if !(32..=126).contains(&i) {
            continue;
        }
        let c = i as char;
        assert_eq!(varicode::code_of(c), Some(code), "wrong code for {c:?} ({i})");
        assert_eq!(varicode::char_of(code), Some(c), "{code} does not decode back to {c:?}");
    }
}

/// The printable range is covered completely and nothing beyond it is claimed.
///
/// The C0 block and the extended set are left out deliberately — see
/// [`varicode`](ragchew::psk::varicode) — and this is what keeps that a
/// decision rather than an omission nobody rechecked.
#[test]
fn the_table_is_exactly_printable_ascii() {
    assert_eq!(VARICODE.len(), 95);
    for (c, _) in VARICODE {
        assert!((' '..='~').contains(c), "{c:?} is outside printable ASCII");
    }
}

/// Leaving the other 161 codes out must not mean quietly decoding them as
/// something else.
///
/// It cannot, because all 256 reference codes are distinct — but that is a
/// property of the reference, not of this crate, so it is checked rather than
/// assumed. If a future table ever aliased a control code onto a printable one,
/// a `<NUL>` on the air would print as a letter.
#[test]
fn the_codes_we_do_not_implement_decode_as_nothing() {
    for (i, code) in reference() {
        if (32..=126).contains(&i) {
            continue;
        }
        assert_eq!(varicode::char_of(code), None, "code {code} for byte {i} aliases a character");
    }
}

/// Round-trip a QSO's worth of text through the reference codes directly,
/// rather than through our own table twice — which would agree with itself
/// whatever it contained.
#[test]
fn text_encodes_to_the_reference_bit_stream() {
    let text = "cq cq de w1aw/4 pse k 73";
    let refs = reference();

    let mut want: Vec<u8> = Vec::new();
    for c in text.chars() {
        want.extend(refs[c as usize].1.bytes().map(|b| b - b'0'));
        want.extend([0, 0]);
    }
    assert_eq!(varicode::encode(text), want);

    // And back, joined mid-stream the way a band scan joins one.
    let mut bits = vec![0u8, 0];
    bits.extend(want);
    let got: String = varicode::decode(&bits).iter().map(|d| d.ch).collect();
    assert_eq!(got, text);
}
