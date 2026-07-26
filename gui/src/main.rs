fn main() {
    #[cfg(feature = "desktop")]
    {
        if let Err(e) = ragchew_gui::app::run() {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
    #[cfg(not(feature = "desktop"))]
    {
        eprintln!("ragchew was built without the `desktop` feature; nothing to run.");
    }
}
