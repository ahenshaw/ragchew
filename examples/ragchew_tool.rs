//! Command-line front end for the `ragchew` codec library.
//!
//!   cargo run --release --example ragchew_tool -- decode in.wav [hz_lo hz_hi]
//!   cargo run --release --example ragchew_tool -- encode <mode> "TEXT" out.wav [hz]
//!   cargo run --release --example ragchew_tool -- modes
//!
//! `decode` scans a band for every mode at once and prints what it finds.
//! `encode` writes a 12 kHz mono WAV: a JS8 frame starts 0.5 s into a nominal
//! cycle, while an Olivia transmission simply runs for as long as the text takes.

use ragchew::js8::message::{self, IType};
use ragchew::js8::modem;
use ragchew::olivia;
use ragchew::protocol::{self, ModeId};
use ragchew::{wav, SAMPLE_RATE};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("encode") => encode(&args),
        Some("decode") => decode(&args),
        Some("modes") => modes(),
        _ => {
            eprintln!("usage:");
            eprintln!("  ragchew_tool decode in.wav [hz_lo hz_hi]");
            eprintln!("  ragchew_tool encode <mode> \"TEXT\" out.wav [hz]");
            eprintln!("  ragchew_tool modes");
            std::process::exit(2);
        }
    }
}

/// Accepts `js8`, `js8:fast`, `olivia:16/500`, or a bare `16/500`.
fn parse_mode(s: &str) -> Option<ModeId> {
    let lower = s.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("js8") {
        use ragchew::js8::Mode::*;
        return Some(ModeId::Js8(match rest.trim_start_matches(':') {
            "" | "normal" => Normal,
            "slow" => Slow,
            "fast" => Fast,
            "turbo" => Turbo,
            _ => return None,
        }));
    }
    olivia::parse(&lower).map(ModeId::Olivia)
}

fn modes() {
    println!("{:<16} {:>12} {:>9}  timing", "mode", "bandwidth", "chars/s");
    for m in protocol::default_modes() {
        let timing = match m.period_s() {
            Some(p) => format!("{p} s cycle"),
            None => "continuous".to_string(),
        };
        let cps = match m {
            ModeId::Olivia(o) => format!("{:.2}", o.chars_per_sec()),
            ModeId::Js8(_) => "—".to_string(),
        };
        println!("{:<16} {:>9.0} Hz {:>9}  {timing}", m.name(), m.bandwidth_hz(), cps);
    }
}

fn encode(args: &[String]) {
    let Some(mode) = args.get(2).and_then(|s| parse_mode(s)) else {
        eprintln!("unknown mode {:?} — try `ragchew_tool modes`", args.get(2));
        std::process::exit(2);
    };
    let text = args.get(3).map(|s| s.as_str()).unwrap_or("");
    let out = args.get(4).map(|s| s.as_str()).unwrap_or("out.wav");
    let hz: f64 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(1500.0);

    let audio = match mode {
        ModeId::Js8(m) => {
            let sm = ragchew::js8::submode::of(m);
            let (a87, consumed) = message::freetext(text, IType::None);
            if consumed < text.chars().count() {
                eprintln!("note: only {consumed} chars fit in one JS8 frame");
            }
            // one nominal cycle: 0.5 s of silence, the frame, then silence
            let mut audio = vec![0.0f32; SAMPLE_RATE as usize / 2];
            audio.extend(modem::encode_audio_sm(&a87, hz, &sm));
            audio.resize(sm.period_s as usize * SAMPLE_RATE as usize, 0.0);
            audio
        }
        ModeId::Olivia(m) => olivia::encode(text, hz, m),
    };

    wav::write(out, &audio, SAMPLE_RATE).expect("write wav");
    println!(
        "wrote {out}: {} @ {hz} Hz, {:.1} s",
        mode.name(),
        audio.len() as f64 / SAMPLE_RATE as f64
    );
}

fn decode(args: &[String]) {
    let inp = args.get(2).map(|s| s.as_str()).unwrap_or("in.wav");
    let hz_lo: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(300.0);
    let hz_hi: f64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(2600.0);

    let (samples, rate) = wav::read(inp).expect("read wav");
    if rate != SAMPLE_RATE {
        eprintln!("warning: expected {SAMPLE_RATE} Hz, got {rate} Hz");
    }

    let modes = protocol::default_modes();
    let results = protocol::decode_all(&samples, hz_lo, hz_hi, &modes);
    if results.is_empty() {
        println!("(nothing decoded in {hz_lo:.0}–{hz_hi:.0} Hz)");
    }
    for d in results {
        println!(
            "{:7.1} Hz  t={:7.2}s  {:>14}  q={:5.1}  {:?}",
            d.hz,
            d.time_s,
            d.mode.name(),
            d.quality,
            d.text
        );
    }
}
