//! The irontile compositor.
//!
//! The binary owns every Wayland and rendering concern and holds no layout
//! policy of its own: protocol events become [`irontile_layout`] commands, and
//! the frame that comes back becomes surface configures and render elements.

mod backend;
mod input;
mod keymap;
mod registry;
mod render;
mod shell;
mod state;
mod theme;

fn main() -> anyhow::Result<()> {
    init_tracing();
    backend::nested::run()
}

fn init_tracing() {
    use std::io::IsTerminal;
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        // Escape codes in a redirected log make it unreadable and unparseable.
        .with_ansi(std::io::stdout().is_terminal())
        .init();
}
