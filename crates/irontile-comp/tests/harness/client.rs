//! A minimal Wayland client, for tests that need real windows.
//!
//! Deliberately hand-rolled against `wayland-client` rather than built on a
//! toolkit: a test wants to say exactly when a surface commits and exactly what
//! it commits, and a toolkit's own dispatch loop gets in the way of that.
//!
//! It maps `xdg_toplevel`s backed by a single shared-memory buffer. Nothing is
//! drawn into them; the compositor only needs a buffer to exist for a window to
//! count as mapped.

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_keyboard::{self, WlKeyboard},
    wl_registry,
    wl_seat::WlSeat,
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

const TIMEOUT: Duration = Duration::from_secs(10);

/// What the compositor told one window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Configured {
    pub width: i32,
    pub height: i32,
    pub activated: bool,
    pub fullscreen: bool,
    /// True when every edge was reported tiled.
    pub tiled: bool,
    /// What the compositor chose, once it has said.
    pub decoration: Option<zxdg_toplevel_decoration_v1::Mode>,
    /// The exact scale of the display, in 120ths, once the compositor has said.
    pub fractional_scale: Option<u32>,
    /// How many displays the surface has been told it is on.
    pub outputs: usize,
}

struct Window {
    surface: WlSurface,
    xdg_surface: XdgSurface,
    toplevel: XdgToplevel,
    decoration: Option<ZxdgToplevelDecorationV1>,
    fractional: Option<WpFractionalScaleV1>,
    buffer: WlBuffer,
    /// Pending values from the last configure, applied when it is acked.
    pending: Configured,
    current: Configured,
    configures: u32,
    closed: bool,
    mapped: bool,
}

#[derive(Default)]
struct Globals {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    wm_base: Option<XdgWmBase>,
    decoration_manager: Option<ZxdgDecorationManagerV1>,
    layer_shell: Option<ZwlrLayerShellV1>,
    fractional_scale: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    /// In the order the compositor advertised them, which is the order it holds
    /// its displays in.
    outputs: Vec<wayland_client::protocol::wl_output::WlOutput>,
    session_lock: Option<ExtSessionLockManagerV1>,
    screencopy: Option<ZwlrScreencopyManagerV1>,
    seat: Option<WlSeat>,
}

struct State {
    /// Every interface the compositor advertised, for tests that pin the
    /// protocol surface.
    advertised: Vec<String>,
    /// The surface the compositor last gave the keyboard to, if it is one of
    /// ours. This is how a test sees where focus actually went.
    keyboard_focus: Option<WlSurface>,
    keymap: Option<String>,
    /// Where the pointer is and on which of our surfaces, if any.
    pointer: Option<(WlSurface, (f64, f64))>,
    /// The session lock, while this client holds one.
    lock: Option<ExtSessionLockV1>,
    locks: Vec<LockPanel>,
    /// Set once the compositor confirms the session is locked.
    locked: bool,
    /// Set when the compositor refuses a lock.
    refused: bool,
    /// The shape of buffer a screen copy was offered: width, height, stride.
    offered: Option<(u32, u32, u32)>,
    /// Button codes pressed, in order.
    buttons: Vec<u32>,
    globals: Globals,
    windows: Vec<Window>,
    layers: Vec<LayerPanel>,
}

struct LayerPanel {
    surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    buffer: WlBuffer,
    /// Held for as long as the panel: dropping it stops the scale reports.
    #[allow(dead_code)]
    fractional: Option<WpFractionalScaleV1>,
    /// The exact scale of the display, in 120ths, once the compositor has said.
    fractional_scale: Option<u32>,
    configures: u32,
    /// How many configures had arrived when this panel last unmapped itself.
    unmapped_at: u32,
    /// Frame callbacks the compositor has released.
    frames: u32,
    mapped: bool,
}

/// One display's share of the lock screen.
struct LockPanel {
    surface: WlSurface,
    #[allow(dead_code)]
    shell: ExtSessionLockSurfaceV1,
    configured: bool,
    /// The size the compositor asked for. A lock surface whose buffer is any
    /// other size is a protocol error, which is the compositor making sure a
    /// lock screen really covers the display it claims to.
    size: (u32, u32),
}

/// Which panel a fractional-scale object belongs to.
///
/// A type of its own rather than a bare index, so it cannot be confused with
/// the one windows use.
#[derive(Debug, Clone, Copy)]
struct PanelScale(usize);

/// A handle to one mapped window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowId(usize);

/// A handle to one mapped layer surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerId(usize);

pub struct TestClient {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    pool: Option<(WlShmPool, WlBuffer)>,
}

impl TestClient {
    /// Connects to a compositor by its Wayland socket name.
    pub fn connect(socket: &str, runtime_dir: &std::path::Path) -> TestClient {
        // `WAYLAND_DISPLAY` is read from the environment by wayland-client, and
        // the tests must not disturb the environment of the process running
        // them, so the socket path is built here instead.
        let path = runtime_dir.join(socket);
        let stream = std::os::unix::net::UnixStream::connect(&path)
            .unwrap_or_else(|e| panic!("could not connect to {}: {e}", path.display()));
        let connection = Connection::from_socket(stream).expect("failed to start a connection");

        let display = connection.display();
        let queue = connection.new_event_queue();
        let handle = queue.handle();
        display.get_registry(&handle, ());

        let mut client = TestClient {
            connection,
            queue,
            state: State {
                advertised: Vec::new(),
                keyboard_focus: None,
                keymap: None,
                pointer: None,
                lock: None,
                locks: Vec::new(),
                locked: false,
                refused: false,
                offered: None,
                buttons: Vec::new(),
                globals: Globals::default(),
                windows: Vec::new(),
                layers: Vec::new(),
            },
            pool: None,
        };
        client.roundtrip();
        assert!(
            client.state.globals.compositor.is_some(),
            "compositor advertised no wl_compositor"
        );
        assert!(
            client.state.globals.shm.is_some(),
            "compositor advertised no wl_shm"
        );
        assert!(
            client.state.globals.wm_base.is_some(),
            "compositor advertised no xdg_wm_base"
        );
        client
    }

    pub fn has_decoration_manager(&self) -> bool {
        self.state.globals.decoration_manager.is_some()
    }

    pub fn has_layer_shell(&self) -> bool {
        self.state.globals.layer_shell.is_some()
    }

    /// Every interface the compositor advertised.
    pub fn advertised(&self) -> &[String] {
        &self.state.advertised
    }

    /// Maps a layer surface that asks for the keyboard, as a launcher does.
    pub fn map_launcher(&mut self, height: i32) -> LayerId {
        // Its own name: a namespace is how panels are told apart, both here
        // and by anything asking the compositor what is on screen.
        self.map_top_bar_inner(height, 0, true, None, "irontile-test-launcher")
    }

    /// Maps a layer surface anchored to the top of the display, reserving
    /// `exclusive` pixels — which is what a bar does.
    pub fn map_top_bar(&mut self, height: i32, exclusive: i32) -> LayerId {
        self.map_top_bar_inner(height, exclusive, false, None, "irontile-test-bar")
    }

    /// Maps a bar on a named display rather than letting the compositor choose.
    ///
    /// Displays are numbered in the order the compositor advertised them.
    pub fn map_top_bar_on(&mut self, display: usize, height: i32, exclusive: i32) -> LayerId {
        self.map_top_bar_inner(height, exclusive, false, Some(display), "irontile-test-bar")
    }

    /// How many displays the compositor has advertised.
    pub fn display_count(&mut self) -> usize {
        self.roundtrip();
        self.state.globals.outputs.len()
    }

    fn map_top_bar_inner(
        &mut self,
        height: i32,
        exclusive: i32,
        keyboard: bool,
        display: Option<usize>,
        namespace: &str,
    ) -> LayerId {
        let handle = self.queue.handle();
        let compositor = self
            .state
            .globals
            .compositor
            .clone()
            .expect("checked at connect");
        let layer_shell = self
            .state
            .globals
            .layer_shell
            .clone()
            .expect("compositor advertised no layer shell");

        let surface = compositor.create_surface(&handle, ());
        let index = self.state.layers.len();
        let output = display.map(|n| {
            self.state
                .globals
                .outputs
                .get(n)
                .unwrap_or_else(|| panic!("no display {n} was advertised"))
                .clone()
        });
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            output.as_ref(),
            zwlr_layer_shell_v1::Layer::Top,
            namespace.to_owned(),
            &handle,
            index,
        );
        layer_surface.set_size(0, height as u32);
        layer_surface.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        );
        layer_surface.set_exclusive_zone(exclusive);
        if keyboard {
            layer_surface.set_keyboard_interactivity(
                zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive,
            );
        }
        let fractional = self
            .state
            .globals
            .fractional_scale
            .clone()
            .map(|manager| manager.get_fractional_scale(&surface, &handle, PanelScale(index)));
        let buffer = self.buffer();
        surface.commit();

        self.state.layers.push(LayerPanel {
            surface,
            layer_surface,
            buffer,
            fractional,
            fractional_scale: None,
            configures: 0,
            unmapped_at: 0,
            frames: 0,
            mapped: false,
        });

        self.wait_until(
            |state| state.layers[index].configures > 0,
            "the layer surface's first configure",
        );

        let panel = &mut self.state.layers[index];
        panel.surface.attach(Some(&panel.buffer), 0, 0);
        panel.surface.damage(0, 0, i32::MAX, i32::MAX);
        panel.surface.commit();
        panel.mapped = true;
        self.roundtrip();

        LayerId(index)
    }

    /// Draws another frame on a layer surface, the way a bar does when the
    /// clock ticks.
    pub fn redraw_layer(&mut self, id: LayerId) {
        let panel = &mut self.state.layers[id.0];
        panel.surface.attach(Some(&panel.buffer), 0, 0);
        panel.surface.damage(0, 0, i32::MAX, i32::MAX);
        panel.surface.commit();
        self.roundtrip();
    }

    /// The keymap the compositor compiled and sent, as xkb text.
    pub fn keymap(&mut self) -> Option<String> {
        self.roundtrip();
        self.state.keymap.clone()
    }

    /// Where the pointer is, in surface-local coordinates, and on which
    /// surface -- or `None` if it is not on any of this client's.
    pub fn pointer_on(&mut self) -> Option<(WlSurface, (f64, f64))> {
        self.roundtrip();
        self.state.pointer.clone()
    }

    /// Whether the pointer is on this panel.
    pub fn pointer_on_layer(&mut self, id: LayerId) -> bool {
        let surface = self.state.layers[id.0].surface.clone();
        self.pointer_on().is_some_and(|(on, _)| on == surface)
    }

    /// Buttons pressed since the client connected, as `(code, surface)`.
    pub fn buttons(&mut self) -> Vec<u32> {
        self.roundtrip();
        self.state.buttons.clone()
    }

    /// Asks for a copy of a display and reports the buffer it is offered.
    ///
    /// Only the offer: filling it needs a renderer, and a headless compositor
    /// has none.
    pub fn ask_for_a_copy(&mut self, display: usize) -> Option<(u32, u32, u32)> {
        let handle = self.queue.handle();
        let manager = self.state.globals.screencopy.clone()?;
        let output = self.state.globals.outputs.get(display)?.clone();
        let _frame = manager.capture_output(0, &output, &handle, ());
        self.wait_until(
            |state| state.offered.is_some(),
            "an offer of a buffer to copy into",
        );
        self.state.offered
    }

    /// Locks the session and covers every display, the way a lock screen does.
    ///
    /// Returns once the compositor has said the session is locked, which it
    /// only does when every display is covered by a surface that has drawn.
    pub fn lock_session(&mut self) {
        let handle = self.queue.handle();
        let manager = self
            .state
            .globals
            .session_lock
            .clone()
            .expect("the compositor advertised no session lock");
        let compositor = self
            .state
            .globals
            .compositor
            .clone()
            .expect("checked at connect");
        let lock = manager.lock(&handle, ());

        let outputs = self.state.globals.outputs.clone();
        let buffer = self.buffer();
        for (index, output) in outputs.iter().enumerate() {
            let surface = compositor.create_surface(&handle, ());
            let shell = lock.get_lock_surface(&surface, output, &handle, index);
            self.state.locks.push(LockPanel {
                surface,
                shell,
                configured: false,
                size: (0, 0),
            });
        }
        // Each has to be configured before it may attach a buffer, and the
        // compositor only calls the session locked once they all have.
        let _ = buffer;
        let count = outputs.len();
        self.wait_until(
            |state| state.locks.iter().take(count).all(|lock| lock.configured),
            "a configure for every lock surface",
        );
        for index in 0..count {
            let (w, h) = self.state.locks[index].size;
            let covering = self.sized_buffer(w.max(1) as i32, h.max(1) as i32);
            let panel = &self.state.locks[index];
            panel.surface.attach(Some(&covering), 0, 0);
            panel.surface.damage(0, 0, i32::MAX, i32::MAX);
            panel.surface.commit();
        }
        self.state.lock = Some(lock);
        self.wait_until(|state| state.locked, "the session to be locked");
    }

    /// Asks to lock a session that may already be locked.
    ///
    /// Returns whether the compositor allowed it. A refusal arrives as
    /// `finished` rather than an error, so this waits for one or the other.
    pub fn try_lock_session(&mut self) -> bool {
        let handle = self.queue.handle();
        let Some(manager) = self.state.globals.session_lock.clone() else {
            return false;
        };
        let lock = manager.lock(&handle, ());
        self.state.lock = Some(lock);
        // A refusal is immediate; a success would need surfaces, which this
        // deliberately does not provide.
        for _ in 0..10 {
            self.roundtrip();
            if self.state.refused {
                return false;
            }
        }
        !self.state.refused
    }

    /// Unlocks it again.
    pub fn unlock_session(&mut self) {
        if let Some(lock) = self.state.lock.take() {
            lock.unlock_and_destroy();
        }
        self.state.locked = false;
        self.roundtrip();
    }

    /// Asks for a frame callback on a panel, the way a toolkit does before
    /// drawing its next frame.
    pub fn request_frame(&mut self, id: LayerId) {
        let handle = self.queue.handle();
        let panel = &mut self.state.layers[id.0];
        panel.surface.frame(&handle, id.0);
        panel.surface.commit();
        self.roundtrip();
    }

    /// How many frame callbacks this panel has been given.
    pub fn frames(&mut self, id: LayerId) -> u32 {
        self.roundtrip();
        self.state.layers[id.0].frames
    }

    /// The exact scale the compositor says this panel's display is at, in
    /// 120ths.
    pub fn layer_fractional_scale(&mut self, id: LayerId) -> Option<u32> {
        self.roundtrip();
        self.state.layers[id.0].fractional_scale
    }

    /// How many times the compositor has configured a layer surface.
    pub fn layer_configures(&mut self, id: LayerId) -> u32 {
        self.roundtrip();
        self.state.layers[id.0].configures
    }

    /// Hides a panel by attaching no buffer, the way one that can be toggled
    /// does, keeping the layer surface itself.
    pub fn unmap_layer(&mut self, id: LayerId) {
        let panel = &mut self.state.layers[id.0];
        // Noted before the unmap: the configure that lets it come back is the
        // one this unmap provokes, so counting from after would wait for a
        // second one that never comes.
        panel.unmapped_at = panel.configures;
        panel.surface.attach(None, 0, 0);
        panel.surface.commit();
        panel.mapped = false;
        self.roundtrip();
    }

    /// Shows it again. Layer-shell says an unmapped surface may not attach a
    /// buffer until it has been configured afresh, so this waits for that
    /// rather than attaching straight away.
    pub fn remap_layer(&mut self, id: LayerId) {
        // What the count was before the unmap, because the configure being
        // waited for is the one the unmap itself provokes.
        let before = self.state.layers[id.0].unmapped_at;
        self.wait_until(
            |state| state.layers[id.0].configures > before,
            "a configure after unmapping, without which a panel may never come back",
        );
        let panel = &mut self.state.layers[id.0];
        panel.surface.attach(Some(&panel.buffer), 0, 0);
        panel.surface.damage(0, 0, i32::MAX, i32::MAX);
        panel.surface.commit();
        panel.mapped = true;
        self.roundtrip();
    }

    /// Unmaps a layer surface, which should give its reserved space back.
    pub fn close_layer(&mut self, id: LayerId) {
        let panel = &mut self.state.layers[id.0];
        panel.layer_surface.destroy();
        panel.surface.destroy();
        panel.mapped = false;
        self.roundtrip();
    }

    pub fn roundtrip(&mut self) {
        self.queue
            .roundtrip(&mut self.state)
            .expect("wayland roundtrip failed");
    }

    /// Creates a toplevel, waits for its first configure, and gives it a
    /// buffer, which is the point at which the compositor treats it as mapped.
    pub fn map_window(&mut self, title: &str) -> WindowId {
        let id = self.create_toplevel_without_buffer(title);
        self.attach_buffer(id);
        id
    }

    /// Declares the part of the buffer that is the window, the way a client
    /// drawing its own shadows does: everything outside this rectangle is
    /// decoration the compositor is meant to place *outside* the cell.
    pub fn set_window_geometry(&mut self, id: WindowId, x: i32, y: i32, w: i32, h: i32) {
        let window = &self.state.windows[id.0];
        window.xdg_surface.set_window_geometry(x, y, w, h);
        window.surface.commit();
        self.roundtrip();
    }

    /// Creates the toplevel and commits it, without a buffer.
    fn begin_window(&mut self, title: &str) -> usize {
        let handle = self.queue.handle();
        let globals = &self.state.globals;
        let compositor = globals.compositor.clone().expect("checked at connect");
        let wm_base = globals.wm_base.clone().expect("checked at connect");
        let decoration_manager = globals.decoration_manager.clone();

        let surface = compositor.create_surface(&handle, ());
        let index = self.state.windows.len();
        let xdg_surface = wm_base.get_xdg_surface(&surface, &handle, index);
        let toplevel = xdg_surface.get_toplevel(&handle, index);
        toplevel.set_title(title.to_owned());
        toplevel.set_app_id(format!("irontile.test.{title}"));

        let decoration = decoration_manager.as_ref().map(|manager| {
            let decoration = manager.get_toplevel_decoration(&toplevel, &handle, index);
            // Ask for no client-side decoration; the compositor draws the only
            // decoration there is.
            decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            decoration
        });

        // Asking for the exact scale is what a client that wants to render
        // sharply on a scaled display does.
        let fractional = self
            .state
            .globals
            .fractional_scale
            .clone()
            .map(|manager| manager.get_fractional_scale(&surface, &handle, index));

        let buffer = self.buffer();
        surface.commit();

        self.state.windows.push(Window {
            surface,
            xdg_surface,
            toplevel,
            decoration,
            fractional,
            buffer,
            pending: Configured::default(),
            current: Configured::default(),
            configures: 0,
            closed: false,
            mapped: false,
        });

        index
    }

    /// Creates a toplevel and waits for its first configure, but attaches no
    /// buffer, so the compositor sees a window that has not drawn yet.
    pub fn create_toplevel_without_buffer(&mut self, title: &str) -> WindowId {
        let index = self.begin_window(title);
        self.wait_until(
            |state| state.windows[index].configures > 0,
            "first configure",
        );
        WindowId(index)
    }

    /// Gives a window its first buffer, which is what makes it mapped.
    pub fn attach_buffer(&mut self, id: WindowId) {
        let buffer = self.buffer();
        let window = &mut self.state.windows[id.0];
        window.surface.attach(Some(&buffer), 0, 0);
        window.surface.damage(0, 0, i32::MAX, i32::MAX);
        window.surface.commit();
        window.mapped = true;
        self.roundtrip();
    }

    /// Unmaps a window without destroying it, by attaching a null buffer.
    ///
    /// xdg-shell's way of saying "this window is not on screen now": the
    /// toplevel object stays alive and can be mapped again by attaching a
    /// buffer. A browser does this with a window it is keeping around.
    pub fn unmap_window(&mut self, id: WindowId) {
        let window = &mut self.state.windows[id.0];
        window.surface.attach(None, 0, 0);
        window.surface.commit();
        window.mapped = false;
        self.roundtrip();
    }

    /// Renames a window, as a browser does when you change tab.
    pub fn set_title(&mut self, id: WindowId, title: &str) {
        self.state.windows[id.0]
            .toplevel
            .set_title(title.to_owned());
        self.state.windows[id.0].surface.commit();
        self.roundtrip();
    }

    /// Unmaps and destroys a window.
    pub fn close_window(&mut self, id: WindowId) {
        let window = &mut self.state.windows[id.0];
        if let Some(fractional) = window.fractional.take() {
            fractional.destroy();
        }
        if let Some(decoration) = window.decoration.take() {
            decoration.destroy();
        }
        window.toplevel.destroy();
        window.xdg_surface.destroy();
        window.surface.destroy();
        window.mapped = false;
        self.roundtrip();
    }

    /// The most recent configure the compositor sent for a window.
    pub fn configured(&mut self, id: WindowId) -> Configured {
        self.roundtrip();
        self.state.windows[id.0].current
    }

    /// The surface the compositor has given the keyboard to, if it is one of
    /// this client's.
    pub fn keyboard_focus(&mut self) -> Option<WlSurface> {
        self.roundtrip();
        self.state.keyboard_focus.clone()
    }

    pub fn window_has_keyboard(&mut self, id: WindowId) -> bool {
        let surface = self.state.windows[id.0].surface.clone();
        self.keyboard_focus().as_ref() == Some(&surface)
    }

    pub fn layer_has_keyboard(&mut self, id: LayerId) -> bool {
        let surface = self.state.layers[id.0].surface.clone();
        self.keyboard_focus().as_ref() == Some(&surface)
    }

    /// Whether the compositor asked the window to close.
    pub fn was_asked_to_close(&mut self, id: WindowId) -> bool {
        self.roundtrip();
        self.state.windows[id.0].closed
    }

    /// Pumps the connection until `done`, or fails the test.
    pub fn wait_for(&mut self, mut done: impl FnMut(&mut TestClient) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.roundtrip();
            if done(self) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting on the compositor");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for something to become true, or fails the test.
    ///
    /// Polled rather than blocked on: an event that never arrives would
    /// otherwise leave the test sitting in `blocking_dispatch` for ever,
    /// because the deadline is only looked at between events. A test that hangs
    /// says far less than one that fails, and takes much longer to say it.
    fn wait_until(&mut self, done: impl Fn(&State) -> bool, what: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while !done(&self.state) {
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}");
            }
            self.queue.flush().expect("wayland flush failed");
            self.queue
                .dispatch_pending(&mut self.state)
                .expect("wayland dispatch failed");
            if done(&self.state) {
                return;
            }
            let fd = self.connection.prepare_read().map(|guard| {
                let fd = guard.connection_fd().try_clone_to_owned();
                drop(guard);
                fd
            });
            if let Some(Ok(fd)) = fd {
                let mut fds = [rustix::event::PollFd::new(
                    &fd,
                    rustix::event::PollFlags::IN,
                )];
                let tick = rustix::time::Timespec {
                    tv_sec: 0,
                    tv_nsec: 20_000_000,
                };
                let _ = rustix::event::poll(&mut fds, Some(&tick));
            }
            self.queue
                .roundtrip(&mut self.state)
                .expect("wayland roundtrip failed");
        }
    }

    /// A buffer of an exact size, for the surfaces that must match one.
    fn sized_buffer(&mut self, width: i32, height: i32) -> WlBuffer {
        let handle = self.queue.handle();
        let shm = self.state.globals.shm.clone().expect("checked at connect");
        let stride = width * 4;
        let size = stride * height;
        let file = rustix::fs::memfd_create(c"irontile-test", rustix::fs::MemfdFlags::CLOEXEC)
            .expect("failed to create a buffer");
        rustix::fs::ftruncate(&file, size as u64).expect("failed to size a buffer");
        let pool = shm.create_pool(file.as_fd(), size, &handle, ());
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            stride,
            wl_shm::Format::Argb8888,
            &handle,
            (),
        );
        pool.destroy();
        buffer
    }

    /// One shared buffer for every window. Nothing reads the contents; only its
    /// existence matters.
    fn buffer(&mut self) -> WlBuffer {
        if let Some((_, buffer)) = &self.pool {
            return buffer.clone();
        }
        let handle = self.queue.handle();
        let shm = self.state.globals.shm.clone().expect("checked at connect");

        let (width, height) = (64, 64);
        let stride = width * 4;
        let size = stride * height;
        let file = rustix::fs::memfd_create(c"irontile-test", rustix::fs::MemfdFlags::CLOEXEC)
            .expect("failed to create a buffer");
        rustix::fs::ftruncate(&file, size as u64).expect("failed to size a buffer");

        let pool = shm.create_pool(file.as_fd(), size, &handle, ());
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            stride,
            wl_shm::Format::Argb8888,
            &handle,
            (),
        );
        self.pool = Some((pool, buffer.clone()));
        buffer
    }
}

impl Drop for TestClient {
    fn drop(&mut self) {
        // Closing the connection is what makes the compositor unmap anything
        // still open, which some tests depend on.
        let _ = self.connection.flush();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        state.advertised.push(interface.clone());
        match interface.as_str() {
            "wl_compositor" => {
                state.globals.compositor = Some(registry.bind(name, version.min(6), handle, ()));
            }
            "wl_shm" => {
                state.globals.shm = Some(registry.bind(name, version.min(1), handle, ()));
            }
            "wl_seat" => {
                let seat: WlSeat = registry.bind(name, version.min(7), handle, ());
                // Bound only to observe: nothing is ever typed and the pointer
                // is moved by the compositor, not from here.
                seat.get_keyboard(handle, ());
                seat.get_pointer(handle, ());
                state.globals.seat = Some(seat);
            }
            "xdg_wm_base" => {
                state.globals.wm_base = Some(registry.bind(name, version.min(5), handle, ()));
            }
            "zxdg_decoration_manager_v1" => {
                state.globals.decoration_manager =
                    Some(registry.bind(name, version.min(1), handle, ()));
            }
            "wp_fractional_scale_manager_v1" => {
                state.globals.fractional_scale =
                    Some(registry.bind(name, version.min(1), handle, ()));
            }
            "wp_viewporter" => {
                state.globals.viewporter = Some(registry.bind(name, version.min(1), handle, ()));
            }
            "zwlr_layer_shell_v1" => {
                state.globals.layer_shell = Some(registry.bind(name, version.min(4), handle, ()));
            }
            "wl_output" => {
                state
                    .globals
                    .outputs
                    .push(registry.bind(name, version.min(4), handle, ()));
            }
            "zwlr_screencopy_manager_v1" => {
                state.globals.screencopy = Some(registry.bind(name, version.min(2), handle, ()));
            }
            "ext_session_lock_manager_v1" => {
                state.globals.session_lock = Some(registry.bind(name, version.min(1), handle, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // A client that does not pong is disconnected as unresponsive.
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, usize> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let xdg_surface::Event::Configure { serial } = event else {
            return;
        };
        xdg_surface.ack_configure(serial);
        let window = &mut state.windows[*index];
        // The toplevel configure that preceded this one becomes current only
        // now, which is what the protocol means by an atomic configure.
        window.current = window.pending;
        window.configures += 1;
        if window.mapped {
            window.surface.attach(Some(&window.buffer), 0, 0);
            window.surface.commit();
        }
    }
}

impl Dispatch<XdgToplevel, usize> for State {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let window = &mut state.windows[*index];
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                window.pending.width = width;
                window.pending.height = height;
                let states: Vec<xdg_toplevel::State> = states
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter_map(|c| {
                        let raw = u32::from_ne_bytes(*c);
                        xdg_toplevel::State::try_from(raw).ok()
                    })
                    .collect();
                let has = |s: xdg_toplevel::State| states.contains(&s);
                window.pending.activated = has(xdg_toplevel::State::Activated);
                window.pending.fullscreen = has(xdg_toplevel::State::Fullscreen);
                window.pending.tiled = has(xdg_toplevel::State::TiledLeft)
                    && has(xdg_toplevel::State::TiledRight)
                    && has(xdg_toplevel::State::TiledTop)
                    && has(xdg_toplevel::State::TiledBottom);
            }
            xdg_toplevel::Event::Close => window.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<ZxdgToplevelDecorationV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgToplevelDecorationV1,
        event: zxdg_toplevel_decoration_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_toplevel_decoration_v1::Event::Configure { mode } = event
            && let Ok(mode) = mode.into_result()
        {
            state.windows[*index].pending.decoration = Some(mode);
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for State {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                layer_surface.ack_configure(serial);
                let panel = &mut state.layers[*index];
                panel.configures += 1;
                if panel.mapped {
                    panel.surface.attach(Some(&panel.buffer), 0, 0);
                    panel.surface.commit();
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.layers[*index].mapped = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // The compiled keymap, as every client receives it. Reading it is
            // the only way from outside to see which keymap the compositor
            // actually built.
            wl_keyboard::Event::Keymap { fd, size, .. } => {
                use std::io::Read as _;
                let file = std::fs::File::from(fd);
                let mut text = String::new();
                if file.take(u64::from(size)).read_to_string(&mut text).is_ok() {
                    state.keymap = Some(text);
                }
            }
            wl_keyboard::Event::Enter { surface, .. } => state.keyboard_focus = Some(surface),
            wl_keyboard::Event::Leave { surface, .. }
                if state.keyboard_focus.as_ref() == Some(&surface) =>
            {
                state.keyboard_focus = None;
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ignore ExtSessionLockManagerV1);
delegate_noop!(State: ignore ZwlrScreencopyManagerV1);

impl Dispatch<WpFractionalScaleV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            // Unlike the toplevel configure, this applies at once rather than
            // on the next ack.
            state.windows[*index].pending.fractional_scale = Some(scale);
            state.windows[*index].current.fractional_scale = Some(scale);
        }
    }
}

/// A frame callback released on a panel.
impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, usize> for State {
    fn event(
        state: &mut Self,
        _: &wayland_client::protocol::wl_callback::WlCallback,
        _: wayland_client::protocol::wl_callback::Event,
        panel: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(panel) = state.layers.get_mut(*panel) {
            panel.frames += 1;
        }
    }
}

impl Dispatch<WpFractionalScaleV1, PanelScale> for State {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        panel: &PanelScale,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.layers[panel.0].fractional_scale = Some(scale);
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_screencopy_frame_v1::Event::Buffer {
            width,
            height,
            stride,
            ..
        } = event
        {
            state.offered = Some((width, height, stride));
        }
    }
}

impl Dispatch<ExtSessionLockV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_session_lock_v1::Event::Locked => state.locked = true,
            // The compositor refused; a lock screen that hears this must not
            // pretend the session is locked.
            ext_session_lock_v1::Event::Finished => {
                state.locked = false;
                state.refused = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockSurfaceV1, usize> for State {
    fn event(
        state: &mut Self,
        shell: &ExtSessionLockSurfaceV1,
        event: ext_session_lock_surface_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            shell.ack_configure(serial);
            if let Some(panel) = state.locks.get_mut(*index) {
                panel.configured = true;
                panel.size = (width, height);
            }
        }
    }
}

impl Dispatch<wayland_client::protocol::wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wayland_client::protocol::wl_pointer::WlPointer,
        event: wayland_client::protocol::wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_pointer::Event;
        match event {
            Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => state.pointer = Some((surface, (surface_x, surface_y))),
            Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if let Some((_, at)) = &mut state.pointer {
                    *at = (surface_x, surface_y);
                }
            }
            Event::Leave { .. } => state.pointer = None,
            Event::Button {
                button,
                state:
                    wayland_client::WEnum::Value(
                        wayland_client::protocol::wl_pointer::ButtonState::Pressed,
                    ),
                ..
            } => {
                state.buttons.push(button);
            }
            _ => {}
        }
    }
}

impl Dispatch<wayland_client::protocol::wl_output::WlOutput, ()> for State {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_output::WlOutput,
        _: wayland_client::protocol::wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore WpFractionalScaleManagerV1);
delegate_noop!(State: ignore WpViewporter);
delegate_noop!(State: ignore ZwlrLayerShellV1);
delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore ZxdgDecorationManagerV1);
