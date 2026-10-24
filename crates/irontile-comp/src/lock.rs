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
use std::time::{Duration, Instant};

use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::{
    Error, ExtSessionLockV1, Request,
};
use smithay::reexports::wayland_server::Resource as _;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle};
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLockState, SessionLocker,
};

use crate::state::Irontile;
use irontile_layout::OutputId;

/// A locked session.
#[derive(Debug)]
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
    /// Set once the failure has been written down, so the log gets one line
    /// rather than one per pass round the event loop.
    reported_dead: bool,
    /// When the lock was last seen covering every display, or when it was
    /// taken out if it never has. How long ago that was is the difference
    /// between a locker that is drawing and one that has stopped, which is
    /// not a question its process being alive can answer.
    covered_at: Instant,
}

/// Whether a lock is still doing its job.
///
/// Covering the displays is the job, so a lock that covers them is working and
/// there is nothing else to ask. Not covering them is either a moment or a
/// failure, and which one depends on whether anybody is still attached: a
/// client that is gone will never draw again and can be replaced at once,
/// while one that is still there gets [`WEDGED_AFTER`] to put a surface up
/// before it is treated as stuck. Waiting out that grace for a client that has
/// already died would leave a blank screen unreplaceable for no reason.
fn still_working(
    covers: bool,
    client_alive: bool,
    surfaces_all_dead: bool,
    since_covered: Duration,
) -> bool {
    if surfaces_all_dead {
        // Every surface it had is gone. Waiting out the grace buys nothing: a
        // dead surface never draws again, and the only thing that could help
        // is a new surface, which is what the locker asking to take over is
        // offering. This is the case that cost two recoveries: both attempts
        // landed inside a ten second wait for a corpse to move.
        return false;
    }
    covers || (client_alive && since_covered < WEDGED_AFTER)
}

/// How long a lock may fail to cover the displays before it counts as wedged.
///
/// Long enough to sit out the moments when not covering is normal -- a display
/// being plugged in, a mode changing, a session coming back from suspend, all
/// of which leave a locker briefly a surface short. Short enough that somebody
/// standing in front of a blank screen is not waiting on it.
const WEDGED_AFTER: Duration = Duration::from_secs(10);

impl Lock {
    /// Whether the locker is still there at all.
    ///
    /// Either it holds a live lock object, which covers the moment between
    /// asking for the lock and drawing anything, or one of its surfaces is
    /// still alive.
    pub fn alive(&self) -> bool {
        self.object.as_ref().is_some_and(|object| object.is_alive())
            || self.surfaces.values().any(|surface| surface.alive())
    }

    /// Whether this lock is still doing the job, given the displays there are.
    ///
    /// Not the same question as whether its client is alive, and asking that
    /// one instead is how a session was lost: a locker came back from suspend
    /// still connected, still holding the lock, and drawing nothing at all, so
    /// every attempt to put a working lock screen up was refused on the
    /// grounds that one was already there. From the chair it was a blank
    /// screen that would not take a password and could not be replaced.
    ///
    /// A lock that has not covered the displays for [`WEDGED_AFTER`] is not
    /// holding anything, whoever is still attached to it.
    pub fn working(&self, outputs: &[OutputId]) -> bool {
        let (surfaces, alive, _) = self.tally();
        still_working(
            self.covers(outputs),
            self.alive(),
            surfaces > 0 && alive == 0,
            self.covered_at.elapsed(),
        )
    }

    /// Notes that the displays are covered, so the clock above starts again.
    pub fn seen_covering(&mut self) {
        self.covered_at = Instant::now();
    }

    /// What this lock amounts to right now, for the log.
    ///
    /// A blank screen under a lock has two quite different causes -- surfaces
    /// that went away, and surfaces that are still there with nothing in them
    /// -- and they are told apart by counting. Guessing which one it was, from
    /// a report of a dark screen hours later, is what the last three of these
    /// cost.
    pub fn tally(&self) -> (usize, usize, usize) {
        let alive = self.surfaces.values().filter(|s| s.alive()).count();
        let drawn = self
            .surfaces
            .values()
            .filter(|s| s.alive() && crate::state::has_buffer(s.wl_surface()))
            .count();
        (self.surfaces.len(), alive, drawn)
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

    /// Whether a surface came from the client this lock belongs to.
    ///
    /// Compared by client rather than by lock object because the surface does
    /// not carry the object it was made from; a client with a stale lock is
    /// still a different client, which is the case worth catching.
    pub fn belongs_to(&self, surface: &LockSurface) -> bool {
        let holder = self.object.as_ref().and_then(|object| object.client());
        let sender = surface.wl_surface().client();
        match (holder, sender) {
            (Some(holder), Some(sender)) => holder.id() == sender.id(),
            // No holder recorded: nothing to compare against, so take it. This
            // is the moment between asking for the lock and drawing.
            (None, _) => true,
            (_, None) => false,
        }
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

impl Irontile {
    /// Writes down the moment a locked session loses the thing drawing it.
    ///
    /// From the chair that moment is a screen that goes blank and a keyboard
    /// that stops going anywhere, which is indistinguishable from the machine
    /// having hung -- and the first question afterwards is whether the locker
    /// died or the compositor broke. Nothing else answers it: the locker's own
    /// output goes whichever way the thing that started it was pointed, and on
    /// a laptop that locked itself on an idle timer, that is a virtual
    /// terminal nobody will read.
    pub fn notice_a_dead_lock(&mut self) {
        let outputs: Vec<_> = self.arrangement.iter().map(|spec| spec.id).collect();
        let Some(lock) = &mut self.session_lock else {
            return;
        };
        if lock.covers(&outputs) {
            lock.seen_covering();
            lock.reported_dead = false;
            return;
        }
        if lock.working(&outputs) || lock.reported_dead {
            return;
        }
        lock.reported_dead = true;
        let alive = lock.alive();
        let (surfaces, alive_surfaces, drawn) = lock.tally();
        tracing::warn!(
            client_alive = alive,
            displays = outputs.len(),
            surfaces,
            alive = alive_surfaces,
            drawn,
            "the lock screen has stopped covering the displays: they are blank and the \
             session stays locked. Run irontile-lock again, from a virtual terminal if \
             there is no other way in, and it will take the lock over"
        );
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
        let outputs: Vec<_> = self.arrangement.iter().map(|spec| spec.id).collect();
        if let Some(existing) = &self.session_lock {
            if existing.working(&outputs) {
                tracing::warn!("refused a second lock on an already locked session");
                return;
            }
            // The locker is gone and its surfaces with it, so every display is
            // blank and the keyboard goes nowhere. Refusing here would leave
            // the machine in a state a reboot is the only way out of, which is
            // worse in every case than letting somebody else draw a lock
            // screen: the session stays locked throughout, and whoever takes
            // over still has to satisfy PAM before anything is given back.
            tracing::warn!(
                alive = existing.alive(),
                "the lock screen is not covering the displays; letting a new one take over"
            );
            // Told, rather than left to wonder. A locker whose lock has been
            // taken has nothing left to do, and one that is never told sits
            // connected for the rest of the session holding a lock object it
            // can still make surfaces with -- which is how a stale locker
            // reached the map above in the first place.
            if let Some(object) = &existing.object {
                object.finished();
            }
        }
        tracing::info!("locking the session");
        self.session_lock = Some(Lock {
            surfaces: HashMap::new(),
            object: Some(confirmation.ext_session_lock().clone()),
            pending: Some(confirmation),
            reported_dead: false,
            covered_at: Instant::now(),
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

        let Some(lock) = &mut self.session_lock else {
            return;
        };
        // Only from the client that holds the lock. A locker whose lock has
        // ended does not always go away -- nothing tells it to -- and it keeps
        // a live lock object it can still make surfaces with. One of those
        // arriving here used to replace the surface the *current* locker had
        // drawn, under the same display, and the screen went blank with a
        // healthy locker sitting behind it wondering why it was never asked to
        // draw again.
        if !lock.belongs_to(&surface) {
            tracing::warn!("ignored a lock surface from a client that does not hold the lock");
            return;
        }
        lock.surfaces.insert(id, surface);
        self.dirty = true;
    }
}

impl Irontile {
    /// Whether this is the object the session is actually locked by.
    ///
    /// Confirmed, not merely held. A client that asked for a lock and was
    /// never sent `locked` is required by the protocol to destroy its object
    /// rather than unlock with it, and `pending` still being `Some` is what
    /// never having been confirmed looks like from here.
    fn holds_the_lock(&self, lock: &ExtSessionLockV1) -> bool {
        self.session_lock
            .as_ref()
            .is_some_and(|held| held.pending.is_none() && held.object.as_ref() == Some(lock))
    }
}

/// Every request on a lock object, with the one that needs authorising checked
/// before smithay sees it.
///
/// [`SessionLockHandler::unlock`] is told nothing about who asked -- it takes
/// `&mut self` and nothing else -- so the check cannot live there. And
/// smithay's own guard does not hold: its `UnlockAndDestroy` arm posts
/// `invalid_unlock` for a lock that was never confirmed and then calls
/// `state.unlock()` regardless, because the `post_error` has no `return` after
/// it and posting an error only marks a client for disconnection rather than
/// unwinding. The same function returns early after `post_error` three times
/// in its `GetLockSurface` arm, so the omission is specific to this one.
///
/// The consequence was a complete bypass. Any client could bind the manager
/// (anything must be able to, or nothing could ever lock), ask for a lock and
/// be refused -- refusal only sends `finished`, it does not destroy the object
/// -- and then unlock the session with the object it was refused, having
/// authenticated nothing. The desktop came back: every gate on windows,
/// panels, keyboard focus, pointer hit-testing and bindings asks whether
/// `session_lock` is `Some`, and this set it to `None`. The real lock screen
/// went on running, holding a password prompt nobody could see.
///
/// So the request is matched here, where the object making it is still in
/// hand, and an unlock from anything but the confirmed holder is refused
/// without reaching the handler. Everything else is smithay's business and is
/// forwarded untouched, which is what `delegate_session_lock!` would have done
/// for this interface.
impl Dispatch<ExtSessionLockV1, SessionLockState> for Irontile {
    fn request(
        state: &mut Self,
        client: &Client,
        lock: &ExtSessionLockV1,
        request: Request,
        data: &SessionLockState,
        display: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if matches!(request, Request::UnlockAndDestroy) && !state.holds_the_lock(lock) {
            tracing::warn!("refused an unlock from a client that does not hold the lock");
            lock.post_error(
                Error::InvalidUnlock,
                "only the client holding a confirmed lock may unlock the session",
            );
            return;
        }
        <SessionLockManagerState as Dispatch<ExtSessionLockV1, SessionLockState, Self>>::request(
            state, client, lock, request, data, display, data_init,
        );
    }
}

// The rest of the protocol, delegated as before. Only the lock object itself is
// handled above, so these are the other three the macro would have written.
smithay::reexports::wayland_server::delegate_global_dispatch!(Irontile: [
    smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_manager_v1::ExtSessionLockManagerV1: smithay::wayland::session_lock::SessionLockManagerGlobalData
] => SessionLockManagerState);

smithay::reexports::wayland_server::delegate_dispatch!(Irontile: [
    smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_manager_v1::ExtSessionLockManagerV1: ()
] => SessionLockManagerState);

smithay::reexports::wayland_server::delegate_dispatch!(Irontile: [
    smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_surface_v1::ExtSessionLockSurfaceV1: smithay::wayland::session_lock::ExtLockSurfaceUserData
] => SessionLockManagerState);

#[cfg(test)]
mod tests {
    use super::{WEDGED_AFTER, still_working};
    use std::time::Duration;

    /// The arguments in the order they are asked about, for readability.
    fn working(covers: bool, client_alive: bool, all_dead: bool, since: Duration) -> bool {
        still_working(covers, client_alive, all_dead, since)
    }

    #[test]
    fn a_lock_that_covers_the_displays_is_working() {
        assert!(working(true, true, false, Duration::from_secs(0)));
        // Even a client that has gone: what is on screen is on screen, and
        // nothing else may be put over it until somebody unlocks.
        assert!(working(true, false, false, WEDGED_AFTER * 2));
    }

    #[test]
    fn a_locker_that_died_is_replaceable_at_once() {
        assert!(!working(false, false, false, Duration::from_secs(0)));
    }

    #[test]
    fn a_live_locker_gets_a_moment_before_it_counts_as_stuck() {
        // Coming back from suspend, or a display being plugged in, leaves a
        // locker briefly a surface short. That is not a wedged session.
        assert!(working(false, true, false, Duration::from_secs(1)));
    }

    #[test]
    fn a_live_locker_that_never_draws_again_is_still_a_blank_screen() {
        assert!(!working(
            false,
            true,
            false,
            WEDGED_AFTER + Duration::from_secs(1)
        ));
    }

    #[test]
    fn surfaces_that_have_all_died_are_not_worth_waiting_for() {
        // What the logs finally showed: surfaces=1 alive=0, a connected client,
        // and two recovery attempts refused inside the grace. A dead surface
        // never draws again, so there is nothing to wait for and the wait is
        // the whole problem.
        assert!(!working(false, true, true, Duration::from_secs(0)));
        assert!(!working(false, true, true, Duration::from_millis(1)));
    }
}
