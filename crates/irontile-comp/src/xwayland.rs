//! XWayland: X11 clients as ordinary tiled windows.
//!
//! An X11 client cannot talk Wayland, so something has to be both an X server
//! for it and a Wayland client on its behalf. That is Xwayland, and what it
//! does not do is decide where windows go -- X11 expects a window manager to
//! be a separate program, and here that program is this compositor. So there
//! are two halves: starting the server, and answering it as its window
//! manager.
//!
//! What comes out the other side is a [`Window`] like any other. The layout
//! engine never learns which kind it placed, because a tiling compositor that
//! tiled its X11 windows differently would be a compositor with two answers to
//! every question.

use irontile_layout::{Command, InsertTarget};
use smithay::desktop::Window;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::selection::SelectionTarget;
use smithay::xwayland::xwm::{Reorder, ResizeEdge, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler};

use crate::state::Irontile;

/// The X server, and the window manager answering it.
pub struct Xwayland {
    /// The event source the server is, which the loop owns. Held so that the
    /// server can be stopped: removing the source drops it, and a server with
    /// nothing on the other end of its socket exits.
    #[allow(dead_code)]
    token: smithay::reexports::calloop::RegistrationToken,
    /// Present once the server says it is ready, which is a moment or two after
    /// it is started.
    pub wm: Option<X11Wm>,
    /// The display number, for `DISPLAY`. Also `None` until it is ready: a
    /// client started before then would find no server, so the variable is not
    /// set at all rather than set to something that is not listening yet.
    pub display: Option<u32>,
}

impl std::fmt::Debug for Xwayland {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Xwayland")
            .field("display", &self.display)
            .field("wm", &self.wm.is_some())
            .finish()
    }
}

impl Xwayland {
    /// What to put in `DISPLAY`, once there is something listening.
    pub fn display_name(&self) -> Option<String> {
        display_name(self.display)
    }
}

/// The name of a display number, which is what `DISPLAY` holds.
///
/// Free-standing so it can be checked without an X server: what goes in that
/// variable is the whole of how a child finds the server, and a spelling that
/// is nearly right is a program that cannot open a window.
fn display_name(display: Option<u32>) -> Option<String> {
    display.map(|number| format!(":{number}"))
}

/// Starts the X server, and arranges to become its window manager when it is up.
///
/// Started with the session rather than on demand. Xwayland is told to
/// terminate when its last client goes away, and the window manager below is
/// itself a client that never does, so one started here stays for the session:
/// the cost is a process, and the alternative is the first X11 program of the
/// day waiting for a server to boot.
pub fn start(state: &mut Irontile) {
    let handle = state.loop_handle.clone();
    let server = XWayland::spawn(
        &state.display_handle,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        // Xwayland is loud, and none of it is actionable from here: what
        // matters is whether windows appear, which the log below says.
        std::process::Stdio::null(),
        std::process::Stdio::null(),
        |_| {},
    );
    let (server, client) = match server {
        Ok(pair) => pair,
        Err(err) => {
            // Not fatal, and not silent either. A session without Xwayland
            // still runs every Wayland client, so this is one category of
            // program failing to start rather than a session that cannot.
            tracing::warn!(%err, "could not start Xwayland; X11 clients will not run");
            return;
        }
    };

    // Known before the server is listening, because the socket is made here
    // and the server is handed it. That matters: a client started in the same
    // breath can connect and wait, where one told nothing would have had to be
    // started again after the server came up.
    let display_number = server.display_number();

    let inserted = handle.insert_source(server, move |event, _, state| match event {
        XWaylandEvent::Ready { x11_socket, .. } => {
            let wm = X11Wm::start_wm(state.loop_handle.clone(), x11_socket, client.clone());
            match wm {
                Ok(wm) => {
                    if let Some(xwayland) = state.xwayland.as_mut() {
                        xwayland.wm = Some(wm);
                    }
                    tracing::info!(display = display_number, "Xwayland is up");
                }
                Err(err) => tracing::warn!(%err, "could not manage Xwayland's windows"),
            }
        }
        XWaylandEvent::Error => {
            tracing::warn!("Xwayland exited while starting up");
            if let Some(xwayland) = state.xwayland.as_mut() {
                xwayland.display = None;
                xwayland.wm = None;
            }
        }
    });

    match inserted {
        Ok(token) => {
            state.xwayland = Some(Xwayland {
                token,
                wm: None,
                display: Some(display_number),
            });
            tracing::info!(display = display_number, "Xwayland is starting");
        }
        Err(err) => tracing::warn!(%err, "could not watch Xwayland"),
    }
}

impl XwmHandler for Irontile {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwayland
            .as_mut()
            .and_then(|x| x.wm.as_mut())
            .expect("the window manager answers only while it exists")
    }

    /// A window exists, which is not the same as being on screen.
    ///
    /// X11 separates creating a window from mapping it, and a great many are
    /// created and never mapped. Nothing happens here for that reason.
    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    /// The client is asking for its window to be shown.
    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        // Saying yes is what makes the surface appear; until then there is
        // nothing to place.
        if let Err(err) = window.set_mapped(true) {
            tracing::warn!(%err, "could not map an X11 window");
            return;
        }
        let window = Window::new_x11_window(window);
        let id = self.windows.insert(window);
        self.apply(Command::AddWindow {
            window: id,
            workspace: None,
            target: InsertTarget::default(),
        });
        self.reflow();
    }

    /// An override-redirect window has appeared: a menu, a tooltip, a drag icon.
    ///
    /// These are the windows X11 lets a client place itself, and a window
    /// manager that moved them would be moving a menu away from the thing it
    /// dropped out of. It is shown where it asked to be and never tiled.
    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.unmanaged.push(window);
        self.redraw = true;
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.unmanaged.retain(|other| other != &window);
        let Some(id) = self.window_id_of_x11(&window) else {
            self.redraw = true;
            return;
        };
        let placed = self.windows.in_tree(id);
        self.windows.remove(id);
        if placed {
            self.apply(Command::RemoveWindow { window: id });
        }
        self.reflow();
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        // Unmapping usually came first, in which case there is nothing left to
        // forget. A window destroyed while still mapped arrives here only.
        self.unmapped_window(_xwm, window);
    }

    /// The client would like to be a particular size or in a particular place.
    ///
    /// Answered rather than ignored, and answered with the truth: X11 requires
    /// a reply, and a window left waiting for one sits unresized forever. A
    /// tiled window is told the cell it already has, which is the same
    /// conversation an xdg-shell client has when it asks for a size.
    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        if let Some(id) = self.window_id_of_x11(&window)
            && let Some(rect) = self.cell_of_x11(id)
        {
            let _ = window.configure(Some(rect));
            return;
        }
        // Not ours to place: an override-redirect window, or one that has not
        // been mapped yet. It gets what it asked for.
        let mut geometry = window.geometry();
        if let Some(x) = x {
            geometry.loc.x = x;
        }
        if let Some(y) = y {
            geometry.loc.y = y;
        }
        if let Some(w) = w {
            geometry.size.w = w as i32;
        }
        if let Some(h) = h {
            geometry.size.h = h as i32;
        }
        let _ = window.configure(Some(geometry));
    }

    /// An override-redirect window moved itself, which it is allowed to do.
    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        self.redraw = true;
    }

    /// Resizing and moving belong to the tree, not to the window.
    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _edges: ResizeEdge,
    ) {
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    /// The clipboard, which X11 and Wayland each have their own idea of.
    ///
    /// Both directions are handled by smithay once these say yes: without them
    /// copying in an X11 program and pasting in a Wayland one silently does
    /// nothing, which is the kind of breakage people blame on the application.
    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        // Only for the client that holds the keyboard, matching what the
        // Wayland clipboard already does: the selection follows focus, and
        // nothing in the background can read it by asking.
        self.seat
            .get_keyboard()
            .is_some_and(|keyboard| keyboard.current_focus().is_some())
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: std::os::unix::io::OwnedFd,
    ) {
        let dh = self.display_handle.clone();
        match selection {
            SelectionTarget::Clipboard => {
                if let Err(err) =
                    smithay::wayland::selection::data_device::request_data_device_client_selection(
                        &self.seat, mime_type, fd,
                    )
                {
                    tracing::warn!(%err, "could not hand the clipboard to an X11 client");
                }
            }
            SelectionTarget::Primary => {
                if let Err(err) =
                    smithay::wayland::selection::primary_selection::request_primary_client_selection(
                        &self.seat, mime_type, fd,
                    )
                {
                    tracing::warn!(%err, "could not hand the selection to an X11 client");
                }
            }
        }
        let _ = dh;
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        match selection {
            SelectionTarget::Clipboard => {
                smithay::wayland::selection::data_device::set_data_device_selection(
                    &self.display_handle,
                    &self.seat,
                    mime_types,
                    (),
                )
            }
            SelectionTarget::Primary => {
                smithay::wayland::selection::primary_selection::set_primary_selection(
                    &self.display_handle,
                    &self.seat,
                    mime_types,
                    (),
                )
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        match selection {
            SelectionTarget::Clipboard => {
                smithay::wayland::selection::data_device::clear_data_device_selection(
                    &self.display_handle,
                    &self.seat,
                )
            }
            SelectionTarget::Primary => {
                smithay::wayland::selection::primary_selection::clear_primary_selection(
                    &self.display_handle,
                    &self.seat,
                )
            }
        }
    }
}

impl smithay::wayland::xwayland_shell::XWaylandShellHandler for Irontile {
    fn xwayland_shell_state(
        &mut self,
    ) -> &mut smithay::wayland::xwayland_shell::XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

smithay::delegate_xwayland_shell!(Irontile);

impl Irontile {
    /// The layout's id for an X11 window, if the tree has one.
    pub fn window_id_of_x11(&self, surface: &X11Surface) -> Option<irontile_layout::WindowId> {
        let wl = surface.wl_surface()?;
        self.windows.id_of(&wl)
    }

    /// Where the tree says an X11 window goes, in the space the displays share.
    ///
    /// X11 has one coordinate space for every screen, and it is the same one
    /// the layout engine works in, so a placement converts without translating.
    fn cell_of_x11(&self, id: irontile_layout::WindowId) -> Option<Rectangle<i32, Logical>> {
        let placement = self.placements.placements.iter().find(|p| p.window == id)?;
        let content = self.content_rect(placement.rect, placement.kind);
        Some(Rectangle::new(
            (content.x, content.y).into(),
            (content.w.max(1), content.h.max(1)).into(),
        ))
    }
}

impl Irontile {
    /// Puts the focused X11 window at the top of X11's own stacking order.
    ///
    /// Tiled windows do not overlap, so this changes nothing on screen. It
    /// changes what X11 clients believe: a great deal of X11 software asks the
    /// server which window is on top and behaves differently for the answer --
    /// menus that dismiss themselves, dialogs that decide they are obscured.
    /// Keeping the server's idea in step with ours costs one request per focus
    /// change.
    pub fn raise_focused_x11(&mut self) {
        let Some(id) = self.placements.focused else {
            return;
        };
        let Some(surface) = self
            .windows
            .window(id)
            .and_then(|window| window.x11_surface())
            .cloned()
        else {
            return;
        };
        if let Some(wm) = self.xwayland.as_mut().and_then(|x| x.wm.as_mut())
            && let Err(err) = wm.raise_window(&surface)
        {
            tracing::debug!(%err, "could not raise an X11 window");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::display_name;

    #[test]
    fn a_display_number_is_named_the_way_x11_names_it() {
        assert_eq!(display_name(Some(0)).as_deref(), Some(":0"));
        assert_eq!(display_name(Some(12)).as_deref(), Some(":12"));
    }

    #[test]
    fn no_server_means_the_variable_is_not_set_at_all() {
        // Rather than set to something empty or plausible: a child that finds
        // DISPLAY set and nothing listening waits, where one that finds it
        // unset says so immediately.
        assert_eq!(display_name(None), None);
    }
}
