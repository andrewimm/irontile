//! What keyboard focus can point at.
//!
//! Focus is not always a toplevel. A popup takes focus for as long as its grab
//! lasts, and a layer surface that asks for keyboard interactivity takes it
//! away from the tiling tree entirely. Modelling that as an enum rather than a
//! bare `WlSurface` is what lets smithay's popup grab machinery work, since a
//! grab has to be able to say "focus went back to the window underneath".

use std::borrow::Cow;

use smithay::desktop::{LayerSurface, PopupKind, Window};
use smithay::input::Seat;
use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Serial};
use smithay::wayland::seat::WaylandFocus;

use crate::state::Irontile;

#[derive(Clone, Debug, PartialEq)]
pub enum KeyboardFocus {
    Window(Window),
    /// Boxed because a `PopupKind` is an order of magnitude larger than the
    /// other variants, and focus is copied around on every input event.
    Popup(Box<PopupKind>),
    Layer(LayerSurface),
    /// The lock screen, which takes the keyboard from everything else for as
    /// long as the session is locked.
    Lock(smithay::wayland::session_lock::LockSurface),
}

impl KeyboardFocus {
    pub fn surface(&self) -> WlSurface {
        match self {
            KeyboardFocus::Window(window) => window
                .toplevel()
                .map(|t| t.wl_surface().clone())
                .expect("a managed window is always a wayland toplevel"),
            KeyboardFocus::Popup(popup) => popup.wl_surface().clone(),
            KeyboardFocus::Layer(layer) => layer.wl_surface().clone(),
            KeyboardFocus::Lock(lock) => lock.wl_surface().clone(),
        }
    }
}

impl IsAlive for KeyboardFocus {
    fn alive(&self) -> bool {
        match self {
            KeyboardFocus::Window(window) => window.alive(),
            KeyboardFocus::Popup(popup) => popup.alive(),
            KeyboardFocus::Layer(layer) => layer.alive(),
            KeyboardFocus::Lock(lock) => lock.alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocus {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            KeyboardFocus::Window(window) => window.wl_surface(),
            KeyboardFocus::Popup(popup) => Some(Cow::Owned(popup.wl_surface().clone())),
            KeyboardFocus::Layer(layer) => Some(Cow::Owned(layer.wl_surface().clone())),
            KeyboardFocus::Lock(lock) => Some(Cow::Owned(lock.wl_surface().clone())),
        }
    }

    fn same_client_as(&self, object_id: &ObjectId) -> bool {
        use smithay::reexports::wayland_server::Resource;
        self.surface().id().same_client_as(object_id)
    }
}

impl From<PopupKind> for KeyboardFocus {
    fn from(popup: PopupKind) -> Self {
        KeyboardFocus::Popup(Box::new(popup))
    }
}

/// Pointer focus stays a plain surface: the pointer cares only about which
/// surface is under it, never about what role that surface plays.
impl From<KeyboardFocus> for WlSurface {
    fn from(focus: KeyboardFocus) -> Self {
        focus.surface()
    }
}

/// Every variant ultimately delivers to a `wl_surface`, so each method hands
/// off to the surface's own implementation.
impl KeyboardTarget<Irontile> for KeyboardFocus {
    fn enter(
        &self,
        seat: &Seat<Irontile>,
        data: &mut Irontile,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        KeyboardTarget::enter(&self.surface(), seat, data, keys, serial);
    }

    fn leave(&self, seat: &Seat<Irontile>, data: &mut Irontile, serial: Serial) {
        KeyboardTarget::leave(&self.surface(), seat, data, serial);
    }

    fn key(
        &self,
        seat: &Seat<Irontile>,
        data: &mut Irontile,
        key: KeysymHandle<'_>,
        state: smithay::backend::input::KeyState,
        serial: Serial,
        time: u32,
    ) {
        KeyboardTarget::key(&self.surface(), seat, data, key, state, serial, time);
    }

    fn modifiers(
        &self,
        seat: &Seat<Irontile>,
        data: &mut Irontile,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        KeyboardTarget::modifiers(&self.surface(), seat, data, modifiers, serial);
    }
}
