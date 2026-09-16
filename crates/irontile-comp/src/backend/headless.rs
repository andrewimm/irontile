//! The headless backend: no renderer, displays described on the command line.
//!
//! This exists so irontile can be driven entirely through its control socket,
//! with as many displays as a test cares to ask for. It is the only way to
//! exercise arrangement, desktop transfer and hotplug without the hardware to
//! do it on.

use std::time::Duration;

use anyhow::Context as _;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::Display;

use crate::backend::Options;
use crate::ipc;
use crate::state::{Irontile, OutputSpec};

/// How often pending layout changes are pushed out to clients. Nothing is
/// drawn, so this only bounds how stale a client's configure can be.
const TICK: Duration = Duration::from_millis(16);

pub fn run(outputs: Vec<OutputSpec>, options: Options) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<Irontile> = EventLoop::try_new()?;
    let display: Display<Irontile> = Display::new()?;
    let display_handle = display.handle();

    let socket = super::bind_socket(options.wayland_display.as_deref())?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();

    let mut state = Irontile::new(
        display_handle,
        socket_name.clone(),
        options.config,
        options.config_path,
        event_loop.handle(),
    );
    state.add_keyboard()?;
    state.seat.add_pointer();
    state.configure_outputs(&outputs);
    state.reflow();

    let handle = event_loop.handle();
    super::insert_wayland_sources(&handle, display, socket)?;
    let control =
        ipc::listen(&handle, &socket_name).context("failed to bind the control socket")?;

    handle
        .insert_source(Timer::immediate(), move |_, _, state: &mut Irontile| {
            if !state.running {
                return TimeoutAction::Drop;
            }
            if state.dirty {
                state.reflow();
            }
            // Clients still expect their frame callbacks to come back, even
            // with nothing on screen, or they will never draw again.
            state.send_frame_callbacks();
            TimeoutAction::ToDuration(TICK)
        })
        .map_err(|e| anyhow::anyhow!("failed to insert the tick source: {e}"))?;

    tracing::info!(
        socket = %socket_name,
        control = %control.path().display(),
        displays = outputs.len(),
        "irontile is running headless"
    );
    state.run_startup_commands();

    let signal = event_loop.get_signal();
    event_loop.run(Some(TICK), &mut state, move |state| {
        if !state.running {
            signal.stop();
            return;
        }
        state.popups.cleanup();
        state.notice_a_dead_lock();
        if let Err(err) = state.display_handle.flush_clients() {
            tracing::warn!(%err, "failed to flush clients");
        }
    })?;

    Ok(())
}
