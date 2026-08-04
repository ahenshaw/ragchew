//! Write the application icon to a PNG, at any size.
//!
//!   cargo run -p ragchew-gui --no-default-features --example icon_png -- out.png 512
//!
//! The drawing itself lives in `ragchew_gui::icon`, which is also what the
//! window icon is built from, so what this writes is what the app shows. Here
//! for producing the files a desktop entry wants, and for looking at it.

use std::fs::File;
use std::io::BufWriter;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let out = a.get(1).cloned().unwrap_or_else(|| "icon.png".to_string());
    let size: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(512);

    let rgba = ragchew_gui::icon::rgba(size);
    let file = File::create(&out).expect("create png");
    let mut enc = png::Encoder::new(BufWriter::new(file), size as u32, size as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&rgba).unwrap();
    eprintln!("wrote {out} ({size}x{size})");
}
