//! The demo band, decoded end to end.
//!
//! This is the closest thing to an integration test the project has: synthesize
//! eleven stations across two protocols, scan the band with every mode enabled,
//! and check that each station comes back on its own channel. It exercises the
//! whole chain — both modems, the protocol dispatcher, mode auto-detection and
//! the channel tracker — against a signal nobody hand-tuned it for.
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
    let n = demo::JS8_STATIONS.len() + demo::OLIVIA_STATIONS.len();
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
