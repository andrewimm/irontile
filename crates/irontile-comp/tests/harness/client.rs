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
    wl_registry,
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
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
}

struct State {
    /// Every interface the compositor advertised, for tests that pin the
    /// protocol surface.
    advertised: Vec<String>,
    globals: Globals,
    windows: Vec<Window>,
    layers: Vec<LayerPanel>,
}

struct LayerPanel {
    surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    buffer: WlBuffer,
    configures: u32,
    mapped: bool,
}

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

    /// Maps a layer surface anchored to the top of the display, reserving
    /// `exclusive` pixels — which is what a bar does.
    pub fn map_top_bar(&mut self, height: i32, exclusive: i32) -> LayerId {
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
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            None,
            zwlr_layer_shell_v1::Layer::Top,
            "irontile-test-bar".to_owned(),
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
        let buffer = self.buffer();
        surface.commit();

        self.state.layers.push(LayerPanel {
            surface,
            layer_surface,
            buffer,
            configures: 0,
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

    fn wait_until(&mut self, done: impl Fn(&State) -> bool, what: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while !done(&self.state) {
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}");
            }
            self.queue
                .blocking_dispatch(&mut self.state)
                .expect("wayland dispatch failed");
        }
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
                    .chunks_exact(4)
                    .filter_map(|c| {
                        let raw = u32::from_ne_bytes([c[0], c[1], c[2], c[3]]);
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
