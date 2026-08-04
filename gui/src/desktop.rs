//! Installing a launcher entry, so the app can be started like an application
//! rather than from the directory it was built in.
//!
//! Done by the app itself rather than by a script, for one reason that matters:
//! a `.desktop` file has to name the binary by absolute path, and the only
//! thing that reliably knows where this binary is, is this binary. A script
//! would have to be told, and would be wrong the first time the build directory
//! moved.
//!
//! Everything lands under `$XDG_DATA_HOME` (or `~/.local/share`), which needs
//! no privileges and is searched before the system directories, so this cannot
//! collide with a packaged copy and does not need to be undone before one is
//! installed.

use std::path::{Path, PathBuf};

/// The name the entry, the icon and the window class all share.
///
/// One string, because a desktop matches a running window to its launcher by
/// comparing the window's class against `StartupWMClass` — get them out of step
/// and the taskbar shows a second, iconless entry for a running app. This is
/// the same id [`crate::app::run`] gives the viewport.
pub const ID: &str = "ragchew";

/// Icon sizes installed into the theme.
///
/// The set a desktop actually asks for: menus and taskbars want the small ones,
/// task switchers and the "about" dialogue the large. Each is drawn at its own
/// size rather than scaled from one, which is why they are cheap to offer.
pub const SIZES: [usize; 7] = [16, 24, 32, 48, 64, 128, 256];

/// Write the launcher entry and its icons under `root`, for a binary at `exec`.
///
/// `root` is the data directory — separated out so this can be tested somewhere
/// harmless. Returns what was written, in the order it was written.
pub fn install(root: &Path, exec: &Path) -> Result<Vec<PathBuf>, String> {
    let mut wrote = Vec::new();

    for size in SIZES {
        let dir = root.join(format!("icons/hicolor/{size}x{size}/apps"));
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(format!("{ID}.png"));
        write_png(&path, size).map_err(|e| format!("{}: {e}", path.display()))?;
        wrote.push(path);
    }

    let dir = root.join("applications");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("{ID}.desktop"));
    std::fs::write(&path, entry(exec)).map_err(|e| format!("{}: {e}", path.display()))?;
    wrote.push(path);

    Ok(wrote)
}

/// The contents of the `.desktop` file.
///
/// Pure, so what goes in the file can be checked without writing one.
pub fn entry(exec: &Path) -> String {
    // Categories: `HamRadio` is an additional category and the spec requires a
    // main one alongside it, so `AudioVideo` leads and `Audio` narrows it.
    // Without a valid main category some menus file the app under nothing at
    // all and it simply never appears.
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.5\n\
         Name=ragchew\n\
         GenericName=Digital mode monitor\n\
         Comment=Panoramic waterfall for keyboard-to-keyboard amateur radio modes\n\
         Exec={exec}\n\
         Icon={ID}\n\
         Terminal=false\n\
         Categories=AudioVideo;Audio;HamRadio;\n\
         Keywords=amateur radio;ham;JS8;Olivia;PSK31;waterfall;spectrogram;\n\
         StartupWMClass={ID}\n",
        exec = exec.display(),
    )
}

/// Write one icon, at one size.
///
/// A minimal PNG writer rather than a dependency: this needs exactly one
/// colour type at one bit depth, written once at install time, and the crate
/// that would otherwise do it is already only a dev-dependency.
fn write_png(path: &Path, size: usize) -> std::io::Result<()> {
    let rgba = crate::icon::rgba(size);
    let mut raw = Vec::with_capacity(rgba.len() + size);
    for row in rgba.chunks_exact(size * 4) {
        raw.push(0u8); // filter: none
        raw.extend_from_slice(row);
    }

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(size as u32).to_be_bytes());
    ihdr.extend_from_slice(&(size as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, no filter, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &deflate_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    std::fs::write(path, out)
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let at = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    // The CRC covers the type and the data together, which is exactly the range
    // just appended.
    let crc = crc32(&out[at..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// A zlib stream of stored (uncompressed) deflate blocks.
///
/// An icon is a few hundred kilobytes uncompressed and is written once, so the
/// compressor is not worth having. Every PNG reader handles stored blocks —
/// they are the format's own escape hatch for incompressible data.
fn deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01]; // zlib header: deflate, 32K window, no dict
    for (i, block) in data.chunks(65_535).enumerate() {
        let last = (i + 1) * 65_535 >= data.len();
        out.push(u8::from(last));
        let n = block.len() as u16;
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&(!n).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &x in data {
        c ^= x as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    c ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window class in the entry has to be the id the app gives its
    /// viewport, or the desktop cannot tell that the window on screen belongs
    /// to the launcher that started it — and shows a second entry, without an
    /// icon, beside the first.
    #[test]
    fn the_entry_names_the_binary_and_matches_the_window_class() {
        let text = entry(Path::new("/home/andy/repos/ragchew/target/release/ragchew"));
        assert!(
            text.contains("Exec=/home/andy/repos/ragchew/target/release/ragchew\n"),
            "the binary is not named by absolute path: {text}"
        );
        assert!(text.contains(&format!("StartupWMClass={ID}\n")));
        assert!(text.contains(&format!("Icon={ID}\n")));
        assert!(text.starts_with("[Desktop Entry]\n"), "the group header must come first");
        assert!(text.ends_with('\n'), "a key/value file ends its last line");
    }

    /// `HamRadio` is an additional category, and the specification requires a
    /// main one alongside it. Without a valid main category some menus file the
    /// app under nothing at all, and it never appears.
    #[test]
    fn the_categories_include_a_main_one() {
        let text = entry(Path::new("/usr/bin/ragchew"));
        let line = text
            .lines()
            .find(|l| l.starts_with("Categories="))
            .expect("an entry with no categories is filed nowhere");
        let cats: Vec<&str> = line["Categories=".len()..].split(';').collect();
        const MAIN: [&str; 5] = ["AudioVideo", "Network", "Utility", "Science", "Education"];
        assert!(
            cats.iter().any(|c| MAIN.contains(c)),
            "{line} has no main category, so menus may drop it"
        );
        assert!(cats.contains(&"HamRadio"), "{line} should say what this is");
        assert!(line.ends_with(';'), "the spec wants a trailing semicolon: {line}");
    }

    /// Everything lands where a desktop looks for it, and the icons are real
    /// PNGs of the size their directory claims.
    #[test]
    fn install_writes_an_entry_and_a_full_set_of_icons() {
        let root = std::env::temp_dir().join(format!("ragchew-desktop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let exec = Path::new("/opt/ragchew/bin/ragchew");
        let wrote = install(&root, exec).expect("install");

        assert_eq!(wrote.len(), SIZES.len() + 1, "wrote {wrote:?}");
        let entry = root.join("applications/ragchew.desktop");
        assert!(entry.is_file(), "no launcher entry at {}", entry.display());
        assert!(std::fs::read_to_string(&entry).unwrap().contains("Exec=/opt/ragchew/bin/ragchew"));

        for size in SIZES {
            let p = root.join(format!("icons/hicolor/{size}x{size}/apps/ragchew.png"));
            let bytes = std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{} is not a PNG", p.display());
            // The width and height in the header, which is what an icon theme
            // trusts — a file in the 48x48 directory that is not 48 square is
            // silently ignored by some themes and stretched by others.
            let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
            let h = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
            assert_eq!((w, h), (size as u32, size as u32), "{} is {w}x{h}", p.display());
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The PNGs must be readable by something other than the code that wrote
    /// them — a hand-rolled writer that only satisfies itself is worth nothing.
    #[test]
    fn the_icons_decode_as_written() {
        let root = std::env::temp_dir().join(format!("ragchew-png-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        install(&root, Path::new("/usr/bin/ragchew")).expect("install");

        let p = root.join("icons/hicolor/32x32/apps/ragchew.png");
        let file = std::fs::File::open(&p).unwrap();
        let decoder = png::Decoder::new(file);
        let mut reader = decoder.read_info().expect("a PNG decoder should read this");
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).expect("and decode its pixels");
        assert_eq!((info.width, info.height), (32, 32));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(&buf[..info.buffer_size()], &crate::icon::rgba(32)[..], "pixels changed");

        let _ = std::fs::remove_dir_all(&root);
    }
}
