//! Compositor state and protocol plumbing.
//!
//! [`Irontile`] holds the Wayland protocol state alongside an
//! [`irontile_layout::Layout`]. It owns no layout policy of its own: protocol
//! events become layout commands, and the [`Frame`] that comes back becomes
//! surface configures and render elements.

use std::time::Instant;

use irontile_layout::{
    Command, Direction, Event, Frame, InsertTarget, Layout, LayoutError, Output as LayoutOutput,
    OutputId, PlacementKind, Rect, WindowId, WorkspaceId, dispatch, frame,
};
use smithay::desktop::PopupManager;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DisplayHandle};
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size as SmithaySize};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
};

use crate::config::Config;
use crate::registry::Registry;

/// The display of the nested backend, which always has exactly one.
pub const NESTED_OUTPUT: OutputId = OutputId(1);

/// A display as a backend describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSpec {
    pub id: OutputId,
    pub name: String,
    /// Position and size in the global logical coordinate space.
    pub logical: Rect,
    pub refresh: i32,
    pub transform: smithay::utils::Transform,
}

impl OutputSpec {
    pub fn new(id: OutputId, name: impl Into<String>, logical: Rect) -> Self {
        Self {
            id,
            name: name.into(),
            logical,
            refresh: 60_000,
            transform: smithay::utils::Transform::Normal,
        }
    }
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
    pub shm_state: ShmState,
    /// Held for its protocol globals; dropping it would withdraw `wl_output`
    /// and `xdg_output` from clients.
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
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
    pub placements: Frame,
    pub config: Config,
    /// Where the configuration came from, so a reload reads the same file.
    pub config_path: std::path::PathBuf,
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

        Self {
            display_handle: display_handle.clone(),
            start_time: Instant::now(),
            socket_name,
            running: true,
            dirty: true,
            compositor_state: CompositorState::new::<Self>(dh),
            xdg_shell_state: XdgShellState::new::<Self>(dh),
            shm_state: ShmState::new::<Self>(dh, Vec::new()),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(dh),
            seat_state,
            data_device_state: DataDeviceState::new::<Self>(dh),
            popups: PopupManager::default(),
            seat,
            outputs: Vec::new(),
            peers: crate::ipc::Peers::default(),
            layout: Layout::new(layout_config),
            windows: Registry::default(),
            placements: Frame::default(),
            config,
            config_path,
        }
    }

    /// Re-reads the configuration file.
    ///
    /// A configuration that fails to load leaves the running one in place. The
    /// alternative -- exiting over a typo -- would take the session with it.
    pub fn reload_config(&mut self) {
        match Config::load_from(&self.config_path) {
            Ok(config) => {
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
                size: (spec.logical.w, spec.logical.h).into(),
                refresh: spec.refresh,
            };
            let entry = match self.outputs.iter().find(|e| e.id == spec.id) {
                Some(entry) => entry,
                None => {
                    let output = Output::new(
                        spec.name.clone(),
                        PhysicalProperties {
                            size: (0, 0).into(),
                            subpixel: Subpixel::Unknown,
                            make: "irontile".into(),
                            model: spec.name.clone(),
                        },
                    );
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
                None,
                Some((spec.logical.x, spec.logical.y).into()),
            );
            entry.output.set_preferred(mode);
        }

        let layout_outputs: Vec<LayoutOutput> = specs
            .iter()
            .map(|spec| {
                // The nested and headless backends have no exclusive zones, so
                // the work area is the whole display. A session backend
                // subtracts layer-shell reservations here.
                LayoutOutput::new(spec.id, spec.name.clone(), spec.logical)
            })
            .collect();
        let events = self.layout.reconfigure_outputs(layout_outputs);
        self.number_unnamed_workspaces();
        self.dirty = true;
        self.peers.broadcast(&events);
        events
    }

    /// The protocol object for a display.
    pub fn smithay_output(&self, id: OutputId) -> Option<&Output> {
        self.outputs.iter().find(|e| e.id == id).map(|e| &e.output)
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
                .map(|o| OutputSpec::new(o.id, o.name.clone(), o.logical))
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
        let next = frame(&self.layout);
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

        for placement in &self.placements.placements {
            let Some(window) = self.windows.window(placement.window) else {
                continue;
            };
            let Some(toplevel) = window.toplevel() else {
                continue;
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

        self.refresh_keyboard_focus();
    }

    fn refresh_keyboard_focus(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let target = self
            .placements
            .focused
            .and_then(|id| self.windows.window(id))
            .and_then(|w| w.toplevel())
            .map(|t| t.wl_surface().clone());
        if keyboard.current_focus() == target {
            return;
        }
        keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
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

    /// Admits a window that has drawn its first frame into the tiling tree.
    fn map_window(&mut self, window: WindowId) {
        self.apply(Command::AddWindow {
            window,
            workspace: None,
            target: InsertTarget::default(),
        });
        // Reflow now rather than on the next tick, so the client's very next
        // configure carries the cell it was just given.
        self.reflow();
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
        // Children must not inherit the parent session's display, or they would
        // connect to the compositor irontile is nested inside instead.
        command.env_remove("DISPLAY");
        match command.spawn() {
            Ok(_) => tracing::info!(program, "spawned"),
            Err(err) => tracing::warn!(program, %err, "failed to spawn"),
        }
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

        let Some((id, window)) = self.windows.find(surface) else {
            return;
        };
        window.on_commit();

        if self.windows.is_unmapped(id) {
            if has_buffer(surface) && self.windows.mark_mapped(id) {
                self.map_window(id);
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

impl ShmHandler for Irontile {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for Irontile {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
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

impl ClientDndGrabHandler for Irontile {}

impl ServerDndGrabHandler for Irontile {
    fn send(&mut self, _mime: String, _fd: std::os::unix::io::OwnedFd, _seat: Seat<Self>) {}
}

delegate_compositor!(Irontile);
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
