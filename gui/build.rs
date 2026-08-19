//! Windows resources: the icon and the version block compiled into the
//! executable.
//!
//! Explorer, the taskbar and Alt-Tab read an application's icon out of the
//! executable's resources, not from the application — a window icon set at
//! runtime, which is what `icon.rs` is for, arrives far too late for any of
//! them. So the same drawing goes in twice, once as pixels the app paints and
//! once as a resource the shell reads, and `gui/assets/ragchew.ico` is the
//! second of those. A test in `icon.rs` holds the file to what the code draws.
//!
//! Everything here is a no-op off Windows, and the crate that does the work is
//! a build dependency there only. Cross-compiling *to* Windows from something
//! else skips it too, quietly: the resource compiler is the host's, and the
//! dependency is gated on the host having one. The release build is native on
//! a Windows runner, which is the case this is for.

fn main() {
    println!("cargo:rerun-if-changed=assets/ragchew.ico");
    println!("cargo:rerun-if-changed=build.rs");

    // The *target*, not the host: a build script runs on the machine doing the
    // building, so `cfg!(windows)` here answers the wrong question and answers
    // it wrongly when cross-compiling.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/ragchew.ico");
        // What the file's Properties dialog shows. The version comes from the
        // manifest through Cargo's own environment, so it cannot disagree with
        // the number the settings menu prints.
        res.set("ProductName", "ragchew");
        res.set("FileDescription", "ragchew — panoramic multi-protocol monitor");
        res.set("LegalCopyright", "GPL-3.0-or-later");
        if let Err(e) = res.compile() {
            // Not fatal: an executable without an icon is a working executable,
            // and a resource compiler missing from a machine is not a reason to
            // refuse to build the radio software on it.
            println!("cargo:warning=windows resources not compiled: {e}");
        }
    }
}
