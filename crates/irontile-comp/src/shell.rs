//! xdg-shell: mapping toplevels into the tiling tree.

use irontile_layout::Command;
use smithay::delegate_xdg_shell;
use smithay::desktop::{PopupKind, Window};
use smithay::reexports::wayland_server::protocol::{wl_output::WlOutput, wl_seat::WlSeat};
use smithay::utils::Serial;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};

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
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!(%err, "failed to track popup");
        }
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
        // Popup grabs are not implemented yet; a popup still maps and renders,
        // it just does not take an exclusive pointer grab.
        // TODO: keyboard and pointer grabs for popups, with dismiss-on-click-out.
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

delegate_xdg_shell!(Irontile);
