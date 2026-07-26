//! # JS8
//!
//! A pure-Rust encoder and decoder for **JS8**, the weak-signal digital mode
//! used by [JS8Call](https://js8call.com) for keyboard-to-keyboard messaging.
//!
//! ```text
//! text ──varicode──▶ 72-bit payload ──+itype──▶ 75 bits ──+CRC12──▶ 87 bits
//!      ──LDPC(174,87)──▶ 174 bits ──recode──▶ 79 8-FSK symbols (3 Costas sync)
//!      ──continuous-phase FSK──▶ 12 kHz audio
//! ```
//!
//! and the reverse (sync search, FFT tone demod, soft-decision LDPC).
//!
//! Protocol constants and behavior are validated bit-for-bit against Robert
//! Morris (AB1HL)'s independent JS8 implementation; see `tools/ref`.
//!
//! ## Quick start
//! ```
//! use ragchew::js8::message::IType;
//! let audio = ragchew::js8::encode_freetext("HELLO WORLD", IType::None, 1500.0);
//! let msg = ragchew::js8::decode_near(&audio, 1500.0, 0).unwrap();
//! assert_eq!(msg.text(), "HELLO WORLD");
//! ```

pub mod crc;
pub mod ldpc;
pub mod message;
pub mod modem;
pub mod submode;
pub mod tables;
pub mod varicode;

pub use message::{IType, Message};
pub use submode::{Mode, Submode};

/// Number of samples of leading silence in a nominal JS8 cycle (0.5 s at 12 kHz).
pub const CYCLE_LEAD_SAMPLES: usize = (modem::SAMPLE_RATE as usize) / 2;

/// Encode free text into a JS8 audio burst (no leading silence) at `base_hz`.
///
/// Returns the modulated samples. Text longer than one frame is truncated to
/// what fits; use [`encode_freetext_consumed`] to learn how much was used.
pub fn encode_freetext(text: &str, itype: IType, base_hz: f64) -> Vec<f32> {
    encode_freetext_consumed(text, itype, base_hz).0
}

/// Like [`encode_freetext`] but also returns how many characters were consumed.
pub fn encode_freetext_consumed(text: &str, itype: IType, base_hz: f64) -> (Vec<f32>, usize) {
    let (a87, consumed) = message::freetext(text, itype);
    (modem::encode_audio(&a87, base_hz), consumed)
}

/// Decode a single JS8 signal near a known carrier and start offset, returning
/// the parsed [`Message`] if the CRC checks out.
pub fn decode_near(samples: &[f32], base_hz: f64, start_sample: usize) -> Option<Message> {
    let r = modem::decode_near(samples, base_hz, start_sample, 4, 30)?;
    if message::crc_ok(&r.a87) {
        Some(message::unpack(&r.a87))
    } else {
        None
    }
}

/// Convenience wrapper returning the message only when it is free text.
pub fn decode_freetext_near(samples: &[f32], base_hz: f64, start_sample: usize) -> Option<Message> {
    decode_near(samples, base_hz, start_sample)
}
