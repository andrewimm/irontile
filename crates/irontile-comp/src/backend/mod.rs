//! Backends.
//!
//! Each backend owns a renderer, a set of displays, and an input source, and
//! drives the same [`Irontile`] state. The nested backend runs irontile as a
//! window inside another compositor, which is the development loop; the
//! headless one runs it with no renderer at all, driven over the control
//! socket. A session backend on DRM will slot in alongside them.

pub mod headless;
pub mod nested;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::wayland::socket::ListeningSocketSource;

use crate::state::{ClientState, Irontile};

/// Wires the Wayland listening socket and the client display into the loop.
///
/// Shared because every backend needs exactly this, and getting the display
/// source wrong is the kind of mistake that only shows up as clients hanging.
pub fn insert_wayland_sources(
    handle: &LoopHandle<'static, Irontile>,
    display: Display<Irontile>,
    socket: ListeningSocketSource,
) -> anyhow::Result<()> {
    handle
        .insert_source(socket, move |stream, _, state: &mut Irontile| {
            if let Err(err) = state
                .display_handle
                .insert_client(stream, std::sync::Arc::new(ClientState::default()))
            {
                tracing::warn!(%err, "failed to accept a client");
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert the socket source: {e}"))?;

    handle
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state: &mut Irontile| {
                // SAFETY: the display is owned by this source for the lifetime
                // of the event loop and is never moved out of it, so the
                // reference handed back is valid for the duration of the call.
                #[allow(unsafe_code)]
                let display = unsafe { display.get_mut() };
                display.dispatch_clients(state)?;
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert the display source: {e}"))?;

    Ok(())
}
