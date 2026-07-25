//! Headless validation: render a recording's waterfall to a PNG and overlay a
//! marker at every frame the decoder found, so the visualization can be checked
//! without a display.
//!
//!   cargo run -p js8-gui --no-default-features --example render_png -- \
//!       ../tests/vectors/john_3_16.wav out.png

use std::fs::File;
use std::io::BufWriter;

use js8::{modem, wav, Spectrogram};
use js8_gui::waterfall::{self, Viewport};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let wav_path = args.get(1).map(|s| s.as_str()).unwrap_or("tests/vectors/john_3_16.wav");
    let out = args.get(2).map(|s| s.as_str()).unwrap_or("waterfall.png");

    let (samples, rate) = wav::read(wav_path).expect("read wav");
    assert_eq!(rate, modem::SAMPLE_RATE);

    // full-band view, ~4 columns per symbol time resolution
    let spec = Spectrogram::compute(&samples, 0.0, 4000.0, modem::SAMPLES_PER_SYMBOL / 4);
    let vp = Viewport { f_lo: 0.0, f_hi: 4000.0 };
    let (w, h) = (1200usize, 760usize);
    let mut img = waterfall::render(&spec, &vp, w, h);

    // overlay a small green cross at each decoded frame (freq, start time)
    let decodes = modem::decode_all(&samples, 300.0, 2600.0, 30);
    eprintln!("decoded {} frames", decodes.len());
    for d in &decodes {
        if let Some((x, y)) = waterfall::map_point(&spec, &vp, w, h, d.hz, d.offset) {
            cross(&mut img.rgba, w, h, x, y);
        }
    }

    let file = File::create(out).expect("create png");
    let mut enc = png::Encoder::new(BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&img.rgba).unwrap();
    eprintln!("wrote {out} ({w}x{h})");
}

fn cross(rgba: &mut [u8], w: usize, h: usize, cx: usize, cy: usize) {
    let green = [80u8, 255, 120];
    for d in -4i32..=4 {
        for &(x, y) in &[(cx as i32 + d, cy as i32), (cx as i32, cy as i32 + d)] {
            if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                let i = ((y as usize) * w + x as usize) * 4;
                rgba[i] = green[0];
                rgba[i + 1] = green[1];
                rgba[i + 2] = green[2];
                rgba[i + 3] = 255;
            }
        }
    }
}
