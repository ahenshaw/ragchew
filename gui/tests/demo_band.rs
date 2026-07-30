//! The demo band, decoded end to end.
//!
//! This is the closest thing to an integration test the project has: synthesize
//! twelve stations across three protocols, scan the band with every mode
//! enabled, and check that each station comes back on its own channel. It
//! exercises the whole chain — all three modems, the protocol dispatcher, mode
//! auto-detection and the channel tracker — against a signal nobody hand-tuned
//! it for.
//!
//! It takes about fifteen seconds in release mode, so run the suite with
//! `--release`.

use ragchew::protocol::{self, Protocol};
use ragchew_gui::channels::ChannelSet;
use ragchew_gui::demo;

/// Decode the demo band once and return the tracked channels.
fn channels() -> ChannelSet {
    let samples = demo::synth();
    let modes = protocol::default_modes();
    ChannelSet::from_decodes(protocol::decode_all(&samples, 300.0, 2600.0, &modes), 15.0)
}

/// The text of the channel nearest `hz`, of the given protocol.
fn text_near(set: &ChannelSet, hz: f64, p: Protocol) -> String {
    set.channels()
        .iter()
        .filter(|c| c.mode.protocol() == p)
        .min_by(|a, b| (a.hz - hz).abs().partial_cmp(&(b.hz - hz).abs()).unwrap())
        .map(|c| c.text.clone())
        .unwrap_or_default()
}

/// Every station in the demo band is found, on its own channel, in its own mode.
#[test]
fn every_station_is_decoded() {
    let set = channels();
    let n =
        demo::JS8_STATIONS.len() + demo::OLIVIA_STATIONS.len() + demo::PSK_STATIONS.len();
    assert_eq!(set.channels().len(), n, "expected {n} channels");

    for (hz, sm, msgs) in demo::JS8_STATIONS {
        let want: String = msgs.concat();
        let got = text_near(&set, *hz, Protocol::Js8);
        assert_eq!(got, want, "JS8 {} at {hz} Hz", sm.mode.name());
        let ch = set
            .channels()
            .iter()
            .find(|c| c.text == want)
            .expect("channel exists");
        assert_eq!(ch.mode.short_name(), sm.mode.name(), "wrong submode detected at {hz} Hz");
    }

    // The PSK31 station is checked for its calls rather than for exact
    // equality. It is the only mode here with no error correction and no frame
    // to synchronise on: the detector needs a station on the air for most of an
    // 8.2-second block before it will believe it, so the opening of a
    // transmission is missed — which is also what happens when you tune across
    // a real one. What has to come back is the call, cleanly and repeatedly.
    for (hz, _, call, repeats) in demo::PSK_STATIONS {
        let got = text_near(&set, *hz, Protocol::Psk);
        assert!(got.contains(call.trim_end()), "PSK31 at {hz} Hz decoded {got:?}");
        let copies = got.matches(call.trim_end()).count();
        assert!(
            copies >= repeats - 2,
            "PSK31 at {hz} Hz copied its call {copies} times of {repeats}: {got:?}"
        );
    }

    // Olivia stations in the clear are copied exactly; the one deliberately
    // sharing spectrum with a JS8 station is checked by the test below.
    for (hz, m, _, want) in demo::OLIVIA_STATIONS {
        let jammed = demo::JS8_STATIONS
            .iter()
            .any(|(j, sm, _)| (j - hz).abs() < (m.bandwidth as f64 + 8.0 * sm.spacing()) / 2.0);
        if !jammed {
            assert_eq!(text_near(&set, *hz, Protocol::Olivia), *want, "{} at {hz} Hz", m.name());
        }
    }
}

/// A JS8 station transmitting inside an Olivia signal's bandwidth: both are
/// copied, and they stay two separate conversations.
///
/// They interfere for real — the JS8 carrier sits in one of Olivia's sixteen
/// tone bins and costs it a couple of blocks — so the Olivia text is checked
/// for its distinctive parts rather than exact equality. The JS8 frame is
/// checked exactly: an Olivia signal spreads its power so thinly that only a
/// tone or two ever lands in JS8's 50 Hz, and the LDPC shrugs that off.
#[test]
fn co_channel_protocols_are_both_copied() {
    const HZ: f64 = 2000.0;
    let set = channels();

    let js8 = text_near(&set, HZ, Protocol::Js8);
    assert_eq!(js8, "DE W2QRM IN THE MUD TU 73 ", "JS8 lost under the Olivia signal");

    let olivia = text_near(&set, HZ, Protocol::Olivia);
    for part in ["GM", "IN EM73", "BTU"] {
        assert!(olivia.contains(part), "Olivia lost {part:?} under the JS8 signal: {olivia:?}");
    }

    // two stations, one patch of spectrum, two rows of text
    let here: Vec<&ragchew_gui::channels::Channel> =
        set.channels().iter().filter(|c| (c.hz - HZ).abs() < 60.0).collect();
    assert_eq!(here.len(), 2, "co-channel stations should not share a channel");
    assert_ne!(here[0].mode.protocol(), here[1].mode.protocol());
}

/// The weak-signal band decodes exactly as far down as it claims to.
///
/// Each station's text states its own mode and SNR, so these assertions are
/// what keeps the demo honest: if the decoder's reach changes, the labels
/// become a lie and this fails.
#[test]
fn weak_signal_demo_copies_what_it_claims() {
    use ragchew::olivia;
    use ragchew_gui::demo::WEAK_STATIONS;

    let samples = demo::synth_weak();
    let res = olivia::decode_all(&samples, 300.0, 2600.0, &olivia::COMMON);

    for st in WEAK_STATIONS {
        let got: String = res
            .iter()
            .filter(|r| r.mode == st.mode && (r.hz - st.hz).abs() < st.mode.spacing() * 2.0)
            .map(|r| r.text())
            .collect();
        if st.full_copy {
            assert_eq!(got, st.text, "{} at {} Hz should copy in full", st.mode.name(), st.hz);
        } else {
            // the one past its reach: recognisable, but visibly damaged
            assert!(!got.is_empty(), "{} vanished entirely", st.mode.name());
            assert_ne!(got, st.text, "{} copied in full — recalibrate the demo", st.mode.name());
        }
    }

    // nothing is claimed that no station transmitted
    for r in &res {
        let real = WEAK_STATIONS
            .iter()
            .any(|st| r.mode == st.mode && (r.hz - st.hz).abs() < st.mode.spacing() * 2.0);
        assert!(real, "invented a {} signal at {:.0} Hz", r.mode.name(), r.hz);
    }
}

/// Stations really are stacked on top of each other, and really are under the
/// noise — the two things the band exists to demonstrate.
#[test]
fn weak_band_overlaps_and_sits_below_the_noise() {
    use ragchew_gui::demo::WEAK_STATIONS;

    let quiet = WEAK_STATIONS.iter().filter(|st| st.snr_db < 0.0).count();
    assert_eq!(quiet, WEAK_STATIONS.len(), "every station should be below the noise floor");

    let overlaps = WEAK_STATIONS
        .iter()
        .enumerate()
        .flat_map(|(i, a)| WEAK_STATIONS[i + 1..].iter().map(move |b| (a, b)))
        .filter(|(a, b)| {
            let gap = (a.hz - b.hz).abs();
            gap < (a.mode.bandwidth + b.mode.bandwidth) as f64 / 2.0
        })
        .count();
    assert!(overlaps >= 2, "only {overlaps} overlapping pairs");

    // overlapping neighbours stay within a few dB, or the louder simply buries
    // the quieter and the demo shows physics rather than decoding
    for (i, a) in WEAK_STATIONS.iter().enumerate() {
        for b in &WEAK_STATIONS[i + 1..] {
            let gap = (a.hz - b.hz).abs();
            if gap < (a.mode.bandwidth + b.mode.bandwidth) as f64 / 2.0 {
                let d = (a.snr_db - b.snr_db).abs();
                assert!(d <= 6.0, "{} and {} differ by {d} dB", a.mode.name(), b.mode.name());
            }
        }
    }
}

/// A PSK over reaches the QSO log as one entry, not one per character.
///
/// The synthetic tests in `qso.rs` place their own decodes in time. This one
/// takes the times the decoder actually produces from audio — every character
/// separately, roughly five a second, from overlapping analysis blocks — and
/// feeds them to a QSO the way the app does, one absorb per decode. What must
/// come back is one line per over: the demo station calls CQ seven times
/// without pausing, so that is one line.
#[test]
fn a_psk_over_reaches_the_log_as_one_entry() {
    use ragchew::protocol::ModeId;
    use ragchew::psk::PSK31;
    use ragchew_gui::qso::QsoSet;

    let (hz, _, call, repeats) = demo::PSK_STATIONS[0];
    // Only the one station and only its mode: the whole-band scan is what makes
    // the tests above slow, and none of it is under test here.
    let decodes = protocol::decode_all(
        &demo::synth(),
        hz - 60.0,
        hz + 60.0,
        &[ModeId::Psk(PSK31)],
    );
    assert!(decodes.len() > 100, "the station barely decoded: {} characters", decodes.len());
    assert!(
        decodes.iter().all(|d| d.text.chars().count() == 1),
        "a PSK decode is one character; this test's premise has changed"
    );

    let mut set = ChannelSet::new(15.0);
    let mut qsos = QsoSet::new();
    for d in decodes {
        set.add(d);
        if qsos.qsos().is_empty() {
            qsos.open_for_channel(&set.channels()[0], 0.0);
        }
        qsos.absorb(&set);
    }

    let q = &qsos.qsos()[0];
    assert_eq!(
        q.log.len(),
        1,
        "one uninterrupted over became {} log entries",
        q.log.len()
    );
    let copies = q.log[0].text.matches(call.trim_end()).count();
    assert!(copies >= repeats - 2, "logged {copies} copies of the call: {:?}", q.log[0].text);
    assert_eq!(q.call, "KB9PSK", "the call was never read out of the over");
}

/// The reported SNR is the SNR the station was built with — as closely as a
/// band this crowded allows.
///
/// The weak-signal band declares each station's level in dB against the noise
/// in 2.5 kHz and scales it to exactly that, so it is its own ground truth.
/// This is where the whole chain meets it: synthesis, the band scan,
/// `Decode::snr_db`, and the per-mode measurement bandwidth behind it.
/// `examples/snr.rs` measures the estimator far more finely, on one station at
/// a time; what this adds is five of them inside each other's skirts.
///
/// Two errors are intrinsic to measuring a signal by integrating a band, and
/// this asserts them rather than papering over them:
///
/// - A station with a **narrower one inside its bandwidth** reads high, because
///   the guest's power is inside the host's band and no amount of arithmetic
///   can separate them. 32/1000 hosts 8/250 and 16/500 hosts 4/125.
/// - A station whose **guard region is mostly other stations** reads low,
///   because the noise floor is read off that region. Lowering the quantile
///   buys tolerance of it — 8/500, with four neighbours in its skirts, gained
///   0.55 dB from that — but not immunity.
#[test]
fn the_reported_snr_matches_the_band_it_was_built_with() {
    use ragchew::protocol::ModeId;
    use ragchew_gui::demo::WEAK_STATIONS;

    let samples = demo::synth_weak();
    let modes: Vec<ModeId> = WEAK_STATIONS.iter().map(|st| ModeId::Olivia(st.mode)).collect();
    let out = protocol::decode_all(&samples, 300.0, 2600.0, &modes);

    for st in WEAK_STATIONS {
        let mode = ModeId::Olivia(st.mode);
        let reported: Vec<f32> = out
            .iter()
            .filter(|d| d.mode == mode && (d.hz - st.hz).abs() < st.mode.spacing() * 2.0)
            .filter_map(|d| d.snr_db)
            .collect();
        assert!(!reported.is_empty(), "{} at {} Hz reported no SNR", st.mode.name(), st.hz);
        let got = reported.iter().sum::<f32>() / reported.len() as f32;
        let err = got - st.snr_db;
        let what = format!(
            "{} at {} Hz: built {:+.1} dB, reads {got:+.2} dB over {} decodes",
            st.mode.name(),
            st.hz,
            st.snr_db,
            reported.len()
        );

        // A station narrower than this one, transmitting inside its band.
        let guest = WEAK_STATIONS.iter().any(|o| {
            o.hz != st.hz
                && (o.hz - st.hz).abs() < mode.power_bandwidth_hz() / 2.0
                && (o.mode.bandwidth as f64) <= mode.power_bandwidth_hz()
        });

        if guest {
            assert!(
                (-0.5..3.0).contains(&err),
                "{what} — a host should read high by its guest's power, not {err:+.2} dB"
            );
        } else if st.full_copy {
            assert!(err.abs() < 1.0, "{what} — {err:+.2} dB off in the clear");
        } else {
            // The station deliberately past its reach. Its decodes land at
            // ragged offsets, which drags the average down; it is here to be
            // seen at all, not to be measured.
            assert!(err.abs() < 3.0, "{what} — even the marginal station is {err:+.2} dB out");
        }
    }
}

/// Opening a QSO on a JS8 station fills the report it would send.
///
/// The unit tests in `qso.rs` place the SNR by hand, so they cannot tell
/// whether a real decode carries one at all. This runs the actual scan over a
/// demo JS8 station and checks the field arrives filled, in the form JS8Call
/// writes it, agreeing with the measurement it came from.
#[test]
fn a_js8_qso_offers_the_report_it_measured() {
    use ragchew::protocol::ModeId;
    use ragchew_gui::qso::{suggested_report, QsoSet};

    let (hz, sm, _) = demo::JS8_STATIONS[1]; // 700 Hz, Normal
    let mode = ModeId::Js8(sm.mode);
    let decodes = protocol::decode_all(&demo::synth(), hz - 100.0, hz + 100.0, &[mode]);
    assert!(!decodes.is_empty(), "the station did not decode");

    let mut set = ChannelSet::new(15.0);
    let mut qsos = QsoSet::new();
    for d in decodes {
        set.add(d);
        if qsos.qsos().is_empty() {
            qsos.open_for_channel(&set.channels()[0], 0.0);
        }
        qsos.absorb(&set);
    }

    let q = &qsos.qsos()[0];
    let snr = q.snr_db.expect("a real JS8 decode carries a measurement");
    assert_eq!(
        q.rst_sent,
        suggested_report(mode, snr).expect("JS8 has a report"),
        "the field does not match the {snr:+.1} dB it was measured at"
    );
    assert!(q.rst_sent_auto, "nothing edited it, so it is still ours to keep current");
    // The demo band is built well above its noise, so a sane report and not,
    // say, a "-99" out of an unmeasured decode.
    assert!(
        q.rst_sent.starts_with('+') || q.rst_sent.starts_with('-'),
        "malformed report {:?}",
        q.rst_sent
    );
    assert!((-40.0..40.0).contains(&snr), "implausible {snr} dB off the demo band");
}
