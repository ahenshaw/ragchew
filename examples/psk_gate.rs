//! Feasibility and calibration harness for the PSK detection gate.
//!
//! BPSK31 has no FEC, so nothing in the demodulator says "this was a signal".
//! [`ragchew::psk::modem`] therefore gates in three stages, and every threshold
//! it uses is empirical. This is where those numbers come from, and it runs on
//! the shipping code — [`Block::screen`] and [`Block::framing`] — rather than a
//! parallel implementation, so it cannot drift away from what the app does.
//!
//!   A  false alarm — stages one and two over a band of pure noise
//!   B  sensitivity — detection probability vs SNR
//!   C  frequency tolerance — how coarse the candidate grid can be
//!   D  discrimination — what the *other* modes in the band score
//!   E  framing — stage three, on real text against everything else
//!   F  cost — what a screening pass over the band actually costs
//!
//! Run with `cargo run --release --example psk_gate`, optionally naming modes:
//! `cargo run --release --example psk_gate -- psk31 psk63`.

use ragchew::olivia;
use ragchew::psk::modem::Block;
use ragchew::psk::{self, Mode, PSK31, PSK63};
use ragchew::{js8, SAMPLE_RATE};

const BAND: (f64, f64) = (300.0, 2600.0);

/// Same noise level and reference bandwidth the weak-signal demo uses, so SNRs
/// here mean what they mean in the README table.
const NOISE_RMS: f32 = 0.10;
const SNR_REF_BW: f32 = 2500.0;

// ---- deterministic Gaussian noise ----

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 1.0) / (1u64 << 53) as f64
    }
    fn gaussian(&mut self) -> f64 {
        (-2.0 * self.unit().ln()).sqrt() * (2.0 * std::f64::consts::PI * self.unit()).cos()
    }
}

fn rms(v: &[f32]) -> f32 {
    (v.iter().map(|x| x * x).sum::<f32>() / v.len().max(1) as f32).sqrt()
}

fn noise(n: usize, seed: u64) -> Vec<f32> {
    let mut rng = Rng(seed);
    (0..n).map(|_| NOISE_RMS * rng.gaussian() as f32).collect()
}

/// Scale factor putting `sig` at `snr_db` relative to the noise in
/// [`SNR_REF_BW`] — the same arithmetic `gui/src/demo.rs` uses.
fn scale_for_snr(sig: &[f32], snr_db: f32) -> f32 {
    let noise_in_ref = NOISE_RMS * NOISE_RMS * SNR_REF_BW / (SAMPLE_RATE as f32 / 2.0);
    let want = noise_in_ref * 10f32.powf(snr_db / 10.0);
    let have = rms(sig).powi(2);
    if have > 0.0 {
        (want / have).sqrt()
    } else {
        0.0
    }
}

/// `sig` at `snr_db` over noise, in a buffer one block long.
fn over_noise(sig: &[f32], snr_db: f32, n: usize, seed: u64) -> Vec<f32> {
    let amp = scale_for_snr(sig, snr_db);
    let mut buf = noise(n, seed);
    for (i, s) in sig.iter().enumerate() {
        if i < buf.len() {
            buf[i] += s * amp;
        }
    }
    buf
}

/// A continuous PSK transmission that fills a block of `mode`.
fn psk_signal(mode: Mode) -> Vec<f32> {
    let mut text = String::new();
    while text.chars().count() < (mode.block_secs() * 6.0) as usize + 200 {
        text.push_str("cq cq de w1aw w1aw k the quick brown fox ");
    }
    psk::encode(&text, 1500.0, mode)
}

/// Repeat `sig` until it fills `n` samples. Only for signals whose phase is
/// continuous across the join, or which are noise-like anyway.
fn fill(mut sig: Vec<f32>, n: usize) -> Vec<f32> {
    if sig.is_empty() {
        return vec![0.0; n];
    }
    while sig.len() < n {
        sig.extend_from_within(..);
    }
    sig
}

fn pct(sorted: &[f64], q: f64) -> f64 {
    sorted[(((sorted.len() - 1) as f64) * q).round() as usize]
}

fn sorted(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v
}

// ---- experiments ----

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let modes: Vec<Mode> = if args.is_empty() {
        vec![PSK31, PSK63]
    } else {
        args.iter()
            .filter_map(|a| psk::parse(a))
            .collect()
    };
    for (i, mode) in modes.iter().enumerate() {
        if i > 0 {
            println!("\n{}", "=".repeat(72));
        }
        run(*mode);
    }
}

fn run(mode: Mode) {
    let n = mode.block_samples();
    let step = mode.candidate_step_hz();
    let candidates: Vec<f64> = {
        let count = ((BAND.1 - BAND.0) / 5.0) as usize;
        (0..=count).map(|i| BAND.0 + i as f64 * 5.0).collect()
    };
    println!(
        "### {} — {:.2} baud, block {:.3} s ({} samples), slice {:.0} Hz, grid {:.0} Hz",
        mode.name(),
        mode.baud(),
        mode.block_secs(),
        n,
        mode.slice_hz(),
        step
    );
    println!(
        "    thresholds in force: screen {:.2}, conc {:.3}, tight {:.2}, valid {:.2}, words {}",
        psk::modem::SCREEN_THRESHOLD,
        psk::modem::CONC_THRESHOLD,
        psk::modem::TIGHT_RATIO_MIN,
        psk::modem::VALID_RATIO_MIN,
        psk::modem::MIN_WORDS,
    );

    // ---- A: false alarm over pure noise ----
    println!("\n== A. false alarm: pure noise, {} candidates x 5 Hz ==", candidates.len());
    let records = 120;
    let mut env = Vec::new();
    let mut conc = Vec::new();
    let mut per_pass_max = Vec::new();
    let mut survived = 0usize;
    let mut tested = 0usize;
    for r in 0..records {
        let block = Block::new(&noise(n, 0xBEEF_0000 + r as u64), 0, mode);
        let mut worst = 0.0f64;
        for &hz in &candidates {
            let (s, c) = block.screen(hz);
            env.push(s);
            conc.push(c);
            worst = worst.max(s);
            tested += 1;
            if s >= psk::modem::SCREEN_THRESHOLD && c >= psk::modem::CONC_THRESHOLD {
                survived += 1;
            }
        }
        per_pass_max.push(worst);
    }
    let (env, conc, per_pass_max) = (sorted(env), sorted(conc), sorted(per_pass_max));
    println!("   {tested} candidates from {records} noise records");
    println!("     quantile   envelope   concentration");
    for q in [0.5, 0.9, 0.99, 0.999, 0.9999] {
        println!("     p{:<8.4} {:8.2} {:14.3}", q, pct(&env, q), pct(&conc, q));
    }
    println!("     max      {:8.2} {:14.3}", env[env.len() - 1], conc[conc.len() - 1]);
    println!("   per-pass max envelope: p50 {:.2}  p99 {:.2}  max {:.2}",
        pct(&per_pass_max, 0.5), pct(&per_pass_max, 0.99), per_pass_max[per_pass_max.len() - 1]);
    println!("   -> {survived} of {tested} cleared BOTH stages");

    // ---- B: sensitivity ----
    println!("\n== B. sensitivity: real {} vs SNR, 30 trials ==", mode.name());
    println!("   SNR(2.5k)  med env   med conc   stage1+2   +stage3");
    for snr_db in [-15.0f32, -12.0, -10.0, -8.0, -6.0, -3.0, 0.0, 6.0] {
        let trials = 30;
        let (mut es, mut cs) = (Vec::new(), Vec::new());
        let (mut hit12, mut hit3) = (0, 0);
        for t in 0..trials {
            let hz = 600.0 + (t as f64 * 137.0) % 1600.0 + 1.7;
            let one = psk::encode(
                &"cq cq de w1aw w1aw k the quick brown fox ".repeat(40),
                hz,
                mode,
            );
            let buf = over_noise(&one, snr_db, n, 0xC0DE_0000 + t as u64);
            let block = Block::new(&buf, 0, mode);
            let near = (hz / step).round() * step;
            let (s, c) = block.screen(near);
            es.push(s);
            cs.push(c);
            if s >= psk::modem::SCREEN_THRESHOLD && c >= psk::modem::CONC_THRESHOLD {
                hit12 += 1;
                let f = block.framing(near);
                if f.words >= psk::modem::MIN_WORDS
                    && f.tight_ratio() >= psk::modem::TIGHT_RATIO_MIN
                    && f.valid_ratio() >= psk::modem::VALID_RATIO_MIN
                {
                    hit3 += 1;
                }
            }
        }
        let (es, cs) = (sorted(es), sorted(cs));
        println!(
            "   {:>6.1} dB  {:>8.2} {:>10.3}   {:>6.0}%   {:>6.0}%",
            snr_db,
            es[es.len() / 2],
            cs[cs.len() / 2],
            100.0 * hit12 as f64 / trials as f64,
            100.0 * hit3 as f64 / trials as f64
        );
    }
    // ---- C: frequency tolerance ----
    println!("\n== C. frequency tolerance: envelope score vs candidate offset, -6 dB ==");
    let hz = 1500.0;
    let one = psk::encode(&"cq cq de w1aw k the quick brown fox ".repeat(40), hz, mode);
    let buf = over_noise(&one, -6.0, n, 0x5EED);
    let block = Block::new(&buf, 0, mode);
    let offsets: Vec<f64> = (0..=8).map(|i| i as f64 * mode.slice_hz() / 12.5).collect();
    print!("   offset Hz:");
    for o in &offsets {
        print!("{o:>8.0}");
    }
    println!();
    print!("   score    :");
    for o in &offsets {
        print!("{:>8.1}", block.screen(hz + o).0);
    }
    println!();

    // ---- D + E: discrimination and framing ----
    println!("\n== D/E. other modes at -6 dB and at +20 dB, scored at their centre ==");
    println!(
        "   {:<20} {:>9} {:>7} {:>6} {:>7} {:>7}   {}",
        "signal", "envelope", "conc", "words", "tight", "valid", "verdict"
    );

    let show = |name: &str, sig: &[f32], hz: f64, snr: f32| {
        let buf = over_noise(sig, snr, n, 0xA11CE);
        let block = Block::new(&buf, 0, mode);
        let (s, c) = block.screen(hz);
        let pass12 = s >= psk::modem::SCREEN_THRESHOLD && c >= psk::modem::CONC_THRESHOLD;
        let f = block.framing(hz);
        let pass3 = f.words >= psk::modem::MIN_WORDS
            && f.tight_ratio() >= psk::modem::TIGHT_RATIO_MIN
            && f.valid_ratio() >= psk::modem::VALID_RATIO_MIN;
        let verdict = match (pass12, pass3) {
            (false, _) => "rejected at stage 1/2",
            (true, false) => "rejected by framing",
            (true, true) => "PASSES ALL THREE",
        };
        println!(
            "   {name:<20} {s:>9.1} {c:>7.3} {:>6} {:>7.3} {:>7.3}   {verdict}",
            f.words,
            f.tight_ratio(),
            f.valid_ratio()
        );
    };

    for snr in [-6.0f32, 20.0] {
        println!("   --- at {snr:+.0} dB ---");
        show(&format!("{} (real)", mode.name()), &psk_signal(mode), 1500.0, snr);
        for other in [PSK31, PSK63] {
            if other != mode {
                show(&format!("{} (other PSK)", other.name()), &psk_signal(other), 1500.0, snr);
            }
        }
        for (name, m) in [
            ("Olivia 4/125", olivia::OL_4_125),
            ("Olivia 8/250", olivia::OL_8_250),
            ("Olivia 16/500", olivia::OL_16_500),
            ("Olivia 32/1000", olivia::OL_32_1000),
            ("Olivia 8/500", olivia::OL_8_500),
            ("Olivia 16/1000", olivia::OL_16_1000),
        ] {
            let sig = fill(olivia::encode("CQ CQ DE W1AW W1AW K ", 1500.0, m), n);
            show(name, &sig, 1500.0, snr);
        }
        for jm in [js8::Mode::Normal, js8::Mode::Turbo] {
            let (a87, _) = js8::message::freetext("CQ DE W1AW", js8::message::IType::None);
            let sig = fill(js8::modem::encode_audio(&a87, 1500.0), n);
            show(&format!("JS8 {}", jm.name()), &sig, 1500.0, snr);
        }
    }

    // ---- F: cost ----
    println!("\n== F. cost of one screening pass over the band ==");
    let grid: Vec<f64> = {
        let count = ((BAND.1 - BAND.0) / step) as usize;
        (0..=count).map(|i| BAND.0 + i as f64 * step).collect()
    };
    let audio = noise(n, 0xF00D);
    let t0 = std::time::Instant::now();
    let reps = 20;
    for _ in 0..reps {
        let block = Block::new(&audio, 0, mode);
        for &hz in &grid {
            std::hint::black_box(block.screen(hz));
        }
    }
    println!(
        "   {:>3} candidates on a {:.0} Hz grid: {:.1} ms per block ({:.1} ms per second of audio)",
        grid.len(),
        step,
        t0.elapsed().as_secs_f64() * 1000.0 / reps as f64,
        t0.elapsed().as_secs_f64() * 1000.0 / reps as f64 / mode.block_secs(),
    );
}
