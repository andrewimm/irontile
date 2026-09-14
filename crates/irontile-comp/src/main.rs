//! The irontile compositor.
//!
//! This binary owns every Wayland and rendering concern. It holds no layout
//! policy of its own: it translates protocol events into [`irontile_layout`]
//! commands and turns the resulting [`Frame`] back into surface configures.
//!
//! [`Frame`]: irontile_layout::Frame

fn main() {
    // TODO: bring up the Smithay backend, seat, and output handling, then drive
    // `irontile_layout::dispatch` from protocol events.
    eprintln!(
        "irontile {}: compositor backend is not implemented yet",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(1);
}
