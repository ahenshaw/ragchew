//! Headless validation of Milestones 2–3: synthesize a multi-station band
//! (with close pairs to force row nudging), track channels, lay out text rows,
//! and composite waterfall + connector strip + leader lines + text bars to PNG.
//!
//!   cargo run -p js8-gui --no-default-features --example scene_png -- out.png

use std::fs::File;
use std::io::BufWriter;

use js8::{modem, Spectrogram};
use js8_gui::channels::{ChannelSet, Decode};
use js8_gui::scene::{self, Geometry};
use js8_gui::waterfall::{self, Viewport};

const RATE: usize = 12000;

const FRAME_SECS: f64 = 79.0 * 1920.0 / 12000.0; // 12.64 s

fn main() {
    // args: [out.png] [t_now_secs]. With t_now, render the streaming state at
    // that playback time (windowed waterfall + only-revealed channels).
    let out = std::env::args().nth(1).unwrap_or_else(|| "scene.png".to_string());
    let t_now: Option<f64> = std::env::args().nth(2).and_then(|s| s.parse().ok());
    let samples = js8_gui::demo::synth();

    // decode all submodes; reveal frames whose audio has finished by t_now
    let decodes = modem::decode_all_multi(&samples, 300.0, 2600.0, 30);
    eprintln!("decoded {} frames", decodes.len());
    let set = ChannelSet::from_decodes(
        decodes
            .iter()
            .filter(|r| {
                let reveal = r.offset as f64 / RATE as f64 + FRAME_SECS;
                t_now.map_or(true, |t| reveal <= t)
            })
            .map(|r| Decode {
                hz: r.hz,
                time_s: r.offset as f64 / RATE as f64,
                text: js8::message::unpack(&r.a87).text(),
                mode: r.mode,
            }),
        15.0,
    );
    eprintln!("at t={:?}: {} channels", t_now, set.channels().len());
    for c in set.channels() {
        eprintln!("  {:7.1} Hz  [{:6}]  {:?}", c.hz, c.mode.name(), c.text);
    }

    // geometry + view
    let g = Geometry { width: 1200.0, height: 760.0, waterfall_w: 240.0, strip_w: 32.0 };
    let vp = Viewport { f_lo: 0.0, f_hi: 4000.0 };
    let (w, h) = (g.width as usize, g.height as usize);
    let mut canvas = vec![10u8, 12, 16, 255].repeat(w * h); // dark bg (RGBA)

    // waterfall into the right region (windowed if t_now given)
    let spec = Spectrogram::compute(&samples, vp.f_lo, vp.f_hi, modem::SAMPLES_PER_SYMBOL / 4);
    let norm = waterfall::percentiles(&spec);
    let wf = match t_now {
        Some(t) => {
            let hop = spec.hop as f64;
            let col_hi = (t * RATE as f64 / hop).round() as i64;
            let span_cols = (45.0 * RATE as f64 / hop).round() as usize;
            waterfall::render_window(&spec, &vp, g.waterfall_w as usize, h, col_hi, span_cols, Some(norm))
        }
        None => waterfall::render(&spec, &vp, g.waterfall_w as usize, h),
    };
    blit(&mut canvas, w, h, &wf, g.waterfall_x0() as usize, 0);

    // channel rows: nudge layout on ideal y from frequency
    let visible: Vec<&js8_gui::channels::Channel> =
        set.channels().iter().filter(|c| g.visible(&vp, c.hz)).collect();
    let ideals: Vec<f32> = visible.iter().map(|c| g.hz_to_y(&vp, c.hz)).collect();
    let line_h = 18.0;
    let rows = js8_gui::layout::place(&ideals, line_h, 10.0, g.height - 10.0);

    let palette = [
        [120u8, 200, 255],
        [255, 180, 120],
        [160, 255, 160],
        [255, 140, 200],
        [220, 220, 120],
    ];
    for (i, ch) in visible.iter().enumerate() {
        let color = palette[i % palette.len()];
        let y_row = rows[i];
        let y_freq = g.hz_to_y(&vp, ch.hz);

        // text bar in the text panel (length ~ text width), right end at strip
        let char_w = 6.0f32;
        let bar_w = (ch.text.chars().count() as f32 * char_w).min(g.strip_x0() - 12.0);
        let x1 = g.strip_x0() - 6.0;
        let x0 = (x1 - bar_w).max(2.0);
        fill_rect(&mut canvas, w, h, x0, y_row - 7.0, x1, y_row + 7.0, dim(color));
        // "now" end cap so newest side is obvious
        fill_rect(&mut canvas, w, h, x1 - 2.0, y_row - 7.0, x1, y_row + 7.0, color);

        // leader across the strip, then a dot at the true frequency (waterfall edge)
        let p = scene::leader(&g, y_row, y_freq);
        for seg in p.windows(2) {
            line(&mut canvas, w, h, seg[0], seg[1], color);
        }
        dot(&mut canvas, w, h, g.waterfall_x0(), y_freq, color);
    }

    // dividers
    vline(&mut canvas, w, h, g.strip_x0(), [60, 66, 76]);
    vline(&mut canvas, w, h, g.strip_x1(), [60, 66, 76]);

    let file = File::create(&out).expect("create png");
    let mut enc = png::Encoder::new(BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&canvas).unwrap();
    eprintln!("wrote {out} ({w}x{h})");
}

// ---- tiny raster helpers (RGBA) ----

fn dim(c: [u8; 3]) -> [u8; 3] {
    [c[0] / 3, c[1] / 3, c[2] / 3]
}
fn put(buf: &mut [u8], w: usize, h: usize, x: i32, y: i32, c: [u8; 3]) {
    if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
        let i = ((y as usize) * w + x as usize) * 4;
        buf[i] = c[0];
        buf[i + 1] = c[1];
        buf[i + 2] = c[2];
        buf[i + 3] = 255;
    }
}
fn blit(dst: &mut [u8], dst_w: usize, dst_h: usize, img: &waterfall::Image, ox: usize, oy: usize) {
    for y in 0..img.height {
        for x in 0..img.width {
            let s = (y * img.width + x) * 4;
            put(dst, dst_w, dst_h, (ox + x) as i32, (oy + y) as i32,
                [img.rgba[s], img.rgba[s + 1], img.rgba[s + 2]]);
        }
    }
}
fn fill_rect(buf: &mut [u8], w: usize, h: usize, x0: f32, y0: f32, x1: f32, y1: f32, c: [u8; 3]) {
    for y in y0.round() as i32..=y1.round() as i32 {
        for x in x0.round() as i32..=x1.round() as i32 {
            put(buf, w, h, x, y, c);
        }
    }
}
fn line(buf: &mut [u8], w: usize, h: usize, a: (f32, f32), b: (f32, f32), c: [u8; 3]) {
    let (mut x0, mut y0) = (a.0 as i32, a.1 as i32);
    let (x1, y1) = (b.0 as i32, b.1 as i32);
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        put(buf, w, h, x0, y0, c);
        put(buf, w, h, x0, y0 + 1, c); // 2px for visibility
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}
fn dot(buf: &mut [u8], w: usize, h: usize, x: f32, y: f32, c: [u8; 3]) {
    for dy in -2..=2 {
        for dx in -2..=2 {
            put(buf, w, h, x as i32 + dx, y as i32 + dy, c);
        }
    }
}
fn vline(buf: &mut [u8], w: usize, h: usize, x: f32, c: [u8; 3]) {
    for y in 0..h {
        put(buf, w, h, x as i32, y as i32, c);
    }
}
