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
use ragchew::SAMPLE_RATE;
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

/// The weak band's PSK31 station copies in full at the level it claims, and
/// claims a level well above every Olivia station around it.
///
/// That gap is the point of having it there. Olivia spreads each character over
/// 64 chips and reads perfectly nine to twelve decibels below the noise; PSK31
/// has no error correction at all and needs four. If a change ever closes that
/// gap, either the demo has stopped being honest or something has gone very
/// right, and both are worth stopping for.
#[test]
fn the_weak_psk_station_copies_at_the_level_it_claims() {
    use ragchew::psk;
    use ragchew_gui::demo::{WEAK_PSK_STATIONS, WEAK_STATIONS};

    let samples = demo::synth_weak();
    for st in WEAK_PSK_STATIONS {
        let want = demo::psk_text(st.call, st.repeats);
        let res = psk::decode_all(&samples, 300.0, 2600.0, &[st.mode]);
        let got: String = res
            .iter()
            .filter(|r| (r.hz - st.hz).abs() < 20.0)
            .map(|r| r.text.clone())
            .collect();
        // Nothing after the station stops, checked before the equality below
        // because it is the same failure said usefully. This band is well
        // shaped to catch it: the over ends around 47 seconds and the band runs
        // for sixty, so a receiver that goes on reading has thirteen seconds of
        // bare noise to read — and noise demodulated as differential BPSK
        // frames and decodes and prints as words. The equality would fail on
        // that too, but only by saying two long strings differ.
        let sig = psk::encode(&want, st.hz, st.mode);
        let ends_at = st.start_s + sig.len() as f64 / SAMPLE_RATE as f64;
        let after: String = res
            .iter()
            .filter(|r| (r.hz - st.hz).abs() < 20.0)
            // A symbol's slack and no more. The garbage this catches begins
            // within a second of the last real character — it is the tail of
            // the block that straddles the end of the over — so a generous
            // margin here is an assertion that cannot fail.
            .filter(|r| r.offset as f64 / SAMPLE_RATE as f64 > ends_at + 0.1)
            .map(|r| r.text.clone())
            .collect();
        assert!(
            after.is_empty(),
            "kept printing for {:.0} s after the station stopped at {ends_at:.1} s: {after:?}",
            60.0 - ends_at,
        );

        if st.full_copy {
            assert_eq!(got, want, "PSK31 at {} Hz, {} dB should copy in full", st.hz, st.snr_db);
        } else {
            assert!(!got.is_empty(), "the PSK31 station vanished entirely");
            assert_ne!(got, want, "it copied in full — recalibrate the demo");
        }

        // Its own text states its level, as every station's here does.
        assert!(
            st.call.contains(&format!("{:.0} db", st.snr_db)),
            "{:?} does not say it is at {} dB",
            st.call,
            st.snr_db
        );

        // Below the noise floor, like everything else in this band...
        assert!(st.snr_db < 0.0, "the weak band's PSK station is above the noise");
        // ...but not nearly as far below, which is the demonstration.
        let deepest_olivia = WEAK_STATIONS.iter().map(|o| o.snr_db).fold(f32::MAX, f32::min);
        assert!(
            st.snr_db > deepest_olivia + 4.0,
            "PSK31 at {} dB is within 4 dB of Olivia's {deepest_olivia} — the band no longer \
             shows what coding gain buys",
            st.snr_db
        );

        // And nothing invented elsewhere in the band by the PSK scan.
        let phantoms: Vec<f64> =
            res.iter().filter(|r| (r.hz - st.hz).abs() >= 20.0).map(|r| r.hz).collect();
        assert!(phantoms.is_empty(), "invented PSK stations at {phantoms:?}");

    }
}

/// The crowded band: five Olivia modes on top of each other, every one read in
/// full, and nothing invented.
///
/// The weak band asks how far under the noise a signal can be. This asks how
/// many can be in the same place, which is the more ordinary situation on a
/// real band and the other half of what the coding buys. Two things have to
/// hold at once and they pull against each other: every station copies
/// *exactly*, and the scan does not report a station nobody transmitted.
#[test]
fn the_crowded_band_reads_every_station() {
    use ragchew::olivia;
    use ragchew_gui::demo::CROWDED_STATIONS;

    let samples = demo::synth_crowded();
    let res = olivia::decode_all(&samples, 300.0, 2600.0, &olivia::COMMON);

    for st in CROWDED_STATIONS {
        let got: String = res
            .iter()
            .filter(|r| r.mode == st.mode && (r.hz - st.hz).abs() < st.mode.spacing() * 2.0)
            .map(|r| r.text())
            .collect();
        assert!(st.full_copy, "the crowded band is the one where everything copies");
        assert_eq!(got, st.text, "{} at {} Hz", st.mode.name(), st.hz);
    }

    // And now the part a reader actually sees. The decoder being right is not
    // the same claim as the screen being right: threads are grouped by protocol
    // and frequency, *not* by mode, so two Olivia stations closer than the
    // wider one's association tolerance land on one row with their texts woven
    // together — which is precisely what this band did when the 8/500 sat dead
    // centre of the 16/1000, and precisely what the assertions above could not
    // see. Five stations, five threads, each carrying one station's text whole.
    let set = ChannelSet::from_decodes(
        protocol::decode_all(&samples, 300.0, 2600.0, &protocol::default_modes()),
        15.0,
    );
    let rows: Vec<String> = set.channels().iter().map(|c| format!("{:.0} Hz {:?}", c.hz, c.text)).collect();
    assert_eq!(
        set.channels().len(),
        CROWDED_STATIONS.len(),
        "{} stations came back as {} threads:\n  {}",
        CROWDED_STATIONS.len(),
        set.channels().len(),
        rows.join("\n  ")
    );
    for st in CROWDED_STATIONS {
        let ch = set
            .channels()
            .iter()
            .find(|c| c.mode == protocol::ModeId::Olivia(st.mode))
            .unwrap_or_else(|| panic!("no thread for {}", st.mode.name()));
        assert_eq!(ch.text, st.text, "{} at {} Hz read as a thread", st.mode.name(), st.hz);
    }

    // Nothing invented. This is the assertion that keeps the band honest about
    // what it is showing: a scan of five stacked signals has every opportunity
    // to report a sixth that is really two of the others, and a band built to
    // demonstrate successful decoding would be worth nothing if it did.
    for r in &res {
        let real = CROWDED_STATIONS
            .iter()
            .any(|st| r.mode == st.mode && (r.hz - st.hz).abs() < st.mode.spacing() * 2.0);
        assert!(real, "invented a {} at {:.0} Hz: {:?}", r.mode.name(), r.hz, r.text());
    }
}

/// The crowded band really is crowded, and really is above the noise — the two
/// things that make it the weak band's opposite rather than its twin.
#[test]
fn the_crowded_band_is_stacked_and_audible() {
    use ragchew_gui::demo::{CROWDED_STATIONS, WEAK_STATIONS};

    // Above the noise, unlike every station in the weak band. The difficulty
    // here is the neighbours, not the level, and a reader can see these on the
    // waterfall — which is the visible contrast between the two bands.
    for st in CROWDED_STATIONS {
        assert!(st.snr_db > 0.0, "{} is below the noise floor", st.mode.name());
    }
    assert!(WEAK_STATIONS.iter().all(|st| st.snr_db < 0.0), "the weak band stopped being weak");

    // Every station overlaps at least one other, and there are at least three
    // overlapping pairs — one narrow station buried in a wide one, and two more
    // piled across the seam where the two wide ones meet.
    let overlaps = |a: &ragchew_gui::demo::OliviaStation, b: &ragchew_gui::demo::OliviaStation| {
        (a.hz - b.hz).abs() < (a.mode.bandwidth + b.mode.bandwidth) as f64 / 2.0
    };
    let pairs = CROWDED_STATIONS
        .iter()
        .enumerate()
        .flat_map(|(i, a)| CROWDED_STATIONS[i + 1..].iter().map(move |b| (a, b)))
        .filter(|(a, b)| overlaps(a, b))
        .count();
    assert!(pairs >= 3, "only {pairs} overlapping pairs — the band is not crowded");

    // There is deliberately no static rule here about how far apart two
    // stations must be. Threads are grouped by protocol and frequency and not
    // by mode, so two Olivia stations can share one row — but `ChannelSet::add`
    // assigns a decode to the *nearest* matching channel, so a pair inside each
    // other's association tolerance is still fine as long as each has a nearer
    // home of its own. The 32/1000 at 900 and the 16/500 at 700 are 200 Hz
    // apart against a 250 Hz tolerance and never merge, for exactly that
    // reason. A distance rule strict enough to be sound would reject this
    // band; what actually settles it is decoding the thing and counting the
    // threads, which `the_crowded_band_reads_every_station` does.

    // All at one level. A station well above its neighbour buries it, and the
    // band would be showing physics rather than decoding.
    let (lo, hi) = CROWDED_STATIONS
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), st| (lo.min(st.snr_db), hi.max(st.snr_db)));
    assert!(hi - lo <= 3.0, "levels span {:.1} dB — the loud ones are burying the quiet", hi - lo);

    // Five distinct modes, which is the other half of the point.
    let mut modes: Vec<String> = CROWDED_STATIONS.iter().map(|st| st.mode.name()).collect();
    modes.sort();
    modes.dedup();
    assert_eq!(modes.len(), CROWDED_STATIONS.len(), "two stations share a mode: {modes:?}");
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

/// The "know-your-signals" band: three QSOs, three threads, every over read.
///
/// This is the band's whole claim — that a reader watching it sees three
/// conversations and not a heap of decodes — so it is asserted through
/// `ChannelSet` rather than off the decoder. The Olivia thread is the one that
/// could fail quietly: it changes mode twice mid-QSO, and it stays one thread
/// only because channels are grouped by protocol and frequency and not by mode.
#[test]
fn the_signals_band_holds_three_conversations() {
    use ragchew_gui::demo::{
        SIGNALS_JS8_HZ, SIGNALS_JS8_OVERS, SIGNALS_OLIVIA_HZ, SIGNALS_OLIVIA_OVERS,
        SIGNALS_PSK_HZ, SIGNALS_PSK_OVERS,
    };

    let samples = demo::synth_signals();
    let set = ChannelSet::from_decodes(
        protocol::decode_all(&samples, 300.0, 2600.0, &protocol::default_modes()),
        15.0,
    );
    let rows: Vec<String> =
        set.channels().iter().map(|c| format!("{:.0} Hz {:?}", c.hz, c.text)).collect();
    assert_eq!(set.channels().len(), 3, "three QSOs came back as:\n  {}", rows.join("\n  "));

    // Olivia: three modes, one thread, every character of the exchange in the
    // order it was spoken.
    let want: String = SIGNALS_OLIVIA_OVERS.iter().map(|(_, _, t)| *t).collect();
    assert_eq!(
        text_near(&set, SIGNALS_OLIVIA_HZ, Protocol::Olivia),
        want,
        "the Olivia thread did not read as one conversation"
    );

    // JS8: the frames of each over, in order, with the mark that ends it.
    let want: String =
        SIGNALS_JS8_OVERS.iter().map(|(_, t)| format!("{t}<>")).collect::<Vec<_>>().concat();
    assert_eq!(text_near(&set, SIGNALS_JS8_HZ, Protocol::Js8), want, "the JS8 thread");

    // PSK31 loses the opening of every over at any level whatever — the gate
    // wants a station on the air for most of an 8.2-second block before it will
    // believe in one — so what is asserted is that the informative half of each
    // over arrives. The overs open with the callsigns twice over for exactly
    // this reason; see `SIGNALS_PSK_OVERS`.
    let got = text_near(&set, SIGNALS_PSK_HZ, Protocol::Psk);
    for (_, over) in SIGNALS_PSK_OVERS {
        let tail: String = over.chars().skip(over.chars().count() / 2).collect();
        assert!(got.contains(tail.trim()), "PSK31 lost {tail:?} of its over: {got:?}");
    }
}

/// The call the QSO panel shows is the whole of it, however the frames fell.
///
/// The answering station is K2LOW, and "W1JSK DE K2LOW 2W ONLY" is longer than
/// a Normal frame carries — so it goes on the air as "W1JSK DE K2L" and
/// "OW 2W ONLY", and the first of those names a station that does not exist.
/// It is decoded audio that exposes this and nothing else does: every unit test
/// that fed the tracker whole sentences was green while the tab said K2L.
///
/// Both ways of arriving at the thread are checked, because they read different
/// text. Open it from the first decode and the calling station names it, which
/// is the ordinary case. Click it while the other operator is part way through
/// answering — the case an operator meets every time they tune across a QSO in
/// progress — and the name comes out of the over that is landing, half a call
/// at first and whole a cycle later.
#[test]
fn a_call_broken_over_two_js8_frames_reaches_the_panel_whole() {
    use ragchew::protocol::ModeId;
    use ragchew_gui::demo::SIGNALS_JS8_HZ;
    use ragchew_gui::qso::QsoSet;

    // The submode the band sends this thread in; see `demo::synth_signals`.
    let mode = ModeId::Js8(ragchew::js8::submode::NORMAL.mode);
    let decodes = protocol::decode_all(
        &demo::synth_signals(),
        SIGNALS_JS8_HZ - 100.0,
        SIGNALS_JS8_HZ + 100.0,
        &[mode],
    );
    assert!(!decodes.is_empty(), "the JS8 thread did not decode");

    // Each decode as it lands, as the app does it. `open_at` is the decode the
    // operator clicks on: the first one, or the one that starts the answer.
    let run = |open_at: fn(&str) -> bool| -> String {
        let mut set = ChannelSet::new(15.0);
        let mut qsos = QsoSet::new();
        for d in decodes.iter().cloned() {
            let click = open_at(&d.text);
            set.add(d);
            if qsos.qsos().is_empty() && click {
                qsos.open_for_channel(&set.channels()[0], 0.0);
            }
            if !qsos.qsos().is_empty() {
                qsos.absorb(&set);
            }
        }
        qsos.qsos()[0].label()
    };

    assert_eq!(run(|_| true), "W1JSK", "the station calling CQ did not name the thread");
    assert_eq!(
        run(|t| t.contains("DE K2L")),
        "K2LOW",
        "a station clicked mid-over kept the half of its call that had arrived"
    );
}

/// Every over in the band arrives as its own over, on all three protocols.
///
/// A thread is a frequency, not a speaker: both operators in a QSO transmit on
/// the same one, so what separates their overs is `Channel::breaks` — which is
/// where the text panel draws a gap and where the QSO log starts a new entry.
/// The three protocols reach it by different routes, and only one of them is
/// free. JS8 sets a flag on the last frame of an over and the tracker reads it.
/// Olivia and PSK31 carry no such thing and no callsign the tracker can parse
/// either, so the *only* cue they leave is the turnaround, which has to be
/// longer than `channels::over_gap_s` — 8.2 s for Olivia, 5 for PSK31.
///
/// This band left 1.5 s between overs to begin with, and the two conversational
/// threads arrived as one unbroken paragraph apiece with both operators run
/// together. See `demo::signals_gap_s`. The Olivia thread is the one to watch:
/// its three mode changes are mid-QSO, and each has to land on a line of its
/// own rather than in the middle of the previous operator's sentence.
#[test]
fn every_over_in_the_signals_band_is_its_own_over() {
    use ragchew_gui::demo::{
        SIGNALS_JS8_HZ, SIGNALS_JS8_OVERS, SIGNALS_OLIVIA_HZ, SIGNALS_OLIVIA_OVERS,
        SIGNALS_PSK_HZ, SIGNALS_PSK_OVERS,
    };

    let samples = demo::synth_signals();
    let set = ChannelSet::from_decodes(
        protocol::decode_all(&samples, 300.0, 2600.0, &protocol::default_modes()),
        15.0,
    );

    let cases = [
        ("JS8", SIGNALS_JS8_HZ, Protocol::Js8, SIGNALS_JS8_OVERS.len()),
        ("PSK31", SIGNALS_PSK_HZ, Protocol::Psk, SIGNALS_PSK_OVERS.len()),
        ("Olivia", SIGNALS_OLIVIA_HZ, Protocol::Olivia, SIGNALS_OLIVIA_OVERS.len()),
    ];
    for (what, hz, proto, overs) in cases {
        let ch = set
            .channels()
            .iter()
            .filter(|c| c.mode.protocol() == proto)
            .min_by(|a, b| (a.hz - hz).abs().partial_cmp(&(b.hz - hz).abs()).unwrap())
            .unwrap_or_else(|| panic!("no {what} thread"));
        // One break between each pair of overs, and none before the first.
        let segments: Vec<String> = {
            let chars: Vec<char> = ch.text.chars().collect();
            let mut bounds: Vec<usize> = ch.breaks.iter().map(|&(i, _)| i).collect();
            bounds.push(chars.len());
            let mut from = 0;
            bounds
                .into_iter()
                .map(|to| {
                    let s: String = chars[from..to].iter().collect();
                    from = to;
                    s
                })
                .collect()
        };
        assert_eq!(
            segments.len(),
            overs,
            "the {what} thread's {overs} overs read as {} :\n  {}",
            segments.len(),
            segments.join("\n  ")
        );
    }

    // And the Olivia thread through the log, which is the thing a reader keeps.
    // The channel's breaks are where a new entry starts, so six overs — three
    // of them in a mode the one before was not — have to be six lines.
    use ragchew_gui::qso::QsoSet;
    // Distinct modes: the thread QSYs twice and comes back to nothing, so the
    // overs name three modes between six of them. Handing `decode_all` the list
    // with its repeats scans each mode twice and lands two copies of every
    // block on one channel, which reads exactly as badly as it sounds —
    // "CQ CQ DE DE W1SW1SLO LO 8/28/250".
    let mut modes: Vec<protocol::ModeId> =
        SIGNALS_OLIVIA_OVERS.iter().map(|(_, m, _)| protocol::ModeId::Olivia(*m)).collect();
    modes.dedup();
    let decodes = protocol::decode_all(
        &samples,
        SIGNALS_OLIVIA_HZ - 600.0,
        SIGNALS_OLIVIA_HZ + 600.0,
        &modes,
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
    let lines: Vec<&str> = q.log.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(
        q.log.len(),
        SIGNALS_OLIVIA_OVERS.len(),
        "the Olivia QSO logged {} lines for {} overs:\n  {}",
        q.log.len(),
        SIGNALS_OLIVIA_OVERS.len(),
        lines.join("\n  ")
    );
    for ((_, _, sent), got) in SIGNALS_OLIVIA_OVERS.iter().zip(&lines) {
        assert_eq!(got.trim(), sent.trim(), "logged the wrong over");
    }
}

/// Every number the band prints is one the code can still stand behind.
///
/// The point of the band is the two figures in each over — words per minute and
/// the level the mode copies at — and a demo that states them is a demo that
/// can be caught stating them wrongly. `SIGNALS_MODES` is where they are
/// measured to; this is what keeps the texts agreeing with it.
#[test]
fn the_signals_band_quotes_the_numbers_it_was_measured_at() {
    use ragchew::protocol::ModeId;
    use ragchew_gui::demo::{
        signals_floor_db, Op, SIGNALS_JS8_OPS, SIGNALS_JS8_OVERS, SIGNALS_MARGIN_DB,
        SIGNALS_MODES, SIGNALS_OLIVIA_OPS, SIGNALS_OLIVIA_OVERS, SIGNALS_PSK_OPS,
        SIGNALS_PSK_OVERS,
    };
    use ragchew::psk::PSK31;

    // Each thread's text, and the modes it claims to be speaking in.
    let js8: String = SIGNALS_JS8_OVERS.iter().map(|(_, t)| *t).collect();
    let psk: String = SIGNALS_PSK_OVERS.iter().map(|(_, t)| *t).collect();
    let olivia: String = SIGNALS_OLIVIA_OVERS.iter().map(|(_, _, t)| *t).collect();
    // Upper-cased for the comparison only: PSK31 is typed in lower case, and
    // what is under test is the numbers rather than the shift key.
    let threads = [
        (ModeId::Psk(PSK31), psk.to_uppercase()),
        (ModeId::Js8(ragchew::js8::Mode::Normal), js8.clone()),
    ];

    for (mode, text) in &threads {
        let (_, wpm, floor) = SIGNALS_MODES.iter().find(|(m, _, _)| m == mode).unwrap();
        assert!(text.contains(&format!("{wpm:.0} WPM")), "{} never says its speed", mode.name());
        assert!(
            text.contains(&format!("{floor:.0} DB")),
            "{} never says it copies at {floor} dB",
            mode.name()
        );
    }
    // The Olivia thread says all three, one per mode it QSYs to.
    for (op, mode, _) in SIGNALS_OLIVIA_OVERS {
        if *op != 0 {
            continue; // the loud station announces each QSY; its partner agrees
        }
        let id = ModeId::Olivia(*mode);
        let (_, wpm, floor) = SIGNALS_MODES.iter().find(|(m, _, _)| *m == id).unwrap();
        assert!(
            olivia.contains(&format!("{} {wpm:.0} WPM {floor:.0} DB", mode.short_name())),
            "{} never states its speed and reach: {olivia:?}",
            id.name()
        );
    }

    // And the powers. Every loud station runs the same hundred watts, and every
    // quiet one is the same margin above the floor of the hardest mode its
    // thread uses — which is what makes the three threads comparable at all. If
    // this drifts, the band stops being about the modes and starts being about
    // whichever station happens to be loudest.
    let worst =
        |modes: &[ModeId]| modes.iter().map(|m| signals_floor_db(*m)).fold(f32::MIN, f32::max);
    // The Olivia modes come from the overs rather than a list written out here:
    // adding a fourth gear to that QSO must move its operator's power, and a
    // list of its own would let the two drift apart quietly.
    let olivia: Vec<ModeId> =
        SIGNALS_OLIVIA_OVERS.iter().map(|(_, m, _)| ModeId::Olivia(*m)).collect();
    let cases: [(&[ModeId], &[Op; 2]); 3] = [
        (&[ModeId::Psk(PSK31)], &SIGNALS_PSK_OPS),
        (&[ModeId::Js8(ragchew::js8::Mode::Normal)], &SIGNALS_JS8_OPS),
        (&olivia, &SIGNALS_OLIVIA_OPS),
    ];
    for (modes, ops) in cases {
        assert_eq!(ops[0].watts, 100.0, "{} is not the band's reference station", ops[0].call);
        let margin = ops[1].snr_db() - worst(modes);
        assert!(
            (margin - SIGNALS_MARGIN_DB).abs() < 0.5,
            "{} at {} W arrives {:.1} dB above its floor, not {SIGNALS_MARGIN_DB}",
            ops[1].call,
            ops[1].watts,
            margin
        );
    }
}

/// The three threads keep out of each other's way.
///
/// Deliberately unlike the other bands: interference is what those are for, and
/// a collision here would teach a second lesson over the top of the first. The
/// Olivia thread is the one to watch — it grows to a kilohertz wide when it
/// QSYs to 32/1000, and it is the last mode change that would land on a
/// neighbour if one were too close.
#[test]
fn the_signals_band_threads_stay_clear_of_each_other() {
    use ragchew::protocol::ModeId;
    use ragchew::psk::PSK31;
    use ragchew_gui::demo::{
        SIGNALS_JS8_HZ, SIGNALS_OLIVIA_HZ, SIGNALS_OLIVIA_OVERS, SIGNALS_PSK_HZ,
    };

    let mut spans = vec![
        (SIGNALS_PSK_HZ, ModeId::Psk(PSK31).bandwidth_hz(), "PSK31"),
        (SIGNALS_JS8_HZ, ModeId::Js8(ragchew::js8::Mode::Normal).bandwidth_hz(), "JS8 Normal"),
    ];
    for (_, mode, _) in SIGNALS_OLIVIA_OVERS {
        spans.push((SIGNALS_OLIVIA_HZ, mode.bandwidth as f64, "Olivia"));
    }
    for (i, a) in spans.iter().enumerate() {
        for b in &spans[i + 1..] {
            if a.2 == b.2 {
                continue; // the Olivia modes share a centre frequency on purpose
            }
            let gap = (a.0 - b.0).abs() - (a.1 + b.1) / 2.0;
            assert!(gap > 0.0, "{} and {} overlap by {:.0} Hz", a.2, b.2, -gap);
        }
    }
    // And inside the band the app scans.
    for (hz, bw, what) in &spans {
        assert!(hz - bw / 2.0 >= 300.0 && hz + bw / 2.0 <= 2600.0, "{what} runs off the band");
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
