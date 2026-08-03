//! What the routing submenu would offer, printed. Headless check of the parse
//! against a live daemon.
fn main() {
    let r = ragchew_gui::pipewire::survey();
    println!("routing available from this process: {}", r.available);
    for s in &r.sources {
        println!("  node {:>4}  {}", s.node, s.label());
    }
    println!("currently feeding this process: {:?}", r.current.map(|s| s.label()));
}
