//! The band scan is threaded, and its answer must not be.
//!
//! `protocol::decode_all` runs a thread per mode. That is worth about four
//! times the throughput on a live acquisition, and it is worth nothing at all
//! if what comes back depends on which thread finished first — the app groups
//! decodes into conversations by arrival, the CSV is a record, and the demo
//! band's tests assert exact text. So the merge is by mode order and the final
//! sort is stable, and this is what holds them to it.

use ragchew::js8::message::{self, IType};
use ragchew::js8::modem;
use ragchew::protocol::{self, Decode, ModeId, Protocol};
use ragchew::{olivia, psk, SAMPLE_RATE};

const BAND: (f64, f64) = (300.0, 2600.0);

fn noise(n: usize, rms: f32, seed: u64) -> Vec<f32> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((s >> 40) as f32 / (1u32 << 24) as f32 - 0.5) * rms * 3.46
        })
        .collect()
}

fn mix(buf: &mut [f32], at: usize, sig: &[f32], amp: f32) {
    for (i, s) in sig.iter().enumerate() {
        if at + i < buf.len() {
            buf[at + i] += s * amp;
        }
    }
}

/// A band with something for every thread to find: two JS8 submodes, two
/// Olivia modes at different widths, and a PSK31 station.
fn band() -> Vec<f32> {
    let rate = SAMPLE_RATE as usize;
    let mut audio = noise(30 * rate, 0.02, 0x5EED_1234);

    let (a87, _) = message::freetext("DE K2ABC EM73", IType::None);
    mix(&mut audio, rate / 2, &modem::encode_audio(&a87, 800.0), 0.45);

    let mut long = String::new();
    while long.chars().count() < 300 {
        long.push_str("cq cq de w1aw k ");
    }
    mix(&mut audio, 0, &psk::encode(&long, 1500.0, psk::PSK31), 0.4);

    mix(&mut audio, rate, &olivia::encode("DE VE3XYZ RST 599 ", 2200.0, olivia::OL_16_500), 0.35);
    mix(&mut audio, rate, &olivia::encode("DE W1WIDE QRP ", 1100.0, olivia::OL_32_1000), 0.35);
    audio
}

/// One decode, as a value that can be compared: everything the app or the log
/// would ever show.
fn shape(d: &Decode) -> String {
    format!("{:?} {:.3} {:.4} {:.4} {:?} {} {:?}",
            d.mode, d.hz, d.time_s, d.quality, d.snr_db, d.ends_over, d.text)
}

/// The same audio scanned twice gives the same answer, decode for decode and
/// in the same order.
#[test]
fn a_scan_is_the_same_scan_twice() {
    let audio = band();
    let modes = protocol::default_modes();
    let a = protocol::decode_all(&audio, BAND.0, BAND.1, &modes);
    let b = protocol::decode_all(&audio, BAND.0, BAND.1, &modes);
    assert!(!a.is_empty(), "the band decoded to nothing");
    let (a, b): (Vec<String>, Vec<String>) =
        (a.iter().map(shape).collect(), b.iter().map(shape).collect());
    assert_eq!(a, b, "two scans of one band disagreed");
}

/// Splitting the scan across threads neither loses a decode nor invents one.
///
/// Held against the modes scanned one at a time, which is the arrangement the
/// threads replace. PSK is left out of the comparison on purpose: it is judged
/// against what the other modes found, so a PSK decode that survives alone can
/// legitimately lose in company — the property this crate has tests for
/// elsewhere, and the reason PSK is a second phase rather than another thread.
#[test]
fn every_mode_reports_what_it_would_have_reported_alone() {
    let audio = band();
    let together = protocol::decode_all(&audio, BAND.0, BAND.1, &protocol::default_modes());

    for m in protocol::default_modes() {
        if m.protocol() == Protocol::Psk {
            continue;
        }
        let alone = protocol::decode_all(&audio, BAND.0, BAND.1, &[m]);
        let mut want: Vec<String> = alone.iter().map(shape).collect();
        let mut got: Vec<String> =
            together.iter().filter(|d| d.mode == m).map(shape).collect();
        want.sort();
        got.sort();
        assert_eq!(got, want, "{} differs when scanned with the others", m.name());
    }
}

/// A scan of one mode is still a scan of one mode: the fast path that keeps it
/// on the caller's thread has to agree with the threaded one.
#[test]
fn one_mode_alone_agrees_with_one_mode_among_many() {
    let audio = band();
    let m = ModeId::Olivia(olivia::OL_16_500);
    let alone: Vec<String> =
        protocol::decode_all(&audio, BAND.0, BAND.1, &[m]).iter().map(shape).collect();
    let pair = [m, ModeId::Olivia(olivia::OL_32_1000)];
    let among: Vec<String> = protocol::decode_all(&audio, BAND.0, BAND.1, &pair)
        .iter()
        .filter(|d| d.mode == m)
        .map(shape)
        .collect();
    assert_eq!(alone, among);
}
