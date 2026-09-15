//! Locking the session and holding it until somebody proves they may have it.
//!
//! One surface per display, because the protocol demands it: a lock that
//! covered one screen and left the other showing the desktop would not be a
//! lock. The compositor does not consider the session locked until every one of
//! them has drawn, and if this process dies the compositor keeps the screens
//! blank rather than handing them back -- which is what makes a lock screen
//! something that can be worked on without gambling the machine.

use std::io::Write as _;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags};
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_callback::{self, WlCallback},
    wl_compositor::WlCompositor,
    wl_keyboard::{self, WlKeyboard},
    wl_output::{self, WlOutput},
    wl_registry,
    wl_seat::{self, WlSeat},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use xkbcommon::xkb;

use crate::paint::{self, Palette, Screen, Status, Text};

/// How long a refusal stays on screen before the field goes quiet again.
const DENIED_FOR: Duration = Duration::from_secs(2);

/// The longest password that will be accepted.
///
/// Long enough for any passphrase somebody actually types, short enough that a
/// key stuck down cannot grow it without bound.
const MAX_PASSWORD: usize = 256;

/// How long to wait for a frame callback before drawing anyway.
///
/// The compositor asks for the next frame when it is ready for one, which is
/// what keeps this quiet while the displays are asleep. A compositor that goes
/// quiet without turning the displays off would otherwise stop the clock, so
/// there is a floor under how stale it can get.
const FRAME_BACKSTOP: Duration = Duration::from_secs(2);

/// How many slots a lock surface cycles through. Same reasoning as the bar: one
/// on screen, one being drawn, and a buffer handed over belongs to the
/// compositor until it says otherwise.
const SLOTS: usize = 2;

/// What the authentication thread sends back.
struct Verdict(Result<(), crate::auth::Denied>);

/// One display's lock surface.
struct Panel {
    surface: WlSurface,
    viewport: Option<WpViewport>,
    /// The size the compositor asked for, in logical pixels.
    logical: (u32, u32),
    /// Scale in 120ths, which is how the fractional-scale protocol counts. A
    /// display that never says otherwise stays at one.
    scale_120: u32,
    /// Set by the first configure. Nothing may be attached before it.
    configured: bool,
    pool: Option<Pool>,
    /// Somewhere to rasterise into, kept between frames rather than allocated
    /// per frame: at full screen this is the largest thing the locker holds.
    canvas: Option<tiny_skia::Pixmap>,
    /// Set when a frame was dropped because both slots were still out.
    pending: bool,
    /// Set between committing a frame and the compositor asking for the next
    /// one. While the displays are asleep it never clears, and nothing is
    /// drawn -- which is most of what a lock screen does.
    awaiting_frame: bool,
    /// When the last frame was committed, for the backstop above.
    committed: Instant,
}

impl Panel {
    fn scale(&self) -> f32 {
        self.scale_120 as f32 / 120.0
    }

    /// The buffer size in real pixels, which is the logical size scaled.
    fn pixels(&self) -> (i32, i32) {
        let scale = self.scale();
        (
            ((self.logical.0 as f32 * scale).round() as i32).max(1),
            ((self.logical.1 as f32 * scale).round() as i32).max(1),
        )
    }
}

/// The shared memory one lock surface draws into.
struct Pool {
    pool: WlShmPool,
    file: std::fs::File,
    slots: [Slot; SLOTS],
    /// The pixel size the slots are cut for. A different one means starting
    /// over, which is what a display changing mode looks like.
    size: (i32, i32),
}

struct Slot {
    buffer: WlBuffer,
    /// Shared with the buffer's event handler, which is the only thing that
    /// clears it.
    busy: Arc<AtomicBool>,
    offset: u64,
}

impl Pool {
    fn new(shm: &WlShm, handle: &QueueHandle<State>, size: (i32, i32)) -> Option<Pool> {
        let (w, h) = size;
        let stride = w.checked_mul(4)?;
        let slot = i64::from(stride) * i64::from(h);
        let total = i32::try_from(slot * SLOTS as i64).ok()?;
        if total <= 0 {
            return None;
        }

        let file =
            rustix::fs::memfd_create(c"irontile-lock", rustix::fs::MemfdFlags::CLOEXEC).ok()?;
        rustix::fs::ftruncate(&file, total as u64).ok()?;
        let file = std::fs::File::from(file);
        let pool = shm.create_pool(file.as_fd(), total, handle, ());

        let slots = std::array::from_fn(|i| {
            let busy = Arc::new(AtomicBool::new(false));
            let offset = slot * i as i64;
            let buffer = pool.create_buffer(
                offset as i32,
                w,
                h,
                stride,
                wl_shm::Format::Argb8888,
                handle,
                Arc::clone(&busy),
            );
            Slot {
                buffer,
                busy,
                offset: offset as u64,
            }
        });
        Some(Pool {
            pool,
            file,
            slots,
            size,
        })
    }

    /// The first slot the compositor is not still reading from.
    fn free(&self) -> Option<&Slot> {
        self.slots
            .iter()
            .find(|slot| !slot.busy.load(Ordering::Acquire))
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // Buffers first: they are cut from the pool and outlive it otherwise.
        for slot in &self.slots {
            slot.buffer.destroy();
        }
        self.pool.destroy();
    }
}

#[derive(Default)]
struct Globals {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    lock_manager: Option<ExtSessionLockManagerV1>,
    viewporter: Option<WpViewporter>,
    fractional: Option<WpFractionalScaleManagerV1>,
    outputs: Vec<WlOutput>,
}

/// The xkb half of a keyboard: the keymap the compositor handed over, and where
/// the modifiers currently stand.
struct Keyboard {
    context: xkb::Context,
    state: Option<xkb::State>,
}

impl Keyboard {
    fn new() -> Keyboard {
        Keyboard {
            context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            state: None,
        }
    }

    /// Whether caps lock is on, which is worth saying out loud on a screen
    /// where what is typed cannot be read back.
    fn caps(&self) -> bool {
        self.state.as_ref().is_some_and(|state| {
            state.mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_EFFECTIVE)
        })
    }
}

struct State {
    globals: Globals,
    lock: Option<ExtSessionLockV1>,
    panels: Vec<Panel>,
    xkb: Keyboard,

    user: String,
    host: String,
    service: String,
    /// What has been typed. Emptied the moment it is handed to PAM.
    entry: String,
    status: Status,
    denied_at: Option<Instant>,
    /// Set while PAM has the password and has not answered.
    checking: bool,
    finished: bool,
    unlocked: bool,
    /// The clock as last drawn, so a repaint happens when it changes rather
    /// than on every turn round the loop.
    clock: String,

    text: Text,
    palette: Palette,
    dirty: bool,
    verdicts: Receiver<Verdict>,
    answers: Sender<Verdict>,
    /// The writing end of the pipe the loop below waits on. A channel alone
    /// cannot wake it: the loop is asleep on file descriptors.
    waker: OwnedFd,
    /// Told once, when the displays are covered, so that a parent waiting to
    /// report the screen locked can stop waiting. Nothing to tell when the
    /// locker was not asked to fork.
    ready: Option<OwnedFd>,
}

/// Locks every display and does not return until the session is unlocked.
///
/// `ready` is written one byte the moment the compositor reports the session
/// locked, and closed when this process stops. It is how `--daemonize` tells
/// the half that already exited whether the screens were ever covered.
pub fn run(service: &str, user: &str, ready: Option<OwnedFd>) -> Result<(), String> {
    let connection =
        Connection::connect_to_env().map_err(|err| format!("no compositor to lock: {err}"))?;
    let mut queue: EventQueue<State> = connection.new_event_queue();
    let handle = queue.handle();
    connection.display().get_registry(&handle, ());

    let (answers, verdicts) = channel();
    // The reading end stays here rather than in the state, so that watching it
    // does not borrow the thing the event loop needs to hand to the dispatcher.
    let (wake, waker) = rustix::pipe::pipe_with(
        rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
    )
    .map_err(|err| format!("could not make a wake pipe: {err}"))?;

    let mut state = State {
        globals: Globals::default(),
        lock: None,
        panels: Vec::new(),
        xkb: Keyboard::new(),
        user: user.to_string(),
        host: crate::hostname(),
        service: service.to_string(),
        entry: String::new(),
        status: Status::Typing(0),
        denied_at: None,
        checking: false,
        finished: false,
        unlocked: false,
        clock: String::new(),
        text: Text::new(&["Anonymous Pro".to_string()]),
        palette: Palette::default(),
        dirty: true,
        verdicts,
        answers,
        waker,
        ready,
    };

    queue
        .roundtrip(&mut state)
        .map_err(|err| format!("could not talk to the compositor: {err}"))?;

    let manager = state
        .globals
        .lock_manager
        .clone()
        .ok_or("this compositor does not support ext-session-lock")?;
    let lock = manager.lock(&handle, ());
    state.lock = Some(lock.clone());
    state.cover_outputs(&handle);

    while !state.finished {
        state.redraw(&handle);
        queue
            .flush()
            .map_err(|err| format!("could not flush: {err}"))?;

        let read = queue
            .prepare_read()
            .ok_or("the event queue is already being read")?;
        // Copied out before the guard is consumed: reading takes it by value
        // and the descriptor is only borrowed from it.
        let wayland_fd = read
            .connection_fd()
            .try_clone_to_owned()
            .map_err(|err| format!("could not watch the compositor: {err}"))?;
        let mut fds = [
            PollFd::new(&wayland_fd, PollFlags::IN),
            PollFd::new(&wake, PollFlags::IN),
        ];

        // Until the next second turns over, so the clock changes when it should
        // rather than up to a second late.
        let wait = until_next_second().min(state.denial_due().unwrap_or(Duration::MAX));
        let timeout = rustix::time::Timespec {
            tv_sec: wait.as_secs() as i64,
            tv_nsec: wait.subsec_nanos() as i64,
        };
        // An error leaves every `revents` clear, which reads as "nothing
        // happened" and skips the read: events pile up in the socket and the
        // lock screen stops answering the keyboard with the process still up.
        if let Err(err) = rustix::event::poll(&mut fds, Some(&timeout)) {
            drop(read);
            if err != rustix::io::Errno::INTR {
                return Err(format!("waiting on the compositor failed: {err}"));
            }
            continue;
        }

        if fds[0].revents().contains(PollFlags::IN) {
            let _ = read.read();
        } else {
            drop(read);
        }
        queue
            .dispatch_pending(&mut state)
            .map_err(|err| format!("could not read events: {err}"))?;

        if fds[1].revents().contains(PollFlags::IN) {
            let mut sink = [0u8; 16];
            while rustix::io::read(&wake, &mut sink).is_ok_and(|read| read > 0) {}
        }
        state.collect();
        state.expire_denial();
        state.tick();
    }

    if state.unlocked {
        // Only this hands the displays back. Leaving any other way keeps them
        // blank, which is the right end for a lock screen that goes wrong.
        lock.unlock_and_destroy();
        connection
            .roundtrip()
            .map_err(|err| format!("could not finish unlocking: {err}"))?;
    }
    Ok(())
}

/// How long until the wall clock's seconds change.
fn until_next_second() -> Duration {
    let nanos = chrono::Timelike::nanosecond(&chrono::Local::now()).min(999_999_999);
    Duration::from_nanos(1_000_000_000 - u64::from(nanos))
}

impl State {
    /// Puts a surface on every display that does not have one.
    fn cover_outputs(&mut self, handle: &QueueHandle<State>) {
        let (Some(compositor), Some(lock)) = (self.globals.compositor.clone(), self.lock.clone())
        else {
            return;
        };
        for index in self.panels.len()..self.globals.outputs.len() {
            let output = self.globals.outputs[index].clone();
            let surface = compositor.create_surface(handle, ());
            // Nothing keeps hold of the lock surface: after its configure is
            // acknowledged there is nothing left to say to it, and it lives as
            // long as the connection does.
            lock.get_lock_surface(&surface, &output, handle, index);
            let viewport = self
                .globals
                .viewporter
                .as_ref()
                .map(|viewporter| viewporter.get_viewport(&surface, handle, ()));
            if let Some(fractional) = &self.globals.fractional {
                fractional.get_fractional_scale(&surface, handle, index);
            }
            self.panels.push(Panel {
                surface,
                viewport,
                logical: (0, 0),
                scale_120: 120,
                configured: false,
                pool: None,
                canvas: None,
                pending: false,
                awaiting_frame: false,
                committed: Instant::now(),
            });
        }
    }

    fn screen(&self) -> Screen {
        let now = chrono::Local::now();
        Screen {
            time: now.format("%H:%M").to_string(),
            seconds: now.format(":%S").to_string(),
            date: now.format("%A, %-d %B").to_string().to_lowercase(),
            user: self.user.clone(),
            host: self.host.clone(),
            status: self.status.clone(),
            caps: self.xkb.caps(),
        }
    }

    /// Repaints when the clock has moved on.
    fn tick(&mut self) {
        let now = chrono::Local::now().format("%H:%M:%S").to_string();
        if self.clock != now {
            self.clock = now;
            self.dirty = true;
        }
    }

    fn redraw(&mut self, handle: &QueueHandle<State>) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let screen = self.screen();
        let Some(shm) = self.globals.shm.clone() else {
            return;
        };

        for (index, panel) in self.panels.iter_mut().enumerate() {
            if !panel.configured {
                continue;
            }
            if panel.awaiting_frame && panel.committed.elapsed() < FRAME_BACKSTOP {
                // Drawing now would only queue a frame the compositor is not
                // ready for. Whatever asked for this repaint still wants it.
                self.dirty = true;
                continue;
            }
            let size = panel.pixels();
            let scale = panel.scale();
            let logical = panel.logical;

            if panel
                .canvas
                .as_ref()
                .is_none_or(|canvas| (canvas.width() as i32, canvas.height() as i32) != size)
            {
                panel.canvas = tiny_skia::Pixmap::new(size.0 as u32, size.1 as u32);
            }
            if panel.pool.as_ref().is_none_or(|pool| pool.size != size) {
                // Dropped first, so its buffers are gone before the
                // replacements are cut.
                panel.pool = None;
                panel.pool = Pool::new(&shm, handle, size);
            }
            let (Some(canvas), Some(pool)) = (panel.canvas.as_mut(), panel.pool.as_ref()) else {
                continue;
            };
            let Some(slot) = pool.free() else {
                // Both slots are still the compositor's. Ask again when one
                // comes back rather than dropping the frame on the floor.
                panel.pending = true;
                continue;
            };

            paint::draw(
                &mut canvas.as_mut(),
                &screen,
                &mut self.text,
                &self.palette,
                scale,
            );

            // tiny-skia stores premultiplied RGBA; wayland's Argb8888 is
            // little-endian BGRA, so the two outer channels swap. Swapped in
            // place rather than into a copy: at full screen the copy is the
            // largest thing on either side of this, and the next frame fills
            // the whole canvas again before it reads any of it.
            let bytes = canvas.data_mut();
            for pixel in bytes.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
            if pool.file.write_all_at(bytes, slot.offset).is_err() {
                continue;
            }

            slot.busy.store(true, Ordering::Release);
            panel.pending = false;
            // What the buffer is in pixels and what it means in logical space
            // are two different numbers whenever the scale is not one, and only
            // the viewport can hold a fraction between them.
            if let Some(viewport) = &panel.viewport {
                viewport.set_destination(logical.0.max(1) as i32, logical.1.max(1) as i32);
            }
            panel.surface.attach(Some(&slot.buffer), 0, 0);
            panel.surface.damage_buffer(0, 0, size.0, size.1);
            panel.surface.frame(handle, index);
            panel.awaiting_frame = true;
            panel.committed = Instant::now();
            panel.surface.commit();
        }
    }

    /// Hands the password to PAM on a thread of its own.
    ///
    /// PAM takes a couple of seconds to refuse, by design -- it is what makes
    /// guessing expensive. Asking it here would stop the clock and the keyboard
    /// for that whole time, which reads as a lock screen that has crashed at
    /// exactly the moment somebody is anxious about it.
    fn submit(&mut self) {
        if self.checking || self.entry.is_empty() {
            return;
        }
        let Ok(waker) = self.waker.try_clone() else {
            return;
        };
        self.checking = true;
        self.status = Status::Checking;
        self.denied_at = None;
        self.dirty = true;

        let service = self.service.clone();
        let user = self.user.clone();
        // Taken rather than copied: the password leaves this struct at the same
        // moment it is asked about.
        let password = std::mem::take(&mut self.entry);
        let answers = self.answers.clone();
        std::thread::spawn(move || {
            let verdict = crate::auth::verify(&service, &user, &password);
            let _ = answers.send(Verdict(verdict));
            // The loop is asleep in poll, and an answer arriving is something
            // happening.
            let _ = std::fs::File::from(waker).write_all(b"1");
        });
    }

    fn collect(&mut self) {
        while let Ok(Verdict(result)) = self.verdicts.try_recv() {
            self.checking = false;
            self.dirty = true;
            match result {
                Ok(()) => {
                    self.status = Status::Accepted;
                    self.unlocked = true;
                    self.finished = true;
                }
                Err(why) => {
                    self.status = Status::Denied(why.to_string());
                    self.denied_at = Some(Instant::now());
                }
            }
        }
    }

    /// How long the refusal on screen has left, if there is one.
    fn denial_due(&self) -> Option<Duration> {
        self.denied_at
            .map(|at| DENIED_FOR.saturating_sub(at.elapsed()))
    }

    fn expire_denial(&mut self) {
        if self.denial_due().is_some_and(|left| left.is_zero()) {
            self.denied_at = None;
            self.status = Status::Typing(0);
            self.dirty = true;
        }
    }

    /// Anything typed clears a refusal, so the screen stops shouting the moment
    /// the next attempt begins.
    fn editing(&mut self) {
        self.denied_at = None;
        self.status = Status::Typing(self.entry.chars().count());
        self.dirty = true;
    }

    fn typed(&mut self, text: &str) {
        if self.checking || self.entry.len() + text.len() > MAX_PASSWORD {
            return;
        }
        self.entry.push_str(text);
        self.editing();
    }

    fn backspace(&mut self) {
        if self.checking {
            return;
        }
        self.entry.pop();
        self.editing();
    }

    fn clear(&mut self) {
        if self.checking {
            return;
        }
        self.entry.clear();
        self.editing();
    }

    fn pressed(&mut self, code: u32) {
        let Some(xkb) = self.xkb.state.as_ref() else {
            return;
        };
        // Wayland reports evdev codes; xkb counts from eight higher.
        let key = xkb::Keycode::from(code + 8);
        let sym = xkb.key_get_one_sym(key).raw();
        let typed = xkb.key_get_utf8(key);
        match sym {
            xkb::keysyms::KEY_Return | xkb::keysyms::KEY_KP_Enter => self.submit(),
            xkb::keysyms::KEY_BackSpace => self.backspace(),
            xkb::keysyms::KEY_Escape => self.clear(),
            // A keysym with no text of its own is a modifier or a function key,
            // and a control character is not password material either.
            _ if !typed.is_empty() && !typed.chars().any(char::is_control) => self.typed(&typed),
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => {
                state.globals.compositor = Some(registry.bind(name, version.min(4), handle, ()));
            }
            "wl_shm" => state.globals.shm = Some(registry.bind(name, version.min(1), handle, ())),
            "wl_seat" => {
                let seat: WlSeat = registry.bind(name, version.min(7), handle, ());
                seat.get_keyboard(handle, ());
            }
            "wl_output" => {
                state
                    .globals
                    .outputs
                    .push(registry.bind(name, version.min(4), handle, ()));
                // A display plugged in while the session is locked needs
                // covering too, and the compositor will show nothing on it
                // until it is.
                state.cover_outputs(handle);
            }
            "ext_session_lock_manager_v1" => {
                state.globals.lock_manager = Some(registry.bind(name, version.min(1), handle, ()));
            }
            "wp_viewporter" => {
                state.globals.viewporter = Some(registry.bind(name, version.min(1), handle, ()));
            }
            "wp_fractional_scale_manager_v1" => {
                state.globals.fractional = Some(registry.bind(name, version.min(1), handle, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // Every display is covered and the desktop is no longer visible.
            // The one moment anything waiting on this locker may go ahead: a
            // machine told to suspend before this has arrived would go to
            // sleep with the desktop still on screen.
            ext_session_lock_v1::Event::Locked => {
                if let Some(ready) = state.ready.take() {
                    let _ = rustix::io::write(&ready, b"1");
                }
            }
            // The compositor refused the lock, or took it away. Stopping
            // without unlocking is deliberate: the screens stay blank.
            ext_session_lock_v1::Event::Finished => {
                eprintln!("irontile-lock: the compositor ended the lock");
                state.finished = true;
                state.unlocked = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockSurfaceV1, usize> for State {
    fn event(
        state: &mut Self,
        surface: &ExtSessionLockSurfaceV1,
        event: ext_session_lock_surface_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        else {
            return;
        };
        // Acknowledged before anything is drawn: attaching a buffer of any size
        // other than the one just agreed to is a protocol error, and the
        // compositor is right to hang up over it.
        surface.ack_configure(serial);
        if let Some(panel) = state.panels.get_mut(*index) {
            panel.logical = (width, height);
            panel.configured = true;
            state.dirty = true;
        }
    }
}

impl Dispatch<WpFractionalScaleV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(panel) = state.panels.get_mut(*index)
            && scale > 0
            && panel.scale_120 != scale
        {
            panel.scale_120 = scale;
            state.dirty = true;
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if format != WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) {
                    return;
                }
                // Read rather than mapped: mapping it is what xkbcommon
                // offers, but it is an unsafe call for a few kilobytes that
                // arrive once, and the only thing on the other side of this
                // descriptor is text.
                //
                // From an explicit offset, because the compositor may hand the
                // same open file to every client it has, and a plain read would
                // move a position that is not this client's to move.
                let mut source = vec![0u8; size as usize];
                if std::fs::File::from(fd)
                    .read_exact_at(&mut source, 0)
                    .is_err()
                {
                    return;
                }
                // The compositor sends the keymap NUL-terminated, and a stray
                // NUL is enough to stop it compiling.
                let Ok(source) = String::from_utf8(source) else {
                    return;
                };
                let source = source.trim_end_matches('\0').to_string();
                // A keymap that will not compile leaves the previous one in
                // place, which beats a lock screen that types nothing.
                if let Some(keymap) = xkb::Keymap::new_from_string(
                    &state.xkb.context,
                    source,
                    xkb::KEYMAP_FORMAT_TEXT_V1,
                    xkb::KEYMAP_COMPILE_NO_FLAGS,
                ) {
                    state.xkb.state = Some(xkb::State::new(&keymap));
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb) = state.xkb.state.as_mut() {
                    xkb.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                    // Caps lock is on the screen, so its state is a repaint.
                    state.dirty = true;
                }
            }
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(wl_keyboard::KeyState::Pressed),
                ..
            } => state.pressed(key),
            _ => {}
        }
    }
}

impl Dispatch<WlBuffer, Arc<AtomicBool>> for State {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        busy: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            busy.store(false, Ordering::Release);
            // Only when a frame was actually held back: every redraw releases
            // the one before it, and redrawing on that would never stop.
            if state.panels.iter().any(|panel| panel.pending) {
                state.dirty = true;
            }
        }
    }
}

impl Dispatch<WlCallback, usize> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Per display, not shared: one screen still awake would otherwise keep
        // asking the sleeping one to draw.
        if let wl_callback::Event::Done { .. } = event
            && let Some(panel) = state.panels.get_mut(*index)
        {
            panel.awaiting_frame = false;
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        _: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore ExtSessionLockManagerV1);
delegate_noop!(State: ignore WpViewporter);
delegate_noop!(State: ignore WpViewport);
delegate_noop!(State: ignore WpFractionalScaleManagerV1);
