//! A synthetic multi-station band for the app's `--demo` mode and the headless
//! scene validator. Ten stations across all four submodes (Slow/Normal/Fast/
//! Turbo), each on its own cadence and spaced so their bandwidths don't overlap,
//! with light noise.

use js8::message::{self, IType};
use js8::modem::{self, SAMPLE_RATE};
use js8::submode::{self, Submode};

/// (carrier Hz, submode, per-cycle frame texts). Each station keeps its carrier
/// and transmits one frame per cycle of its mode. Carriers are chosen so the
/// per-mode bandwidths (Slow 25, Normal 50, Fast 80, Turbo 160 Hz) don't collide.
pub const STATIONS: &[(f64, Submode, &[&str])] = &[
    (500.0, submode::SLOW, &["CQ DE W1SLW ", "EM73 PSE K "]),
    (700.0, submode::NORMAL, &["CQ CQ DE K2N ", "FN20 5W ", "73 GL "]),
    (850.0, submode::NORMAL, &["DE VE3XYZ ", "RRR TU ", "SK E E "]),
    (1050.0, submode::FAST, &["CQ DE N0FST ", "DN70 ", "QRZ? "]),
    (1300.0, submode::TURBO, &["TEST DE W9T ", "FAST QSO ", "FB 73 GL "]),
    (1600.0, submode::NORMAL, &["DE G4ABC ", "IO91 QRP ", "TU 72 "]),
    (1800.0, submode::FAST, &["CQ DX DE JA1F ", "PM95 ", "UP 2 "]),
    (2000.0, submode::SLOW, &["DE VK4SLW ", "QG62 GD DX "]),
    (2150.0, submode::NORMAL, &["DE KH6X ", "BL11 ", "ALOHA 73 "]),
    (2350.0, submode::TURBO, &["DE ZL2T ", "QRO 400W ", "GL SK "]),
];

/// Synthesize a multi-station band (12 kHz mono).
pub fn synth() -> Vec<f32> {
    let rate = SAMPLE_RATE as usize;
    let total = 60 * rate;
    let mut buf = vec![0.0f32; total];

    for (hz, sm, msgs) in STATIONS {
        for (cyc, m) in msgs.iter().enumerate() {
            let (a87, _) = message::freetext(m, IType::None);
            let burst = modem::encode_audio_sm(&a87, *hz, sm);
            // frame starts 0.5 s into cycle `cyc` of this mode
            let start = (cyc * sm.period_s as usize) * rate + rate / 2;
            for (i, s) in burst.iter().enumerate() {
                if start + i < total {
                    buf[start + i] += *s * 0.45;
                }
            }
        }
    }

    // light deterministic noise
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    for s in buf.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let n = ((seed >> 40) as f32 / (1u32 << 24) as f32) - 0.5;
        *s += n * 0.05;
    }
    buf
}
