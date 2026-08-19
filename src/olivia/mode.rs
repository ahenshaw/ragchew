//! Olivia mode parameters: the `tones/bandwidth` pair everything else follows
//! from.
//!
//! An Olivia mode is named for its number of tones and its occupied bandwidth
//! in Hz — "Olivia 32/1000" is 32 tones spread over 1000 Hz. Those two numbers
//! fix the tone spacing (`bandwidth / tones`), the symbol rate (numerically the
//! same, one baud per Hz of spacing), and the payload rate (`log2(tones)`
//! characters per 64-symbol FEC block).
//!
//! Wider bandwidth is faster; more tones is more robust but slower, because a
//! block always costs 64 symbols no matter how many characters it carries.

use crate::SAMPLE_RATE;

/// Symbols in one FEC block. Fixed by Olivia's 7-bit characters: each is spread
/// over a Walsh function of length `2^(7-1)`.
pub const SYMBOLS_PER_BLOCK: usize = 64;

/// An Olivia mode.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Mode {
    /// Number of tones (a power of two, 2..=256).
    pub tones: u16,
    /// Occupied bandwidth in Hz (125, 250, 500, 1000 or 2000).
    pub bandwidth: u16,
}

impl Mode {
    /// A mode from its `tones/bandwidth` pair, e.g. `Mode::new(32, 1000)`.
    pub const fn new(tones: u16, bandwidth: u16) -> Mode {
        Mode { tones, bandwidth }
    }

    /// Bits carried by one symbol, i.e. `log2(tones)`. Also the number of
    /// characters in a FEC block.
    pub fn bits_per_symbol(&self) -> usize {
        self.tones.trailing_zeros() as usize
    }

    /// Characters carried by one FEC block (one per bit of a symbol).
    pub fn chars_per_block(&self) -> usize {
        self.bits_per_symbol()
    }

    /// Tone spacing in Hz (`bandwidth / tones`).
    pub fn spacing(&self) -> f64 {
        self.bandwidth as f64 / self.tones as f64
    }

    /// Symbol rate in baud. Numerically equal to the tone spacing: Olivia sends
    /// exactly one symbol per tone-spacing period, which is what makes adjacent
    /// tones orthogonal.
    pub fn baud(&self) -> f64 {
        self.spacing()
    }

    /// Samples between the start of one symbol and the next.
    pub fn symbol_separ(&self) -> usize {
        (SAMPLE_RATE as f64 / self.baud()).round() as usize
    }

    /// Samples in one symbol's raised-cosine envelope. Symbols overlap 50%, so
    /// this is twice [`symbol_separ`](Self::symbol_separ) — and it is also the
    /// analysis window length, giving an FFT bin of half a tone spacing.
    pub fn symbol_len(&self) -> usize {
        2 * self.symbol_separ()
    }

    /// Duration of one 64-symbol FEC block, in seconds.
    pub fn block_secs(&self) -> f64 {
        (SYMBOLS_PER_BLOCK * self.symbol_separ()) as f64 / SAMPLE_RATE as f64
    }

    /// Throughput in characters per second.
    pub fn chars_per_sec(&self) -> f64 {
        self.chars_per_block() as f64 / self.block_secs()
    }

    /// Frequency of tone `k` (0-based, low to high) for a signal centered on
    /// `center_hz`.
    ///
    /// Tones are laid out symmetrically about the center: the block of tones
    /// spans `center ± bandwidth/2`, with each tone half a spacing in from the
    /// edge. (fldigi's own layout sits a quarter of a tone spacing higher, an
    /// artefact of rounding the first carrier to an FFT bin; that is well
    /// inside the decoder's tuning tolerance.)
    pub fn tone_hz(&self, center_hz: f64, k: usize) -> f64 {
        center_hz - self.bandwidth as f64 / 2.0 + (k as f64 + 0.5) * self.spacing()
    }

    /// `"32/1000"`.
    pub fn short_name(&self) -> String {
        format!("{}/{}", self.tones, self.bandwidth)
    }

    /// `"Olivia 32/1000"`.
    pub fn name(&self) -> String {
        format!("Olivia {}", self.short_name())
    }

    /// Whether the parameters are a legal Olivia combination this crate can
    /// process at [`SAMPLE_RATE`].
    pub fn is_valid(&self) -> bool {
        self.tones.is_power_of_two()
            && (2..=256).contains(&self.tones)
            && matches!(self.bandwidth, 125 | 250 | 500 | 1000 | 2000)
            && SAMPLE_RATE as f64 % self.baud() == 0.0
    }
}

/// Parse a mode name: `"32/1000"`, `"olivia 32/1000"` or `"Olivia:32/1000"`.
pub fn parse(s: &str) -> Option<Mode> {
    let s = s.trim();
    let s = s.strip_prefix("olivia").or_else(|| s.strip_prefix("Olivia")).unwrap_or(s);
    let s = s.trim().trim_start_matches(':').trim();
    let (t, b) = s.split_once('/')?;
    let m = Mode::new(t.trim().parse().ok()?, b.trim().parse().ok()?);
    m.is_valid().then_some(m)
}

/// Olivia 4/125 — the narrowest and most robust of the common modes.
pub const OL_4_125: Mode = Mode::new(4, 125);
/// Olivia 8/250.
pub const OL_8_250: Mode = Mode::new(8, 250);
/// Olivia 8/500.
pub const OL_8_500: Mode = Mode::new(8, 500);
/// Olivia 16/500 — one of the two most common calling modes.
pub const OL_16_500: Mode = Mode::new(16, 500);
/// Olivia 16/1000.
pub const OL_16_1000: Mode = Mode::new(16, 1000);
/// Olivia 32/1000 — the classic wide, robust ragchew mode.
pub const OL_32_1000: Mode = Mode::new(32, 1000);

/// The modes actually worth scanning for on the HF calling frequencies, in the
/// order a band scan should try them.
///
/// Scanning costs time per mode, so this is deliberately the on-air short list
/// rather than every legal `tones/bandwidth` pair.
pub const COMMON: [Mode; 6] =
    [OL_8_250, OL_16_500, OL_32_1000, OL_8_500, OL_16_1000, OL_4_125];

#[cfg(test)]
mod tests {
    use super::*;

    /// Published rates for the standard modes (fldigi's mode table).
    #[test]
    fn parameters_match_published_values() {
        // (mode, baud, chars/sec)
        let cases = [
            (OL_4_125, 31.25, 0.98),
            (OL_8_250, 31.25, 1.46),
            (OL_16_500, 31.25, 1.95),
            (OL_32_1000, 31.25, 2.44),
            (OL_8_500, 62.5, 2.93),
            (OL_16_1000, 62.5, 3.91),
        ];
        for (m, baud, cps) in cases {
            assert!(m.is_valid(), "{} should be valid", m.name());
            assert_eq!(m.baud(), baud, "{} baud", m.name());
            assert!((m.chars_per_sec() - cps).abs() < 0.01, "{} chars/sec", m.name());
            assert_eq!(m.spacing(), m.bandwidth as f64 / m.tones as f64);
        }
    }

    #[test]
    fn tones_span_the_bandwidth_symmetrically() {
        let m = OL_32_1000;
        let lo = m.tone_hz(1500.0, 0);
        let hi = m.tone_hz(1500.0, m.tones as usize - 1);
        assert_eq!(lo + hi, 3000.0, "tone block should be centered");
        assert!((hi - lo - (m.bandwidth as f64 - m.spacing())).abs() < 1e-9);
    }

    #[test]
    fn parses_mode_names() {
        assert_eq!(parse("32/1000"), Some(OL_32_1000));
        assert_eq!(parse("Olivia 8/250"), Some(OL_8_250));
        assert_eq!(parse("olivia:16/500"), Some(OL_16_500));
        assert_eq!(parse("7/1000"), None);
        assert_eq!(parse("nonsense"), None);
    }
}
