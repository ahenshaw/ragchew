//! Per-submode round-trip and multi-mode auto-detection.

use js8::message::{self, IType};
use js8::{modem, submode, Mode};

fn cycle_audio(bursts: &[(Vec<f32>, f64)], total_s: usize) -> Vec<f32> {
    let rate = modem::SAMPLE_RATE as usize;
    let mut audio = vec![0.0f32; total_s * rate];
    let start = rate / 2; // 0.5 s into the cycle
    for (burst, amp) in bursts {
        for (i, s) in burst.iter().enumerate() {
            if start + i < audio.len() {
                audio[start + i] += *s * *amp as f32;
            }
        }
    }
    audio
}

/// Every submode encodes and decodes its own signal, tagged with the right mode.
#[test]
fn per_submode_roundtrip() {
    for sm in submode::ALL {
        let text = "TEST DE MODE";
        let (a87, _) = message::freetext(text, IType::None);
        let burst = modem::encode_audio_sm(&a87, 1500.0, &sm);
        // window long enough for the frame + lead/tail
        let secs = (sm.frame_secs().ceil() as usize) + 4;
        let audio = cycle_audio(&[(burst, 1.0)], secs);

        let res = modem::decode_all_sm(&audio, 300.0, 2600.0, 30, &sm);
        let ok = res.iter().any(|r| {
            r.mode == sm.mode
                && message::crc_ok(&r.a87)
                && message::unpack(&r.a87).text() == text
        });
        assert!(ok, "submode {:?} failed to round-trip", sm.mode);
    }
}

/// Three different submodes coexisting in one band are each auto-detected.
#[test]
fn multimode_autodetect() {
    let cases = [
        ("HELLO NORMAL", 1000.0, submode::NORMAL),
        ("HI FAST", 1500.0, submode::FAST),
        ("YO TURBO", 2100.0, submode::TURBO),
    ];
    let bursts: Vec<(Vec<f32>, f64)> = cases
        .iter()
        .map(|(text, hz, sm)| {
            let (a87, _) = message::freetext(text, IType::None);
            (modem::encode_audio_sm(&a87, *hz, sm), 0.5)
        })
        .collect();
    let audio = cycle_audio(&bursts, 40);

    let res = modem::decode_all_multi(&audio, 300.0, 2600.0, 30);
    for (text, _, sm) in cases {
        let found = res
            .iter()
            .find(|r| r.mode == sm.mode && message::unpack(&r.a87).text() == text);
        assert!(found.is_some(), "missing {text:?} decoded as {:?}", sm.mode);
    }
    // sanity: the tag helper works
    assert_eq!(Mode::Normal.tag(), 'A');
}
