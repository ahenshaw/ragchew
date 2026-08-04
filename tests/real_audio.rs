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

/// A real station's over ends where it said it did, and nowhere earlier.
///
/// The itype flags are what a reader splits a JS8 conversation on — a station
/// sending nine frames means one thing, not nine — so what matters is that a
/// real transmitter, off the air, sets them the way the specification says and
/// this crate reads them back. It does: eight frames clear, the ninth flagged,
/// and it is the ninth that renders with the `<>` JS8Call draws.
#[test]
fn the_recording_flags_only_its_last_frame_as_the_end_of_the_over() {
    let (samples, _) = wav::read("tests/vectors/john_3_16.wav").expect("read wav");
    let mut results = modem::decode_all(&samples, 1200.0, 1600.0, 30);
    results.sort_by_key(|r| r.offset);
    assert!(results.len() >= 9, "expected all 9 frames, got {}", results.len());

    let flags: Vec<bool> = results.iter().map(|r| message::ends_over(&r.a87)).collect();
    let want: Vec<bool> = (0..results.len()).map(|i| i + 1 == results.len()).collect();
    assert_eq!(flags, want, "end-of-over flags across the recording");
    assert!(
        message::unpack(&results[results.len() - 1].a87).text().ends_with("<>"),
        "the flagged frame did not render as the end of an over"
    );
}
