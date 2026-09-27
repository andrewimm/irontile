//! The surfaces: popups at the corner, and the panel down the side.
//!
//! Layer-shell clients with no special access to anything. The popups sit on
//! the overlay because a notification that a fullscreen video covered would be
//! a notification nobody sees; the panel sits on the top layer and takes the
//! keyboard while it is open, so escape closes it.
//!
//! One loop drives both, waiting on Wayland's socket, the pipe the bus thread
//! rings, and whichever notification expires soonest.

use std::collections::HashMap;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags};
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_compositor::WlCompositor,
    wl_keyboard::{self, WlKeyboard},
    wl_pointer::{self, WlPointer},
    wl_registry,
    wl_seat::{self, WlSeat},
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

use crate::draw::TextRenderer;
use crate::icon::IconSet;
use crate::notify::model::{Centre, Hit, Notification, Urgency, power_buttons};
use crate::notify::paint::{self, Palette, size};
use crate::notify::service::{Closed, Reply, Service};

/// How many buffers each surface cycles through.
///
/// Two, for the reason the lock screen and the polkit dialog use two: a buffer
/// handed over belongs to the compositor until it says otherwise, so a surface
/// with one draws its first frame and then waits forever for a release that
/// only arrives when something else is attached.
const SLOTS: usize = 2;

/// How long an ordinary notification stays on screen.
const LINGER: Duration = Duration::from_secs(6);
/// A low-urgency one goes sooner; nobody is waiting for it.
const LINGER_LOW: Duration = Duration::from_secs(3);
/// How far from the corner the popups sit.
const MARGIN: i32 = 12;

/// Which surface an event belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Popup(u32),
    Panel,
}

#[derive(Default)]
struct Globals {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    seat: Option<WlSeat>,
    viewporter: Option<WpViewporter>,
    fractional: Option<WpFractionalScaleManagerV1>,
}

struct Slot {
    buffer: WlBuffer,
    busy: Arc<AtomicBool>,
    offset: u64,
}

struct Pool {
    file: std::fs::File,
    slots: [Slot; SLOTS],
    size: (i32, i32),
}

impl Pool {
    fn free(&self) -> Option<&Slot> {
        self.slots
            .iter()
            .find(|slot| !slot.busy.load(Ordering::Acquire))
    }

    fn new(shm: &WlShm, handle: &QueueHandle<State>, size: (i32, i32)) -> Option<Pool> {
        let (w, h) = size;
        let stride = w.checked_mul(4)?;
        let slot = i64::from(stride) * i64::from(h);
        let total = i32::try_from(slot * SLOTS as i64).ok()?;
        if total <= 0 {
            return None;
        }
        let file =
            rustix::fs::memfd_create(c"irontile-notify", rustix::fs::MemfdFlags::CLOEXEC).ok()?;
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
        pool.destroy();
        Some(Pool { file, slots, size })
    }
}

/// One layer surface and everything needed to paint it.
struct Panel {
    surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    viewport: Option<WpViewport>,
    pool: Option<Pool>,
    /// Logical size, which is what the compositor configures.
    size: (i32, i32),
    /// Scale in 120ths, the way the fractional-scale protocol counts.
    scale_120: u32,
    configured: bool,
    dirty: bool,
}

impl Panel {
    fn scale(&self) -> f32 {
        self.scale_120 as f32 / 120.0
    }

    fn pixels(&self) -> (i32, i32) {
        let scale = self.scale();
        (
            ((self.size.0 as f32 * scale).round() as i32).max(1),
            ((self.size.1 as f32 * scale).round() as i32).max(1),
        )
    }

    fn destroy(self) {
        self.layer.destroy();
        if let Some(viewport) = &self.viewport {
            viewport.destroy();
        }
        self.surface.destroy();
    }
}

/// A notification on screen, and when it should stop being.
struct Popup {
    panel: Panel,
    note: Notification,
    until: Option<Instant>,
}

struct State {
    globals: Globals,
    handle: QueueHandle<State>,
    popups: Vec<Popup>,
    centre: Option<Panel>,
    /// Where the pointer is, per surface, in that surface's logical pixels.
    pointer: HashMap<Key, (f32, f32)>,
    /// Which surface the pointer is on, if any.
    over: Option<Key>,
    /// What was decided this pass, for the loop to act on.
    pressed: Vec<(Key, (f32, f32))>,
    /// Set when escape was pressed while the panel had the keyboard.
    escaped: bool,
    hovered: Option<Hit>,
    quit: bool,
}

/// Runs the surfaces until something says to stop.
pub fn run(service: &Service, theme: &str, families: &[String]) -> Result<(), String> {
    let connection =
        Connection::connect_to_env().map_err(|err| format!("no compositor to draw on: {err}"))?;
    let mut queue = connection.new_event_queue();
    let handle = queue.handle();
    connection.display().get_registry(&handle, ());

    let mut state = State {
        globals: Globals::default(),
        handle: handle.clone(),
        popups: Vec::new(),
        centre: None,
        pointer: HashMap::new(),
        over: None,
        pressed: Vec::new(),
        escaped: false,
        hovered: None,
        quit: false,
    };
    queue
        .roundtrip(&mut state)
        .map_err(|err| format!("could not read the compositor's globals: {err}"))?;
    if state.globals.layer_shell.is_none() {
        return Err("the compositor offers no layer shell, so there is nowhere to draw".into());
    }

    let mut icons = IconSet::new(theme, None);
    let mut text = TextRenderer::new(families, size::BODY);
    let palette = Palette::default();
    let buttons = power_buttons();
    let started = Instant::now();
    // When each notification arrived, so the panel can say how long ago.
    let mut arrived: HashMap<u32, Instant> = HashMap::new();

    while !state.quit {
        service.drain();
        let shared = service.state();

        // Notifications that have gone from the bus side take their popup with
        // them: a sender that closed one means it.
        let gone: Vec<u32> = state
            .popups
            .iter()
            .map(|popup| popup.note.id)
            .filter(|id| !shared.live.iter().any(|note| note.id == *id))
            .collect();
        for id in gone {
            close_popup(&mut state, id);
        }

        for note in &shared.live {
            arrived.entry(note.id).or_insert_with(Instant::now);
            let silent = shared.silent.contains(&note.id);
            let shown = state.popups.iter().any(|popup| popup.note.id == note.id);
            if shown || silent {
                continue;
            }
            // Only what has not been seen: a notification that expired stays
            // in the panel, and putting it back on screen would be a popup
            // that came back by itself.
            if arrived
                .get(&note.id)
                .is_some_and(|at| at.elapsed() > LINGER * 2)
            {
                continue;
            }
            let until = match note.urgency {
                // Critical stays until it is dismissed. That is the whole of
                // what critical means, and a timer would take it away while
                // somebody was reading it.
                Urgency::Critical => None,
                Urgency::Low => Some(Instant::now() + LINGER_LOW),
                Urgency::Normal => Some(Instant::now() + LINGER),
            };
            if let Some(popup) = make_popup(&mut state, note, &mut text) {
                state.popups.push(Popup {
                    panel: popup,
                    note: note.clone(),
                    until,
                });
            }
        }

        // Expiry.
        let now = Instant::now();
        let expired: Vec<u32> = state
            .popups
            .iter()
            .filter(|popup| popup.until.is_some_and(|at| at <= now))
            .map(|popup| popup.note.id)
            .collect();
        for id in expired {
            close_popup(&mut state, id);
            service.reply(Reply::Closed(id, Closed::Expired));
        }

        // The panel follows the switch on the bus side.
        match (shared.panel, state.centre.is_some()) {
            (true, false) => state.centre = make_centre(&mut state),
            (false, true) => {
                if let Some(panel) = state.centre.take() {
                    panel.destroy();
                }
                state.hovered = None;
            }
            _ => {}
        }

        // What was clicked, now that both sides agree on what is where.
        let presses = std::mem::take(&mut state.pressed);
        for (key, at) in presses {
            match key {
                Key::Popup(id) => {
                    let note = state
                        .popups
                        .iter()
                        .find(|popup| popup.note.id == id)
                        .map(|popup| popup.note.clone());
                    let Some(note) = note else { continue };
                    let scale = state
                        .panel_mut(key)
                        .map(|panel| panel.scale())
                        .unwrap_or(1.0);
                    let spots = paint::card_spots(&mut text, &note, (0.0, 0.0), scale);
                    let point = (at.0 * scale, at.1 * scale);
                    press(service, &note, paint::spot_at(&spots, point));
                    close_popup(&mut state, id);
                    close_notification(service, id);
                }
                Key::Panel => {
                    let centre = centre_state(&shared, &buttons, &arrived, state.hovered);
                    let Some(panel) = &state.centre else { continue };
                    let scale = panel.scale();
                    let spots = paint::spots(
                        &mut text,
                        &centre,
                        (panel.size.0 as f32 * scale, panel.size.1 as f32 * scale),
                        scale,
                    );
                    let point = (at.0 * scale, at.1 * scale);
                    match paint::spot_at(&spots, point) {
                        Some(Hit::Quiet) => toggle_quiet(service),
                        Some(Hit::Clear) => clear_all(service, &shared.live),
                        Some(hit @ (Hit::Card(id) | Hit::Action(id, _))) => {
                            if let Some(note) = shared.live.iter().find(|note| note.id == id) {
                                press(service, note, Some(hit));
                            }
                            close_popup(&mut state, id);
                            close_notification(service, id);
                        }
                        Some(Hit::Power(index)) => {
                            if let Some(button) = buttons.get(index) {
                                spawn(&button.run);
                                // Whatever was asked for, the panel has served
                                // its purpose and should not be left over the
                                // top of a machine going to sleep.
                                shut_panel(service);
                            }
                        }
                        None => {}
                    }
                }
            }
        }

        if state.escaped {
            state.escaped = false;
            shut_panel(service);
        }

        // Hover, which only the panel shows.
        if let Some(panel) = &state.centre {
            let scale = panel.scale();
            let centre = centre_state(&shared, &buttons, &arrived, state.hovered);
            let spots = paint::spots(
                &mut text,
                &centre,
                (panel.size.0 as f32 * scale, panel.size.1 as f32 * scale),
                scale,
            );
            let hovered = match (state.over, state.pointer.get(&Key::Panel)) {
                (Some(Key::Panel), Some(at)) => {
                    paint::spot_at(&spots, (at.0 * scale, at.1 * scale))
                }
                _ => None,
            };
            if hovered != state.hovered {
                state.hovered = hovered;
                if let Some(panel) = &mut state.centre {
                    panel.dirty = true;
                }
            }
        }

        // Draw whatever is owed a frame.
        let centre = centre_state(&shared, &buttons, &arrived, state.hovered);
        draw_all(
            &mut state, &mut icons, &mut text, &palette, &centre, started,
        );

        connection
            .flush()
            .map_err(|err| format!("could not flush: {err}"))?;

        // Wait for the compositor, the bus, or the next expiry -- and no
        // longer than a second even then, so "4m ago" in the panel does not
        // sit at four minutes while the clock moves on.
        let read = queue
            .prepare_read()
            .ok_or("the event queue is already being read")?;
        let wayland = read
            .connection_fd()
            .try_clone_to_owned()
            .map_err(|err| format!("could not watch the compositor: {err}"))?;
        let mut fds = [
            PollFd::new(&wayland, PollFlags::IN),
            PollFd::new(&service.wake, PollFlags::IN),
        ];
        let wait = next_wake(&state).min(Duration::from_secs(1));
        let wait = rustix::time::Timespec {
            tv_sec: wait.as_secs() as i64,
            tv_nsec: wait.subsec_nanos() as i64,
        };
        match rustix::event::poll(&mut fds, Some(&wait)) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => {
                drop(read);
                continue;
            }
            Err(err) => {
                drop(read);
                return Err(format!("waiting failed: {err}"));
            }
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
    Ok(())
}

/// How long until something needs doing without anybody asking.
fn next_wake(state: &State) -> Duration {
    let now = Instant::now();
    state
        .popups
        .iter()
        .filter_map(|popup| popup.until)
        .map(|at| at.saturating_duration_since(now))
        .min()
        .unwrap_or(Duration::from_secs(1))
}

/// The panel's view of the world, assembled from what the bus knows.
fn centre_state(
    shared: &crate::notify::service::Shared,
    buttons: &[crate::notify::model::Power],
    arrived: &HashMap<u32, Instant>,
    hovered: Option<Hit>,
) -> Centre {
    let mut notifications: Vec<Notification> = shared
        .live
        .iter()
        .rev()
        .map(|note| {
            let mut note = note.clone();
            note.age = arrived
                .get(&note.id)
                .map(Instant::elapsed)
                .unwrap_or_default();
            note
        })
        .collect();
    notifications.truncate(24);
    Centre {
        notifications,
        quiet: shared.quiet,
        buttons: buttons.to_vec(),
        hovered,
    }
}

fn make_popup(state: &mut State, note: &Notification, text: &mut TextRenderer) -> Option<Panel> {
    let compositor = state.globals.compositor.clone()?;
    let shell = state.globals.layer_shell.clone()?;
    let handle = state.handle.clone();
    let key = Key::Popup(note.id);

    let surface = compositor.create_surface(&handle, key);
    let layer = shell.get_layer_surface(
        &surface,
        None,
        // Above everything, including a fullscreen window: a notification
        // nobody can see is a notification that did not happen.
        zwlr_layer_shell_v1::Layer::Overlay,
        "irontile-notification".to_string(),
        &handle,
        key,
    );
    layer.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Right);
    // Stacked downwards by however much is already there.
    let offset = MARGIN + stacked_height(state, text);
    layer.set_margin(offset, MARGIN, 0, 0);
    let height = paint::card_height(text, note, 1.0).ceil() as i32;
    layer.set_size(size::CARD_W as u32, height.max(1) as u32);
    layer.set_exclusive_zone(0);
    Some(new_panel(
        state,
        surface,
        layer,
        key,
        (size::CARD_W as i32, height),
    ))
}

/// How much room the popups already take, in logical pixels.
fn stacked_height(state: &State, text: &mut TextRenderer) -> i32 {
    state
        .popups
        .iter()
        .map(|popup| paint::card_height(text, &popup.note, 1.0).ceil() as i32 + size::GAP as i32)
        .sum()
}

fn make_centre(state: &mut State) -> Option<Panel> {
    let compositor = state.globals.compositor.clone()?;
    let shell = state.globals.layer_shell.clone()?;
    let handle = state.handle.clone();
    let key = Key::Panel;

    let surface = compositor.create_surface(&handle, key);
    let layer = shell.get_layer_surface(
        &surface,
        None,
        // The top layer rather than the overlay: the panel is a thing you
        // opened, and a lock screen must still be able to cover it.
        zwlr_layer_shell_v1::Layer::Top,
        "irontile-notification-centre".to_string(),
        &handle,
        key,
    );
    layer.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer.set_margin(MARGIN, MARGIN, MARGIN, 0);
    // Height zero with both vertical anchors set means "as tall as there is",
    // which is what the protocol asks for and what a side panel wants.
    layer.set_size(size::PANEL_W as u32, 0);
    layer.set_exclusive_zone(0);
    // Escape closes it, so it has to be able to hear escape.
    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive);
    Some(new_panel(
        state,
        surface,
        layer,
        key,
        (size::PANEL_W as i32, 0),
    ))
}

fn new_panel(
    state: &mut State,
    surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    key: Key,
    size: (i32, i32),
) -> Panel {
    let handle = state.handle.clone();
    let viewport = state
        .globals
        .viewporter
        .as_ref()
        .map(|viewporter| viewporter.get_viewport(&surface, &handle, ()));
    if let Some(fractional) = &state.globals.fractional {
        fractional.get_fractional_scale(&surface, &handle, key);
    }
    surface.commit();
    Panel {
        surface,
        layer,
        viewport,
        pool: None,
        size,
        scale_120: 120,
        configured: false,
        dirty: true,
    }
}

fn close_popup(state: &mut State, id: u32) {
    if let Some(index) = state.popups.iter().position(|popup| popup.note.id == id) {
        let popup = state.popups.remove(index);
        popup.panel.destroy();
    }
}

fn draw_all(
    state: &mut State,
    icons: &mut IconSet,
    text: &mut TextRenderer,
    palette: &Palette,
    centre: &Centre,
    started: Instant,
) {
    let _ = started;
    let shm = state.globals.shm.clone();
    let Some(shm) = shm else { return };
    let handle = state.handle.clone();

    for index in 0..state.popups.len() {
        let note = state.popups[index].note.clone();
        let panel = &mut state.popups[index].panel;
        if !panel.configured || !panel.dirty {
            continue;
        }
        let scale = panel.scale();
        paint_into(panel, &shm, &handle, |pixmap, scale| {
            pixmap.fill(tiny_skia::Color::TRANSPARENT);
            paint::card(pixmap, icons, text, &note, palette, (0.0, 0.0), scale);
        });
        let _ = scale;
    }

    if let Some(panel) = &mut state.centre
        && panel.configured
        && panel.dirty
    {
        {
            let size = panel.pixels();
            paint_into(panel, &shm, &handle, |pixmap, scale| {
                paint::centre(
                    pixmap,
                    icons,
                    text,
                    centre,
                    palette,
                    (size.0 as f32, size.1 as f32),
                    scale,
                );
            });
        }
    }
}

/// Fills a surface's next free buffer and hands it over.
fn paint_into(
    panel: &mut Panel,
    shm: &WlShm,
    handle: &QueueHandle<State>,
    mut fill: impl FnMut(&mut tiny_skia::PixmapMut<'_>, f32),
) {
    let (w, h) = panel.pixels();
    let stale = panel.pool.as_ref().is_none_or(|pool| pool.size != (w, h));
    if stale {
        let Some(pool) = Pool::new(shm, handle, (w, h)) else {
            return;
        };
        panel.pool = Some(pool);
    }
    let Some(pool) = &panel.pool else { return };
    // Both still out: whatever asked for this redraw still wants it, and the
    // release that is coming brings the loop back here.
    let Some(slot) = pool.free() else { return };
    let Some(mut canvas) = tiny_skia::Pixmap::new(w as u32, h as u32) else {
        return;
    };
    fill(&mut canvas.as_mut(), panel.scale());

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
    if let Some(viewport) = &panel.viewport {
        viewport.set_destination(panel.size.0.max(1), panel.size.1.max(1));
    }
    panel.surface.attach(Some(&slot.buffer), 0, 0);
    panel.surface.damage_buffer(0, 0, w, h);
    panel.surface.commit();
    panel.dirty = false;
}

fn toggle_quiet(service: &Service) {
    if let Ok(mut shared) = service.shared.lock() {
        shared.quiet = !shared.quiet;
    }
}

fn shut_panel(service: &Service) {
    if let Ok(mut shared) = service.shared.lock() {
        shared.panel = false;
    }
}

fn close_notification(service: &Service, id: u32) {
    if let Ok(mut shared) = service.shared.lock() {
        shared.live.retain(|note| note.id != id);
    }
}

fn clear_all(service: &Service, live: &[Notification]) {
    for note in live {
        service.reply(Reply::Closed(note.id, Closed::Dismissed));
    }
    if let Ok(mut shared) = service.shared.lock() {
        shared.live.clear();
        shared.silent.clear();
    }
}

/// Runs one of the session buttons.
///
/// Detached deliberately: `systemctl poweroff` outliving this process is the
/// point, and a child waited on by a daemon that is about to be killed by the
/// very command it started would be a zombie either way.
fn spawn(argv: &[String]) {
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    match std::process::Command::new(program).args(args).spawn() {
        Ok(_) => {}
        Err(err) => eprintln!("irontile-notify: could not run {program}: {err}"),
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
            "wl_shm" => state.globals.shm = Some(registry.bind(name, 1, handle, ())),
            "zwlr_layer_shell_v1" => {
                state.globals.layer_shell = Some(registry.bind(name, version.min(4), handle, ()));
            }
            "wl_seat" => {
                state.globals.seat = Some(registry.bind(name, version.min(7), handle, ()));
            }
            "wp_viewporter" => {
                state.globals.viewporter = Some(registry.bind(name, 1, handle, ()));
            }
            "wp_fractional_scale_manager_v1" => {
                state.globals.fractional = Some(registry.bind(name, 1, handle, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, Key> for State {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        key: &Key,
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
                let Some(panel) = state.panel_mut(*key) else {
                    return;
                };
                if width > 0 {
                    panel.size.0 = width as i32;
                }
                if height > 0 {
                    panel.size.1 = height as i32;
                }
                panel.configured = true;
                panel.dirty = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                // The compositor took it away -- a display going, or a lock
                // screen arriving. Whatever it was, it is not ours any more.
                match key {
                    Key::Popup(id) => close_popup(state, *id),
                    Key::Panel => {
                        if let Some(panel) = state.centre.take() {
                            panel.destroy();
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl State {
    fn panel_mut(&mut self, key: Key) -> Option<&mut Panel> {
        match key {
            Key::Panel => self.centre.as_mut(),
            Key::Popup(id) => self
                .popups
                .iter_mut()
                .find(|popup| popup.note.id == id)
                .map(|popup| &mut popup.panel),
        }
    }
}

impl Dispatch<WpFractionalScaleV1, Key> for State {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        key: &Key,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && scale > 0
            && let Some(panel) = state.panel_mut(*key)
            && panel.scale_120 != scale
        {
            panel.scale_120 = scale;
            panel.dirty = true;
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

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Pointer) {
                seat.get_pointer(handle, ());
            }
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(handle, ());
            }
        }
        let _ = state;
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                let key = state.key_of(&surface);
                state.over = key;
                if let Some(key) = key {
                    state
                        .pointer
                        .insert(key, (surface_x as f32, surface_y as f32));
                }
            }
            wl_pointer::Event::Leave { .. } => {
                state.over = None;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if let Some(key) = state.over {
                    state
                        .pointer
                        .insert(key, (surface_x as f32, surface_y as f32));
                }
            }
            wl_pointer::Event::Button {
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } => {
                if let Some(key) = state.over
                    && let Some(at) = state.pointer.get(&key).copied()
                {
                    state.pressed.push((key, at));
                }
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
        if let wl_keyboard::Event::Key {
            key,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = event
        {
            // Escape, by its evdev code. No keymap is loaded here: the panel
            // reads one key and one key only, and asking xkb about it would be
            // a font of work for a single comparison.
            const ESCAPE: u32 = 1;
            if key == ESCAPE {
                state.escaped = true;
            }
        }
    }
}

impl State {
    fn key_of(&self, surface: &WlSurface) -> Option<Key> {
        if self
            .centre
            .as_ref()
            .is_some_and(|panel| &panel.surface == surface)
        {
            return Some(Key::Panel);
        }
        self.popups
            .iter()
            .find(|popup| &popup.panel.surface == surface)
            .map(|popup| Key::Popup(popup.note.id))
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore ZwlrLayerShellV1);
delegate_noop!(State: ignore WpViewporter);
delegate_noop!(State: ignore WpViewport);
delegate_noop!(State: ignore WpFractionalScaleManagerV1);

impl Dispatch<WlSurface, Key> for State {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: <WlSurface as wayland_client::Proxy>::Event,
        _: &Key,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Tells the sender what was pressed, and why the notification is going away.
fn press(service: &Service, note: &Notification, hit: Option<Hit>) {
    for reply in replies_for(note, hit) {
        service.reply(reply);
    }
}

/// What a press means, as messages back to the sender.
///
/// A button press names the action it carried. A press anywhere else invokes
/// `default` if the sender offered one -- which is how "click the notification
/// to open the thing" has always worked -- and is otherwise just a dismissal.
/// Either way the notification goes: a card that stayed after being pressed
/// would be a card that looks like nothing happened.
///
/// Separated from the sending so it can be checked without a bus: what a press
/// means is a decision, and the sending is plumbing.
fn replies_for(note: &Notification, hit: Option<Hit>) -> Vec<Reply> {
    let mut out = Vec::new();
    match hit {
        Some(Hit::Action(_, index)) => {
            if let Some(action) = note.actions.get(index) {
                out.push(Reply::Invoked(note.id, action.key.clone()));
            }
        }
        _ => {
            if note.has_default {
                out.push(Reply::Invoked(note.id, "default".to_string()));
            }
        }
    }
    out.push(Reply::Closed(note.id, Closed::Dismissed));
    out
}

#[cfg(test)]
mod presses {
    use super::*;
    use crate::notify::model::Action;

    fn note(has_default: bool) -> Notification {
        Notification {
            id: 3,
            has_default,
            actions: vec![
                Action {
                    key: "open".to_string(),
                    label: "Open folder".to_string(),
                },
                Action {
                    key: "later".to_string(),
                    label: "Later".to_string(),
                },
            ],
            ..Notification::default()
        }
    }

    #[test]
    fn a_button_names_the_action_it_carried() {
        assert_eq!(
            replies_for(&note(false), Some(Hit::Action(3, 1))),
            vec![
                Reply::Invoked(3, "later".to_string()),
                Reply::Closed(3, Closed::Dismissed),
            ]
        );
    }

    #[test]
    fn a_press_on_the_card_takes_the_default_action_when_there_is_one() {
        assert_eq!(
            replies_for(&note(true), Some(Hit::Card(3))),
            vec![
                Reply::Invoked(3, "default".to_string()),
                Reply::Closed(3, Closed::Dismissed),
            ]
        );
    }

    #[test]
    fn a_press_on_a_card_with_no_default_is_only_a_dismissal() {
        assert_eq!(
            replies_for(&note(false), Some(Hit::Card(3))),
            vec![Reply::Closed(3, Closed::Dismissed)]
        );
        assert_eq!(
            replies_for(&note(false), None),
            vec![Reply::Closed(3, Closed::Dismissed)]
        );
    }

    #[test]
    fn a_button_that_is_not_there_dismisses_rather_than_inventing_a_key() {
        // The pointer and the drawing read one table, so this should not
        // happen -- and if it ever does, sending a made-up action to somebody
        // else's program is the wrong way to be wrong.
        assert_eq!(
            replies_for(&note(true), Some(Hit::Action(3, 9))),
            vec![Reply::Closed(3, Closed::Dismissed)]
        );
    }
}
