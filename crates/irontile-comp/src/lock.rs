//! The session lock.
//!
//! A locked session shows one surface per display and nothing else: no windows,
//! no panels, no pointer into anything a client could read. That is the whole
//! guarantee, and it has to hold even when the thing holding the lock goes
//! away -- a lock screen that crashes must leave the session locked rather than
//! open, or it would be worth less than no lock at all.
//!
//! What that means here: the lock outlives the client. If the locker dies
//! without unlocking, its surfaces go with it and every display is painted
//! blank, but the session stays locked and nothing but a new locker can change
//! that.
//!
//! "Nothing but a new locker" is load-bearing, and was not true for a while: a
//! second lock was refused whenever the session was locked, dead client or not,
//! so a locker that died left a blank screen that took the keyboard, answered
//! nothing, and could only be escaped by rebooting the machine. A lock whose
//! client is gone is now replaceable, which is the difference between losing a
//! session and typing a password into a new lock screen.

use std::collections::HashMap;

use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;
use smithay::reexports::wayland_server::Resource as _;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};

use crate::state::Irontile;
use irontile_layout::OutputId;

/// A locked session.
#[derive(Debug, Default)]
pub struct Lock {
    /// One surface per display, by the display it covers.
    surfaces: HashMap<OutputId, LockSurface>,
    /// Held until every display has a surface that has drawn something, at
    /// which point the client is told the session is locked. Dropping it
    /// without that tells the client the lock was refused.
    pending: Option<SessionLocker>,
    /// The client's lock object, kept past the confirmation so that whether
    /// the locker is still there remains a question that can be asked. Without
    /// it a lock is only as alive as its surfaces, and a client that has
    /// locked but not yet drawn has none.
    object: Option<ExtSessionLockV1>,
}

impl Lock {
    /// Whether the locker is still there at all.
    ///
    /// Either it holds a live lock object, which covers the moment between
    /// asking for the lock and drawing anything, or one of its surfaces is
    /// still alive. Neither is true once the process is gone, and that is the
    /// state a new locker is allowed to replace.
    pub fn alive(&self) -> bool {
        self.object.as_ref().is_some_and(|object| object.is_alive())
            || self.surfaces.values().any(|surface| surface.alive())
    }

    /// Whether every display is covered by a surface that still exists.
    ///
    /// Held to until then so that a locker which manages one display of two
    /// cannot leave the other showing what was on it. Liveness is part of the
    /// question: a dead surface is an entry in a map and nothing on screen, so
    /// counting it as cover is how a blank display comes to be called locked.
    pub fn covers(&self, outputs: &[OutputId]) -> bool {
        !outputs.is_empty()
            && outputs
                .iter()
                .all(|id| self.surfaces.get(id).is_some_and(|surface| surface.alive()))
    }

    /// Says the session is locked, once. Returns whether this was the moment.
    pub fn confirm(&mut self) -> bool {
        match self.pending.take() {
            Some(confirmation) => {
                confirmation.lock();
                true
            }
            None => false,
        }
    }

    /// The surface covering a display, if one is still alive.
    ///
    /// A dead one draws nothing, so handing it back would paint a blank
    /// display and call it covered.
    pub fn surface_for(&self, output: OutputId) -> Option<&LockSurface> {
        self.surfaces.get(&output).filter(|surface| surface.alive())
    }

    pub fn surfaces(&self) -> impl Iterator<Item = (&OutputId, &LockSurface)> {
        self.surfaces.iter()
    }

    /// Whether a surface belongs to this lock.
    pub fn owns(&self, surface: &WlSurface) -> bool {
        self.surfaces
            .values()
            .any(|lock| lock.wl_surface() == surface)
    }

    /// The surface to give the keyboard to, which is the first that is alive.
    ///
    /// Any of them will do: they belong to one client, and a lock screen that
    /// is typed into on one display reads the keyboard for all of them.
    pub fn keyboard_target(&self) -> Option<&LockSurface> {
        self.surfaces.values().find(|lock| lock.alive())
    }
}

impl SessionLockHandler for Irontile {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        // Already locked and the locker is still there: the session cannot be
        // locked twice, and telling the newcomer it succeeded would hand it a
        // session somebody else is holding. Dropping the confirmation refuses
        // it.
        if let Some(existing) = &self.session_lock {
            if existing.alive() {
                tracing::warn!("refused a second lock on an already locked session");
                return;
            }
            // The locker is gone and its surfaces with it, so every display is
            // blank and the keyboard goes nowhere. Refusing here would leave
            // the machine in a state a reboot is the only way out of, which is
            // worse in every case than letting somebody else draw a lock
            // screen: the session stays locked throughout, and whoever takes
            // over still has to satisfy PAM before anything is given back.
            tracing::warn!("the locker is gone; letting a new one take the lock");
        }
        tracing::info!("locking the session");
        self.session_lock = Some(Lock {
            surfaces: HashMap::new(),
            object: Some(confirmation.ext_session_lock().clone()),
            pending: Some(confirmation),
        });
        // Nothing else may have the keyboard from this moment, whether or not
        // the locker has drawn yet.
        self.reflow();
    }

    fn unlock(&mut self) {
        tracing::info!("unlocking the session");
        self.session_lock = None;
        self.dirty = true;
        self.reflow();
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(id) = self.output_id_of(&output) else {
            tracing::warn!("a lock surface named a display that is not here");
            return;
        };
        // It covers its display exactly, which is the only size a lock surface
        // is allowed to be.
        let size = self.output_size(id);
        surface.with_pending_state(|state| {
            state.size = Some((size.w.max(1) as u32, size.h.max(1) as u32).into());
        });
        surface.send_configure();

        if let Some(lock) = &mut self.session_lock {
            lock.surfaces.insert(id, surface);
        }
        self.dirty = true;
    }
}

smithay::delegate_session_lock!(Irontile);
