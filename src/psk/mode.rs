//! PSK mode parameters: the symbol rate everything else follows from.
//!
//! A PSK mode is named for its baud rate rounded to a whole number — PSK31 is
//! 31.25 baud, which is 2000/64 and the reason every mode descended from it
//! (including all of Olivia's) shares that rate. Doubling the baud gives PSK63
//! and PSK125: same varicode, same shaping, same detector, twice and four times
//! the speed and the bandwidth.
//!
//! Baud is held in thousandths so a mode stays `Eq + Hash`, which
//! [`ModeId`](crate::protocol::ModeId) requires.

use crate::SAMPLE_RATE;

/// Baseband samples per symbol in the detector's analysis grid. Four is enough
//  to carry the 31.25 Hz envelope line (Nyquist is two) with room over it for
/// the guard band the score is measured against.
pub const SLOTS_PER_SYMBOL: usize = 4;

/// A PSK mode.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Mode {
    /// Symbol rate in thousandths of a baud (31_250 = 31.25 baud).
    pub baud_milli: u32,
}

/// BPSK31: 31.25 baud.
pub const PSK31: Mode = Mode::new(31_250);
/// BPSK63: 62.5 baud.
pub const PSK63: Mode = Mode::new(62_500);
/// BPSK125: 125 baud. Implemented but unmeasured — see [`COMMON`].
pub const PSK125: Mode = Mode::new(125_000);

/// The PSK modes a band scan tries by default.
///
/// PSK31 and PSK63, and not PSK125 — the difference is that the first two have
/// been measured. A mode whose false-alarm rate nobody has established has no
/// business printing text into a band monitor, however cleanly it round-trips.
///
/// That the *same* thresholds serve both is by construction rather than luck:
/// [`modem::SLICE_BINS`](super::modem::SLICE_BINS) is fixed, so a block always
/// holds 256 symbols whatever the baud, and the gate's statistics are counts
/// over symbols. `examples/psk_gate.rs` confirms it — over 55,320 noise
/// candidates apiece, neither mode cleared stages one and two even once, and
/// their noise distributions agree to within a few percent.
///
/// PSK63 detects about 3 dB later than PSK31 (97% at −6 dB against 93% at −10),
/// which is what twice the baud in twice the noise bandwidth costs, and is also
/// roughly how much sooner PSK63 stops being copyable. Add PSK125 by running
/// the harness for it and putting the numbers here.
pub const COMMON: [Mode; 2] = [PSK31, PSK63];

impl Mode {
    /// A mode from its baud rate in thousandths, e.g. `Mode::new(31_250)`.
    pub const fn new(baud_milli: u32) -> Mode {
        Mode { baud_milli }
    }

    /// Symbol rate in baud.
    pub fn baud(&self) -> f64 {
        self.baud_milli as f64 / 1000.0
    }

    /// Full name, e.g. `"PSK31"`. The number is the baud rate rounded, which is
    /// how these modes are always written: 31.25 baud is "PSK31".
    pub fn name(&self) -> String {
        format!("PSK{}", (self.baud_milli + 500) / 1000)
    }

    /// Same as [`name`](Self::name) — a PSK mode's name is already short.
    pub fn short_name(&self) -> String {
        self.name()
    }

    /// Samples between the start of one symbol and the next.
    pub fn symbol_samples(&self) -> usize {
        (SAMPLE_RATE as f64 / self.baud()).round() as usize
    }

    /// Width of the analysis slice in Hz: four times the baud, so the signal
    /// and its skirts fit with margin and the envelope's symbol-rate line lands
    /// at a quarter of the slice's own Nyquist.
    ///
    /// This is also the sample rate of the complex baseband cut out of it.
    pub fn slice_hz(&self) -> f64 {
        SLOTS_PER_SYMBOL as f64 * self.baud()
    }

    /// Decimation from the crate's sample rate down to [`slice_hz`](Self::slice_hz).
    pub fn decim(&self) -> usize {
        (SAMPLE_RATE as f64 / self.slice_hz()).round() as usize
    }

    /// Occupied bandwidth in Hz.
    ///
    /// Twice the baud: the raised-cosine shaping puts the main lobe inside
    /// ±baud/2 and everything that matters inside ±baud.
    pub fn bandwidth_hz(&self) -> f64 {
        2.0 * self.baud()
    }

    /// Spacing of the frequency grid the band is screened on.
    ///
    /// The screen was measured flat out to ±30 Hz of carrier offset at 31.25
    /// baud — a fifth of the slice either side — and only collapses past ±50.
    /// A grid of 1.28 baud therefore never leaves a signal more than 20 Hz from
    /// a candidate, which costs nothing, and covers 300–2600 Hz in 58 tries
    /// instead of the thousand a carrier-tracking demodulator would need.
    pub fn candidate_step_hz(&self) -> f64 {
        1.28 * self.baud()
    }

    /// How long a typical character takes on the air.
    ///
    /// Varicode is variable-length — a space is one bit and a `%` is ten — plus
    /// two bits of separator. Six and a half bits is what English averages out
    /// at, and this is only ever used as a tolerance and an animation length,
    /// never to place a character in time.
    pub fn char_secs(&self) -> f64 {
        6.5 / self.baud()
    }
}

/// Parse a PSK mode name: `"psk31"`, `"PSK 31"`, `"bpsk31"`, or bare `"31"`
/// once a caller has already established it is talking about PSK.
pub fn parse(s: &str) -> Option<Mode> {
    let s = s.trim().to_ascii_lowercase();
    let rest = s
        .strip_prefix("bpsk")
        .or_else(|| s.strip_prefix("psk"))
        .unwrap_or(&s)
        .trim()
        .trim_start_matches(':')
        .trim();
    match rest {
        "31" => Some(PSK31),
        "63" => Some(PSK63),
        "125" => Some(PSK125),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 12 kHz / 31.25 baud = 384 exactly, and 12 kHz / 125 Hz = 96 exactly.
    /// Nothing downstream interpolates the symbol clock or the decimation, so a
    /// mode whose rates do not divide the sample rate would drift against its
    /// own grid.
    #[test]
    fn every_mode_lands_on_whole_samples() {
        assert_eq!(PSK31.symbol_samples(), 384);
        assert_eq!(PSK31.decim(), 96);
        assert_eq!(PSK31.slice_hz(), 125.0);
        for m in [PSK31, PSK63, PSK125] {
            let sps = SAMPLE_RATE as f64 / m.baud();
            assert_eq!(sps.fract(), 0.0, "{} symbol is {sps} samples", m.name());
            let dec = SAMPLE_RATE as f64 / m.slice_hz();
            assert_eq!(dec.fract(), 0.0, "{} decimates by {dec}", m.name());
        }
    }

    #[test]
    fn names_round_trip() {
        for m in [PSK31, PSK63, PSK125] {
            assert_eq!(parse(&m.name()), Some(m));
        }
        assert_eq!(parse("bpsk31"), Some(PSK31));
        assert_eq!(parse("psk:63"), Some(PSK63));
        assert_eq!(parse("psk99"), None);
    }

    #[test]
    fn the_candidate_grid_covers_the_band_cheaply() {
        let step = PSK31.candidate_step_hz();
        assert_eq!(step, 40.0);
        assert_eq!(((2600.0 - 300.0) / step) as usize + 1, 58);
    }
}
