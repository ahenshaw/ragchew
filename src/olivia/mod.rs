//! # Olivia MFSK
//!
//! Olivia is a continuous, conversational digital mode: a stream of MFSK tones
//! carrying 7-bit characters, with no frames, no cycles and no callsign
//! packing. Where JS8 sends a burst on a UTC-aligned schedule, an Olivia
//! station simply keys up and types, and the receiver has to find the signal in
//! frequency and lock to its block timing on its own.
//!
//! ```text
//! text ──7-bit chars──▶ blocks of log2(tones) chars
//!      ──Walsh(64) spread──▶ 64 × log2(tones) bits ──scramble + interleave──▶
//!        64 symbols ──Gray──▶ tones ──overlapped raised-cosine MFSK──▶ audio
//! ```
//!
//! A mode is written `tones/bandwidth`: [`OL_32_1000`] is 32 tones across
//! 1000 Hz. See [`mode`] for what follows from that pair, [`fec`] for the
//! Walsh-Hadamard coding that gives Olivia its weak-signal reach, and
//! [`modem`] for the modulator and the band scanner.
//!
//! The FEC layer is transcribed from — and cross-checked against — Pawel
//! Jalocha's reference implementation, the same code fldigi uses; see
//! `tools/ref/olivia`, which also carries a harness for running that reference
//! receiver over a recording to compare decode yield.
//!
//! ## Quick start
//! ```
//! use ragchew::olivia::{self, OL_16_500};
//!
//! let audio = olivia::encode("CQ DE W1AW K ", 1500.0, OL_16_500);
//! let text: String = olivia::decode_all_mode(&audio, 1200.0, 1800.0, OL_16_500)
//!     .iter()
//!     .map(|r| r.text())
//!     .collect();
//! assert!(text.starts_with("CQ DE W1AW"));
//! ```

pub mod fec;
pub mod fht;
pub mod mode;
pub mod modem;

pub use fec::Block;
pub use mode::{
    parse, Mode, COMMON, OL_16_1000, OL_16_500, OL_32_1000, OL_4_125, OL_8_250, OL_8_500,
    SYMBOLS_PER_BLOCK,
};
pub use modem::{decode_all, decode_all_mode, encode, synthesize, DecodeResult};
