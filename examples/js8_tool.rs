//! Tiny command-line tool for the `js8` crate.
//!
//!   cargo run --release --example js8_tool -- encode "HELLO WORLD" out.wav [hz]
//!   cargo run --release --example js8_tool -- decode in.wav [hz_lo hz_hi]
//!
//! `encode` writes a 15-second, 12 kHz mono WAV with the burst starting 0.5 s
//! in (a nominal JS8 cycle). `decode` scans a band and prints every message
//! whose CRC checks out.

use js8::message::{self, IType};
use js8::{modem, wav};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("encode") => encode(&args),
        Some("decode") => decode(&args),
        _ => {
            eprintln!("usage:");
            eprintln!("  js8_tool encode \"TEXT\" out.wav [hz]");
            eprintln!("  js8_tool decode in.wav [hz_lo hz_hi]");
            std::process::exit(2);
        }
    }
}

fn encode(args: &[String]) {
    let text = args.get(2).map(|s| s.as_str()).unwrap_or("");
    let out = args.get(3).map(|s| s.as_str()).unwrap_or("out.wav");
    let hz: f64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1500.0);

    let (a87, consumed) = message::freetext(text, IType::None);
    let burst = modem::encode_audio(&a87, hz);
    let mut audio = vec![0.0f32; js8::CYCLE_LEAD_SAMPLES];
    audio.extend_from_slice(&burst);
    audio.resize(15 * modem::SAMPLE_RATE as usize, 0.0);
    wav::write(out, &audio, modem::SAMPLE_RATE).expect("write wav");

    if consumed < text.chars().count() {
        eprintln!("note: only {consumed} chars fit in one frame");
    }
    println!("wrote {out} @ {hz} Hz: {:?}", message::unpack(&a87).text());
}

fn decode(args: &[String]) {
    let inp = args.get(2).map(|s| s.as_str()).unwrap_or("in.wav");
    let hz_lo: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(300.0);
    let hz_hi: f64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(2600.0);

    let (samples, rate) = wav::read(inp).expect("read wav");
    if rate != modem::SAMPLE_RATE {
        eprintln!("warning: expected {} Hz, got {rate} Hz", modem::SAMPLE_RATE);
    }
    let results = modem::decode_all(&samples, hz_lo, hz_hi, 30);
    if results.is_empty() {
        println!("(no decodes in {hz_lo:.0}–{hz_hi:.0} Hz)");
    }
    for r in results {
        let secs = r.offset as f64 / modem::SAMPLE_RATE as f64;
        println!(
            "{:6.1} Hz  t={:4.2}s  sync={:.2}  {:?}",
            r.hz,
            secs,
            r.sync,
            message::unpack(&r.a87).text()
        );
    }
}
