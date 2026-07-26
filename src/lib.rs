//! # ragchew
//!
//! A pure-Rust codec library for HAM radio keyboard-to-keyboard digital modes,
//! and the engine behind the `ragchew` panorama monitor — a waterfall that
//! decodes *every* conversation in an audio band at once, whatever mode each
//! one is using.
//!
//! Two protocol families are implemented today:
//!
//! | module            | protocol | modes |
//! |-------------------|----------|-------|
//! | [`js8`]           | JS8 (JS8Call) — burst, cycle-aligned, LDPC | Slow / Normal / Fast / Turbo |
//! | [`olivia`]        | Olivia MFSK — continuous, Walsh-Hadamard FEC | any tones/bandwidth pair, e.g. 8/250, 16/500, 32/1000 |
//!
//! [`protocol`] sits on top of both and is what applications normally use: it
//! erases the difference between "JS8 Normal frame" and "Olivia 32/1000 block"
//! down to a common [`Decode`] — some text, at some frequency, at some time.
//!
//! ```no_run
//! use ragchew::protocol;
//!
//! let (samples, _rate) = ragchew::wav::read("recording.wav").unwrap();
//! let modes = protocol::default_modes();
//! for d in protocol::decode_all(&samples, 300.0, 2600.0, &modes) {
//!     println!("{:7.1} Hz  t={:6.2}s  {:>14}  {}", d.hz, d.time_s, d.mode.name(), d.text);
//! }
//! ```
//!
//! Everything runs at one internal sample rate ([`SAMPLE_RATE`]); the live
//! capture path resamples the sound card to it.

pub mod dsp;
pub mod js8;
pub mod olivia;
pub mod protocol;
pub mod spectrogram;
pub mod wav;

pub use protocol::{Decode, ModeId, Protocol};
pub use spectrogram::Spectrogram;

/// The internal audio sample rate (Hz) every decoder in this crate works at.
///
/// JS8 fixes this at 12 kHz; Olivia has no intrinsic rate, so it uses the same
/// one and a single capture stream can feed both.
pub const SAMPLE_RATE: u32 = 12_000;
