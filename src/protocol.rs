//! One view of every protocol.
//!
//! A JS8 frame and an Olivia FEC block are very different objects — one is a
//! 15-second burst on a UTC grid carrying a packed 87-bit message, the other is
//! five characters out of a continuous stream — but a band monitor only cares
//! that each is *some text, at some frequency, at some time*. That is what
//! [`Decode`] is, and [`decode_all`] produces them for a whole band without the
//! caller knowing which protocol produced what.
//!
//! Adding a protocol means adding a [`ModeId`] variant and a branch in
//! [`decode_all`]; nothing above this module needs to change.

use crate::js8;
use crate::olivia;

/// A protocol family.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Protocol {
    /// JS8 (JS8Call): burst, cycle-aligned, LDPC-coded.
    Js8,
    /// Olivia MFSK: continuous, Walsh-Hadamard-coded.
    Olivia,
}

impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Js8 => "JS8",
            Protocol::Olivia => "Olivia",
        }
    }

    /// Every mode of this protocol worth scanning for, in scan order.
    pub fn modes(self) -> Vec<ModeId> {
        match self {
            Protocol::Js8 => js8::submode::ALL.iter().map(|s| ModeId::Js8(s.mode)).collect(),
            Protocol::Olivia => olivia::COMMON.iter().map(|&m| ModeId::Olivia(m)).collect(),
        }
    }

    /// All protocols this crate implements.
    pub const ALL: [Protocol; 2] = [Protocol::Js8, Protocol::Olivia];
}

/// A specific mode of a specific protocol — everything needed to name, colour
/// and scan for one kind of signal.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ModeId {
    Js8(js8::Mode),
    Olivia(olivia::Mode),
}

impl ModeId {
    pub fn protocol(self) -> Protocol {
        match self {
            ModeId::Js8(_) => Protocol::Js8,
            ModeId::Olivia(_) => Protocol::Olivia,
        }
    }

    /// Full name, e.g. `"JS8 Normal"` or `"Olivia 32/1000"`.
    pub fn name(self) -> String {
        match self {
            ModeId::Js8(m) => format!("JS8 {}", m.name()),
            ModeId::Olivia(m) => m.name(),
        }
    }

    /// Short name for tight UI, e.g. `"Normal"` or `"32/1000"`.
    pub fn short_name(self) -> String {
        match self {
            ModeId::Js8(m) => m.name().to_string(),
            ModeId::Olivia(m) => m.short_name(),
        }
    }

    /// Occupied bandwidth in Hz.
    pub fn bandwidth_hz(self) -> f64 {
        match self {
            // 8 tones, so 7 gaps plus a tone's worth of skirt.
            ModeId::Js8(m) => 8.0 * js8::submode::of(m).spacing(),
            ModeId::Olivia(m) => m.bandwidth as f64,
        }
    }

    /// How far apart two decodes may be in frequency and still be the same
    /// station. Wide modes drift within their own bandwidth; narrow ones need a
    /// floor so a per-frame frequency estimate landing in the next bin does not
    /// split a conversation in two.
    pub fn assoc_tol_hz(self) -> f64 {
        match self {
            ModeId::Js8(m) => js8::submode::of(m).spacing().max(15.0),
            ModeId::Olivia(m) => (m.bandwidth as f64 / 4.0).max(m.spacing()),
        }
    }

    /// Transmission period in seconds for modes that live on a UTC grid; `None`
    /// for continuous modes, which start whenever the operator does.
    pub fn period_s(self) -> Option<u32> {
        match self {
            ModeId::Js8(m) => Some(js8::submode::of(m).period_s),
            ModeId::Olivia(_) => None,
        }
    }

    /// How long one unit of decoded text takes on the air (a JS8 frame, an
    /// Olivia block).
    pub fn chunk_secs(self) -> f64 {
        match self {
            ModeId::Js8(m) => js8::submode::of(m).frame_secs(),
            ModeId::Olivia(m) => m.block_secs(),
        }
    }
}

/// Some text, at some frequency, at some time — whatever protocol carried it.
#[derive(Clone, Debug)]
pub struct Decode {
    pub mode: ModeId,
    /// Centre frequency in Hz.
    pub hz: f64,
    /// Start time in seconds from the beginning of the decoded samples.
    pub time_s: f64,
    /// Decoder confidence, in multiples of that protocol's noise floor: 1.0 is
    /// "no better than noise" and higher is better.
    ///
    /// The two protocols measure entirely different things — JS8 the strength
    /// of its Costas sync, Olivia the peak of a Walsh correlation — so this is
    /// normalised to make the numbers mean the same *kind* of thing, not to
    /// make them precisely comparable.
    pub quality: f32,
    /// The decoded text.
    pub text: String,
}

/// The modes a band scan tries by default: all four JS8 submodes and the common
/// Olivia modes.
///
/// Scanning costs time per mode, so callers watching a known calling frequency
/// should pass a shorter list.
pub fn default_modes() -> Vec<ModeId> {
    let mut v = Protocol::Js8.modes();
    v.extend(Protocol::Olivia.modes());
    v
}

/// Decode everything in `samples` between `hz_lo` and `hz_hi`, for the given
/// modes, in time order.
pub fn decode_all(samples: &[f32], hz_lo: f64, hz_hi: f64, modes: &[ModeId]) -> Vec<Decode> {
    let mut out = Vec::new();

    for m in modes {
        if let ModeId::Js8(mode) = m {
            let sm = js8::submode::of(*mode);
            out.extend(js8::modem::decode_all_sm(samples, hz_lo, hz_hi, 30, &sm).into_iter().map(
                |r| Decode {
                    mode: ModeId::Js8(r.mode),
                    hz: r.hz,
                    time_s: r.offset as f64 / crate::SAMPLE_RATE as f64,
                    quality: (r.sync / js8::modem::NOISE_SYNC) as f32,
                    text: js8::message::unpack(&r.a87).text(),
                },
            ));
        }
    }

    // Olivia modes are scanned together so one signal cannot be reported twice
    // under two neighbouring modes.
    let olivia_modes: Vec<olivia::Mode> = modes
        .iter()
        .filter_map(|m| match m {
            ModeId::Olivia(o) => Some(*o),
            _ => None,
        })
        .collect();
    if !olivia_modes.is_empty() {
        out.extend(olivia::decode_all(samples, hz_lo, hz_hi, &olivia_modes).into_iter().map(|r| {
            Decode {
                mode: ModeId::Olivia(r.mode),
                hz: r.hz,
                time_s: r.offset as f64 / crate::SAMPLE_RATE as f64,
                quality: (r.snr / olivia::modem::NOISE_FLOOR) as f32,
                text: r.text(),
            }
        }));
    }

    out.sort_by(|a, b| a.time_s.partial_cmp(&b.time_s).unwrap());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_modes_cover_both_protocols() {
        let modes = default_modes();
        for p in Protocol::ALL {
            assert!(modes.iter().any(|m| m.protocol() == p), "{} missing", p.name());
        }
    }

    #[test]
    fn mode_names_are_distinct() {
        let names: Vec<String> = default_modes().iter().map(|m| m.name()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate mode names in {names:?}");
    }

    /// A JS8 transmission and an Olivia transmission sharing a band both come
    /// back, each tagged with its own protocol.
    #[test]
    fn decodes_a_mixed_band() {
        use crate::js8::message::{self, IType};

        let rate = crate::SAMPLE_RATE as usize;
        let mut audio = vec![0.0f32; 20 * rate];

        let (a87, _) = message::freetext("HELLO WORLD", IType::None);
        let burst = js8::modem::encode_audio(&a87, 800.0);
        for (i, s) in burst.iter().enumerate() {
            audio[rate / 2 + i] += s * 0.5;
        }

        let ol = olivia::encode("DE W1AW ", 2000.0, olivia::OL_16_500);
        for (i, s) in ol.iter().enumerate() {
            audio[rate + i] += s * 0.5;
        }

        let modes = [ModeId::Js8(js8::Mode::Normal), ModeId::Olivia(olivia::OL_16_500)];
        let out = decode_all(&audio, 300.0, 2600.0, &modes);

        let js8_text: String =
            out.iter().filter(|d| d.mode.protocol() == Protocol::Js8).map(|d| d.text.clone()).collect();
        let ol_text: String = out
            .iter()
            .filter(|d| d.mode.protocol() == Protocol::Olivia)
            .map(|d| d.text.clone())
            .collect();
        assert!(js8_text.contains("HELLO WORLD"), "JS8 text was {js8_text:?}");
        assert!(ol_text.contains("DE W1AW"), "Olivia text was {ol_text:?}");
    }
}
