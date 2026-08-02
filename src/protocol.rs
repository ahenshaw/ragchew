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

use crate::dsp;
use crate::js8;
use crate::olivia;
use crate::psk;

/// A protocol family.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Protocol {
    /// JS8 (JS8Call): burst, cycle-aligned, LDPC-coded.
    Js8,
    /// Olivia MFSK: continuous, Walsh-Hadamard-coded.
    Olivia,
    /// PSK31: continuous, differential BPSK, uncoded.
    Psk,
}

impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Js8 => "JS8",
            Protocol::Olivia => "Olivia",
            Protocol::Psk => "PSK",
        }
    }

    /// Every mode of this protocol worth scanning for, in scan order.
    pub fn modes(self) -> Vec<ModeId> {
        match self {
            Protocol::Js8 => js8::submode::ALL.iter().map(|s| ModeId::Js8(s.mode)).collect(),
            Protocol::Olivia => olivia::COMMON.iter().map(|&m| ModeId::Olivia(m)).collect(),
            Protocol::Psk => psk::COMMON.iter().map(|&m| ModeId::Psk(m)).collect(),
        }
    }

    /// All protocols this crate implements.
    pub const ALL: [Protocol; 3] = [Protocol::Js8, Protocol::Olivia, Protocol::Psk];
}

/// A specific mode of a specific protocol — everything needed to name, colour
/// and scan for one kind of signal.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ModeId {
    Js8(js8::Mode),
    Olivia(olivia::Mode),
    Psk(psk::Mode),
}

impl ModeId {
    pub fn protocol(self) -> Protocol {
        match self {
            ModeId::Js8(_) => Protocol::Js8,
            ModeId::Olivia(_) => Protocol::Olivia,
            ModeId::Psk(_) => Protocol::Psk,
        }
    }

    /// Full name, e.g. `"JS8 Normal"` or `"Olivia 32/1000"`.
    pub fn name(self) -> String {
        match self {
            ModeId::Js8(m) => format!("JS8 {}", m.name()),
            ModeId::Olivia(m) => m.name(),
            ModeId::Psk(m) => m.name(),
        }
    }

    /// Short name for tight UI, e.g. `"Normal"` or `"32/1000"`.
    pub fn short_name(self) -> String {
        match self {
            ModeId::Js8(m) => m.name().to_string(),
            ModeId::Olivia(m) => m.short_name(),
            ModeId::Psk(m) => m.short_name(),
        }
    }

    /// Occupied bandwidth in Hz.
    pub fn bandwidth_hz(self) -> f64 {
        match self {
            // 8 tones, so 7 gaps plus a tone's worth of skirt.
            ModeId::Js8(m) => 8.0 * js8::submode::of(m).spacing(),
            ModeId::Olivia(m) => m.bandwidth as f64,
            ModeId::Psk(m) => m.bandwidth_hz(),
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
            // A PSK31 carrier is estimated to a couple of Hz and stations pack
            // in tighter than any other mode here, so the floor is half a
            // symbol rate rather than the usual 15 Hz.
            ModeId::Psk(m) => m.baud() / 2.0,
        }
    }

    /// How far apart in time two decodes at the same frequency may be and still
    /// be one transmission reported twice.
    ///
    /// The time analogue of [`assoc_tol_hz`](Self::assoc_tol_hz), and it exists
    /// because a live scan re-reads overlapping windows: the same block of
    /// audio is decoded several times over and the repeats have to be
    /// recognised. Too tight and text is doubled; too loose and a decode's
    /// *neighbour* is mistaken for the decode itself and thrown away.
    ///
    /// Half a chunk suits a mode whose chunks all take the same time. It does
    /// not suit PSK, whose "chunk" is a character of *variable* length: the
    /// figure is an average, while the shortest character — a space, one bit —
    /// completes three symbols after the one before it, which is less than half
    /// an average character. Half a chunk therefore swallows every short
    /// character in a re-read stretch, which is what it was doing: live PSK31
    /// lost a space wherever two scans overlapped.
    ///
    /// So PSK gets one symbol, comfortably inside the three that separate the
    /// closest pair of real characters. `psk_dedup_survives_adjacent_characters`
    /// derives that bound from the varicode table rather than assuming it.
    pub fn dup_tol_s(self) -> f64 {
        match self {
            ModeId::Psk(m) => 1.0 / m.baud(),
            _ => self.chunk_secs() / 2.0,
        }
    }

    /// Transmission period in seconds for modes that live on a UTC grid; `None`
    /// for continuous modes, which start whenever the operator does.
    pub fn period_s(self) -> Option<u32> {
        match self {
            ModeId::Js8(m) => Some(js8::submode::of(m).period_s),
            ModeId::Olivia(_) | ModeId::Psk(_) => None,
        }
    }

    /// The band this mode's *power* is in, which is not always the bandwidth it
    /// is named for.
    ///
    /// [`ModeId::bandwidth_hz`] is what the mode occupies for the purposes of
    /// laying stations out and keeping them apart — a channel spacing. Measuring
    /// a signal's strength needs the band its energy is actually in, and for
    /// JS8 the two differ: a frame keeps only 62% of its power inside the
    /// conventional 8 × tone spacing, and 99% inside twice that. Measured on an
    /// off-air recording and on this crate's own synthesis, which agree to
    /// within 4% — see `examples/snr.rs`, section F.
    ///
    /// Olivia and PSK are given no margin on purpose. Their nominal bandwidth
    /// already holds their power, and widening a measurement band reaches into
    /// the neighbours: Olivia 8/250 measured over 500 Hz with a station 200 Hz
    /// away reads 3 dB strong, counting that station as its own.
    pub fn power_bandwidth_hz(self) -> f64 {
        match self {
            ModeId::Js8(_) => 2.0 * self.bandwidth_hz(),
            ModeId::Olivia(_) | ModeId::Psk(_) => self.bandwidth_hz(),
        }
    }

    /// Shortest stretch of audio worth measuring an SNR over.
    ///
    /// A decode's own extent, except for a mode whose decodes are shorter than
    /// the measurement needs: a PSK character is a fifth of a second, which is
    /// less than one analysis window, so its SNR is read from the second of
    /// audio around it instead.
    pub fn snr_window_s(self) -> f64 {
        self.chunk_secs().max(1.0)
    }

    /// How long one unit of decoded text takes on the air (a JS8 frame, an
    /// Olivia block).
    pub fn chunk_secs(self) -> f64 {
        match self {
            ModeId::Js8(m) => js8::submode::of(m).frame_secs(),
            ModeId::Olivia(m) => m.block_secs(),
            ModeId::Psk(m) => m.char_secs(),
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
    /// Each protocol measures an entirely different thing — JS8 the strength of
    /// its Costas sync, Olivia the peak of a Walsh correlation, PSK the
    /// envelope line at the symbol rate — so this is normalised to make the
    /// numbers mean the same *kind* of thing, not to make them comparable.
    /// It says how sure the decoder is, not how loud the station is: for the
    /// latter see [`Decode::snr_db`], which is comparable across every mode
    /// because it is a measurement rather than a decision statistic.
    pub quality: f32,
    /// Signal-to-noise in [`dsp::SNR_REF_BW_HZ`], in dB — the figure a ham
    /// means by SNR, and the one WSJT-X and JS8Call report.
    ///
    /// Measured off the audio around the decode rather than taken from the
    /// decoder, so it means the same thing whatever carried it. `None` when
    /// there was too little audio to measure, which is why it is not simply a
    /// number: a decode at the very start or end of a buffer has no room.
    pub snr_db: Option<f32>,
    /// The decoded text.
    pub text: String,
}

/// Parse a mode name.
///
/// Accepts what [`ModeId::name`] produces (`"JS8 Normal"`, `"Olivia 32/1000"`)
/// so a mode can round-trip through a config file or a command line, plus the
/// terser forms a person would actually type: `js8`, `js8:fast`, `16/500`.
pub fn parse_mode(s: &str) -> Option<ModeId> {
    let s = s.trim().to_ascii_lowercase();
    if let Some(rest) = s.strip_prefix("js8") {
        use js8::Mode::*;
        return Some(ModeId::Js8(match rest.trim().trim_start_matches(':').trim() {
            "" | "normal" => Normal,
            "slow" => Slow,
            "fast" => Fast,
            "turbo" => Turbo,
            _ => return None,
        }));
    }
    if s.starts_with("psk") || s.starts_with("bpsk") {
        return psk::parse(&s).map(ModeId::Psk);
    }
    olivia::parse(&s).map(ModeId::Olivia)
}

/// The modes a band scan tries by default: all four JS8 submodes, and the
/// common Olivia and PSK modes.
///
/// Scanning costs time per mode, so callers watching a known calling frequency
/// should pass a shorter list.
pub fn default_modes() -> Vec<ModeId> {
    let mut v = Protocol::Js8.modes();
    v.extend(Protocol::Olivia.modes());
    v.extend(Protocol::Psk.modes());
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
                |r| {
                    let mode = ModeId::Js8(r.mode);
                    Decode {
                        mode,
                        hz: r.hz,
                        time_s: r.offset as f64 / crate::SAMPLE_RATE as f64,
                        quality: (r.sync / js8::modem::NOISE_SYNC) as f32,
                        snr_db: measure_snr(samples, r.offset, r.hz, mode),
                        text: js8::message::unpack(&r.a87).text(),
                    }
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
            let mode = ModeId::Olivia(r.mode);
            Decode {
                mode,
                hz: r.hz,
                time_s: r.offset as f64 / crate::SAMPLE_RATE as f64,
                quality: (r.snr / olivia::modem::NOISE_FLOOR) as f32,
                snr_db: measure_snr(samples, r.offset, r.hz, mode),
                text: r.text(),
            }
        }));
    }

    // PSK modes are scanned together for the same reason.
    let psk_modes: Vec<psk::Mode> = modes
        .iter()
        .filter_map(|m| match m {
            ModeId::Psk(p) => Some(*p),
            _ => None,
        })
        .collect();
    if !psk_modes.is_empty() {
        let found: Vec<Decode> = psk::decode_all(samples, hz_lo, hz_hi, &psk_modes)
            .into_iter()
            .map(|r| {
                let mode = ModeId::Psk(r.mode);
                Decode {
                    mode,
                    hz: r.hz,
                    time_s: r.offset as f64 / crate::SAMPLE_RATE as f64,
                    quality: (r.score / psk::modem::NOISE_FLOOR) as f32,
                    snr_db: measure_snr(samples, r.offset, r.hz, mode),
                    text: r.text,
                }
            })
            .collect();
        out.extend(arbitrate_psk(found, &out));
    }

    out.sort_by(|a, b| a.time_s.partial_cmp(&b.time_s).unwrap());
    out
}

/// SNR of the signal a decode came from, in dB against
/// [`dsp::SNR_REF_BW_HZ`].
///
/// Measured over the decode's own stretch of audio, or the second around it
/// where that is shorter than the measurement needs — a PSK character being a
/// fifth of a second. The window is centred on the decode when it has to grow,
/// so what is measured stays the signal that produced it.
///
/// `None` at the edges of the buffer, where there is not enough audio either
/// side to measure over. A caller that needs a number for every decode should
/// treat that as "not measured" rather than as a weak signal.
fn measure_snr(samples: &[f32], offset: usize, hz: f64, mode: ModeId) -> Option<f32> {
    let rate = crate::SAMPLE_RATE as f64;
    let want = (mode.snr_window_s() * rate) as usize;
    let extent = (mode.chunk_secs() * rate) as usize;
    // Grown symmetrically, so a short decode is measured on the audio around
    // it rather than on the audio after it.
    let pad = want.saturating_sub(extent) / 2;
    let from = offset.saturating_sub(pad);
    let to = (offset + extent + pad).min(samples.len());
    let win = samples.get(from..to)?;
    dsp::snr_db(win, crate::SAMPLE_RATE, hz, mode.power_bandwidth_hz()).map(|s| s.db as f32)
}

/// Settle the claims two protocols make on the same signal, across a set of
/// decodes that were not all found by the same call.
///
/// [`decode_all`] already does this to what it finds, which is the whole story
/// when one call scans every mode. It is not the whole story when a caller
/// splits the work up by mode to get it done sooner — the file view scans one
/// mode per thread, so it finishes in the time of the slowest single mode
/// rather than the sum of all of them. Arbitration can only weigh a PSK decode
/// against an Olivia decode it can *see*, and split that way neither call sees
/// the other's. Merge the results, pass them through here, and the rule applies
/// as though one call had found them all.
///
/// Idempotent, and harmless on a set that needs nothing done to it: a PSK
/// decode with no Olivia signal on top of it survives any number of passes.
pub fn arbitrate(decodes: Vec<Decode>) -> Vec<Decode> {
    let (psk, others): (Vec<Decode>, Vec<Decode>) =
        decodes.into_iter().partition(|d| d.mode.protocol() == Protocol::Psk);
    if psk.is_empty() {
        return others;
    }
    let mut out = arbitrate_psk(psk, &others);
    out.extend(others);
    out.sort_by(|a, b| a.time_s.partial_cmp(&b.time_s).unwrap());
    out
}

/// Drop PSK decodes that are really an Olivia signal being read twice.
///
/// Every standard Olivia mode runs at 31.25 or 62.5 baud — `bandwidth / tones`
/// works out that way for all of them, because they descend from the same
/// 2000/64 as PSK31 does. Most are saved by width: their energy falls outside
/// the PSK detector's narrow analysis slice and they never trip it. Olivia
/// 4/125 is not — it shares the symbol rate *and* fits inside the slice, and it
/// clears both of the PSK gate's stages.
///
/// No refinement of the gate settles that honestly, because the one measure
/// that separates them at equal SNR — how compact the signal's spectrum is —
/// tracks SNR itself, so a weak PSK31 station reads the same as a strong Olivia
/// 4/125 one. What settles it is that Olivia can prove itself and PSK31 cannot:
/// a Walsh correlation above the noise floor is evidence, and an uncoded bit
/// stream is not. So where the two claim the same signal, PSK31 yields.
///
/// Only narrow Olivia modes arbitrate. A wide one must not silence a genuine
/// PSK31 station sitting inside its bandwidth — stations stacked inside each
/// other is a thing this monitor is meant to show, not hide.
fn arbitrate_psk(found: Vec<Decode>, others: &[Decode]) -> Vec<Decode> {
    /// How far apart in time two decodes may be and still be the same signal.
    /// An Olivia block and a PSK character land at quite different instants
    /// within the same transmission.
    const NEAR_S: f64 = 10.0;

    let olivia: Vec<&Decode> =
        others.iter().filter(|d| d.mode.protocol() == Protocol::Olivia).collect();
    if olivia.is_empty() {
        return found;
    }
    found
        .into_iter()
        .filter(|p| {
            // Only an Olivia mode narrow enough to sit inside the PSK
            // detector's own analysis slice can be mistaken for PSK at all, and
            // that width scales with the mode: PSK31 looks through 125 Hz,
            // PSK63 through 250.
            let slice = match p.mode {
                ModeId::Psk(m) => m.slice_hz(),
                _ => return true,
            };
            !olivia.iter().any(|o| {
                o.mode.bandwidth_hz() <= slice
                    && (o.hz - p.hz).abs() < o.mode.bandwidth_hz() / 2.0
                    && (o.time_s - p.time_s).abs() < NEAR_S
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The duplicate window must be narrower than the closest two real PSK
    /// characters can be, or a live scan throws away a character every time its
    /// windows overlap.
    ///
    /// The bound is derived from the varicode table, not assumed: a code is at
    /// least one bit and every code is followed by the two-bit separator, so
    /// consecutive characters complete at least three symbols apart. The old
    /// rule — half a chunk, where a PSK chunk is an *average* character of 6.5
    /// symbols — was 3.25, above that bound, and so ate short characters. A
    /// space is one bit, which is why spaces were what went missing.
    #[test]
    fn psk_dedup_survives_adjacent_characters() {
        let shortest = crate::psk::varicode::VARICODE
            .iter()
            .map(|(_, code)| code.len())
            .min()
            .expect("the table is not empty");
        assert_eq!(shortest, 1, "a one-bit code is the case this is all about");
        let closest_symbols = (shortest + 2) as f64; // its separator follows it

        for m in [crate::psk::PSK31, crate::psk::PSK63, crate::psk::PSK125] {
            let mode = ModeId::Psk(m);
            let closest_s = closest_symbols / m.baud();
            assert!(
                mode.dup_tol_s() < closest_s,
                "{}: a {:.0} ms window would swallow characters {:.0} ms apart",
                mode.name(),
                mode.dup_tol_s() * 1e3,
                closest_s * 1e3
            );
            // And the rule it replaced would not have.
            assert!(
                mode.chunk_secs() / 2.0 > closest_s,
                "{}: half a chunk no longer overlaps, so this test guards nothing",
                mode.name()
            );
        }

        // The block-decoded modes keep half a chunk, whose chunks are all the
        // same length, so nothing about them changes.
        for m in Protocol::Olivia.modes() {
            assert_eq!(m.dup_tol_s(), m.chunk_secs() / 2.0);
        }
    }

    #[test]
    fn default_modes_cover_both_protocols() {
        let modes = default_modes();
        for p in Protocol::ALL {
            assert!(modes.iter().any(|m| m.protocol() == p), "{} missing", p.name());
        }
    }

    /// Every mode survives a trip through its own name, which is what lets a
    /// settings file or a command line store one.
    #[test]
    fn mode_names_round_trip() {
        for m in default_modes() {
            assert_eq!(parse_mode(&m.name()), Some(m), "{} did not round-trip", m.name());
        }
    }

    #[test]
    fn parses_the_terse_forms_too() {
        assert_eq!(parse_mode("js8"), Some(ModeId::Js8(js8::Mode::Normal)));
        assert_eq!(parse_mode("js8:fast"), Some(ModeId::Js8(js8::Mode::Fast)));
        assert_eq!(parse_mode("JS8 Turbo"), Some(ModeId::Js8(js8::Mode::Turbo)));
        assert_eq!(parse_mode("16/500"), Some(ModeId::Olivia(olivia::OL_16_500)));
        assert_eq!(parse_mode("olivia:8/250"), Some(ModeId::Olivia(olivia::OL_8_250)));
        assert_eq!(parse_mode("js8:sideways"), None);
        assert_eq!(parse_mode("7/1000"), None);
        assert_eq!(parse_mode(""), None);
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
