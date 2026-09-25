//! The window the dialog appears in.
//!
//! A layer surface on the overlay, taking the keyboard exclusively: an
//! authentication prompt that a tiling compositor put in a tile would be a
//! prompt that moved the windows around every time something asked for an
//! administrator, and one that did not hold the keyboard would be a password
//! field typing into whatever was behind it.
//!
//! Its own event loop, run inside the call polkit is waiting on. That is the
//! shape the protocol asks for -- the reply is the answer -- and it means the
//! dialog is up for exactly as long as the question is open.

use std::os::fd::AsFd;
use std::os::unix::fs::FileExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::event::{PollFd, PollFlags};
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_compositor::WlCompositor,
    wl_keyboard::{self, WlKeyboard},
    wl_registry,
    wl_seat::WlSeat,
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};
use xkbcommon::xkb;

use crate::paint::{self, Palette, Prompt, Text};

/// How large the dialog is, in logical pixels.
const WIDTH: i32 = 460;
const HEIGHT: i32 = 200;

/// The longest password that will be accepted, so a key stuck down cannot grow
/// one without bound.
const MAX_PASSWORD: usize = 256;

/// What the person did with the dialog.
pub enum Outcome {
    /// A password to try.
    Entered(String),
    /// Escape, or the compositor took the surface away.
    Dismissed,
}

#[derive(Default)]
struct Globals {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
}

struct State {
    globals: Globals,
    prompt: Prompt,
    palette: Palette,
    text: Text,
    entry: String,
    /// Set once the surface has been told how large it is; nothing may be
    /// attached before that.
    configured: bool,
    /// Logical size the compositor settled on.
    size: (i32, i32),
    dirty: bool,
    done: Option<Outcome>,
    xkb: Option<xkb::State>,
    context: xkb::Context,
    buffer: Option<Buffer>,
}

/// One shared-memory buffer, and whether the compositor still has it.
struct Buffer {
    buffer: WlBuffer,
    file: std::fs::File,
    busy: Arc<AtomicBool>,
    size: (i32, i32),
}

/// Shows the dialog and waits for an answer.
pub fn ask(prompt: Prompt, families: &[String]) -> Result<Outcome, String> {
    let connection =
        Connection::connect_to_env().map_err(|err| format!("no compositor to ask on: {err}"))?;
    let mut queue = connection.new_event_queue();
    let handle = queue.handle();
    let display = connection.display();
    display.get_registry(&handle, ());

    let mut state = State {
        globals: Globals::default(),
        prompt,
        palette: Palette::default(),
        text: Text::new(families),
        entry: String::new(),
        configured: false,
        size: (WIDTH, HEIGHT),
        dirty: true,
        done: None,
        xkb: None,
        context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        buffer: None,
    };

    queue
        .roundtrip(&mut state)
        .map_err(|err| format!("could not read the compositor's globals: {err}"))?;

    let compositor = state
        .globals
        .compositor
        .clone()
        .ok_or("the compositor offers no wl_compositor")?;
    let layer_shell = state
        .globals
        .layer_shell
        .clone()
        .ok_or("the compositor offers no layer shell, so there is nowhere to put a dialog")?;

    let surface = compositor.create_surface(&handle, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "irontile-polkit".to_string(),
        &handle,
        (),
    );
    layer.set_size(WIDTH as u32, HEIGHT as u32);
    // No anchors: a surface anchored to nothing is centred, which is where a
    // question belongs.
    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive);
    surface.commit();

    while state.done.is_none() {
        if state.configured && state.dirty {
            draw(&mut state, &surface, &handle);
        }
        connection
            .flush()
            .map_err(|err| format!("could not flush: {err}"))?;

        let read = queue
            .prepare_read()
            .ok_or("the event queue is already being read")?;
        let fd = read
            .connection_fd()
            .try_clone_to_owned()
            .map_err(|err| format!("could not watch the compositor: {err}"))?;
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        if let Err(err) = rustix::event::poll(&mut fds, None) {
            drop(read);
            if err == rustix::io::Errno::INTR {
                continue;
            }
            return Err(format!("waiting on the compositor failed: {err}"));
        }
        if fds[0].revents().contains(PollFlags::IN) {
            let _ = read.read();
        } else {
            drop(read);
        }
        queue
            .dispatch_pending(&mut state)
            .map_err(|err| format!("the compositor sent something unreadable: {err}"))?;
    }

    layer.destroy();
    surface.destroy();
    let _ = connection.flush();
    Ok(state.done.unwrap_or(Outcome::Dismissed))
}

/// Paints the dialog and hands it over.
fn draw(state: &mut State, surface: &WlSurface, handle: &QueueHandle<State>) {
    let Some(shm) = state.globals.shm.clone() else {
        return;
    };
    let (w, h) = state.size;
    let stride = w * 4;
    let total = (stride * h) as usize;

    // Made once and kept, unless the compositor changed its mind about the
    // size. A dialog redraws on keystrokes, which is slow enough that one
    // buffer is plenty -- but it must not be scribbled on while the compositor
    // is reading it.
    let stale = state
        .buffer
        .as_ref()
        .is_none_or(|buffer| buffer.size != (w, h));
    if stale {
        let Ok(file) =
            rustix::fs::memfd_create(c"irontile-polkit", rustix::fs::MemfdFlags::CLOEXEC)
        else {
            return;
        };
        if rustix::fs::ftruncate(&file, total as u64).is_err() {
            return;
        }
        let file = std::fs::File::from(file);
        let pool = shm.create_pool(file.as_fd(), total as i32, handle, ());
        let busy = Arc::new(AtomicBool::new(false));
        let buffer = pool.create_buffer(
            0,
            w,
            h,
            stride,
            wl_shm::Format::Argb8888,
            handle,
            Arc::clone(&busy),
        );
        pool.destroy();
        state.buffer = Some(Buffer {
            buffer,
            file,
            busy,
            size: (w, h),
        });
    }

    let Some(held) = &state.buffer else {
        return;
    };
    if held.busy.load(Ordering::Acquire) {
        // Still on screen. Whatever asked for this redraw still wants it.
        return;
    }

    let Some(mut canvas) = tiny_skia::Pixmap::new(w as u32, h as u32) else {
        return;
    };
    state.prompt.typed = state.entry.chars().count();
    paint::draw(
        &mut canvas.as_mut(),
        &state.prompt,
        &mut state.text,
        &state.palette,
        1.0,
    );

    // tiny-skia stores premultiplied RGBA; wayland's Argb8888 is little-endian
    // BGRA, so the two outer channels swap.
    let bytes = canvas.data_mut();
    for pixel in bytes.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    if held.file.write_all_at(bytes, 0).is_err() {
        return;
    }

    held.busy.store(true, Ordering::Release);
    surface.attach(Some(&held.buffer), 0, 0);
    surface.damage_buffer(0, 0, w, h);
    surface.commit();
    state.dirty = false;
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
            "zwlr_layer_shell_v1" => {
                state.globals.layer_shell = Some(registry.bind(name, version.min(4), handle, ()));
            }
            "wl_seat" => {
                let seat: WlSeat = registry.bind(name, version.min(7), handle, ());
                seat.get_keyboard(handle, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                // A zero means "you decide", which is what was asked for.
                if width > 0 {
                    state.size.0 = width as i32;
                }
                if height > 0 {
                    state.size.1 = height as i32;
                }
                state.configured = true;
                state.dirty = true;
            }
            // The compositor has taken the surface away: the session is
            // locking, or something else decided this may not be on screen.
            // Answering nothing is the only honest outcome.
            zwlr_layer_surface_v1::Event::Closed => {
                state.done = Some(Outcome::Dismissed);
            }
            _ => {}
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
                if !matches!(format, WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)) {
                    return;
                }
                // Read from an explicit offset, because the compositor may
                // hand the same open file to every client it has and a plain
                // read would move a position that is not this client's to
                // move. The same reasoning as the lock screen, which reads it
                // the same way.
                let mut source = vec![0u8; size as usize];
                if std::fs::File::from(fd)
                    .read_exact_at(&mut source, 0)
                    .is_err()
                {
                    return;
                }
                // It arrives NUL-terminated, and a stray NUL stops it
                // compiling.
                let Ok(source) = String::from_utf8(source) else {
                    return;
                };
                let source = source.trim_end_matches('\0').to_string();
                if let Some(keymap) = xkb::Keymap::new_from_string(
                    &state.context,
                    source,
                    xkb::KEYMAP_FORMAT_TEXT_V1,
                    xkb::KEYMAP_COMPILE_NO_FLAGS,
                ) {
                    state.xkb = Some(xkb::State::new(&keymap));
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb) = &mut state.xkb {
                    xkb.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Key {
                key,
                state: pressed,
                ..
            } => {
                if !matches!(pressed, WEnum::Value(wl_keyboard::KeyState::Pressed)) {
                    return;
                }
                let Some(xkb) = &state.xkb else { return };
                // Wayland keycodes are offset by eight from xkb's.
                let code = key + 8;
                let sym = xkb.key_get_one_sym(code.into());
                match sym {
                    xkb::Keysym::Escape => state.done = Some(Outcome::Dismissed),
                    xkb::Keysym::Return | xkb::Keysym::KP_Enter => {
                        let entered = std::mem::take(&mut state.entry);
                        state.done = Some(Outcome::Entered(entered));
                    }
                    xkb::Keysym::BackSpace => {
                        state.entry.pop();
                        state.dirty = true;
                    }
                    _ => {
                        let typed = xkb.key_get_utf8(code.into());
                        // Control characters are not password material.
                        if !typed.is_empty()
                            && !typed.chars().any(|c| c.is_control())
                            && state.entry.len() + typed.len() <= MAX_PASSWORD
                        {
                            state.entry.push_str(&typed);
                            state.dirty = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlBuffer, Arc<AtomicBool>> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        busy: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            busy.store(false, Ordering::Release);
        }
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ignore ZwlrLayerShellV1);
