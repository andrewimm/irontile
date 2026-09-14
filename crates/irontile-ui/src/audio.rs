//! What the sound server says the volume is.
//!
//! Kept behind a thread of its own because the only way to ask is to hold a
//! connection open: there is no file to read the way there is for a battery or
//! a backlight. The thread owns every PulseAudio object, so nothing here needs
//! locking discipline around a library mainloop -- the only things crossing
//! between threads are a mutex holding the last reading and a pipe that says a
//! new one has arrived.
//!
//! PipeWire answers on this interface through `pipewire-pulse`, which is what a
//! current desktop has running; a machine with neither reports nothing and the
//! module draws nothing rather than failing.

use std::cell::RefCell;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::subscribe::{Facility, InterestMaskSet};
use libpulse_binding::context::{Context, FlagSet, State};
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::volume::Volume as PaVolume;

/// How loud the default output is, and whether it is muted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Volume {
    /// Percent of the normal (unamplified) volume, so software boost above a
    /// hundred reads as more than a hundred rather than being clipped to it.
    pub percent: f64,
    pub muted: bool,
}

/// A live reading of the default output.
#[derive(Debug)]
pub struct Audio {
    latest: Arc<Mutex<Option<Volume>>>,
    /// Readable whenever the reading changed. Polled alongside the Wayland and
    /// control sockets, so pressing a volume key redraws the bar at once
    /// instead of at the next tick.
    wake: OwnedFd,
}

impl Audio {
    /// Starts listening. Returns `None` only if the pipe cannot be made; a
    /// sound server that is absent or slow to appear is the thread's problem,
    /// not the caller's.
    pub fn start() -> Option<Audio> {
        let (read, write) = rustix::pipe::pipe_with(
            // Never blocking: a drain happens when poll says one of these is
            // readable, and with more than one of them the others are not. A
            // blocking read on an empty pipe would stop the bar dead -- no
            // redraws, no pointer, nothing, with the process still running.
            rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
        )
        .ok()?;
        let latest = Arc::new(Mutex::new(None));
        let shared = Shared {
            latest: Arc::clone(&latest),
            wake: Arc::new(write),
        };
        std::thread::Builder::new()
            .name("irontile-bar audio".into())
            .spawn(move || listen(shared))
            .ok()?;
        Some(Audio { latest, wake: read })
    }

    pub fn volume(&self) -> Option<Volume> {
        *self.latest.lock().ok()?
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.wake.as_fd()
    }

    /// Empties the pipe, so one change does not wake the bar for ever. Read
    /// only after poll says there is something there, so it never blocks.
    pub fn drain(&self) {
        let mut buffer = [0u8; 64];
        let _ = rustix::io::read(&self.wake, &mut buffer);
    }
}

/// What the listening thread writes to.
#[derive(Clone)]
struct Shared {
    latest: Arc<Mutex<Option<Volume>>>,
    wake: Arc<OwnedFd>,
}

impl Shared {
    fn publish(&self, volume: Option<Volume>) {
        let changed = match self.latest.lock() {
            Ok(mut slot) => {
                let changed = *slot != volume;
                *slot = volume;
                changed
            }
            Err(_) => false,
        };
        if changed {
            // A full pipe means the bar has not caught up yet, which already
            // means it is about to redraw.
            let _ = rustix::io::write(&*self.wake, b"!");
        }
    }
}

/// Keeps a connection up, reconnecting if the server goes away.
fn listen(shared: Shared) {
    loop {
        let _ = session(&shared);
        // The server is gone: say so rather than leaving a stale number on the
        // bar, then wait before trying again so a missing server costs nothing.
        shared.publish(None);
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// One connection, for as long as it lasts.
fn session(shared: &Shared) -> Option<()> {
    let mut mainloop = Mainloop::new()?;
    let context = Rc::new(RefCell::new(Context::new(&mainloop, "irontile-bar")?));
    context
        .borrow_mut()
        .connect(None, FlagSet::NOFLAGS, None)
        .ok()?;

    // Pump until the connection settles one way or the other.
    loop {
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) | IterateResult::Err(_) => return None,
        }
        match context.borrow().get_state() {
            State::Ready => break,
            State::Failed | State::Terminated => return None,
            _ => {}
        }
    }

    // Every change to a sink or to which sink is the default is a reason to
    // look again. Asking is cheap and the alternative is tracking the server's
    // state here, which is the server's job.
    let refresh = {
        let context = Rc::clone(&context);
        let shared = shared.clone();
        move || read_default_sink(&context, &shared)
    };
    refresh();

    {
        let refresh = refresh.clone();
        context
            .borrow_mut()
            .set_subscribe_callback(Some(Box::new(move |facility, _, _| {
                if matches!(facility, Some(Facility::Sink) | Some(Facility::Server)) {
                    refresh();
                }
            })));
    }
    context
        .borrow_mut()
        .subscribe(InterestMaskSet::SINK | InterestMaskSet::SERVER, |_| {});

    loop {
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) | IterateResult::Err(_) => return None,
        }
        if context.borrow().get_state() != State::Ready {
            return None;
        }
    }
}

/// Asks which sink is the default, then asks that sink how loud it is.
fn read_default_sink(context: &Rc<RefCell<Context>>, shared: &Shared) {
    let introspect = context.borrow().introspect();
    let shared = shared.clone();
    let context = Rc::clone(context);
    introspect.get_server_info(move |info| {
        let Some(name) = info.default_sink_name.as_ref() else {
            return;
        };
        let shared = shared.clone();
        // A fresh introspector: the one that started this is borrowed by the
        // callback it is running inside.
        let inner = context.borrow().introspect();
        inner.get_sink_info_by_name(name, move |result| {
            if let ListResult::Item(sink) = result {
                shared.publish(Some(Volume {
                    percent: f64::from(sink.volume.avg().0) / f64::from(PaVolume::NORMAL.0) * 100.0,
                    muted: sink.mute,
                }));
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reading_is_reported_once_and_only_when_it_changes() {
        // The pipe is what wakes the bar. Writing to it for a reading that did
        // not change would spin the bar at whatever rate the sound server
        // gossips at.
        // Non-blocking, so an empty pipe reports would-block rather than
        // hanging the test if this ever regresses.
        let (read, write) = rustix::pipe::pipe_with(
            rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
        )
        .unwrap();
        let shared = Shared {
            latest: Arc::new(Mutex::new(None)),
            wake: Arc::new(write),
        };
        let quiet = Volume {
            percent: 25.0,
            muted: false,
        };
        shared.publish(Some(quiet));
        shared.publish(Some(quiet));

        let mut buffer = [0u8; 64];
        let woken = rustix::io::read(&read, &mut buffer).unwrap_or(0);
        assert_eq!(woken, 1, "the repeat of the same reading said nothing");
        assert_eq!(*shared.latest.lock().unwrap(), Some(quiet));
    }
}
