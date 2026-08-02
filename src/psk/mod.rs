//! # PSK31
//!
//! PSK31 is the oldest of the keyboard-to-keyboard modes and the least
//! defended. A station keys up, the carrier's phase reverses once per bit at
//! 31.25 baud, and variable-length characters go out back to back with two
//! zeros between them. There is no FEC, no CRC, no sync pattern and no
//! schedule — 31 Hz of bandwidth and nothing to prove itself with.
//!
//! ```text
//! text ──varicode──▶ bits ──00 separators──▶ differential BPSK
//!      ──raised-cosine shaping──▶ audio
//! ```
//!
//! That absence is the whole design problem here. JS8 gates on a Costas array,
//! LDPC and a CRC; Olivia gates on a Walsh correlation against a measured noise
//! floor. PSK31 offers a demodulator nothing but bits, and varicode will make
//! letters out of some of them whatever you feed it — so a band monitor that
//! prints everything it scans needs a gate that PSK31 does not come with.
//!
//! [`modem`] builds one out of three things the mode cannot help doing: the
//! amplitude dip at every phase reversal, which puts a line in the envelope
//! spectrum at exactly the symbol rate; the antipodal phase steps, which say the
//! signal is BPSK and not some other thing modulated at 31.25 baud; and the
//! varicode framing, which says the bits are language. The stages, their
//! measured thresholds, and what still gets through them are documented there.
//! See [`varicode`] for the character code and [`mode`] for the parameters.
//!
//! ## Quick start
//!
//! Note the noise. The gate's first two stages are ratios against the local
//! noise floor, so a band with no noise in it has no floor to measure against,
//! and leakage from a station a kilohertz away scores as well as the station
//! does. Real audio always obliges; synthetic test signals have to be asked.
//!
//! ```
//! use ragchew::psk::{self, PSK31};
//!
//! // One continuous transmission, long enough to fill a couple of blocks.
//! let text = "cq cq de w1aw k ".repeat(20);
//! let sig = psk::encode(&text, 1500.0, PSK31);
//!
//! let mut seed = 0x2545_F491u64;
//! let mut audio: Vec<f32> = (0..sig.len())
//!     .map(|_| {
//!         seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
//!         ((seed >> 40) as f32 / (1u32 << 24) as f32 - 0.5) * 0.05
//!     })
//!     .collect();
//! for (i, s) in sig.iter().enumerate() {
//!     audio[i] += s * 0.5;
//! }
//!
//! let got: String =
//!     psk::decode_all(&audio, 300.0, 2600.0, &[PSK31]).iter().map(|r| r.text.clone()).collect();
//! assert!(got.contains("cq cq de w1aw k"));
//! ```

pub mod mode;
pub mod modem;
pub mod rx;
pub mod varicode;

pub use mode::{parse, Mode, COMMON, PSK125, PSK31, PSK63};
pub use modem::{decode_all, encode, synthesize, DecodeResult, SYMBOLS_PER_BLOCK};
