//! Write the application icon to a Windows `.ico`.
//!
//!   cargo run -p ragchew-gui --no-default-features --example icon_ico -- gui/assets/ragchew.ico
//!
//! The drawing lives in `ragchew_gui::icon`, which is also where the window
//! icon and the desktop entry's PNGs come from, so what this writes is what the
//! app shows. `build.rs` compiles the result into `ragchew.exe`; regenerate it
//! here whenever the drawing changes, which `icon::tests` will insist on.

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "ragchew.ico".to_string());
    let bytes = ragchew_gui::icon::ico();
    std::fs::write(&out, &bytes).expect("write ico");
    eprintln!(
        "wrote {out} ({} KB, {} sizes: {:?})",
        bytes.len() / 1024,
        ragchew_gui::icon::ICO_SIZES.len(),
        ragchew_gui::icon::ICO_SIZES
    );
}
