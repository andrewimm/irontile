//! xdg-shell: mapping toplevels into the tiling tree.

use irontile_layout::Command;
use smithay::{delegate_xdg_decoration, delegate_xdg_shell};
use smithay::desktop::{
    PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, Window,
    find_popup_root_surface,
};
use smithay::input::Seat;
use smithay::input::pointer::Focus;
use smithay::reexports::wayland_server::protocol::{wl_output::WlOutput, wl_seat::WlSeat};
use smithay::utils::Serial;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};

use crate::focus::KeyboardFocus;
use crate::state::Irontile;

impl XdgShellHandler for Irontile {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // The client is owed a configure before it may attach a buffer. It gets
        // one with no size, leaving the first buffer's dimensions to the
        // client; the cell it actually lands in is sent once it maps and the
        // tree has a place for it.
        surface.with_pending_state(|state| {
            use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;
            state.states.set(State::TiledLeft);
            state.states.set(State::TiledRight);
            state.states.set(State::TiledTop);
            state.states.set(State::TiledBottom);
        });
        surface.send_configure();
        self.windows.insert(Window::new_wayland_window(surface));
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.windows.id_of(surface.wl_surface()) else {
            return;
        };
        let mapped = !self.windows.is_unmapped(id);
        self.windows.remove(id);
        // A window that never mapped was never in the tree, so there is nothing
        // for the layout engine to forget.
        if mapped {
            self.apply(Command::RemoveWindow { window: id });
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // The client is waiting for a configure before it may draw. The
        // positioner already decided the geometry, so this just confirms it.
        if let Err(err) = surface.send_configure() {
            tracing::warn!(%err, "failed to configure a popup");
            return;
        }
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!(%err, "failed to track popup");
        }
    }

    /// Takes an exclusive grab for a popup, which is what makes a menu dismiss
    /// when you click or type outside it.
    ///
    /// The grab is refused unless it chains from one this client already holds;
    /// otherwise any client could open a popup and capture the seat.
    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Irontile>::from_resource(&seat) else {
            return;
        };
        let popup = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&popup) else {
            return;
        };
        // The grab hands focus back to the window underneath when it ends, so
        // it needs to know which window that is.
        let Some(window) = self.windows.find(&root).map(|(_, window)| window.clone()) else {
            return;
        };

        let Ok(mut grab) =
            self.popups
                .grab_popup(KeyboardFocus::Window(window), popup, &seat, serial)
        else {
            return;
        };

        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }

        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
        if let Some(window) = self.windows.id_of(surface.wl_surface()) {
            self.apply(Command::SetFullscreen {
                window: Some(window),
                fullscreen: Some(true),
            });
            self.reflow();
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.windows.id_of(surface.wl_surface()) {
            self.apply(Command::SetFullscreen {
                window: Some(window),
                fullscreen: Some(false),
            });
            self.reflow();
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        // Every tiled window is already as large as it is going to get. The
        // protocol still requires a configure in reply, and `reflow` sends one
        // carrying the size the tree assigned.
        if self.windows.id_of(surface.wl_surface()).is_some() {
            self.reflow();
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if self.windows.id_of(surface.wl_surface()).is_some() {
            self.reflow();
        }
    }

    fn move_request(&mut self, _surface: ToplevelSurface, _seat: WlSeat, _serial: Serial) {
        // Position is the tree's decision, not the client's.
    }

    fn resize_request(
        &mut self,
        _surface: ToplevelSurface,
        _seat: WlSeat,
        _serial: Serial,
        _edges: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
    ) {
        // Likewise for size. Interactive resize goes through the layout engine.
        // TODO: pointer-driven resize that converts a drag into `Command::Resize`.
    }
}

/// Decoration is never the client's to choose.
///
/// The compositor draws a border rectangle and nothing else, and a client
/// drawing its own titlebar and shadow on top of that would be both redundant
/// and wrong for a tiling layout. Every request, whatever it asks for, is
/// answered with server-side.
impl XdgDecorationHandler for Irontile {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        set_server_side(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        set_server_side(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        set_server_side(&toplevel);
    }
}

fn set_server_side(toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|state| {
        state.decoration_mode = Some(DecorationMode::ServerSide);
    });
    // Only configure a toplevel that has already had its first one; otherwise
    // the reply belongs to the initial configure the shell handler sends.
    if toplevel.is_initial_configure_sent() {
        toplevel.send_pending_configure();
    }
}

delegate_xdg_shell!(Irontile);
delegate_xdg_decoration!(Irontile);
