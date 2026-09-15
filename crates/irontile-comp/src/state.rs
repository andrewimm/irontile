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
use smithay::backend::renderer::element::solid::SolidColorBuffer;
use smithay::desktop::{PopupManager, layer_map_for_output};
use smithay::input::keyboard::Keycode;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::{LoopHandle, RegistrationToken};
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

/// How long a held binding waits before it starts repeating, and how many times
/// a second it repeats after that. The same numbers clients are told to use for
/// typing, so a held binding and a held letter feel the same.
pub const REPEAT_DELAY_MS: i32 = 200;
pub const REPEAT_RATE_HZ: i32 = 25;

/// A binding firing over and over while its key is held.
#[derive(Debug)]
pub struct KeyRepeat {
    /// The key holding it open. Only its release ends the repeat, so rolling
    /// onto another key does not silently leave one running.
    pub code: Keycode,
    pub token: RegistrationToken,
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
pub struct Drag {
    pub window: WindowId,
    /// Where the pointer was at the last motion, so each step is a delta.
    pub last: Point<f64, Logical>,
    pub kind: DragKind,
}

/// What a drag is doing to the window it holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DragKind {
    Resize {
        /// Which edges follow the pointer. Either may be absent: grabbing along
        /// one edge resizes in that axis alone, and only a corner moves both.
        horizontal: Option<Direction>,
        vertical: Option<Direction>,
    },
    /// The whole window follows the pointer.
    ///
    /// Only a floating window can be moved this way. Where a tiled one sits is
    /// the layout's to decide, and dragging it somewhere would be overruled by
    /// the next reflow.
    Move,
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
    pub session_lock_state: smithay::wayland::session_lock::SessionLockManagerState,
    /// Set while the session is locked. See [`crate::lock`].
    pub session_lock: Option<crate::lock::Lock>,
    /// Screenshots that have been asked for and not yet taken.
    pub screencopy: crate::screencopy::Screencopy,
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
    pub arrangement: Vec<OutputSpec>,
    pub placements: Frame,
    pub config: Config,
    /// Where the configuration came from, so a reload reads the same file.
    pub config_path: std::path::PathBuf,
    /// Set while the pointer is resizing a window.
    pub drag: Option<Drag>,
    /// Total travel of a swipe in progress, if it has the right number of
    /// fingers. `None` means no swipe is being followed.
    pub swipe: Option<(f64, f64)>,
    /// The binding currently repeating, if a key is being held down.
    pub repeat: Option<KeyRepeat>,
    /// Children started and not yet waited for. See [`Irontile::spawn`].
    children: std::cell::RefCell<Vec<std::process::Child>>,
    /// Kept so that a held binding can drive itself from a timer. Holding a key
    /// produces no further events -- repeat is the compositor's job, and for a
    /// binding it cannot be handed to the client the way typing is.
    pub loop_handle: LoopHandle<'static, Irontile>,
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
    /// A pointer image the compositor is imposing, whatever the client asked
    /// for.
    ///
    /// Resizing is the compositor's gesture, not the window's, so the window
    /// has no way to know it should be showing a resize arrow -- and over the
    /// gap between two windows there is no client to ask. Set while the pointer
    /// is on an edge it could grab, and for as long as a drag lasts.
    pub cursor_hint: Option<smithay::input::pointer::CursorIcon>,
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
        loop_handle: LoopHandle<'static, Irontile>,
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
            session_lock_state: smithay::wayland::session_lock::SessionLockManagerState::new::<
                Self,
                _,
            >(
                dh,
                // Any client may lock. A compositor has no way to tell the lock
                // screen it was configured with from anything else asking, and
                // refusing everything would mean no lock at all.
                |_| true,
            ),
            session_lock: None,
            screencopy: crate::screencopy::Screencopy::new(dh),
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
            swipe: None,
            repeat: None,
            children: std::cell::RefCell::new(Vec::new()),
            loop_handle,
            backend: Backend::Headless,
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            cursor: crate::cursor::CursorSource::new(&config_cursor.theme, config_cursor.size),
            cursor_status: smithay::input::pointer::CursorImageStatus::default_named(),
            cursor_hint: None,
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
        // What the compositor is doing outranks what the window last asked for.
        if let Some(icon) = self.cursor_hint {
            return Some(self.cursor.image(icon, scale));
        }
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
        // A client drawing its own pointer still does not get to draw one for a
        // gesture that is not its own.
        if self.cursor_hint.is_some() {
            return None;
        }
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

    /// The desktop `step` places along from the one on screen.
    ///
    /// Counted by number rather than by walking the desktops that exist, so a
    /// swipe past the last one makes the next -- the same as pressing its
    /// number would. Desktops are numbered from one, so stepping back from the
    /// first stays there rather than wrapping round to somewhere unexpected.
    pub fn workspace_step(&mut self, step: i32) -> WorkspaceId {
        let current = self
            .layout
            .focused_output()
            .and_then(|output| self.layout.active_workspace(output))
            .and_then(|id| self.layout.workspaces().find(|ws| ws.id == id))
            .and_then(|ws| ws.name.as_deref())
            .and_then(|name| name.parse::<i32>().ok())
            .unwrap_or(1);
        self.workspace_by_number(current.saturating_add(step).max(1) as u32)
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
        // A layer surface belongs to a display outright rather than through a
        // placement, and it asks this the moment it binds the object -- before
        // it has drawn anything, and so before it appears in any placement.
        if let Some(entry) = self.outputs.iter().find(|entry| {
            layer_map_for_output(&entry.output)
                .layer_for_surface(surface, smithay::desktop::WindowSurfaceType::ALL)
                .is_some()
        }) {
            return display_scale(&entry.output);
        }
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

            // Panels first. A layer surface is on exactly the display that owns
            // it, so it is told so outright -- and without that a bar renders
            // at scale one and is resampled, which is the one thing a bar full
            // of text cannot afford.
            let layers: Vec<_> = {
                // One guard: taking the map again while it is already held is a
                // panic, not a borrow error.
                let map = layer_map_for_output(&entry.output);
                map.layers()
                    .filter_map(|layer| {
                        let size = map.layer_geometry(layer)?.size;
                        Some((layer.wl_surface().clone(), size))
                    })
                    // A panel between sizes has no rectangle for a moment, and
                    // saying so would tell it that it has left the display it
                    // is sitting on. It has not: a layer surface belongs to the
                    // display that owns it for as long as it is mapped, and
                    // going quiet for a frame says nothing untrue.
                    //
                    // A client that hears "you have left" believes it, and a
                    // toolkit that sizes itself as a share of its monitor then
                    // has no monitor to take a share of.
                    .filter(|(_, size)| size.w > 0 && size.h > 0)
                    .collect()
            };
            for (surface, size) in layers {
                smithay::desktop::utils::output_update(
                    &entry.output,
                    Some(smithay::utils::Rectangle::from_size(size)),
                    &surface,
                );
                smithay::wayland::compositor::with_states(&surface, |states| {
                    with_fractional_scale(states, |fractional| {
                        fractional.set_preferred_scale(scale);
                    });
                });
            }

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

    /// Every panel and overlay on screen, bottom stratum first.
    ///
    /// A layer surface is neither a window nor a display, so nothing else
    /// reported here describes one -- which leaves a bar or a notification that
    /// fails to appear with nothing to look at but the screen it is not on.
    pub fn layer_infos(&self) -> Vec<irontile_ipc::LayerInfo> {
        use irontile_ipc::{LayerInfo, LayerKind};
        use smithay::wayland::shell::wlr_layer::{ExclusiveZone, KeyboardInteractivity, Layer};

        let mut out = Vec::new();
        for spec in &self.arrangement {
            let Some(entry) = self.outputs.iter().find(|e| e.id == spec.id) else {
                continue;
            };
            let area = spec.logical();
            let map = layer_map_for_output(&entry.output);
            for layer in map.layers() {
                let Some(geometry) = map.layer_geometry(layer) else {
                    continue;
                };
                // A panel that has unmapped itself keeps its place in the map
                // with nothing in it. It is not on screen, and reporting it as
                // though it were is how "it never opened again" reads as "it is
                // still open".
                if geometry.size.w <= 0 || geometry.size.h <= 0 {
                    continue;
                }
                let state = layer.cached_state();
                out.push(LayerInfo {
                    namespace: layer.namespace().to_owned(),
                    layer: match state.layer {
                        Layer::Background => LayerKind::Background,
                        Layer::Bottom => LayerKind::Bottom,
                        Layer::Top => LayerKind::Top,
                        Layer::Overlay => LayerKind::Overlay,
                    },
                    output: spec.id,
                    // The map works in its display's coordinates; everything
                    // reported over the socket is in the shared one.
                    rect: Rect::new(
                        area.x + geometry.loc.x,
                        area.y + geometry.loc.y,
                        geometry.size.w,
                        geometry.size.h,
                    ),
                    // Neutral means "leave me where the others put me" and
                    // DontCare means "ignore everyone else"; neither reserves
                    // anything, which is what this number is about.
                    exclusive: match state.exclusive_zone {
                        ExclusiveZone::Exclusive(n) => n as i32,
                        ExclusiveZone::Neutral | ExclusiveZone::DontCare => 0,
                    },
                    keyboard: !matches!(state.keyboard_interactivity, KeyboardInteractivity::None),
                });
            }
        }
        out
    }

    /// Brings each window's border strips in line with the frame.
    ///
    /// Done here rather than while rendering because the buffers have to be
    /// mutated, and because their commit counters are what tell damage tracking
    /// a border changed colour. Focus moving changes nothing else about the
    /// elements, so without this the highlight stays where it was.
    fn sync_borders(&mut self) {
        let width = self.config.theme.border_width;
        let focused = self.config.theme.border_focused.clone();
        let unfocused = self.config.theme.border_unfocused.clone();
        for placement in &self.placements.placements {
            let Some(entry) = self.windows.get_mut(placement.window) else {
                continue;
            };
            let paint = if placement.focused {
                &focused
            } else {
                &unfocused
            };
            // A fullscreen window has no border at all, so it owns no strips.
            let visible = placement.kind != PlacementKind::Fullscreen && width > 0;
            let cell = placement.rect;
            let segments = if visible {
                crate::render::border_segments(cell, width, !paint.is_solid())
            } else {
                Vec::new()
            };
            entry
                .border
                .resize_with(segments.len(), SolidColorBuffer::default);
            for (buffer, rect) in entry.border.iter_mut().zip(&segments) {
                let color = crate::render::segment_color(paint, cell, *rect);
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
        // A locked session gives the keyboard to the lock screen and to nothing
        // else. This is the whole guarantee, so it is checked before anything
        // that could outrank it -- and it holds even when the locker has drawn
        // nothing yet, because a keystroke reaching a window behind a
        // half-drawn lock screen is exactly what must not happen.
        if let Some(lock) = &self.session_lock {
            let target = lock
                .keyboard_target()
                .cloned()
                .map(crate::focus::KeyboardFocus::Lock);
            if keyboard.current_focus() != target {
                keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
            }
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
        self.send_panel_frame_callbacks(output, time);
    }

    /// Releases the frame callbacks of the panels on a display.
    ///
    /// A toolkit asks for one of these and waits for it before drawing again,
    /// so a panel that never receives one draws exactly once and then stops --
    /// which looks like a client that renders badly rather than a compositor
    /// that never replied. irontile's own bar draws on its own schedule and so
    /// never noticed; everything else does notice.
    fn send_panel_frame_callbacks(&self, output: OutputId, time: std::time::Duration) {
        let Some(entry) = self.outputs.iter().find(|entry| entry.id == output) else {
            return;
        };
        let map = layer_map_for_output(&entry.output);
        for layer in map.layers() {
            let out = entry.output.clone();
            layer.send_frame(&out, time, None, |_, _| Some(out.clone()));
        }
    }

    /// Which display a client's `wl_output` is, if it is one of ours.
    pub fn output_id_of(
        &self,
        output: &smithay::reexports::wayland_server::protocol::wl_output::WlOutput,
    ) -> Option<OutputId> {
        let found = smithay::output::Output::from_resource(output)?;
        self.outputs
            .iter()
            .find(|entry| entry.output == found)
            .map(|entry| entry.id)
    }

    /// A display's size in its own pixels, which is what a copy of it is
    /// measured in.
    pub fn output_pixels(
        &self,
        id: OutputId,
    ) -> smithay::utils::Size<i32, smithay::utils::Physical> {
        self.arrangement
            .iter()
            .find(|spec| spec.id == id)
            .map(|spec| (spec.physical.w, spec.physical.h).into())
            .unwrap_or_else(|| (0, 0).into())
    }

    /// How many of those pixels there are to a logical one.
    pub fn output_scale(&self, id: OutputId) -> f64 {
        self.arrangement
            .iter()
            .find(|spec| spec.id == id)
            .map(|spec| spec.scale)
            .unwrap_or(1.0)
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
        for entry in &self.outputs {
            self.send_panel_frame_callbacks(entry.id, time);
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
        use smithay::wayland::shell::wlr_layer::Layer;

        // A locked session has exactly one thing under the pointer: the lock
        // screen for the display it is on. Everything else is behind it and
        // must stay unreachable, drawn or not.
        if let Some(lock) = &self.session_lock {
            for spec in &self.arrangement {
                let area = spec.logical();
                if !area.contains(irontile_layout::Point::new(point.x as i32, point.y as i32)) {
                    continue;
                }
                let surface = lock.surface_for(spec.id)?;
                let origin = Point::<i32, Logical>::from((area.x, area.y));
                return Some((surface.wl_surface().clone(), origin.to_f64()));
            }
            return None;
        }

        // Panels above the windows, then the windows, then panels below them --
        // the order things are drawn in, which is the order they are under the
        // pointer in. Without the panels a bar receives no pointer events at
        // all: not a click on a desktop button, not the pointer resting on a
        // module, not even an enter.
        if let Some(found) = self.layer_under(point, &[Layer::Overlay, Layer::Top]) {
            return Some(found);
        }

        let mut ordered: Vec<_> = self.placements.placements.iter().collect();
        ordered.sort_by_key(|p| std::cmp::Reverse(p.z));
        for placement in ordered {
            // A placement whose window has gone is one to skip rather than a
            // reason to stop looking: everything behind it is still there.
            let Some(window) = self.windows.window(placement.window) else {
                continue;
            };
            let content = self.content_rect(placement.rect, placement.kind);
            // The same shift the renderer applies, for the same reason: what is
            // drawn at the cell's corner is the window, not the buffer, and the
            // pointer has to be told about a surface in the coordinates that
            // surface was actually put on screen in. Hit testing the buffer
            // instead would miss along one edge and overshoot along the other,
            // by exactly the width of the client's shadows.
            let inset = window.geometry().loc;
            let origin = Point::<i32, Logical>::from((content.x - inset.x, content.y - inset.y));
            let local = point - origin.to_f64();
            if let Some((surface, offset)) =
                window.surface_under(local, smithay::desktop::WindowSurfaceType::ALL)
            {
                return Some((surface, (origin + offset).to_f64()));
            }
        }

        self.layer_under(point, &[Layer::Bottom, Layer::Background])
    }

    /// The layer surface under a point, in the given layers, topmost first.
    fn layer_under(
        &self,
        point: Point<f64, Logical>,
        layers: &[smithay::wayland::shell::wlr_layer::Layer],
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        for spec in &self.arrangement {
            let area = spec.logical();
            if !area.contains(irontile_layout::Point::new(point.x as i32, point.y as i32)) {
                continue;
            }
            let Some(entry) = self.outputs.iter().find(|e| e.id == spec.id) else {
                continue;
            };
            // A layer map works in its own display's coordinates; everything
            // else here is in the one space the displays share.
            let origin = Point::<i32, Logical>::from((area.x, area.y));
            let local = point - origin.to_f64();
            let map = layer_map_for_output(&entry.output);
            for layer in layers {
                let Some(surface) = map.layer_under(*layer, local) else {
                    continue;
                };
                let Some(geometry) = map.layer_geometry(surface) else {
                    continue;
                };
                if let Some((found, offset)) = surface.surface_under(
                    local - geometry.loc.to_f64(),
                    smithay::desktop::WindowSurfaceType::ALL,
                ) {
                    return Some((found, (origin + geometry.loc + offset).to_f64()));
                }
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

    /// The window edges near a point, for a drag that resizes by grabbing one.
    ///
    /// Only the strip a window's own border and the gap beside it occupy counts,
    /// which is the part of the screen no client has a surface on. Reaching
    /// inside the window instead would be a wider target and a worse trade: the
    /// last few pixels of a window are where scrollbars live, and a compositor
    /// that swallows clicks there breaks every application that has one.
    ///
    /// Either direction may be `None`. Grabbing along an edge resizes in one
    /// axis; grabbing a corner resizes in both.
    pub fn edges_near(
        &self,
        point: Point<f64, Logical>,
        grip: i32,
    ) -> Option<(WindowId, Option<Direction>, Option<Direction>)> {
        let (x, y) = (point.x as i32, point.y as i32);
        let mut ordered: Vec<_> = self.placements.placements.iter().collect();
        ordered.sort_by_key(|p| std::cmp::Reverse(p.z));

        for placement in ordered {
            // A fullscreen window has no edge to grab: there is nothing beside
            // it to give the space to.
            if placement.kind == PlacementKind::Fullscreen {
                continue;
            }
            let cell = placement.rect;
            let near = |a: i32, b: i32| (a - b).abs() <= grip;
            let within = x >= cell.x - grip
                && x <= cell.x + cell.w + grip
                && y >= cell.y - grip
                && y <= cell.y + cell.h + grip;
            if !within {
                continue;
            }

            let horizontal = if near(x, cell.x) {
                Some(Direction::Left)
            } else if near(x, cell.x + cell.w) {
                Some(Direction::Right)
            } else {
                None
            };
            let vertical = if near(y, cell.y) {
                Some(Direction::Up)
            } else if near(y, cell.y + cell.h) {
                Some(Direction::Down)
            } else {
                None
            };
            if horizontal.is_some() || vertical.is_some() {
                return Some((placement.window, horizontal, vertical));
            }
        }
        None
    }

    /// Gives the seat its keyboard, with the configured keymap.
    ///
    /// A keymap that will not compile falls back to the default rather than
    /// refusing to start. A mistyped xkb option is a plausible thing to have in
    /// a file, and on real hardware a compositor that will not start over one
    /// leaves no session to fix it from.
    pub fn add_keyboard(&mut self) -> anyhow::Result<()> {
        use anyhow::Context as _;
        let keyboard = self.config.input.keyboard.clone();
        match self
            .seat
            .add_keyboard(keyboard.xkb(), REPEAT_DELAY_MS, REPEAT_RATE_HZ)
        {
            Ok(_) => Ok(()),
            Err(err) => {
                tracing::error!(
                    %err,
                    layout = keyboard.layout,
                    options = keyboard.options,
                    "the configured keymap would not compile; using the default"
                );
                self.seat
                    .add_keyboard(Default::default(), REPEAT_DELAY_MS, REPEAT_RATE_HZ)
                    .map(|_| ())
                    .context("failed to create a keyboard")
            }
        }
    }

    /// The topmost floating window under a point.
    ///
    /// Tiled windows are deliberately not returned: a drag would move one and
    /// the next reflow would put it straight back.
    pub fn floating_at(&self, point: Point<f64, Logical>) -> Option<WindowId> {
        let at = LayoutPoint::new(point.x as i32, point.y as i32);
        let mut ordered: Vec<_> = self.placements.placements.iter().collect();
        ordered.sort_by_key(|p| std::cmp::Reverse(p.z));
        ordered
            .into_iter()
            .find(|p| p.kind == PlacementKind::Floating && p.rect.contains(at))
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
        // Anything already finished is cleared out first. A process nobody
        // waits on stays in the table as a zombie, and a binding that repeats
        // -- a volume key held down -- starts one every forty milliseconds.
        // Left alone they accumulate until the process table is full, and then
        // nothing can be spawned at all: the binding that worked this morning
        // silently does nothing, which is a miserable thing to debug.
        let mut children = self.children.borrow_mut();
        children.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
        match command.spawn() {
            Ok(child) => {
                children.push(child);
                tracing::info!(program, "spawned");
            }
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
                ws.windows()
                    .into_iter()
                    .map(move |window| (window, ws.id, output))
            })
            .map(|(window, workspace, output)| {
                let (title, app_id) = self.window_names(window);
                irontile_ipc::WindowInfo {
                    id: window,
                    title,
                    app_id,
                    workspace,
                    output,
                    focused: focused == Some(window),
                }
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
    fn initialized(&self, id: ClientId) {
        tracing::debug!(?id, "client connected");
    }

    /// Why a client went away, which is otherwise invisible from this side.
    ///
    /// A client that is gone explains everything it used to do that no longer
    /// happens, and a protocol error says whose fault that was: the compositor
    /// disconnects a client it thinks has misbehaved, and from the outside that
    /// is indistinguishable from the client simply never coming back.
    fn disconnected(&self, id: ClientId, reason: DisconnectReason) {
        match reason {
            DisconnectReason::ConnectionClosed => {
                tracing::debug!(?id, "client disconnected");
            }
            other => tracing::warn!(?id, ?other, "client dropped"),
        }
    }
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

        // A lock screen is only locked once every display is covered by a
        // surface that has actually drawn something. Confirming on the
        // configure instead would say the session is locked while a display
        // still shows what was on it.
        if self
            .session_lock
            .as_ref()
            .is_some_and(|lock| lock.owns(surface))
        {
            let outputs: Vec<_> = self.arrangement.iter().map(|spec| spec.id).collect();
            if has_buffer(surface)
                && let Some(lock) = &mut self.session_lock
                && lock.covers(&outputs)
                && lock.confirm()
            {
                tracing::info!("the session is locked");
            }
            self.dirty = true;
            return;
        }

        if let Some(layer) = self.layer_for_surface(surface) {
            // A layer surface must be configured before it may attach a
            // buffer, and re-arranged after, since its size or exclusive zone
            // may have changed what is left for everything else.
            //
            // With no buffer attached this is either the surface's first commit
            // or one that has just unmapped itself, and layer-shell says a
            // surface returns to its initial state when it unmaps: it may not
            // attach another buffer until it is configured again. Leaving that
            // to `send_pending_configure` means nothing is sent, because
            // nothing about the configuration changed -- and a panel that hides
            // itself then waits for ever to be allowed back. Once it is mapped
            // the opposite applies: a configure on every commit tells a client
            // to redraw because it just drew.
            if has_buffer(surface) {
                layer.layer_surface().send_pending_configure();
            } else {
                layer.layer_surface().send_configure();
            }
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
