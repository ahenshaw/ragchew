//! PSK in a band with everything else in it.
//!
//! The module's own tests cover the modem in isolation. These cover the thing
//! the crate actually claims: that a PSK station, a JS8 station and an Olivia
//! station sharing one stretch of audio all come back from a single scan, each
//! tagged with what carried it — and, just as importantly, that PSK stays quiet
//! when there is no PSK there.

use ragchew::js8::message::{self, IType};
use ragchew::js8::{self, modem};
use ragchew::protocol::{self, ModeId, Protocol};
use ragchew::psk::{self, Mode, PSK31, PSK63};
use ragchew::{olivia, SAMPLE_RATE};

const BAND: (f64, f64) = (300.0, 2600.0);

/// Deterministic noise, at the level a quiet receiver actually presents.
///
/// Not optional. Both stages of the PSK gate are ratios against the local noise
/// floor, so a synthetic band with nothing but signal in it has no floor to
/// measure against.
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

/// A continuous PSK transmission long enough to fill several analysis blocks.
fn psk_station(text: &str, hz: f64) -> Vec<f32> {
    psk_station_in(text, hz, PSK31)
}

fn psk_station_in(text: &str, hz: f64, mode: Mode) -> Vec<f32> {
    let mut s = String::new();
    while s.chars().count() < 300 {
        s.push_str(text);
    }
    psk::encode(&s, hz, mode)
}

fn text_of(decodes: &[protocol::Decode], p: Protocol) -> String {
    decodes.iter().filter(|d| d.mode.protocol() == p).map(|d| d.text.clone()).collect()
}

/// Three protocols, one band, one scan.
#[test]
fn decodes_psk_alongside_js8_and_olivia() {
    let rate = SAMPLE_RATE as usize;
    let mut audio = noise(45 * rate, 0.02, 0xB0FF_1234);

    let psk = psk_station("cq cq de w1aw k ", 1500.0);
    mix(&mut audio, 0, &psk, 0.4);

    let (a87, _) = message::freetext("DE K2ABC EM73", IType::None);
    mix(&mut audio, rate / 2, &modem::encode_audio(&a87, 800.0), 0.45);

    let ol = olivia::encode("DE VE3XYZ RST 599 ", 2200.0, olivia::OL_16_500);
    mix(&mut audio, rate, &ol, 0.35);

    let modes = protocol::default_modes();
    let out = protocol::decode_all(&audio, BAND.0, BAND.1, &modes);

    let psk_text = text_of(&out, Protocol::Psk);
    let js8_text = text_of(&out, Protocol::Js8);
    let ol_text = text_of(&out, Protocol::Olivia);

    assert!(psk_text.contains("cq cq de w1aw k"), "PSK31 text was {psk_text:?}");
    assert!(js8_text.contains("DE K2ABC EM73"), "JS8 text was {js8_text:?}");
    assert!(ol_text.contains("DE VE3XYZ"), "Olivia text was {ol_text:?}");

    // Each is reported at its own frequency, so the panorama can place them.
    let at = |p: Protocol| {
        out.iter().find(|d| d.mode.protocol() == p).map(|d| d.hz).unwrap_or(0.0)
    };
    assert!((at(Protocol::Psk) - 1500.0).abs() < 10.0, "PSK at {}", at(Protocol::Psk));
    assert!((at(Protocol::Js8) - 800.0).abs() < 20.0, "JS8 at {}", at(Protocol::Js8));
    assert!((at(Protocol::Olivia) - 2200.0).abs() < 60.0, "Olivia at {}", at(Protocol::Olivia));
}

/// A band with no PSK in it produces no PSK.
///
/// This is the assertion the whole three-stage gate exists to satisfy. JS8 and
/// Olivia signals demodulated as if they were BPSK yield bits, and five in six
/// of the words those frame into are valid varicode; nothing here may print.
#[test]
fn invents_nothing_from_a_band_of_other_protocols() {
    let rate = SAMPLE_RATE as usize;
    let mut audio = noise(45 * rate, 0.02, 0x5EED_0001);

    for (hz, text) in [(700.0, "CQ DE W1SLW"), (1900.0, "DE VE3XYZ QRP")] {
        let (a87, _) = message::freetext(text, IType::None);
        mix(&mut audio, rate / 2, &modem::encode_audio(&a87, hz), 0.45);
    }
    // Every Olivia mode, including the 4/125 that shares PSK31's symbol rate
    // and is narrow enough to sit inside its analysis slice.
    for (i, m) in olivia::COMMON.iter().enumerate() {
        let hz = 500.0 + i as f64 * 350.0;
        mix(&mut audio, rate, &olivia::encode("CQ DE TEST K ", hz, *m), 0.35);
    }

    let out = protocol::decode_all(&audio, BAND.0, BAND.1, &protocol::default_modes());
    let psk_text = text_of(&out, Protocol::Psk);
    assert!(psk_text.is_empty(), "invented PSK31 text {psk_text:?} from a band with none in it");
}

/// Pure noise, at several levels, prints nothing.
#[test]
fn invents_nothing_from_noise() {
    let rate = SAMPLE_RATE as usize;
    for (i, rms) in [0.01f32, 0.05, 0.2, 1.0].into_iter().enumerate() {
        let audio = noise(30 * rate, rms, 0xC0FF_EE00 + i as u64);
        let out = protocol::decode_all(&audio, BAND.0, BAND.1, &[ModeId::Psk(PSK31)]);
        let text: String = out.iter().map(|d| d.text.clone()).collect();
        assert!(out.is_empty(), "noise at rms {rms} printed {text:?}");
    }
}

/// The PSK31 station is reported as one channel at one frequency, not smeared
/// across the candidate grid it was found on.
#[test]
fn one_station_is_one_carrier() {
    let rate = SAMPLE_RATE as usize;
    // 1517 Hz sits between the 40 Hz screening candidates, which is the case
    // that would split a station in two if refinement did not converge.
    let mut audio = noise(40 * rate, 0.02, 0x1517);
    mix(&mut audio, 0, &psk_station("the quick brown fox ", 1517.0), 0.4);

    let out = protocol::decode_all(&audio, BAND.0, BAND.1, &[ModeId::Psk(PSK31)]);
    assert!(!out.is_empty(), "station not found at all");

    let mut hz: Vec<f64> = out.iter().map(|d| d.hz).collect();
    hz.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let spread = hz[hz.len() - 1] - hz[0];
    assert!(spread < 5.0, "one station came back spread over {spread:.1} Hz: {hz:?}");
    assert!((hz[0] - 1517.0).abs() < 3.0, "carrier estimated at {:.1} Hz", hz[0]);
}

/// A PSK31 mode survives a trip through its own name, as every other mode does.
#[test]
fn the_mode_round_trips_through_its_name() {
    let m = ModeId::Psk(PSK31);
    assert_eq!(protocol::parse_mode(&m.name()), Some(m));
    assert_eq!(protocol::parse_mode("psk31"), Some(m));
    assert_eq!(protocol::parse_mode("bpsk31"), Some(m));
    assert_eq!(protocol::parse_mode("psk63"), Some(ModeId::Psk(PSK63)));
    for m in psk::COMMON {
        assert!(protocol::default_modes().contains(&ModeId::Psk(m)), "{} not scanned", m.name());
    }
    // and does not shadow the modes that were already there
    assert_eq!(protocol::parse_mode("16/500"), Some(ModeId::Olivia(olivia::OL_16_500)));
    assert_eq!(protocol::parse_mode("js8:fast"), Some(ModeId::Js8(js8::Mode::Fast)));
}

/// Every scanned PSK mode round-trips through a band scan.
#[test]
fn every_common_mode_round_trips() {
    let rate = SAMPLE_RATE as usize;
    for (i, mode) in psk::COMMON.iter().enumerate() {
        let want = "cq cq de w1aw k ";
        let mut audio = noise(40 * rate, 0.02, 0x0DD0_0000 + i as u64);
        mix(&mut audio, 0, &psk_station_in(want, 1500.0, *mode), 0.4);

        let out = protocol::decode_all(&audio, BAND.0, BAND.1, &protocol::default_modes());
        let text: String = out
            .iter()
            .filter(|d| d.mode == ModeId::Psk(*mode))
            .map(|d| d.text.clone())
            .collect();
        assert!(text.contains(want), "{} decoded {text:?}", mode.name());
    }
}

/// One PSK mode does not invent the other.
///
/// This is not hypothetical. A BPSK31 envelope is rich in harmonics, so a
/// strong PSK31 station puts a line at 62.5 Hz where the PSK63 detector looks
/// for its own — it clears both of the first two stages outright. Only the
/// framing test rejects it, because 31.25-baud symbols read at 62.5 baud are
/// not varicode and do not frame like it.
#[test]
fn the_psk_modes_do_not_invent_each_other() {
    let rate = SAMPLE_RATE as usize;
    for (i, sent) in psk::COMMON.iter().enumerate() {
        // Loud, because the cross-mode leak is a high-SNR effect.
        let mut audio = noise(40 * rate, 0.02, 0x9A9A_0000 + i as u64);
        mix(&mut audio, 0, &psk_station_in("cq cq de w1aw k ", 1500.0, *sent), 0.9);

        let out = protocol::decode_all(&audio, BAND.0, BAND.1, &protocol::default_modes());
        for other in psk::COMMON.iter().filter(|m| *m != sent) {
            let ghost: String = out
                .iter()
                .filter(|d| d.mode == ModeId::Psk(*other))
                .map(|d| d.text.clone())
                .collect();
            assert!(
                ghost.is_empty(),
                "a {} station produced {} text {ghost:?}",
                sent.name(),
                other.name()
            );
        }
    }
}

/// Two PSK stations in different modes, decoded side by side.
#[test]
fn psk31_and_psk63_share_a_band() {
    let rate = SAMPLE_RATE as usize;
    let mut audio = noise(40 * rate, 0.02, 0x3163);
    mix(&mut audio, 0, &psk_station_in("this is the slow one ", 800.0, PSK31), 0.4);
    mix(&mut audio, 0, &psk_station_in("and this is the fast one ", 1900.0, PSK63), 0.4);

    let out = protocol::decode_all(&audio, BAND.0, BAND.1, &protocol::default_modes());
    let slow: String = out
        .iter()
        .filter(|d| d.mode == ModeId::Psk(PSK31))
        .map(|d| d.text.clone())
        .collect();
    let fast: String = out
        .iter()
        .filter(|d| d.mode == ModeId::Psk(PSK63))
        .map(|d| d.text.clone())
        .collect();
    assert!(slow.contains("this is the slow one"), "PSK31 decoded {slow:?}");
    assert!(fast.contains("and this is the fast one"), "PSK63 decoded {fast:?}");
}

/// Scanning mode by mode and merging must answer exactly as one scan does.
///
/// The file view splits its scan one thread per mode, so it finishes in the
/// time of the slowest single mode rather than the sum. That is sound for
/// detection — a mode's own threshold is what keeps a signal out of its own
/// results — but not for arbitration, which weighs one protocol's claim
/// against another's and so needs both in front of it. A thread scanning PSK
/// alone has no Olivia decode to yield to.
///
/// Olivia 4/125 at 745 Hz is a band that shows it. It shares PSK31's symbol
/// rate and is narrow enough to sit inside the analysis slice, and at this
/// carrier it clears the gate outright: scanned on its own, PSK31 reads a page
/// of rubbish off it at 749 Hz, at every level and every noise seed tried.
#[test]
fn scanning_mode_by_mode_arbitrates_the_same() {
    let rate = SAMPLE_RATE as usize;
    let mut audio = noise(30 * rate, 0.02, 0x9E37_79B9);
    mix(&mut audio, rate, &olivia::encode("CQ DE TEST K ", 745.0, olivia::OL_4_125), 0.35);

    let modes = protocol::default_modes();
    let together = protocol::decode_all(&audio, BAND.0, BAND.1, &modes);
    assert!(
        text_of(&together, Protocol::Psk).is_empty(),
        "one scan invented PSK text {:?}",
        text_of(&together, Protocol::Psk)
    );

    // What the file view's threads produce between them, before anything
    // reconciles them.
    let split: Vec<protocol::Decode> =
        modes.iter().flat_map(|m| protocol::decode_all(&audio, BAND.0, BAND.1, &[*m])).collect();
    assert!(
        !text_of(&split, Protocol::Psk).is_empty(),
        "no phantom to arbitrate: this band no longer exercises the case"
    );

    let settled = protocol::arbitrate(split.clone());
    assert!(
        text_of(&settled, Protocol::Psk).is_empty(),
        "arbitration left PSK text {:?} that a single scan drops",
        text_of(&settled, Protocol::Psk)
    );
    // And it took nothing else with it. Compared as a set, because arbitration
    // hands back what it kept in time order while the split scan produced it in
    // mode order — the ordering the file view wants, since it reveals decodes
    // as the clock reaches them.
    let others = |ds: &[protocol::Decode]| {
        let mut v: Vec<String> = ds
            .iter()
            .filter(|d| d.mode.protocol() != Protocol::Psk)
            .map(|d| format!("{} {:.0} {:.2} {}", d.mode.name(), d.hz, d.time_s, d.text))
            .collect();
        v.sort();
        v
    };
    assert_eq!(others(&settled), others(&split), "arbitration disturbed another protocol");
    assert!(
        settled.windows(2).all(|w| w[0].time_s <= w[1].time_s),
        "results came back out of time order"
    );
    // Running it again changes nothing: the file view arbitrates whenever a
    // scan lands, and a rescan re-runs it over results already settled.
    assert_eq!(protocol::arbitrate(settled.clone()).len(), settled.len(), "not idempotent");
}

/// A genuine PSK station inside a *wide* Olivia signal is not silenced by it:
/// stations stacked inside each other is a thing this monitor exists to show.
#[test]
fn arbitration_spares_a_real_station_under_a_wide_signal() {
    let rate = SAMPLE_RATE as usize;
    let mut audio = noise(40 * rate, 0.02, 0x5AFE_0001);
    mix(&mut audio, 0, &psk_station("cq cq de w1aw k ", 2150.0), 0.4);
    mix(&mut audio, rate, &olivia::encode("DE VE3XYZ RST 599 ", 2000.0, olivia::OL_16_500), 0.35);

    let out = protocol::decode_all(&audio, BAND.0, BAND.1, &protocol::default_modes());
    let psk_text = text_of(&out, Protocol::Psk);
    assert!(psk_text.contains("cq cq de w1aw k"), "the station was arbitrated away: {psk_text:?}");
    assert!(text_of(&out, Protocol::Olivia).contains("DE VE3XYZ"), "the Olivia station went");
}
