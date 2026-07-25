fn main() {
    #[cfg(feature = "desktop")]
    {
        if let Err(e) = js8_gui::app::run() {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
    #[cfg(not(feature = "desktop"))]
    {
        eprintln!("js8-gui was built without the `desktop` feature; nothing to run.");
    }
}
