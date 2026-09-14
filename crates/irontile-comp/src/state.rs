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
use smithay::output::Output;
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

use crate::registry::Registry;
use crate::theme::Theme;

/// The single display of the nested backend.
pub const NESTED_OUTPUT: OutputId = OutputId(1);

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
    pub output: Output,

    pub layout: Layout,
    pub windows: Registry,
    /// The most recent frame, kept so rendering and hit-testing agree with what
    /// clients were last configured for.
    pub placements: Frame,
    pub theme: Theme,
}

impl Irontile {
    pub fn new(display_handle: DisplayHandle, output: Output, socket_name: String) -> Self {
        let dh = &display_handle;
        let theme = Theme::default();
        let mut seat_state = SeatState::new();
        let seat = seat_state.new_wl_seat(dh, "irontile");

        let config = irontile_layout::Config {
            params: theme.layout_params(),
            ..Default::default()
        };

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
            output,
            layout: Layout::new(config),
            windows: Registry::default(),
            placements: Frame::default(),
            theme,
        }
    }

    /// Tells the layout engine how big the display is.
    ///
    /// The nested window is the whole display, so its size is both the output
    /// geometry and the work area; a real session subtracts layer-shell
    /// exclusive zones here.
    pub fn set_output_size(&mut self, size: SmithaySize<i32, Logical>) {
        let logical = Rect::new(0, 0, size.w, size.h);
        let output = LayoutOutput::new(NESTED_OUTPUT, self.output.name(), logical);
        self.layout.reconfigure_outputs(vec![output]);
        self.name_initial_workspace();
        self.dirty = true;
    }

    /// Gives the desktop that comes up on first connect the name "1", so the
    /// numeric bindings line up with what is on screen from the start.
    fn name_initial_workspace(&mut self) {
        if let Some(ws) = self.layout.active_workspace(NESTED_OUTPUT)
            && self.layout.workspace(ws).is_some_and(|w| w.name.is_none())
            && self.layout.workspace_named("1").is_none()
        {
            let _ = self.layout.rename_workspace(ws, Some("1".into()));
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
        match self.layout.workspace_named(&name) {
            Some(ws) => ws.id,
            None => self.layout.create_workspace(Some(name)),
        }
    }

    pub fn apply(&mut self, command: Command) {
        match dispatch(&mut self.layout, command) {
            Ok(events) => self.absorb(&events),
            Err(LayoutError::UnknownWindow(_)) => {}
            Err(err) => tracing::warn!(%err, "layout rejected a command"),
        }
    }

    fn absorb(&mut self, events: &[Event]) {
        if !events.is_empty() {
            self.dirty = true;
        }
        for event in events {
            tracing::debug!(?event);
        }
    }

    /// The rectangle the client's own surface occupies, once the border quad
    /// drawn behind it is accounted for.
    pub fn content_rect(&self, cell: Rect, kind: PlacementKind) -> Rect {
        match kind {
            // A fullscreen window covers the display outright; a border would
            // make it not fullscreen.
            PlacementKind::Fullscreen => cell,
            _ => cell.inset(self.theme.border_width),
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

    /// Logical size of the single display.
    pub fn output_size(&self) -> SmithaySize<i32, Logical> {
        self.layout
            .output(NESTED_OUTPUT)
            .map(|o| SmithaySize::from((o.logical.w, o.logical.h)))
            .unwrap_or_else(|| SmithaySize::from((0, 0)))
    }

    /// Releases the frame callbacks of everything that was just drawn, which is
    /// what lets animating clients produce their next buffer.
    pub fn send_frame_callbacks(&self) {
        let time = self.start_time.elapsed();
        let output = self.output.clone();
        for placement in &self.placements.placements {
            if let Some(window) = self.windows.window(placement.window) {
                window.send_frame(&output, time, None, |_, _| Some(output.clone()));
            }
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
    pub fn send_workspace_to_output(&mut self, dir: Direction) {
        let Some(from) = self.layout.focused_output() else {
            return;
        };
        let Some(to) = self.layout.output_in_direction(from, dir) else {
            return;
        };
        let Some(ws) = self.layout.active_workspace(from) else {
            return;
        };
        self.apply(Command::ShowWorkspace {
            workspace: ws,
            output: Some(to),
        });
    }

    pub fn spawn(&self, program: &str) {
        let mut command = std::process::Command::new(program);
        command.env("WAYLAND_DISPLAY", &self.socket_name);
        // Children must not inherit the parent session's display, or they would
        // connect to the compositor irontile is nested inside instead.
        command.env_remove("DISPLAY");
        match command.spawn() {
            Ok(_) => tracing::info!(program, "spawned"),
            Err(err) => tracing::warn!(program, %err, "failed to spawn"),
        }
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
