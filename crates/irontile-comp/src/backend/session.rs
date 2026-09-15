//! The session backend: real hardware, through DRM.
//!
//! irontile takes the seat from libseat, finds its GPUs through udev, and puts
//! one [`DrmCompositor`] behind every connected connector. Each of those owns a
//! swapchain and a plane assignment and answers page flips, so what this module
//! does is decide *when* to draw and *what* each display is showing; the
//! modesetting detail lives in smithay.
//!
//! Three things make this different from the nested backend, and all three are
//! why the code is shaped the way it is:
//!
//! - **The session can be taken away.** A VT switch pauses the DRM device and
//!   libinput; coming back means re-acquiring both and repainting everything
//!   from scratch, because the state of the display is no longer known.
//! - **Drawing is paced by the display.** There is no timer: a frame is queued,
//!   a page flip completes, and that vblank is what asks for the next one. A
//!   display with nothing to draw goes quiet and costs nothing.
//! - **Displays come and go.** A connector plugged in mid-session becomes an
//!   output, which the layout engine treats exactly like the ones that were
//!   there at startup.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use irontile_layout::{OutputId, Point};
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session as _};
use smithay::backend::udev::{UdevBackend, UdevEvent, primary_gpu};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::{EventLoop, LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::{connector, crtc};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::utils::DeviceFd;
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::backend::{Backend, Options};
use crate::ipc;
use crate::render::{self, Scene};
use crate::state::{Irontile, OutputSpec};

/// Formats a display may scan out from, best first. `Argb8888` is the universal
/// fallback; the others let a driver skip a conversion when it can.
const COLOR_FORMATS: &[smithay::reexports::drm::buffer::DrmFourcc] = &[
    smithay::reexports::drm::buffer::DrmFourcc::Abgr2101010,
    smithay::reexports::drm::buffer::DrmFourcc::Argb2101010,
    smithay::reexports::drm::buffer::DrmFourcc::Abgr8888,
    smithay::reexports::drm::buffer::DrmFourcc::Argb8888,
];

/// The compositor smithay drives one connector with.
type Compositor =
    DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

/// One connector that is on and being drawn.
struct Surface {
    /// How the layout engine refers to this display.
    id: OutputId,
    output: Output,
    compositor: Compositor,
    /// A frame is in flight and the next one waits for its page flip.
    queued: bool,
    /// Something changed while a frame was in flight; draw again on vblank.
    pending: bool,
    /// Whether the driver took the cursor onto its own plane, once known.
    ///
    /// Worth knowing because it is the difference between moving the pointer
    /// costing a cursor-plane position update and it costing a full recomposite
    /// of the screen.
    cursor_plane: Option<bool>,
}

/// One GPU.
struct Device {
    drm: DrmDevice,
    gbm: GbmDevice<DrmDeviceFd>,
    scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, Surface>,
    token: RegistrationToken,
}

/// Everything the session backend owns.
///
/// Hand-written `Debug` because none of the device handles are debuggable and
/// the useful summary is how many there are.
pub struct Session {
    session: LibSeatSession,
    devices: HashMap<DrmNode, Device>,
    /// One renderer, on the primary GPU. Rendering for a secondary GPU would
    /// need the frame copied across, which is a problem for the day a second
    /// GPU is supported.
    renderer: Option<GlesRenderer>,
    primary: Option<DrmNode>,
    /// False between giving up the seat and getting it back.
    ///
    /// Every drawing operation is rejected while the session is paused, so
    /// rendering anyway costs a full frame's GPU work per tick and produces an
    /// error line for each one, which would bury anything that actually went
    /// wrong.
    active: bool,
    /// The render node clients should allocate on.
    ///
    /// Without telling them this, a client cannot work out which GPU to use and
    /// falls back to rendering into shared memory on the CPU -- which works,
    /// but means every frame is drawn by the processor and copied.
    render_node: Option<DrmNode>,
    /// Connector name to display id, kept for the life of the session.
    ///
    /// A display that is unplugged and plugged back in has to come back as the
    /// *same* id, or the desktop that remembers it as its preferred display
    /// will not be restored to it. Handing out a fresh id each time would
    /// quietly break the thing that makes unplugging a monitor safe.
    identities: HashMap<String, OutputId>,
    next_output: u64,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("gpus", &self.devices.len())
            .field(
                "displays",
                &self
                    .devices
                    .values()
                    .map(|device| device.surfaces.len())
                    .sum::<usize>(),
            )
            .field("renders", &self.renderer.is_some())
            .finish()
    }
}

impl Session {
    pub fn renderer(&mut self) -> Option<&mut GlesRenderer> {
        self.renderer.as_mut()
    }

    /// The device clients should allocate buffers on.
    pub fn render_device(&self) -> Option<libc::dev_t> {
        self.render_node.map(|node| node.dev_id())
    }

    /// Switches to another virtual terminal.
    ///
    /// While a compositor holds the VT in graphics mode the kernel no longer
    /// handles the switch itself, so unless the compositor asks for it there is
    /// no way off the session at all.
    pub fn change_vt(&mut self, vt: i32) {
        tracing::info!(
            vt,
            active = self.session.is_active(),
            "switching virtual terminal"
        );
        match self.session.change_vt(vt) {
            // The switch is asynchronous: libseat asks this session to give up
            // the seat and the pause arrives as a session event afterwards, so
            // success here only means the request was accepted.
            Ok(()) => tracing::debug!(vt, "switch requested"),
            Err(err) => tracing::warn!(vt, %err, "failed to switch virtual terminal"),
        }
    }

    /// Every display currently lit, in a stable order.
    fn specs(&self) -> Vec<OutputSpec> {
        let mut specs: Vec<OutputSpec> = self
            .devices
            .values()
            .flat_map(|device| device.surfaces.values())
            .map(|surface| {
                let mode = surface.output.current_mode().unwrap_or(OutputMode {
                    size: (0, 0).into(),
                    refresh: 60_000,
                });
                let mut spec = OutputSpec::new(
                    surface.id,
                    surface.output.name(),
                    irontile_layout::Size::new(mode.size.w, mode.size.h),
                );
                spec.refresh = mode.refresh;
                // The same object the `DrmCompositor` reads its scale from.
                spec.with_output(surface.output.clone())
            })
            .collect();
        specs.sort_by_key(|spec| spec.id.0);
        specs
    }
}

pub fn run(options: Options) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<Irontile> = EventLoop::try_new()?;
    let display: Display<Irontile> = Display::new()?;
    let display_handle = display.handle();

    let (session, session_notifier) =
        LibSeatSession::new().context("could not take a seat; is another compositor running?")?;
    let seat_name = session.seat();

    let socket = super::bind_socket(options.wayland_display.as_deref())?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();

    let mut state = Irontile::new(
        display_handle,
        socket_name.clone(),
        options.config,
        options.config_path,
        event_loop.handle(),
    );
    state
        .seat
        .add_keyboard(
            Default::default(),
            crate::state::REPEAT_DELAY_MS,
            crate::state::REPEAT_RATE_HZ,
        )
        .context("failed to create a keyboard")?;
    state.seat.add_pointer();
    state.backend = Backend::Session(Box::new(Session {
        session: session.clone(),
        devices: HashMap::new(),
        renderer: None,
        primary: primary_gpu(&seat_name)
            .ok()
            .flatten()
            .and_then(|path| DrmNode::from_path(path).ok()),
        active: true,
        render_node: None,
        identities: HashMap::new(),
        next_output: 1,
    }));

    let handle = event_loop.handle();
    super::insert_wayland_sources(&handle, display, socket)?;
    let control =
        ipc::listen(&handle, &socket_name).context("failed to bind the control socket")?;

    // Input.
    let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput
        .udev_assign_seat(&seat_name)
        .map_err(|_| anyhow::anyhow!("could not assign the seat to libinput"))?;
    handle
        .insert_source(
            LibinputInputBackend::new(libinput.clone()),
            move |event, _, state: &mut Irontile| {
                // Every display is in one coordinate space, so absolute input
                // is placed against the whole arrangement rather than a screen.
                let bounds = state.arrangement_size();
                crate::input::handle(state, event, bounds);
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert the input source: {e}"))?;

    // The seat can be taken away and given back.
    handle
        .insert_source(
            session_notifier,
            move |event, _, state: &mut Irontile| match event {
                SessionEvent::PauseSession => {
                    tracing::info!("session paused");
                    libinput.suspend();
                    pause_devices(state);
                }
                SessionEvent::ActivateSession => {
                    tracing::info!("session resumed");
                    if libinput.resume().is_err() {
                        tracing::error!("failed to resume input");
                    }
                    resume_devices(state);
                }
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert the session source: {e}"))?;

    // GPUs, now and as they appear.
    let udev = UdevBackend::new(&seat_name).context("failed to start udev")?;
    for (device_id, path) in udev.device_list() {
        if let Err(err) = device_added(&mut state, &handle, device_id, path) {
            tracing::warn!(path = %path.display(), %err, "skipping a device");
        }
    }
    let udev_handle = handle.clone();
    handle
        .insert_source(udev, move |event, _, state: &mut Irontile| match event {
            UdevEvent::Added { device_id, path } => {
                if let Err(err) = device_added(state, &udev_handle, device_id, &path) {
                    tracing::warn!(path = %path.display(), %err, "failed to add a device");
                }
            }
            UdevEvent::Changed { device_id } => device_changed(state, device_id),
            UdevEvent::Removed { device_id } => device_removed(state, &udev_handle, device_id),
        })
        .map_err(|e| anyhow::anyhow!("failed to insert the udev source: {e}"))?;

    state.advertise_dmabuf();
    state.reflow();
    render_all(&mut state);

    tracing::info!(
        socket = %socket_name,
        control = %control.path().display(),
        seat = %seat_name,
        "irontile is running on the session"
    );
    // Before anything is started, so the first program launched already finds
    // the right display -- and only from the session backend: the compositor
    // that *is* the session is the one entitled to say where the session is.
    if state.config.session.announce {
        crate::environment::publish(&socket_name, state.config.session.announce_to_systemd);
    }
    state.run_startup_commands();

    let signal = event_loop.get_signal();
    let mut last_beat = std::time::Instant::now();
    event_loop.run(Some(Duration::from_millis(16)), &mut state, move |state| {
        // Without this, a log that has gone quiet could mean either "nothing is
        // happening" or "the event loop is wedged", and those need very
        // different fixes.
        if last_beat.elapsed() >= Duration::from_secs(5) {
            last_beat = std::time::Instant::now();
            tracing::debug!(
                windows = state.placements.placements.len(),
                outputs = state.layout.outputs().len(),
                "alive"
            );
        }
        if !state.running {
            signal.stop();
            return;
        }
        if state.dirty {
            state.reflow();
        }
        // Unconditionally, not only when something is known to have changed.
        // Anything that reflows eagerly -- mapping a window does -- clears the
        // dirty flag before this runs, so gating on it loses the very redraw
        // that was needed and leaves the last frame on screen forever. The
        // compositor's own damage tracking makes the nothing-changed case
        // cheap, and reports it as an empty frame so no page flip is queued.
        render_all(state);
        state.popups.cleanup();
        if let Err(err) = state.display_handle.flush_clients() {
            tracing::warn!(%err, "failed to flush clients");
        }
    })?;

    Ok(())
}

/// Opens a GPU and starts watching its connectors.
fn device_added(
    state: &mut Irontile,
    handle: &LoopHandle<'static, Irontile>,
    device_id: libc::dev_t,
    path: &Path,
) -> anyhow::Result<()> {
    let node = DrmNode::from_dev_id(device_id)?;
    let Backend::Session(session) = &mut state.backend else {
        return Ok(());
    };
    if session.devices.contains_key(&node) {
        return Ok(());
    }

    let fd = session
        .session
        .open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .context("the seat would not open the device")?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (drm, drm_notifier) =
        DrmDevice::new(fd.clone(), true).context("not a modesetting device")?;
    let gbm = GbmDevice::new(fd).context("no gbm on this device")?;

    // A page flip completing is what asks for the next frame, so this source is
    // the whole render clock; there is no timer driving the session backend.
    let token = handle
        .insert_source(
            drm_notifier,
            move |event, meta, state: &mut Irontile| match event {
                DrmEvent::VBlank(crtc) => {
                    let _ = meta;
                    frame_finished(state, node, crtc);
                }
                DrmEvent::Error(err) => tracing::error!(%err, "drm error"),
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to watch the device: {e}"))?;

    // The primary GPU is the one that renders. Its EGL display is created from
    // the gbm device so that buffers can be scanned out without a copy.
    let is_primary = session.primary == Some(node) || session.primary.is_none();
    if is_primary && session.renderer.is_none() {
        // SAFETY: the gbm device outlives the renderer, which is dropped with
        // the session, and neither is handed to another thread.
        #[allow(unsafe_code)]
        let renderer = unsafe {
            let egl = EGLDisplay::new(gbm.clone()).context("no EGL for this device")?;
            let context = EGLContext::new(&egl).context("no EGL context")?;
            GlesRenderer::new(context).context("no GL renderer")?
        };
        session.renderer = Some(renderer);
        session.primary = Some(node);
        // Prefer the render node: it is the one a client may open without
        // being the DRM master, which is the whole point of handing it over.
        session.render_node = Some(
            node.node_with_type(NodeType::Render)
                .and_then(Result::ok)
                .unwrap_or(node),
        );
    }

    session.devices.insert(
        node,
        Device {
            drm,
            gbm,
            scanner: DrmScanner::new(),
            surfaces: HashMap::new(),
            token,
        },
    );
    let cursor_size = session
        .devices
        .get(&node)
        .map(|device| device.drm.cursor_size())
        .unwrap_or_default();
    tracing::info!(
        device = %path.display(),
        primary = is_primary,
        cursor = %format!("{}x{}", cursor_size.w, cursor_size.h),
        "gpu added"
    );

    device_changed(state, device_id);
    Ok(())
}

/// Rescans a GPU's connectors, lighting up what is newly plugged in and
/// dropping what is gone.
fn device_changed(state: &mut Irontile, device_id: libc::dev_t) {
    let Ok(node) = DrmNode::from_dev_id(device_id) else {
        return;
    };
    // Cloned out before the backend is borrowed, so the mode a connector should
    // come up in is available while it is being lit.
    let wanted = state.config.outputs.clone();
    let Backend::Session(session) = &mut state.backend else {
        return;
    };
    let Some(device) = session.devices.get_mut(&node) else {
        return;
    };

    let Ok(scan) = device.scanner.scan_connectors(&device.drm) else {
        tracing::warn!("failed to scan connectors");
        return;
    };
    for event in scan {
        match event {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => {
                if let Err(err) = connector_connected(session, node, connector, crtc, &wanted) {
                    tracing::warn!(%err, "failed to light a connector");
                }
            }
            DrmScanEvent::Disconnected {
                crtc: Some(crtc), ..
            } => {
                if let Some(device) = session.devices.get_mut(&node) {
                    device.surfaces.remove(&crtc);
                }
            }
            _ => {}
        }
    }

    let specs = session.specs();
    state.configure_outputs(&specs);
    state.reflow();
    render_all(state);
}

/// Sets a mode on a connector and builds the compositor that will drive it.
fn connector_connected(
    session: &mut Session,
    node: DrmNode,
    connector: connector::Info,
    crtc: crtc::Handle,
    wanted: &[crate::config::OutputConfig],
) -> anyhow::Result<()> {
    if !session.devices.contains_key(&node) {
        return Ok(());
    }

    let name = format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    );

    // A configured mode if one matches, then the mode the display says it
    // prefers, then whatever it listed first.
    let requested = wanted
        .iter()
        .find(|o| o.name == name)
        .or_else(|| wanted.iter().find(|o| o.name == "*"))
        .and_then(|o| o.mode);
    let mode = requested
        .and_then(|want| pick_mode(&connector, want))
        .or_else(|| {
            connector.modes().iter().copied().find(|mode| {
                mode.mode_type()
                    .contains(smithay::reexports::drm::control::ModeTypeFlags::PREFERRED)
            })
        })
        .or_else(|| connector.modes().first().copied())
        .context("connector reports no modes")?;
    if requested.is_some() {
        let (w, h) = mode.size();
        tracing::info!(connector = %name, mode = %format!("{w}x{h}"), "using a configured mode");
    }
    // The same connector always gets the same id, so a desktop that prefers
    // this display is restored to it when it comes back.
    let id = match session.identities.get(&name) {
        Some(id) => *id,
        None => {
            let id = OutputId(session.next_output);
            session.next_output += 1;
            session.identities.insert(name.clone(), id);
            id
        }
    };
    let Some(device) = session.devices.get_mut(&node) else {
        return Ok(());
    };
    let (width, height) = mode.size();
    let surface = device
        .drm
        .create_surface(crtc, mode, &[connector.handle()])
        .context("could not set a mode")?;

    let output_mode = OutputMode {
        size: (i32::from(width), i32::from(height)).into(),
        refresh: refresh_millihertz(&mode),
    };
    // Physical size is reported in millimetres, and as unsigned.
    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let (phys_w, phys_h) = (phys_w as i32, phys_h as i32);
    let output = Output::new(
        name.clone(),
        PhysicalProperties {
            size: (phys_w, phys_h).into(),
            subpixel: Subpixel::Unknown,
            make: "irontile".into(),
            model: name.clone(),
        },
    );
    output.set_preferred(output_mode);
    output.change_current_state(Some(output_mode), None, None, None);

    let allocator = GbmAllocator::new(
        device.gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let renderer_formats = session
        .renderer
        .as_mut()
        .map(|renderer| {
            use smithay::backend::renderer::ImportDma;
            renderer.dmabuf_formats()
        })
        .unwrap_or_default();

    let Some(device) = session.devices.get_mut(&node) else {
        return Ok(());
    };
    let compositor = DrmCompositor::new(
        &output,
        surface,
        None,
        allocator,
        GbmFramebufferExporter::new(
            device.gbm.clone(),
            node.node_with_type(NodeType::Render).and_then(Result::ok),
        ),
        COLOR_FORMATS.iter().copied(),
        renderer_formats,
        device.drm.cursor_size(),
        Some(device.gbm.clone()),
    )
    .context("could not build a drm compositor")?;

    device.surfaces.insert(
        crtc,
        Surface {
            id,
            output,
            compositor,
            queued: false,
            pending: true,
            cursor_plane: None,
        },
    );
    tracing::info!(connector = %name, mode = %format!("{width}x{height}"), "display lit");
    Ok(())
}

/// Finds the listed mode closest to what was asked for.
///
/// An exact size is required, since a display cannot invent one, but the
/// refresh rate is matched loosely: asking for 60 should accept 59.997, which
/// is what a display will actually report.
fn pick_mode(
    connector: &connector::Info,
    want: crate::config::ModeSpec,
) -> Option<smithay::reexports::drm::control::Mode> {
    let matching: Vec<_> = connector
        .modes()
        .iter()
        .filter(|mode| {
            let (w, h) = mode.size();
            i32::from(w) == want.width && i32::from(h) == want.height
        })
        .copied()
        .collect();
    match want.refresh {
        None => matching.iter().copied().max_by_key(refresh_millihertz),
        Some(target) => matching
            .into_iter()
            .min_by_key(|mode| (refresh_millihertz(mode) - target).abs()),
    }
}

fn device_removed(
    state: &mut Irontile,
    handle: &LoopHandle<'static, Irontile>,
    device_id: libc::dev_t,
) {
    let Ok(node) = DrmNode::from_dev_id(device_id) else {
        return;
    };
    let Backend::Session(session) = &mut state.backend else {
        return;
    };
    let Some(device) = session.devices.remove(&node) else {
        return;
    };
    handle.remove(device.token);
    if session.primary == Some(node) {
        // The GPU that was rendering is gone. Nothing can be drawn until
        // another appears, but clients stay connected and keep their windows.
        session.renderer = None;
        session.primary = None;
    }

    let specs = session.specs();
    state.configure_outputs(&specs);
    tracing::info!("gpu removed");
}

fn id_of(surface: &Surface) -> u64 {
    surface.id.0
}

/// A page flip completed: release the frame and draw the next if one is due.
fn frame_finished(state: &mut Irontile, node: DrmNode, crtc: crtc::Handle) {
    let Backend::Session(session) = &mut state.backend else {
        return;
    };
    let Some(surface) = session
        .devices
        .get_mut(&node)
        .and_then(|device| device.surfaces.get_mut(&crtc))
    else {
        return;
    };
    // Logged because the whole render loop hangs off this arriving: if page
    // flips stop being reported, every display freezes on its last frame.
    tracing::trace!(output = id_of(surface), "vblank");
    if let Err(err) = surface.compositor.frame_submitted() {
        tracing::warn!(%err, "failed to release a frame");
    }
    surface.queued = false;
    let id = surface.id;
    let pending = surface.pending;

    // Clients are told the frame is on screen only once it actually is, which
    // is what paces an animating client to the refresh rate.
    state.send_frame_callbacks_for(id);
    if pending {
        render_output(state, id);
    }
}

/// Whether the seat is ours at the moment.
fn is_active(state: &Irontile) -> bool {
    match &state.backend {
        Backend::Session(session) => session.active,
        _ => true,
    }
}

/// Draws every display that has something to show.
fn render_all(state: &mut Irontile) {
    let Backend::Session(session) = &state.backend else {
        return;
    };
    if !session.active {
        return;
    }
    let ids: Vec<OutputId> = session
        .devices
        .values()
        .flat_map(|device| device.surfaces.values())
        .map(|surface| surface.id)
        .collect();
    for id in ids {
        render_output(state, id);
    }
}

/// Draws one display, if it is not already waiting on a page flip.
fn render_output(state: &mut Irontile, id: OutputId) {
    // Paused means the display belongs to someone else. Clients are left
    // without frame callbacks on purpose: there is nothing for them to draw
    // into that anyone would see.
    if !is_active(state) {
        return;
    }
    if compose(state, id) == Composed::Unchanged {
        // Nothing moved, so there is no flip to wait for and no vblank coming.
        // The frame callbacks still have to go out, or a client animating
        // against something off this display would never draw again.
        state.send_frame_callbacks_for(id);
    }
}

#[derive(PartialEq, Eq)]
enum Composed {
    /// A frame was queued; its page flip will ask for the next one.
    Queued,
    /// Nothing to draw, or nothing changed.
    Unchanged,
}

fn compose(state: &mut Irontile, id: OutputId) -> Composed {
    // Nothing else draws a pointer on real hardware. Both the position and the
    // image are resolved before the state is split up for rendering, because
    // resolving the image needs the whole compositor.
    let pointer = state
        .seat
        .get_pointer()
        .map(|pointer| pointer.current_location());
    let display_scale = state
        .smithay_output(id)
        .map(|output| output.current_scale().fractional_scale())
        .unwrap_or(1.0);
    let surface_cursor = state.cursor_surface();
    let image_cursor = match surface_cursor {
        // A client drawing its own pointer needs no image from us.
        Some(_) => None,
        None => state.cursor_image(display_scale),
    };
    let cursor_size = image_cursor
        .as_ref()
        .map(|image| image.size)
        .unwrap_or((0, 0));
    let Irontile {
        backend,
        placements,
        windows,
        outputs,
        layout,
        config,
        session_lock: lock,
        screencopy,
        start_time,
        ..
    } = state;
    let Backend::Session(session) = backend else {
        return Composed::Unchanged;
    };
    let Some(renderer) = session.renderer.as_mut() else {
        return Composed::Unchanged;
    };
    let Some(surface) = session
        .devices
        .values_mut()
        .flat_map(|device| device.surfaces.values_mut())
        .find(|surface| surface.id == id)
    else {
        return Composed::Unchanged;
    };

    if surface.queued {
        // A frame is already in flight; its vblank will come back for this one.
        surface.pending = true;
        return Composed::Queued;
    }
    surface.pending = false;

    // The display's one `Output`, which the `DrmCompositor` also reads when it
    // sizes elements, so the two cannot disagree.
    let scale = outputs
        .iter()
        .find(|entry| entry.id == id)
        .map(|entry| entry.output.current_scale().fractional_scale())
        .unwrap_or(1.0);
    let scene = Scene {
        frame: placements,
        windows,
        outputs,
        layout,
        theme: &config.theme,
        cursor: pointer.and_then(|at| {
            let at = Point::new(at.x as i32, at.y as i32);
            match (&surface_cursor, &image_cursor) {
                (Some((surface, hotspot)), _) => Some(crate::render::Cursor::Surface {
                    surface,
                    hotspot: *hotspot,
                    at,
                }),
                (None, Some(image)) => Some(crate::render::Cursor::Image {
                    buffer: &image.buffer,
                    hotspot: image.hotspot,
                    size: image.logical_size,
                    source: image.size,
                    at,
                }),
                // The client asked for no pointer at all.
                (None, None) => None,
            }
        }),
        lock: lock.as_ref(),
    };
    let elements = render::elements(&scene, renderer, id, scale);

    // Anything waiting for a picture of this display gets one from the same
    // element list that is about to be shown, so what is copied is what is on
    // screen rather than an approximation of it.
    let pixels = outputs
        .iter()
        .find(|entry| entry.id == id)
        .map(|entry| {
            entry
                .output
                .current_mode()
                .map(|m| m.size)
                .unwrap_or_default()
        })
        .unwrap_or_default();
    crate::screencopy::serve(
        screencopy,
        renderer,
        &elements,
        crate::screencopy::Display {
            id,
            size: pixels,
            scale,
            transform: outputs
                .iter()
                .find(|entry| entry.id == id)
                .map(|entry| entry.output.current_transform())
                .unwrap_or(smithay::utils::Transform::Normal),
            clear: config.theme.background,
        },
        start_time.elapsed(),
    );

    match surface
        .compositor
        // Let the driver put what it can on planes rather than compositing it:
        // a fullscreen window or a cursor can often be scanned out directly,
        // which skips the GPU entirely for that frame.
        .render_frame(
            renderer,
            &elements,
            config.theme.background,
            FrameFlags::DEFAULT,
        ) {
        Ok(result) => {
            let on_plane = result.cursor_element.is_some();
            if surface.cursor_plane != Some(on_plane) {
                surface.cursor_plane = Some(on_plane);
                tracing::info!(
                    output = id.0,
                    hardware = on_plane,
                    image = %format!("{}x{}", cursor_size.0, cursor_size.1),
                    "cursor plane assignment changed"
                );
            }
            if result.is_empty {
                return Composed::Unchanged;
            }
            match surface.compositor.queue_frame(()) {
                Ok(()) => {
                    tracing::trace!(output = id.0, "queued a frame");
                    surface.queued = true;
                    Composed::Queued
                }
                Err(err) => {
                    tracing::error!(%err, "failed to queue a frame");
                    Composed::Unchanged
                }
            }
        }
        Err(err) => {
            tracing::error!(%err, "failed to render a frame");
            Composed::Unchanged
        }
    }
}

fn pause_devices(state: &mut Irontile) {
    let Backend::Session(session) = &mut state.backend else {
        return;
    };
    session.active = false;
    for device in session.devices.values_mut() {
        device.drm.pause();
        for surface in device.surfaces.values_mut() {
            // Nothing is in flight across a pause; the page flip that was
            // pending will never arrive.
            surface.queued = false;
        }
    }
}

fn resume_devices(state: &mut Irontile) {
    {
        let Backend::Session(session) = &mut state.backend else {
            return;
        };
        session.active = true;
        for device in session.devices.values_mut() {
            // Disable connectors on the way back in, so the next commit is a
            // full modeset: whoever had the card in between may have changed
            // it, and nothing here can know what they left behind.
            if let Err(err) = device.drm.activate(true) {
                tracing::error!(%err, "failed to reactivate a device");
            }
            for surface in device.surfaces.values_mut() {
                // The display's state is unknown after someone else had the
                // card, so the next frame has to be a full one.
                if let Err(err) = surface.compositor.reset_state() {
                    tracing::warn!(%err, "failed to reset a surface");
                }
                surface.pending = true;
            }
        }
    }
    state.dirty = true;
    render_all(state);
}

/// DRM reports a mode's timings; the refresh rate has to be derived from them.
fn refresh_millihertz(mode: &smithay::reexports::drm::control::Mode) -> i32 {
    let clock = u64::from(mode.clock()) * 1_000_000;
    let (hsync, vsync) = (mode.hsync(), mode.vsync());
    let htotal = u64::from(hsync.2);
    let vtotal = u64::from(vsync.2);
    if htotal == 0 || vtotal == 0 {
        return 60_000;
    }
    let mut refresh = clock / (htotal * vtotal);
    let flags = mode.flags();
    use smithay::reexports::drm::control::ModeFlags;
    if flags.contains(ModeFlags::INTERLACE) {
        refresh *= 2;
    }
    if flags.contains(ModeFlags::DBLSCAN) {
        refresh /= 2;
    }
    if mode.vscan() > 1 {
        refresh /= u64::from(mode.vscan());
    }
    refresh as i32
}
