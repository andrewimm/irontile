//! Backends.
//!
//! Each backend owns a renderer and a set of displays, and drives the same
//! [`Irontile`] state. The nested backend runs irontile as a window inside
//! another compositor, which is the development loop; the headless one runs it
//! with no renderer at all, driven over the control socket; the session backend
//! drives real hardware through DRM.
//!
//! The renderer lives here, inside the compositor state, rather than in the
//! backend's own event loop. That is what lets a client's dmabuf be imported at
//! the moment it is submitted: the import needs the renderer, and the protocol
//! handler only has the compositor.

pub mod headless;
pub mod nested;
pub mod session;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::renderer::ImportDma;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::winit::WinitGraphicsBackend;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::wayland::socket::ListeningSocketSource;

use crate::config::Config;
use crate::state::{ClientState, Irontile};

/// What every backend is given to start with.
pub struct Options {
    pub config: Config,
    pub config_path: std::path::PathBuf,
    /// A specific socket name to bind, rather than the first free one.
    pub wayland_display: Option<String>,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("config_path", &self.config_path)
            .field("wayland_display", &self.wayland_display)
            .finish_non_exhaustive()
    }
}

/// Binds the Wayland socket clients will connect to.
pub fn bind_socket(name: Option<&str>) -> anyhow::Result<ListeningSocketSource> {
    match name {
        Some(name) => ListeningSocketSource::with_name(name)
            .map_err(|e| anyhow::anyhow!("could not bind {name:?}: {e}")),
        None => ListeningSocketSource::new_auto()
            .map_err(|e| anyhow::anyhow!("could not bind a wayland socket: {e}")),
    }
}

/// Whatever is drawing, if anything is.
pub enum Backend {
    /// No renderer. Clients still map and are configured; nothing is drawn.
    Headless,
    Nested(Box<WinitGraphicsBackend<GlesRenderer>>),
    Session(Box<session::Session>),
}

impl Backend {
    pub fn renderer(&mut self) -> Option<&mut GlesRenderer> {
        match self {
            Backend::Headless => None,
            Backend::Nested(graphics) => Some(graphics.renderer()),
            Backend::Session(session) => session.renderer(),
        }
    }

    /// The buffer formats clients may hand over directly.
    pub fn dmabuf_formats(&mut self) -> Option<FormatSet> {
        self.renderer().map(|renderer| renderer.dmabuf_formats())
    }

    /// Asks the host to schedule another frame.
    ///
    /// Only the nested backend needs this: its frames are paced by the
    /// compositor it runs inside, which will not ask for one unless it intends
    /// to show it.
    pub fn request_redraw(&self) {
        if let Backend::Nested(graphics) = self {
            graphics.window().request_redraw();
        }
    }

    /// Switches to another virtual terminal. Only the session backend holds one.
    pub fn change_vt(&mut self, vt: i32) {
        match self {
            Backend::Session(session) => session.change_vt(vt),
            _ => tracing::debug!(vt, "not running on a virtual terminal"),
        }
    }

    /// The device clients should allocate dmabufs on, when one can be named.
    pub fn dmabuf_main_device(&self) -> Option<libc::dev_t> {
        match self {
            Backend::Session(session) => session.render_device(),
            // Nested, the buffers go to the host compositor, which names its
            // own device to its own clients.
            _ => None,
        }
    }

    /// Imports a client's buffer, reporting whether the renderer accepted it.
    ///
    /// Answering honestly matters: a client told its buffer was fine and then
    /// finding nothing drawn has no way to recover, whereas one told the import
    /// failed falls back to shared memory.
    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        match self.renderer() {
            Some(renderer) => renderer.import_dmabuf(dmabuf, None).is_ok(),
            None => false,
        }
    }
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Headless => write!(f, "Headless"),
            Backend::Nested(_) => write!(f, "Nested"),
            Backend::Session(_) => write!(f, "Session"),
        }
    }
}

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
            // Named here, while there is still a socket to ask.
            let client = ClientState::named(&stream);
            let stream2 = stream;
            if let Err(err) = state
                .display_handle
                .insert_client(stream2, std::sync::Arc::new(client))
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
