//! End-to-end decode of a real off-air JS8 recording (John 3:16, sent as a
//! 9-frame transmission with a drifting carrier around ~1390 Hz). This is the
//! same signal the reference decoder recovers 9/9; we assert full parity.

use ragchew::js8::{message, modem};
use ragchew::wav;

#[test]
fn decodes_john_3_16_recording() {
    let (samples, rate) = wav::read("tests/vectors/john_3_16.wav").expect("read wav");
    assert_eq!(rate, modem::SAMPLE_RATE);

    let results = modem::decode_all(&samples, 1200.0, 1600.0, 30);
    let text: String = results
        .iter()
        .map(|r| message::unpack(&r.a87).text())
        .collect();

    // Every frame's CRC must be valid.
    assert!(results.iter().all(|r| message::crc_ok(&r.a87)));

    // The concatenated frames spell out John 3:16 (NET). The carrier drifts
    // across integer-bin boundaries, so this exercises the sub-bin search.
    let phrases = [
        "FOR GOD SO ",
        "LOVED THE WORLD IN ",
        "THIS WAY; HE GAVE ",
        "HIS ONE AND ONLY SO",
        "N, SO THAT EVERYONE ",
        "WHO BELIEVES IN HIM ",
        "WILL NOT PERISH BUT ",
        "HAVE ETERNAL LIFE.",
    ];
    for p in phrases {
        assert!(text.contains(p), "missing phrase {p:?} in decode:\n{text}");
    }
    assert!(
        results.len() >= 9,
        "expected all 9 frames, got {}:\n{text}",
        results.len()
    );
}
