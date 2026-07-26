//! Bit-exact validation of the encoder/decoder against the `fate` reference.
//!
//! `tests/vectors/reference_vectors.txt` is produced by `tools/ref/harness.cc`
//! linked against Robert Morris's independent JS8 implementation. Each line is:
//!
//! ```text
//! KIND|itype|text|a87(87 bits)|a174(174 bits)|a79(79 symbols)|unpacked
//! ```

use ragchew::js8::message::{self, IType};
use ragchew::js8::{ldpc, modem};

fn itype_from(n: u8) -> IType {
    match n {
        0 => IType::None,
        2 => IType::Last,
        3 => IType::FirstAndLast,
        other => panic!("unexpected itype {other}"),
    }
}

fn bits(s: &str) -> Vec<u8> {
    s.bytes().map(|b| b - b'0').collect()
}

struct Vector {
    itype: IType,
    text: String,
    a87: Vec<u8>,
    a174: Vec<u8>,
    a79: Vec<u8>,
    unpacked: String,
}

fn load_vectors() -> Vec<Vector> {
    let raw = include_str!("vectors/reference_vectors.txt");
    raw.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split('|').collect();
            assert!(f.len() >= 7, "malformed line: {l}");
            Vector {
                itype: itype_from(f[1].parse().unwrap()),
                text: f[2].to_string(),
                a87: bits(f[3]),
                a174: bits(f[4]),
                a79: f[5].split(' ').map(|x| x.parse().unwrap()).collect(),
                unpacked: f[6].to_string(),
            }
        })
        .collect()
}

/// The CRC + framing produce exactly the reference's 87 message bits.
#[test]
fn a87_bit_exact() {
    for v in load_vectors() {
        let (a87, _consumed) = message::freetext(&v.text, v.itype);
        assert_eq!(
            a87.to_vec(),
            v.a87,
            "a87 mismatch for {:?} itype {:?}",
            v.text,
            v.itype
        );
    }
}

/// LDPC(174,87) encoding matches the reference codeword.
#[test]
fn ldpc_codeword_bit_exact() {
    for v in load_vectors() {
        let mut a87 = [0u8; 87];
        a87.copy_from_slice(&v.a87);
        let cw = ldpc::encode(&a87);
        assert_eq!(cw.to_vec(), v.a174, "codeword mismatch for {:?}", v.text);
    }
}

/// recode() + Costas insertion produce the reference's 79 tone symbols.
#[test]
fn symbols_bit_exact() {
    for v in load_vectors() {
        let mut cw = [0u8; 174];
        cw.copy_from_slice(&v.a174);
        let syms = modem::recode(&cw);
        assert_eq!(syms.to_vec(), v.a79, "symbols mismatch for {:?}", v.text);
    }
}

/// Parsing the 87 bits reproduces the reference's textual message.
#[test]
fn unpack_matches_reference() {
    for v in load_vectors() {
        let mut a87 = [0u8; 87];
        a87.copy_from_slice(&v.a87);
        assert!(message::crc_ok(&a87), "reference bits must pass CRC: {:?}", v.text);
        let msg = message::unpack(&a87);
        assert_eq!(msg.text(), v.unpacked, "unpack mismatch for {:?}", v.text);
    }
}

/// Full audio round-trip: our encoder -> our decoder recovers the same bits/text.
#[test]
fn audio_roundtrip_clean() {
    for v in load_vectors() {
        let mut a87 = [0u8; 87];
        a87.copy_from_slice(&v.a87);
        let base_hz = 1500.0;
        let burst = modem::encode_audio(&a87, base_hz);
        // place after 0.5 s of silence, like a real cycle
        let mut audio = vec![0.0f32; ragchew::js8::CYCLE_LEAD_SAMPLES];
        audio.extend_from_slice(&burst);
        audio.resize(15 * modem::SAMPLE_RATE as usize, 0.0);

        let r = modem::decode_near(&audio, base_hz, ragchew::js8::CYCLE_LEAD_SAMPLES, 4, 30)
            .expect("sync should be found");
        assert!(r.ldpc_ok, "LDPC should converge for {:?} (score {})", v.text, r.ldpc_score);
        assert_eq!(r.a87.to_vec(), v.a87, "decoded bits differ for {:?}", v.text);
        assert!(message::crc_ok(&r.a87));
        assert_eq!(message::unpack(&r.a87).text(), v.unpacked);
    }
}

/// Decode the reference-synthesized WAV (their modulator -> our decoder).
#[test]
fn decode_reference_wav() {
    let (samples, rate) = ragchew::wav::read("tests/vectors/hello_1500.wav").expect("read wav");
    assert_eq!(rate, modem::SAMPLE_RATE);
    // reference placed the burst after 0.5 s of silence
    let start = ragchew::js8::CYCLE_LEAD_SAMPLES;
    let r = modem::decode_near(&samples, 1500.0, start, 6, 30).expect("sync");
    assert!(r.ldpc_ok, "should decode reference wav (score {})", r.ldpc_score);
    assert_eq!(message::unpack(&r.a87).text(), "HELLO WORLD");
}
