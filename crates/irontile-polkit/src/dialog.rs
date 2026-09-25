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
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
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

/// How many buffers to cycle through.
///
/// Two, for the reason the lock screen and the bar use two: a buffer handed
/// over belongs to the compositor until it says otherwise, so a surface with
/// one buffer draws its first frame and then waits forever for a release that
/// only arrives when something else is attached. That is a dialog that takes
/// the keyboard and never shows a single keystroke.
const SLOTS: usize = 2;

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
    viewporter: Option<WpViewporter>,
    fractional: Option<WpFractionalScaleManagerV1>,
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
    pool: Option<Pool>,
    /// Scale in 120ths, which is how the fractional-scale protocol counts. A
    /// display that never says otherwise stays at one.
    scale_120: u32,
    viewport: Option<WpViewport>,
}

impl State {
    fn scale(&self) -> f32 {
        self.scale_120 as f32 / 120.0
    }

    /// The buffer size in real pixels, which is the logical size scaled.
    fn pixels(&self) -> (i32, i32) {
        in_pixels(self.size, self.scale_120)
    }
}

/// A logical size in real pixels, at a scale counted in 120ths.
///
/// Free-standing because the sum is the whole of the scaling: getting it wrong
/// is a dialog that comes out three-quarter size and soft, and that is worth a
/// test that needs no compositor to run.
fn in_pixels(size: (i32, i32), scale_120: u32) -> (i32, i32) {
    let scale = (scale_120 as f32 / 120.0).max(f32::MIN_POSITIVE);
    (
        ((size.0 as f32 * scale).round() as i32).max(1),
        ((size.1 as f32 * scale).round() as i32).max(1),
    )
}

/// The shared memory this surface draws into, cut into slots.
struct Pool {
    file: std::fs::File,
    slots: [Slot; SLOTS],
    /// The pixel size the slots are cut for. A different one means starting
    /// over, which is what a scale change looks like.
    size: (i32, i32),
}

struct Slot {
    buffer: WlBuffer,
    busy: Arc<AtomicBool>,
    offset: u64,
}

impl Pool {
    /// The first slot the compositor is not still reading from.
    fn free(&self) -> Option<&Slot> {
        self.slots
            .iter()
            .find(|slot| !slot.busy.load(Ordering::Acquire))
    }
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
        pool: None,
        scale_120: 120,
        viewport: None,
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
    // A viewport and the scale that goes with it. Without them the buffer is
    // taken as logical pixels, so on a display at four thirds the dialog comes
    // out three quarters the size it asked for and soft at the edges.
    let viewport = state
        .globals
        .viewporter
        .as_ref()
        .map(|viewporter| viewporter.get_viewport(&surface, &handle, ()));
    if let Some(fractional) = &state.globals.fractional {
        fractional.get_fractional_scale(&surface, &handle, ());
    }
    state.viewport = viewport;

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
    let (w, h) = state.pixels();

    // Cut again whenever the size changes, which is a scale change or the
    // compositor settling on something other than what was asked for.
    let stale = state.pool.as_ref().is_none_or(|pool| pool.size != (w, h));
    if stale {
        let Some(pool) = Pool::new(&shm, handle, (w, h)) else {
            return;
        };
        state.pool = Some(pool);
    }

    let Some(pool) = &state.pool else { return };
    let Some(slot) = pool.free() else {
        // Both are still out. Whatever asked for this redraw still wants it,
        // and the release that is coming will bring the loop back here.
        return;
    };

    let Some(mut canvas) = tiny_skia::Pixmap::new(w as u32, h as u32) else {
        return;
    };
    state.prompt.typed = state.entry.chars().count();
    // Read before the drawing borrows the rest of the state.
    let scale = state.scale();
    let State {
        prompt,
        text,
        palette,
        ..
    } = state;
    paint::draw(&mut canvas.as_mut(), prompt, text, palette, scale);

    // tiny-skia stores premultiplied RGBA; wayland's Argb8888 is little-endian
    // BGRA, so the two outer channels swap.
    let bytes = canvas.data_mut();
    for pixel in bytes.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    if pool.file.write_all_at(bytes, slot.offset).is_err() {
        return;
    }

    slot.busy.store(true, Ordering::Release);
    // What the buffer is in pixels and what it means on screen are two
    // different numbers whenever the scale is not one, and only the viewport
    // can hold a fraction between them.
    if let Some(viewport) = &state.viewport {
        viewport.set_destination(state.size.0.max(1), state.size.1.max(1));
    }
    surface.attach(Some(&slot.buffer), 0, 0);
    surface.damage_buffer(0, 0, w, h);
    surface.commit();
    state.dirty = false;
}

impl Pool {
    /// Cuts a fresh pool into slots of `size` pixels.
    fn new(shm: &WlShm, handle: &QueueHandle<State>, size: (i32, i32)) -> Option<Pool> {
        let (w, h) = size;
        let stride = w.checked_mul(4)?;
        let slot = i64::from(stride) * i64::from(h);
        let total = i32::try_from(slot * SLOTS as i64).ok()?;
        if total <= 0 {
            return None;
        }

        let file =
            rustix::fs::memfd_create(c"irontile-polkit", rustix::fs::MemfdFlags::CLOEXEC).ok()?;
        rustix::fs::ftruncate(&file, total as u64).ok()?;
        let file = std::fs::File::from(file);
        let pool = shm.create_pool(file.as_fd(), total, handle, ());

        let slots = std::array::from_fn(|i| {
            let busy = Arc::new(AtomicBool::new(false));
            let offset = slot * i as i64;
            let buffer = pool.create_buffer(
                i32::try_from(offset).unwrap_or(0),
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
        // The buffers are cut from it and outlive it, so the pool itself is
        // not needed once they exist.
        pool.destroy();
        Some(Pool { file, slots, size })
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
            "wp_viewporter" => {
                state.globals.viewporter = Some(registry.bind(name, version.min(1), handle, ()));
            }
            "wp_fractional_scale_manager_v1" => {
                state.globals.fractional = Some(registry.bind(name, version.min(1), handle, ()));
            }
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

impl Dispatch<WpFractionalScaleV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && scale > 0
            && state.scale_120 != scale
        {
            state.scale_120 = scale;
            state.dirty = true;
        }
    }
}

delegate_noop!(State: ignore WpViewporter);
delegate_noop!(State: ignore WpViewport);
delegate_noop!(State: ignore WpFractionalScaleManagerV1);
delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ignore ZwlrLayerShellV1);

#[cfg(test)]
mod tests {
    use super::{HEIGHT, WIDTH, in_pixels};

    #[test]
    fn a_display_at_one_asks_for_the_size_it_was_given() {
        assert_eq!(in_pixels((WIDTH, HEIGHT), 120), (WIDTH, HEIGHT));
    }

    #[test]
    fn a_fractional_display_asks_for_more_pixels_than_the_dialog_is_wide() {
        // 1.3333, the scale a 1080p panel gets from a 4K-ish desktop. Drawing
        // 460 pixels here and letting the compositor stretch them is the
        // difference between crisp text and soft text.
        let (w, h) = in_pixels((460, 200), 160);
        assert_eq!((w, h), (613, 267));
    }

    #[test]
    fn a_scale_of_nothing_still_leaves_something_to_draw_on() {
        // A compositor should never send this, and a zero-sized buffer is a
        // protocol error rather than a small dialog.
        assert_eq!(in_pixels((460, 200), 0), (1, 1));
    }
}
