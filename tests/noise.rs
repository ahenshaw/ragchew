//! Decoder robustness under additive white Gaussian noise. This exercises the
//! soft-decision LDPC path (clean round-trips alone would pass with hard bits).

use ragchew::js8::message::{self, IType};
use ragchew::js8::modem;

/// Small deterministic Gaussian generator (Box–Muller over a 64-bit LCG),
/// so the test is reproducible without external crates.
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0
    }
    fn unit(&mut self) -> f64 {
        // 53-bit mantissa in (0,1]
        ((self.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    }
    fn gaussian(&mut self) -> f64 {
        let u1 = self.unit();
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

fn build_audio(text: &str, base_hz: f64, sigma: f32, seed: u64) -> Vec<f32> {
    let (a87, _) = message::freetext(text, IType::None);
    let burst = modem::encode_audio(&a87, base_hz);
    let mut audio = vec![0.0f32; ragchew::js8::CYCLE_LEAD_SAMPLES];
    audio.extend_from_slice(&burst);
    audio.resize(15 * modem::SAMPLE_RATE as usize, 0.0);
    let mut rng = Rng(seed);
    for s in audio.iter_mut() {
        *s += sigma * rng.gaussian() as f32;
    }
    audio
}

/// The codec decodes correctly through moderate noise (signal amplitude ~1.0).
#[test]
fn decodes_through_noise() {
    let base_hz = 1500.0;
    for (i, text) in ["HELLO WORLD", "CQ CQ DE TEST", "73 GL"].iter().enumerate() {
        let audio = build_audio(text, base_hz, 0.8, 0x1234 + i as u64);
        let r = modem::decode_near(&audio, base_hz, ragchew::js8::CYCLE_LEAD_SAMPLES, 4, 40)
            .expect("sync found");
        assert!(
            r.ldpc_ok && message::crc_ok(&r.a87),
            "should decode {text:?} at sigma=0.8 (score {})",
            r.ldpc_score
        );
        // reference truncates to what fits in one frame; compare the decode to
        // a clean-path decode of the same input for an exact expected string.
        let (clean_a87, _) = message::freetext(text, IType::None);
        assert_eq!(r.a87.to_vec(), clean_a87.to_vec(), "wrong bits for {text:?}");
    }
}

/// Report the approximate noise level at which decoding starts to fail, as a
/// sanity check that error correction is doing real work (informational).
#[test]
fn noise_threshold_report() {
    let base_hz = 1500.0;
    let text = "HELLO WORLD";
    let mut last_ok = 0.0f32;
    for step in 1..=20 {
        let sigma = step as f32 * 0.2;
        let audio = build_audio(text, base_hz, sigma, 99);
        match modem::decode_near(&audio, base_hz, ragchew::js8::CYCLE_LEAD_SAMPLES, 4, 40) {
            Some(r) if r.ldpc_ok && message::crc_ok(&r.a87) => last_ok = sigma,
            _ => break,
        }
    }
    println!("decoded HELLO WORLD up to sigma = {last_ok:.1} (signal amplitude 1.0)");
    assert!(last_ok >= 0.8, "expected to tolerate at least sigma 0.8, got {last_ok}");
}
