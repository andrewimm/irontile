//! Compositor state and protocol plumbing.
//!
//! [`Irontile`] holds the Wayland protocol state alongside an
//! [`irontile_layout::Layout`]. It owns no layout policy of its own: protocol
//! events become layout commands, and the [`Frame`] that comes back becomes
//! surface configures and render elements.

use std::time::Instant;

use irontile_layout::{
    Command, Direction, Event, Frame, Layout, LayoutError, Output as LayoutOutput, OutputId,
    PlacementKind, Point as LayoutPoint, Rect, Size, WindowId, WorkspaceId, dispatch, frame,
};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::desktop::PopupManager;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DisplayHandle};
use smithay::utils::{IsAlive, Logical, Point, SERIAL_COUNTER, Size as SmithaySize};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{
    DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
};
use smithay::wayland::fractional_scale::{
    FractionalScaleHandler, FractionalScaleManagerState, with_fractional_scale,
};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState,
};
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
};

use crate::backend::Backend;
use crate::config::Config;
use crate::registry::Registry;

/// The display of the nested backend, which always has exactly one.
pub const NESTED_OUTPUT: OutputId = OutputId(1);

/// A display as a backend describes it.
///
/// Physical size and scale are kept apart because they answer different
/// questions: the display scans out `physical` pixels, while windows are laid
/// out in the logical space that falls out of dividing by `scale`. Conflating
/// them is what makes a HiDPI display either tiny or blurry.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputSpec {
    pub id: OutputId,
    pub name: String,
    /// Top-left corner in the global logical coordinate space.
    pub position: LayoutPoint,
    /// Size in physical pixels, as the display scans out.
    pub physical: Size,
    /// Logical pixels per physical pixel.
    pub scale: f64,
    pub refresh: i32,
    pub transform: smithay::utils::Transform,
    /// A protocol object the backend already made for this display.
    ///
    /// The session backend has to create one before it can build a
    /// `DrmCompositor`, and that compositor reads the scale back out of it when
    /// it sizes elements. Two `Output`s for one display means the scale can be
    /// set on the wrong one, and then windows are composited at the wrong size
    /// while everything drawn from an explicit rectangle stays right.
    pub output: Option<Output>,
}

impl OutputSpec {
    pub fn new(id: OutputId, name: impl Into<String>, physical: Size) -> Self {
        Self {
            id,
            name: name.into(),
            position: LayoutPoint::new(0, 0),
            physical,
            scale: 1.0,
            refresh: 60_000,
            transform: smithay::utils::Transform::Normal,
            output: None,
        }
    }

    /// Adopts a protocol object the backend already created.
    pub fn with_output(mut self, output: Output) -> Self {
        self.output = Some(output);
        self
    }

    pub fn at(mut self, position: LayoutPoint) -> Self {
        self.position = position;
        self
    }

    /// The rectangle this display occupies in the space windows live in.
    pub fn logical(&self) -> Rect {
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        // Rounded rather than truncated, so a 1.5 scale on an odd size does not
        // silently lose a pixel column off the right of the display.
        let w = (f64::from(self.physical.w) / scale).round() as i32;
        let h = (f64::from(self.physical.h) / scale).round() as i32;
        Rect::new(self.position.x, self.position.y, w.max(1), h.max(1))
    }
}

/// A pointer drag the compositor is handling itself.
///
/// Implemented as state consulted by the input handler rather than as a
/// `PointerGrab`, because a grab's purpose is to intercept events *and still
/// route them*, and here the client deliberately sees nothing for the duration.
/// Pointer focus is cleared when one starts, so no client is left believing the
/// pointer is still inside it. A formal grab would be the right shape if a
/// future gesture needed the client to see the drag.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResizeDrag {
    pub window: WindowId,
    /// Which edges follow the pointer, chosen from where in the window the
    /// drag started.
    pub horizontal: Direction,
    pub vertical: Direction,
    /// Where the pointer was at the last motion, so each step is a delta.
    pub last: Point<f64, Logical>,
}

/// A connected display and the protocol object advertising it.
#[derive(Debug)]
pub struct OutputEntry {
    pub id: OutputId,
    pub output: Output,
    global: GlobalId,
}

pub struct Irontile {
    pub display_handle: DisplayHandle,
    pub start_time: Instant,
    pub socket_name: String,
    pub running: bool,
    /// Set whenever something changed the layout; the next tick reflows.
    pub dirty: bool,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    /// Held for its global; clients ask through it and are always told
    /// server-side.
    #[allow(dead_code)]
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    /// Held for its protocol globals; dropping it would withdraw `wl_output`
    /// and `xdg_output` from clients.
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    /// Middle-click paste. Held for its global.
    #[allow(dead_code)]
    pub primary_selection_state: PrimarySelectionState,
    /// Lets clients name a cursor instead of supplying a buffer for it. Held
    /// for its global.
    #[allow(dead_code)]
    pub cursor_shape_state: CursorShapeManagerState,
    /// Lets a client be told the exact scale of the display it is on, rather
    /// than the whole number `wl_output` is limited to. Held for its global.
    #[allow(dead_code)]
    pub fractional_scale_state: FractionalScaleManagerState,
    /// Required alongside fractional scale: a client rendering at 1.5x has no
    /// way to say how large the result should be without it. Held for its
    /// global.
    #[allow(dead_code)]
    pub viewporter_state: smithay::wayland::viewporter::ViewporterState,
    pub popups: PopupManager,
    pub seat: Seat<Self>,
    /// Connected displays, paired with the protocol object each is advertised
    /// through. The layout engine's arrangement is the source of truth; these
    /// exist so clients can be told about them.
    pub outputs: Vec<OutputEntry>,
    pub peers: crate::ipc::Peers,

    pub layout: Layout,
    pub windows: Registry,
    /// The most recent frame, kept so rendering and hit-testing agree with what
    /// clients were last configured for.
    /// The displays as the backend described them, before work areas.
    arrangement: Vec<OutputSpec>,
    pub placements: Frame,
    pub config: Config,
    /// Where the configuration came from, so a reload reads the same file.
    pub config_path: std::path::PathBuf,
    /// Set while the pointer is resizing a window.
    pub drag: Option<ResizeDrag>,
    /// The renderer, if there is one. Kept here rather than in the backend's
    /// event loop so that a client's dmabuf can be imported the moment it
    /// arrives.
    pub backend: Backend,
    pub dmabuf_state: DmabufState,
    /// Present once a backend with a renderer has advertised its formats.
    #[allow(dead_code)]
    pub dmabuf_global: Option<DmabufGlobal>,
    /// Where pointer images come from. Only a backend that has to draw its own
    /// pointer uses it.
    pub cursor: crate::cursor::CursorSource,
    /// What the focused client last asked the pointer to look like.
    pub cursor_status: smithay::input::pointer::CursorImageStatus,
}

/// Hand-written because much of the protocol state smithay holds is not
/// `Debug`, and because the useful summary of a compositor is what it is
/// managing, not every delegate it owns.
impl std::fmt::Debug for Irontile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Irontile")
            .field("socket", &self.socket_name)
            .field("outputs", &self.outputs.len())
            .field("workspaces", &self.layout.workspaces().count())
            .field("placements", &self.placements.placements.len())
            .field("focused", &self.layout.focused_window())
            .field("backend", &self.backend)
            .field("running", &self.running)
            .finish_non_exhaustive()
    }
}

impl Irontile {
    pub fn new(
        display_handle: DisplayHandle,
        socket_name: String,
        config: Config,
        config_path: std::path::PathBuf,
    ) -> Self {
        let dh = &display_handle;
        let mut seat_state = SeatState::new();
        let seat = seat_state.new_wl_seat(dh, "irontile");
        let layout_config = config.layout;
        let config_cursor = config.cursor.clone();

        Self {
            display_handle: display_handle.clone(),
            start_time: Instant::now(),
            socket_name,
            running: true,
            dirty: true,
            compositor_state: CompositorState::new::<Self>(dh),
            xdg_shell_state: XdgShellState::new::<Self>(dh),
            layer_shell_state: WlrLayerShellState::new::<Self>(dh),
            xdg_decoration_state: XdgDecorationState::new::<Self>(dh),
            shm_state: ShmState::new::<Self>(dh, Vec::new()),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(dh),
            seat_state,
            data_device_state: DataDeviceState::new::<Self>(dh),
            primary_selection_state: PrimarySelectionState::new::<Self>(dh),
            cursor_shape_state: CursorShapeManagerState::new::<Self>(dh),
            fractional_scale_state: FractionalScaleManagerState::new::<Self>(dh),
            viewporter_state: smithay::wayland::viewporter::ViewporterState::new::<Self>(dh),
            popups: PopupManager::default(),
            seat,
            outputs: Vec::new(),
            peers: crate::ipc::Peers::default(),
            layout: Layout::new(layout_config),
            windows: Registry::default(),
            arrangement: Vec::new(),
            placements: Frame::default(),
            config,
            config_path,
            drag: None,
            backend: Backend::Headless,
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            cursor: crate::cursor::CursorSource::new(&config_cursor.theme, config_cursor.size),
            cursor_status: smithay::input::pointer::CursorImageStatus::default_named(),
        }
    }

    /// Advertises the buffer formats the renderer accepts.
    ///
    /// Called once a backend has its renderer. Without this clients fall back
    /// to shared memory, which means every frame is drawn on the CPU and copied.
    pub fn advertise_dmabuf(&mut self) {
        let Some(formats) = self.backend.dmabuf_formats() else {
            tracing::info!("no renderer; clients will use shared memory");
            return;
        };
        let count = formats.iter().count();

        // Version 4 of the protocol carries feedback naming the device to
        // allocate on. Without it a client has no way to pick a GPU and falls
        // back to shared memory, so this is the difference between clients
        // rendering on the GPU and rendering on the CPU.
        let global = match self.backend.dmabuf_main_device() {
            Some(device) => {
                match DmabufFeedbackBuilder::new(device, formats.clone()).build() {
                    Ok(feedback) => {
                        tracing::info!(formats = count, device, "advertising dmabuf with feedback");
                        self.dmabuf_state
                            .create_global_with_default_feedback::<Self>(
                                &self.display_handle,
                                &feedback,
                            )
                    }
                    Err(err) => {
                        // Falling back still works; clients just cannot tell
                        // which GPU to use.
                        tracing::warn!(%err, "could not build dmabuf feedback");
                        self.dmabuf_state
                            .create_global::<Self>(&self.display_handle, formats)
                    }
                }
            }
            None => {
                tracing::info!(formats = count, "advertising dmabuf");
                self.dmabuf_state
                    .create_global::<Self>(&self.display_handle, formats)
            }
        };
        self.dmabuf_global = Some(global);
    }

    /// The pointer image to draw on a display, and where its hotspot sits.
    ///
    /// `None` means the client asked for no pointer at all, or is drawing one
    /// itself through a surface, which the renderer handles separately.
    pub fn cursor_image(&mut self, scale: f64) -> Option<crate::cursor::CursorImage> {
        use smithay::input::pointer::CursorImageStatus;
        match self.cursor_status.clone() {
            CursorImageStatus::Hidden => None,
            CursorImageStatus::Named(icon) => Some(self.cursor.image(icon, scale)),
            // A surface is composited from its own buffer, not from a theme.
            CursorImageStatus::Surface(_) => None,
        }
    }

    /// The surface a client is drawing the pointer with, and its hotspot.
    pub fn cursor_surface(&self) -> Option<(WlSurface, (i32, i32))> {
        use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
        let CursorImageStatus::Surface(surface) = &self.cursor_status else {
            return None;
        };
        if !surface.alive() {
            return None;
        }
        let hotspot = smithay::wayland::compositor::with_states(surface, |states| {
            states
                .data_map
                .get::<CursorImageSurfaceData>()
                .map(|data| {
                    let attrs = data.lock().unwrap();
                    (attrs.hotspot.x, attrs.hotspot.y)
                })
                .unwrap_or((0, 0))
        });
        Some((surface.clone(), hotspot))
    }

    /// Re-reads the configuration file.
    ///
    /// A configuration that fails to load leaves the running one in place. The
    /// alternative -- exiting over a typo -- would take the session with it.
    pub fn reload_config(&mut self) {
        match Config::load_from(&self.config_path) {
            Ok(config) => {
                if config.cursor != self.config.cursor {
                    self.cursor =
                        crate::cursor::CursorSource::new(&config.cursor.theme, config.cursor.size);
                }
                self.layout.set_config(config.layout);
                self.config = config;
                self.dirty = true;
                tracing::info!("configuration reloaded");
            }
            Err(err) => tracing::error!(%err, "keeping the running configuration"),
        }
    }

    /// Replaces the set of connected displays.
    ///
    /// This is the one path by which displays appear, move, resize or go away,
    /// for every backend. It keeps the protocol objects clients see in step
    /// with the arrangement the layout engine works from.
    pub fn configure_outputs(&mut self, specs: &[OutputSpec]) -> Vec<Event> {
        let specs = &self.arrange(specs);
        // Withdraw displays that are gone, so clients stop referring to them.
        let keep: Vec<OutputId> = specs.iter().map(|s| s.id).collect();
        self.outputs.retain(|entry| {
            if keep.contains(&entry.id) {
                return true;
            }
            self.display_handle
                .remove_global::<Irontile>(entry.global.clone());
            false
        });

        for spec in specs {
            let mode = Mode {
                size: (spec.physical.w, spec.physical.h).into(),
                refresh: spec.refresh,
            };
            let entry = match self.outputs.iter().find(|e| e.id == spec.id) {
                Some(entry) => entry,
                None => {
                    // Adopted when the backend made one, so that a display has
                    // exactly one `Output` and every part of the compositor
                    // reads the same scale from it.
                    let output = spec.output.clone().unwrap_or_else(|| {
                        Output::new(
                            spec.name.clone(),
                            PhysicalProperties {
                                size: (0, 0).into(),
                                subpixel: Subpixel::Unknown,
                                make: "irontile".into(),
                                model: spec.name.clone(),
                            },
                        )
                    });
                    let global = output.create_global::<Irontile>(&self.display_handle);
                    self.outputs.push(OutputEntry {
                        id: spec.id,
                        output,
                        global,
                    });
                    self.outputs.last().expect("just pushed")
                }
            };
            entry.output.change_current_state(
                Some(mode),
                Some(spec.transform),
                Some(smithay::output::Scale::Fractional(spec.scale)),
                Some((spec.position.x, spec.position.y).into()),
            );
            entry.output.set_preferred(mode);
        }

        self.arrangement = specs.to_vec();
        let events = self.publish_outputs();
        self.number_unnamed_workspaces();
        events
    }

    /// Hands the layout engine the current displays and their work areas.
    ///
    /// The work area is a display minus whatever layer-shell surfaces have
    /// reserved on it, which is why this is separate from `configure_outputs`:
    /// a bar appearing changes the work area without changing the arrangement.
    pub fn publish_outputs(&mut self) -> Vec<Event> {
        let layout_outputs: Vec<LayoutOutput> = self
            .arrangement
            .iter()
            .map(|spec| {
                let logical = spec.logical();
                let mut output = LayoutOutput::new(spec.id, spec.name.clone(), logical);
                if let Some(entry) = self.outputs.iter().find(|e| e.id == spec.id) {
                    let mut map = smithay::desktop::layer_map_for_output(&entry.output);
                    // The zone is only recomputed when the map is arranged, and
                    // it is derived from the display's size. Reading it without
                    // arranging returns the zone for whatever size the display
                    // was when the map was first built, which is wrong the
                    // moment a display is resized.
                    map.arrange();
                    let zone = map.non_exclusive_zone();
                    // The zone is relative to its display; the layout engine
                    // works in one global coordinate space.
                    output.work_area = Rect::new(
                        logical.x + zone.loc.x,
                        logical.y + zone.loc.y,
                        zone.size.w,
                        zone.size.h,
                    );
                }
                output
            })
            .collect();
        let events = self.layout.reconfigure_outputs(layout_outputs);
        self.dirty = true;
        self.peers.broadcast(&events);
        events
    }

    /// Applies the configured settings to a set of displays and decides where
    /// the ones without a position go.
    ///
    /// Displays with an explicit position keep it, and the rest are laid end to
    /// end to the right of everything placed. That ordering matters: plugging
    /// in a monitor should never shift one the user has already positioned.
    pub fn arrange(&self, specs: &[OutputSpec]) -> Vec<OutputSpec> {
        let mut out: Vec<OutputSpec> = Vec::with_capacity(specs.len());
        let mut unplaced: Vec<usize> = Vec::new();

        for spec in specs {
            let mut spec = spec.clone();
            let mut placed = false;
            if let Some(config) = self.config.output(&spec.name) {
                if !config.enabled {
                    // A display turned off in the configuration is not part of
                    // the arrangement at all; the layout engine never sees it.
                    continue;
                }
                if let Some(scale) = config.scale {
                    spec.scale = scale;
                }
                if let Some(transform) = config.transform {
                    spec.transform = transform_of(transform);
                }
                if let Some((x, y)) = config.position {
                    spec.position = LayoutPoint::new(x, y);
                    placed = true;
                }
            }
            if !placed {
                unplaced.push(out.len());
            }
            out.push(spec);
        }

        let mut next_x = out
            .iter()
            .enumerate()
            .filter(|(i, _)| !unplaced.contains(i))
            .map(|(_, spec)| spec.logical().right())
            .max()
            .unwrap_or(0);
        for index in unplaced {
            let spec = &mut out[index];
            spec.position = LayoutPoint::new(next_x, 0);
            next_x += spec.logical().w;
        }
        out
    }

    /// The protocol object for a display.
    pub fn smithay_output(&self, id: OutputId) -> Option<&Output> {
        self.outputs.iter().find(|e| e.id == id).map(|e| &e.output)
    }

    /// The protocol object for the focused display.
    pub fn focused_smithay_output(&self) -> Option<&Output> {
        self.layout
            .focused_output()
            .and_then(|id| self.smithay_output(id))
            .or_else(|| self.outputs.first().map(|e| &e.output))
    }

    /// Gives every unnamed desktop the lowest free number.
    ///
    /// A desktop the layout engine created on its own -- the first one on a
    /// display, or the replacement left behind when a desktop is moved away --
    /// would otherwise have no name, and so no way to reach it by number.
    fn number_unnamed_workspaces(&mut self) {
        let unnamed: Vec<_> = self
            .layout
            .workspaces()
            .filter(|w| w.name.is_none())
            .map(|w| w.id)
            .collect();
        for ws in unnamed {
            let Some(number) =
                (1..=999).find(|n| self.layout.workspace_named(&n.to_string()).is_none())
            else {
                break;
            };
            let _ = self.layout.rename_workspace(ws, Some(number.to_string()));
        }
    }

    /// Resolves a desktop number to an id, creating the desktop on first use.
    ///
    /// The layout engine addresses desktops by id and knows nothing about
    /// numbering; numeric names are layered on here, which is what makes
    /// "switch to workspace 4" work without the engine having a fixed set of
    /// numbered slots.
    pub fn workspace_by_number(&mut self, number: u32) -> WorkspaceId {
        let name = number.to_string();
        if let Some(ws) = self.layout.workspace_named(&name) {
            return ws.id;
        }
        let workspace = self.layout.create_workspace(Some(name));
        // This does not go through `dispatch`, so the event has to be raised
        // here; a subscriber that missed it would never learn the desktop
        // exists.
        self.peers
            .broadcast(&[Event::WorkspaceCreated { workspace }]);
        workspace
    }

    /// Applies a command, reporting the resulting events.
    pub fn apply(&mut self, command: Command) -> Vec<Event> {
        match self.try_apply(command) {
            Ok(events) => events,
            // A binding pressed with nothing focused is a no-op, not a fault.
            Err(LayoutError::UnknownWindow(_)) => Vec::new(),
            Err(err) => {
                tracing::warn!(%err, "layout rejected a command");
                Vec::new()
            }
        }
    }

    /// Applies a command, surfacing a rejection so the control socket can
    /// report it rather than swallowing it.
    pub fn try_apply(&mut self, command: Command) -> Result<Vec<Event>, LayoutError> {
        // A display change has to go through `configure_outputs`, which also
        // creates and withdraws the protocol objects clients see. Letting it
        // reach the layout engine directly would leave the two disagreeing
        // about which displays exist.
        if let Command::ReconfigureOutputs { outputs } = &command {
            let specs: Vec<OutputSpec> = outputs
                .iter()
                .map(|o| {
                    // A caller naming displays over the socket describes them
                    // in logical terms, which is the only space it knows about.
                    OutputSpec::new(o.id, o.name.clone(), Size::new(o.logical.w, o.logical.h))
                        .at(LayoutPoint::new(o.logical.x, o.logical.y))
                })
                .collect();
            return Ok(self.configure_outputs(&specs));
        }

        let events = dispatch(&mut self.layout, command)?;
        if !events.is_empty() {
            self.dirty = true;
        }
        for event in &events {
            tracing::debug!(?event);
        }
        if events
            .iter()
            .any(|e| matches!(e, Event::WorkspaceCreated { .. }))
        {
            self.number_unnamed_workspaces();
        }
        // Subscribers see events from key bindings and from the socket alike.
        self.peers.broadcast(&events);
        Ok(events)
    }

    /// The rectangle the client's own surface occupies, once the border quad
    /// drawn behind it is accounted for.
    pub fn content_rect(&self, cell: Rect, kind: PlacementKind) -> Rect {
        match kind {
            // A fullscreen window covers the display outright; a border would
            // make it not fullscreen.
            PlacementKind::Fullscreen => cell,
            _ => cell.inset(self.config.theme.border_width),
        }
    }

    /// Recomputes the frame and pushes it out to clients.
    ///
    /// Configures are sent through `send_pending_configure`, which is a no-op
    /// when nothing about a window actually changed, so this is safe to call
    /// whenever anything might have moved.
    pub fn reflow(&mut self) {
        self.dirty = false;
        let computed = frame(&self.layout);

        // Configure every window the tree placed, including ones that have not
        // drawn yet: telling a new window its cell before it paints is what
        // stops it painting at the wrong size and then snapping.
        for placement in &computed.placements {
            self.configure(placement);
        }

        // What is actually on screen, though, is only what has drawn. Filtering
        // once here rather than at render time keeps hit testing and the
        // control socket agreeing with the display.
        let next = Frame {
            placements: computed
                .placements
                .into_iter()
                .filter(|p| !self.windows.is_unmapped(p.window))
                .collect(),
            focused: computed.focused,
        };

        if next != self.placements {
            for p in &next.placements {
                tracing::debug!(
                    window = p.window.0,
                    workspace = p.workspace.0,
                    kind = ?p.kind,
                    focused = p.focused,
                    rect = format!("{}x{}+{}+{}", p.rect.w, p.rect.h, p.rect.x, p.rect.y),
                    "placed"
                );
            }
        }
        self.placements = next;
        self.sync_borders();
        self.sync_surface_outputs();
        self.refresh_keyboard_focus();
    }

    /// Tells one client the cell it has been given.
    fn configure(&self, placement: &irontile_layout::Placement) {
        {
            let Some(window) = self.windows.window(placement.window) else {
                return;
            };
            let Some(toplevel) = window.toplevel() else {
                return;
            };
            let content = self.content_rect(placement.rect, placement.kind);
            let fullscreen = placement.kind == PlacementKind::Fullscreen;
            toplevel.with_pending_state(|state| {
                use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
                state.size = Some((content.w.max(1), content.h.max(1)).into());
                // Every edge is tiled: the client is not free to resize itself
                // and should square off its corners.
                state.states.set(State::TiledLeft);
                state.states.set(State::TiledRight);
                state.states.set(State::TiledTop);
                state.states.set(State::TiledBottom);
                if fullscreen {
                    state.states.set(State::Fullscreen);
                } else {
                    state.states.unset(State::Fullscreen);
                }
                if placement.focused {
                    state.states.set(State::Activated);
                } else {
                    state.states.unset(State::Activated);
                }
            });
            toplevel.send_pending_configure();
        }
    }

    /// The topmost layer surface asking for the keyboard, if any.
    ///
    /// Overlay before top before the rest, so a launcher drawn over a bar is
    /// the one that receives what is typed.
    fn keyboard_layer(&self) -> Option<smithay::desktop::LayerSurface> {
        use smithay::wayland::shell::wlr_layer::Layer;
        for wanted in [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background] {
            for entry in &self.outputs {
                let map = smithay::desktop::layer_map_for_output(&entry.output);
                if let Some(layer) = map
                    .layers()
                    .rev()
                    .find(|l| l.layer() == wanted && l.can_receive_keyboard_focus())
                {
                    return Some(layer.clone());
                }
            }
        }
        None
    }

    /// The scale of the display a surface is being shown on.
    ///
    /// Falls back to the focused display, which is where a surface that is not
    /// placed yet -- one still waiting for its first buffer -- will appear.
    fn scale_for_surface(&self, surface: &WlSurface) -> f64 {
        let output = self
            .windows
            .find(surface)
            .and_then(|(id, _)| {
                self.placements
                    .placements
                    .iter()
                    .find(|p| p.window == id)
                    .map(|p| p.output)
            })
            .or_else(|| self.layout.focused_output());
        output
            .and_then(|id| self.smithay_output(id))
            .map(|output| output.current_scale().fractional_scale())
            .unwrap_or(1.0)
    }

    /// Tells each window which display it is on, and at what scale.
    ///
    /// Without the enter event a client has no idea which display it is on and
    /// so renders at scale one, which the compositor then has to resample. This
    /// is what makes a window sharp on a scaled display rather than merely the
    /// right size.
    fn sync_surface_outputs(&self) {
        for entry in &self.outputs {
            let scale = display_scale(&entry.output);
            for placement in &self.placements.placements {
                let Some(window) = self.windows.window(placement.window) else {
                    continue;
                };
                let Some(surface) = window.toplevel().map(|t| t.wl_surface().clone()) else {
                    continue;
                };
                // The overlap is given in surface-local coordinates, so a
                // window on this display covers all of itself and a window
                // elsewhere covers none of it.
                let overlap = (placement.output == entry.id).then(|| {
                    smithay::utils::Rectangle::from_size(
                        (placement.rect.w, placement.rect.h).into(),
                    )
                });
                smithay::desktop::utils::output_update(&entry.output, overlap, &surface);

                if placement.output == entry.id {
                    smithay::wayland::compositor::with_states(&surface, |states| {
                        with_fractional_scale(states, |fractional| {
                            fractional.set_preferred_scale(scale);
                        });
                    });
                }
            }
        }
    }

    /// Brings each window's border strips in line with the frame.
    ///
    /// Done here rather than while rendering because the buffers have to be
    /// mutated, and because their commit counters are what tell damage tracking
    /// a border changed colour. Focus moving changes nothing else about the
    /// elements, so without this the highlight stays where it was.
    fn sync_borders(&mut self) {
        let width = self.config.theme.border_width;
        let focused = self.config.theme.border_focused;
        let unfocused = self.config.theme.border_unfocused;
        for placement in &self.placements.placements {
            let Some(entry) = self.windows.get_mut(placement.window) else {
                continue;
            };
            let color = if placement.focused {
                focused
            } else {
                unfocused
            };
            let visible = placement.kind != PlacementKind::Fullscreen && width > 0;
            // A fullscreen window has no border; empty strips draw nothing but
            // keep the element set stable.
            let rects = if visible {
                crate::render::border_rects(placement.rect, width)
            } else {
                [Rect::ZERO; 4]
            };
            for (buffer, rect) in entry.border.iter_mut().zip(rects) {
                buffer.update((rect.w.max(0), rect.h.max(0)), color);
            }
        }
    }

    fn refresh_keyboard_focus(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        // A popup grab owns the keyboard for as long as it lasts; stealing it
        // back on the next reflow would dismiss the menu the user just opened.
        if keyboard.is_grabbed() {
            return;
        }
        // A layer surface that asked for the keyboard outranks the tiling tree
        // -- that is what makes a launcher able to read what is typed into it.
        // The window underneath keeps its place and gets focus back when the
        // layer surface goes away.
        let target = self
            .keyboard_layer()
            .map(crate::focus::KeyboardFocus::Layer)
            .or_else(|| {
                self.placements
                    .focused
                    .and_then(|id| self.windows.window(id))
                    .cloned()
                    .map(crate::focus::KeyboardFocus::Window)
            });
        if keyboard.current_focus() == target {
            return;
        }
        keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
    }

    /// The bounding size of every display together.
    ///
    /// Absolute pointer input is placed against the whole arrangement rather
    /// than one screen, because a pointer crosses between displays.
    pub fn arrangement_size(&self) -> SmithaySize<i32, Logical> {
        let (mut w, mut h) = (0, 0);
        for output in self.layout.outputs() {
            w = w.max(output.logical.right());
            h = h.max(output.logical.bottom());
        }
        SmithaySize::from((w, h))
    }

    /// Releases the frame callbacks of one display's windows.
    ///
    /// Separate from [`Irontile::send_frame_callbacks`] because on real
    /// hardware each display flips independently, and a client should be paced
    /// by the display it is actually on.
    pub fn send_frame_callbacks_for(&self, output: OutputId) {
        let time = self.start_time.elapsed();
        let Some(smithay_output) = self.smithay_output(output) else {
            return;
        };
        for placement in &self.placements.placements {
            if placement.output != output {
                continue;
            }
            if let Some(window) = self.windows.window(placement.window) {
                let out = smithay_output.clone();
                window.send_frame(&out, time, None, |_, _| Some(out.clone()));
            }
        }
    }

    /// Logical size of a display.
    pub fn output_size(&self, id: OutputId) -> SmithaySize<i32, Logical> {
        self.layout
            .output(id)
            .map(|o| SmithaySize::from((o.logical.w, o.logical.h)))
            .unwrap_or_else(|| SmithaySize::from((0, 0)))
    }

    /// Releases the frame callbacks of everything that was just drawn, which is
    /// what lets animating clients produce their next buffer.
    pub fn send_frame_callbacks(&self) {
        let time = self.start_time.elapsed();
        for placement in &self.placements.placements {
            let (Some(window), Some(output)) = (
                self.windows.window(placement.window),
                self.smithay_output(placement.output),
            ) else {
                continue;
            };
            let output = output.clone();
            window.send_frame(&output, time, None, |_, _| Some(output.clone()));
        }
    }

    /// The surface under a point, topmost first, along with where that surface
    /// starts. Popups are included, which is why this walks windows rather than
    /// testing the placement rectangles directly: a menu sticks out past the
    /// cell its window occupies.
    pub fn surface_under(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let mut ordered: Vec<_> = self.placements.placements.iter().collect();
        ordered.sort_by_key(|p| std::cmp::Reverse(p.z));
        for placement in ordered {
            let window = self.windows.window(placement.window)?;
            let content = self.content_rect(placement.rect, placement.kind);
            let origin = Point::<i32, Logical>::from((content.x, content.y));
            let local = point - origin.to_f64();
            if let Some((surface, offset)) =
                window.surface_under(local, smithay::desktop::WindowSurfaceType::ALL)
            {
                return Some((surface, (origin + offset).to_f64()));
            }
        }
        None
    }

    /// The cell a window currently occupies.
    pub fn cell_of(&self, window: WindowId) -> Option<Rect> {
        self.placements
            .placements
            .iter()
            .find(|p| p.window == window)
            .map(|p| p.rect)
    }

    pub fn window_at(&self, point: Point<f64, Logical>) -> Option<WindowId> {
        let mut ordered: Vec<_> = self.placements.placements.iter().collect();
        ordered.sort_by_key(|p| std::cmp::Reverse(p.z));
        ordered
            .into_iter()
            .find(|p| {
                p.rect
                    .contains(irontile_layout::Point::new(point.x as i32, point.y as i32))
            })
            .map(|p| p.window)
    }

    pub fn close_focused(&mut self) {
        let Some(id) = self.layout.focused_window() else {
            return;
        };
        if let Some(window) = self.windows.window(id)
            && let Some(toplevel) = window.toplevel()
        {
            toplevel.send_close();
        }
    }

    /// Sends the desktop currently on screen to the display in `dir`.
    ///
    /// With one display this does nothing, but it is the same call a
    /// multi-display session makes, so the binding does not have to change when
    /// a second monitor appears.
    pub fn send_workspace_to_output(&mut self, dir: Direction) -> Vec<Event> {
        let Some(from) = self.layout.focused_output() else {
            return Vec::new();
        };
        let Some(to) = self.layout.output_in_direction(from, dir) else {
            return Vec::new();
        };
        let Some(ws) = self.layout.active_workspace(from) else {
            return Vec::new();
        };
        self.apply(Command::ShowWorkspace {
            workspace: ws,
            output: Some(to),
        })
    }

    /// Launches whatever the configuration says to launch at startup.
    pub fn run_startup_commands(&self) {
        for argv in &self.config.startup {
            self.spawn(argv);
        }
    }

    pub fn spawn(&self, argv: &[String]) {
        let Some((program, args)) = argv.split_first() else {
            return;
        };
        let mut command = std::process::Command::new(program);
        command.args(args);
        command.env("WAYLAND_DISPLAY", &self.socket_name);
        command.env(
            "IRONTILE_SOCKET",
            irontile_ipc::socket_path(&self.socket_name),
        );
        // Clients predating `wp_cursor_shape_v1` load the theme themselves from
        // these. Without them a client picks a different theme than the one the
        // compositor draws, and the pointer changes appearance depending on
        // which window it happens to be over.
        if !self.cursor.theme_name().is_empty() {
            command.env("XCURSOR_THEME", self.cursor.theme_name());
        }
        command.env("XCURSOR_SIZE", self.cursor.base_size().to_string());
        // Children must not inherit the parent session's display, or they would
        // connect to the compositor irontile is nested inside instead.
        command.env_remove("DISPLAY");
        match command.spawn() {
            Ok(_) => tracing::info!(program, "spawned"),
            Err(err) => tracing::warn!(program, %err, "failed to spawn"),
        }
    }

    /// What the control socket reports for `windows`.
    pub fn window_infos(&self) -> Vec<irontile_ipc::WindowInfo> {
        let focused = self.layout.focused_window();
        self.layout
            .workspaces()
            .flat_map(|ws| {
                let output = self.layout.output_showing(ws.id);
                ws.windows().into_iter().map(move |window| (window, ws.id, output))
            })
            .filter_map(|(window, workspace, output)| {
                let (title, app_id) = self.window_names(window);
                Some(irontile_ipc::WindowInfo {
                    id: window,
                    title,
                    app_id,
                    workspace,
                    output,
                    focused: focused == Some(window),
                })
            })
            .collect()
    }

    /// What a window calls itself, if it has said.
    pub fn window_names(&self, window: WindowId) -> (Option<String>, Option<String>) {
        let Some(toplevel) = self.windows.window(window).and_then(|w| w.toplevel()) else {
            return (None, None);
        };
        smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .map(|data| {
                    let attrs = data.lock().unwrap();
                    (attrs.title.clone(), attrs.app_id.clone())
                })
                .unwrap_or((None, None))
        })
    }

    /// What the control socket reports for `workspaces`.
    pub fn workspace_summaries(&self) -> Vec<irontile_ipc::WorkspaceSummary> {
        let focused = self.layout.focused_workspace();
        self.layout
            .workspaces()
            .map(|ws| irontile_ipc::WorkspaceSummary {
                id: ws.id,
                name: ws.name.clone(),
                output: self.layout.output_showing(ws.id),
                focused: Some(ws.id) == focused,
                windows: ws.windows(),
            })
            .collect()
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _id: ClientId) {}
    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

impl CompositorHandler for Irontile {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        smithay::backend::renderer::utils::on_commit_buffer_handler::<Self>(surface);
        self.popups.commit(surface);

        if let Some(layer) = self.layer_for_surface(surface) {
            // A layer surface must be configured before it may attach a buffer,
            // and re-arranged after, since its size or exclusive zone may have
            // changed what is left for everything else.
            layer.layer_surface().send_configure();
            self.refresh_layers();
            return;
        }

        let Some((id, window)) = self.windows.find(surface) else {
            return;
        };
        window.on_commit();

        if self.windows.is_unmapped(id) {
            if has_buffer(surface) && self.windows.mark_mapped(id) {
                // It was already given a cell when it appeared; this is only
                // the point at which it starts being drawn.
                self.dirty = true;
            }
            return;
        }
        // A mapped client may have committed a size of its own; re-running the
        // layout puts it back in the cell the tree assigned it.
        self.dirty = true;
    }
}

impl BufferHandler for Irontile {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
    }
}

impl DmabufHandler for Irontile {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if self.backend.import_dmabuf(&dmabuf) {
            let _ = notifier.successful::<Irontile>();
        } else {
            // Refusing is what lets the client fall back to shared memory
            // instead of drawing into a buffer that will never be shown.
            notifier.failed();
        }
    }
}

impl ShmHandler for Irontile {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for Irontile {
    type KeyboardFocus = crate::focus::KeyboardFocus;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&Self::KeyboardFocus>) {}

    /// A client asked the pointer to look like something.
    ///
    /// Remembered rather than acted on: what it should look like is only needed
    /// when a frame is actually being drawn.
    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        self.cursor_status = image;
    }
}

impl smithay::wayland::output::OutputHandler for Irontile {}

impl SelectionHandler for Irontile {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Irontile {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

/// The cursor-shape protocol covers tablet tools as well as pointers, so it
/// requires this even on a compositor with no tablet support of its own.
impl smithay::wayland::tablet_manager::TabletSeatHandler for Irontile {}

impl FractionalScaleHandler for Irontile {
    /// A client asked what scale its surface is being shown at.
    ///
    /// Answering immediately matters: a client that gets no reply falls back to
    /// the whole-number scale from `wl_output`, renders at that, and is then
    /// resampled to the real one.
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self.scale_for_surface(&surface);
        smithay::wayland::compositor::with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}

impl PrimarySelectionHandler for Irontile {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

impl ClientDndGrabHandler for Irontile {}

impl ServerDndGrabHandler for Irontile {
    fn send(&mut self, _mime: String, _fd: std::os::unix::io::OwnedFd, _seat: Seat<Self>) {}
}

delegate_compositor!(Irontile);
smithay::delegate_dmabuf!(Irontile);
smithay::delegate_fractional_scale!(Irontile);
smithay::delegate_viewporter!(Irontile);
smithay::delegate_primary_selection!(Irontile);
smithay::delegate_cursor_shape!(Irontile);
delegate_shm!(Irontile);
delegate_seat!(Irontile);
delegate_data_device!(Irontile);
delegate_output!(Irontile);

/// Whether a surface has committed a buffer, and so has something to show.
fn has_buffer(surface: &WlSurface) -> bool {
    smithay::backend::renderer::utils::with_renderer_surface_state(surface, |state| {
        state.buffer().is_some()
    })
    .unwrap_or(false)
}

/// Maps a configured orientation onto the one smithay understands.
fn transform_of(transform: crate::config::OutputTransform) -> smithay::utils::Transform {
    use crate::config::OutputTransform as T;
    use smithay::utils::Transform as S;
    match transform {
        T::Normal => S::Normal,
        T::Rotate90 => S::_90,
        T::Rotate180 => S::_180,
        T::Rotate270 => S::_270,
        T::Flipped => S::Flipped,
        T::Flipped90 => S::Flipped90,
        T::Flipped180 => S::Flipped180,
        T::Flipped270 => S::Flipped270,
    }
}

fn display_scale(output: &Output) -> f64 {
    output.current_scale().fractional_scale()
}
