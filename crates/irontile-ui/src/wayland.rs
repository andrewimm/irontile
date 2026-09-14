//! The bar's Wayland client.
//!
//! Waits on two sockets at once: the Wayland connection and the compositor's
//! control socket. Polling either one would mean the bar either lagged behind
//! what it is showing or spun when nothing was happening, so both descriptors
//! go into the same poll along with whatever interval the modules ask for.

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use crate::bar::{self, Frame};
use crate::config::{Config, Position};
use crate::draw::TextRenderer;
use crate::icon::IconSet;
use crate::module::{Button, Click, Snapshot};
use crate::world::{System, spawn};
use irontile_ipc::{Action, Client, OutputId, Query, ResponsePayload};
use rustix::event::{PollFd, PollFlags};
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_output::{self, WlOutput},
    wl_pointer::{self, WlPointer},
    wl_registry,
    wl_seat::WlSeat,
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
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

/// The longest the bar will sit without redrawing, so a clock still ticks when
/// nothing else is happening.
const MAX_WAIT: Duration = Duration::from_secs(1);

/// Marks the tooltip's layer surface apart from the bars', which are numbered
/// by display. There is never more than one tooltip.
const TOOLTIP: usize = usize::MAX;

pub fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let connection = Connection::connect_to_env()
        .map_err(|e| format!("no wayland compositor to connect to: {e}"))?;
    let mut queue: EventQueue<State> = connection.new_event_queue();
    let handle = queue.handle();
    connection.display().get_registry(&handle, ());

    let mut control =
        Client::connect_default().map_err(|e| format!("no irontile control socket: {e}"))?;
    control
        .subscribe()
        .map_err(|e| format!("could not subscribe: {e}"))?;

    let text = TextRenderer::new(&config.font, config.font_size);
    let icons = IconSet::new(&config.icon_theme, config.icon_path.clone());
    let mut state = State {
        config,
        text,
        icons,
        world: System::new(),
        globals: Globals::default(),
        bars: Vec::new(),
        snapshot: Snapshot::default(),
        pointer_at: (0.0, 0.0),
        pointer_on: None,
        alt: std::collections::BTreeSet::new(),
        hover: None,
        tip_stale: false,
        tip: None,
        scratch: Vec::new(),
        dirty: true,
        running: true,
    };

    queue.roundtrip(&mut state)?;
    if state.globals.layer_shell.is_none() {
        return Err("the compositor does not offer wlr-layer-shell".into());
    }
    // Displays arrive during the first roundtrip; a second lets their geometry
    // events land before anything is sized against them.
    queue.roundtrip(&mut state)?;
    state.create_bars(&handle);

    let mut last_draw = Instant::now();
    while state.running {
        if state.dirty {
            state.refresh(&mut control);
            state.redraw(&handle);
            // The modules were rebuilt, so what the pointer is resting on may
            // have moved or stopped saying anything.
            state.hovered();
            last_draw = Instant::now();
        }
        // Put up whatever the pointer has now rested on for long enough, and
        // paint it once the compositor has said how large it may be.
        if state.tip_due().is_some_and(|left| left.is_zero()) {
            state.show_tip(&handle);
        }
        if state.tip_stale {
            state.refresh_tip();
        }
        state.draw_tip(&handle);

        queue.flush()?;
        let read = queue
            .prepare_read()
            .ok_or("wayland queue is already being read")?;
        // Copied out before the guard is consumed, since reading takes it by
        // value and the descriptor is only borrowed from it.
        let wayland_fd = read.connection_fd().try_clone_to_owned()?;
        let control_fd = control.as_fd();
        // A source that changes under the bar rather than because the bar
        // asked -- the volume, so far. Without it a volume key would show up
        // whenever the next tick happened to come round.
        let wake_fd = state.world.wake();

        let mut fds = vec![
            PollFd::new(&wayland_fd, PollFlags::IN),
            PollFd::new(&control_fd, PollFlags::IN),
        ];
        if let Some(wake) = &wake_fd {
            fds.push(PollFd::new(wake, PollFlags::IN));
        }
        // Whichever comes first: the next clock tick, or a tooltip falling due.
        let wait = MAX_WAIT
            .saturating_sub(last_draw.elapsed())
            .min(state.tip_due().unwrap_or(MAX_WAIT));
        let timeout = rustix::time::Timespec {
            tv_sec: wait.as_secs() as i64,
            tv_nsec: wait.subsec_nanos() as i64,
        };
        let _ = rustix::event::poll(&mut fds, Some(&timeout));

        let wayland_ready = fds[0].revents().contains(PollFlags::IN);
        let control_ready = fds[1].revents().contains(PollFlags::IN);
        let woken = fds
            .get(2)
            .is_some_and(|fd| fd.revents().contains(PollFlags::IN));
        if wayland_ready {
            let _ = read.read();
        } else {
            drop(read);
        }
        queue.dispatch_pending(&mut state)?;

        if control_ready {
            // Every event that is waiting, not one per repaint. Anything the
            // compositor reports may change what is shown and working out which
            // did is not worth it, but a burst -- a desktop key held down, a
            // window closing everything it owned -- arrives faster than two
            // bars can be repainted, and reading one at a time leaves a backlog
            // that only grows.
            loop {
                match control.next_event() {
                    Ok(_) => {}
                    // The compositor is gone, and so is the bar. Said out loud,
                    // because a bar that vanishes without a word is the hardest
                    // kind of thing to go looking for afterwards.
                    Err(err) => {
                        eprintln!("irontile-bar: the compositor closed the control socket: {err}");
                        return Ok(());
                    }
                }
                if !readable(control.as_fd()) {
                    break;
                }
            }
            state.dirty = true;
        }
        if woken {
            state.world.drain_wake();
            state.dirty = true;
        }
        if last_draw.elapsed() >= MAX_WAIT {
            state.dirty = true;
        }
    }
    Ok(())
}

/// The three buttons a bar knows what to do with.
///
/// Linux input codes rather than a protocol enum: `wl_pointer` reports the
/// kernel's numbering directly.
fn which(button: u32) -> Option<Button> {
    match button {
        0x110 => Some(Button::Left),
        0x111 => Some(Button::Right),
        0x112 => Some(Button::Middle),
        _ => None,
    }
}

/// A module the pointer is sitting on, waiting to see whether it stays.
#[derive(Clone, Debug)]
struct Hover {
    bar: usize,
    /// Where the module sits in the bar's list of segments. This is what says
    /// two readings are the same module: the text is not, because a module
    /// with a clock in its tooltip says something different every second.
    index: usize,
    text: String,
    /// Where on the bar it started, so the tooltip appears under the module
    /// rather than under wherever the pointer drifted to.
    x: f32,
    since: Instant,
}

/// A tooltip that is up.
struct Tip {
    surface: WlSurface,
    /// Held for as long as the tooltip is up: dropping it unmaps the surface.
    #[allow(dead_code)]
    layer: ZwlrLayerSurfaceV1,
    viewport: Option<WpViewport>,
    pool: Option<Pool>,
    /// The size the compositor has been asked for, so a repaint that changes
    /// shape knows to ask again.
    logical: (i32, i32),
    /// What it says, so a redraw that leaves the pointer on the same module
    /// does not take the tooltip down and put an identical one back up.
    text: String,
    configured: bool,
    /// Kept until the compositor releases the buffer, which is after the
    /// surface is gone.
    #[allow(dead_code)]
    frame: Option<Frame>,
}

/// Whether a descriptor has something waiting, without waiting itself.
fn readable(fd: std::os::fd::BorrowedFd<'_>) -> bool {
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let nothing = rustix::time::Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    rustix::event::poll(&mut fds, Some(&nothing)).is_ok_and(|ready| ready > 0)
        && fds[0].revents().contains(PollFlags::IN)
}

#[derive(Default)]
struct Globals {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    /// The pair that makes a fraction expressible: one says what the scale
    /// actually is, the other says how large the result should end up. Without
    /// the second there is no way to say that a 1.3333x buffer is one bar tall.
    fractional_scale: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    outputs: Vec<(WlOutput, Option<OutputId>, String)>,
}

/// One bar, on one display.
struct Bar {
    surface: WlSurface,
    /// Held for as long as the bar is up: dropping it unmaps the surface.
    #[allow(dead_code)]
    layer: ZwlrLayerSurfaceV1,
    output: usize,
    width: u32,
    scale: f32,
    configured: bool,
    frame: Option<Frame>,
    /// Says how large the buffer should appear, whatever size it is in pixels.
    /// Absent only if the compositor offers no viewporter, and then the scale
    /// has to be a whole number.
    viewport: Option<WpViewport>,
    /// Held for as long as the surface: dropping it stops the scale reports.
    #[allow(dead_code)]
    fractional: Option<WpFractionalScaleV1>,
    /// The memory this bar draws into, kept rather than made per frame.
    pool: Option<Pool>,
    /// Set when a redraw found every slot still in the compositor's hands, so
    /// that the release which frees one asks for the redraw again.
    pending: bool,
}

/// How many slots a bar cycles through.
///
/// Two is enough: one on screen and one being drawn. A third would only help if
/// the compositor were more than a frame behind, and then the bar has bigger
/// problems than which buffer it is writing into.
const SLOTS: usize = 2;

/// The shared memory one bar draws into.
///
/// A buffer handed to the compositor belongs to the compositor until it says
/// otherwise, so it cannot be overwritten and it cannot be thrown away. Making
/// a new one per frame means the compositor holds every frame the bar has ever
/// drawn -- a few hundred kilobytes each, at whatever rate the bar redraws --
/// until something runs out. Two slots reused forever is the whole fix.
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
    busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
            rustix::fs::memfd_create(c"irontile-bar", rustix::fs::MemfdFlags::CLOEXEC).ok()?;
        rustix::fs::ftruncate(&file, total as u64).ok()?;
        let file = std::fs::File::from(file);
        let pool = shm.create_pool(file.as_fd(), total, handle, ());

        let slots = std::array::from_fn(|i| {
            let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let offset = slot * i as i64;
            let buffer = pool.create_buffer(
                offset as i32,
                w,
                h,
                stride,
                wl_shm::Format::Argb8888,
                handle,
                std::sync::Arc::clone(&busy),
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
            .find(|slot| !slot.busy.load(std::sync::atomic::Ordering::Acquire))
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

struct State {
    config: Config,
    text: TextRenderer,
    icons: IconSet,
    /// Kept rather than made per draw, so that remembered command output
    /// survives from one redraw to the next.
    world: System,
    globals: Globals,
    bars: Vec<Bar>,
    snapshot: Snapshot,
    pointer_at: (f64, f64),
    /// Which bar the pointer is over, if any.
    pointer_on: Option<usize>,
    /// Modules showing their second format, by name. A set rather than a flag
    /// per module because most modules have no second format at all.
    alt: std::collections::BTreeSet<String>,
    /// What the pointer is resting on, and since when.
    hover: Option<Hover>,
    /// Set when the module under a tooltip has changed what it says.
    tip_stale: bool,
    /// The tooltip currently on screen, if any.
    tip: Option<Tip>,
    /// Reused between frames: at a few hundred kilobytes a bar, allocating this
    /// per redraw is the largest thing the bar would do per frame.
    scratch: Vec<u8>,
    dirty: bool,
    running: bool,
}

impl State {
    fn create_bars(&mut self, handle: &QueueHandle<State>) {
        let (Some(compositor), Some(shell)) = (
            self.globals.compositor.clone(),
            self.globals.layer_shell.clone(),
        ) else {
            return;
        };
        for index in 0..self.globals.outputs.len() {
            if self.bars.iter().any(|bar| bar.output == index) {
                continue;
            }
            let output = self.globals.outputs[index].0.clone();
            let surface = compositor.create_surface(handle, ());
            let layer = shell.get_layer_surface(
                &surface,
                Some(&output),
                zwlr_layer_shell_v1::Layer::Top,
                "irontile-bar".to_owned(),
                handle,
                index,
            );
            layer.set_size(0, self.config.height as u32);
            let edge = match self.config.position {
                Position::Top => zwlr_layer_surface_v1::Anchor::Top,
                Position::Bottom => zwlr_layer_surface_v1::Anchor::Bottom,
            };
            layer.set_anchor(
                edge | zwlr_layer_surface_v1::Anchor::Left | zwlr_layer_surface_v1::Anchor::Right,
            );
            // Reserving its own height is what keeps windows from tiling
            // underneath it.
            layer.set_exclusive_zone(self.config.height);
            // A bar is not typed into; taking the keyboard would stop the
            // window underneath receiving anything.
            layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);

            let index_of_bar = self.bars.len();
            let fractional = self
                .globals
                .fractional_scale
                .as_ref()
                .map(|manager| manager.get_fractional_scale(&surface, handle, index_of_bar));
            let viewport = self
                .globals
                .viewporter
                .as_ref()
                .map(|viewporter| viewporter.get_viewport(&surface, handle, ()));
            surface.commit();

            self.bars.push(Bar {
                surface,
                layer,
                output: index,
                width: 0,
                scale: 1.0,
                configured: false,
                frame: None,
                viewport,
                fractional,
                pool: None,
                pending: false,
            });
        }
    }

    /// Asks the compositor what to show.
    fn refresh(&mut self, control: &mut Client) {
        self.dirty = false;
        if let Ok(ResponsePayload::Workspaces(workspaces)) = control.query(Query::Workspaces) {
            self.snapshot.workspaces = workspaces;
        }
        if let Ok(ResponsePayload::Windows(windows)) = control.query(Query::Windows) {
            self.snapshot.windows = windows;
        }
    }

    fn redraw(&mut self, handle: &QueueHandle<State>) {
        for index in 0..self.bars.len() {
            if !self.bars[index].configured || self.bars[index].width == 0 {
                continue;
            }
            let (width, scale) = (self.bars[index].width, self.bars[index].scale);
            // A bar on this display shows this display's desktops.
            self.snapshot.output = self.globals.outputs[self.bars[index].output].1;

            // Drawn at the display's real scale and then told how large that
            // is meant to look. Drawing at logical size and letting the
            // compositor magnify the result is what makes a bar look soft, and
            // drawing at a scale the compositor is not told about is what makes
            // one come out the wrong size.
            let pixels = (width as f32 * scale).round().max(1.0) as u32;
            let alt = &self.alt;
            let frame = bar::draw(
                &self.config,
                &mut self.text,
                &mut self.icons,
                &self.snapshot,
                &self.world,
                &bar::Target {
                    width: pixels,
                    scale,
                    alt: &|name: &str| alt.contains(name),
                },
            );
            let shm = self.globals.shm.clone();
            let logical = (width as i32, self.config.height);
            attach(
                shm.as_ref(),
                handle,
                &mut self.bars[index],
                &frame,
                logical,
                &mut self.scratch,
            );
            self.bars[index].frame = Some(frame);
        }
    }

    /// Notes what the pointer is resting on.
    ///
    /// Restarting the clock only when the module under it changes is what lets
    /// a tooltip survive the pointer moving a few pixels within one module,
    /// and stops one appearing while the pointer is merely crossing the bar.
    fn hovered(&mut self) {
        let found = self.pointer_on.and_then(|index| {
            let bar = self.bars.get(index)?;
            let frame = bar.frame.as_ref()?;
            let x = self.pointer_at.0 as f32 * bar.scale;
            let (at, hit) = bar::at(&frame.hits, x)?;
            let text = hit.tooltip.clone()?;
            Some(Hover {
                bar: index,
                index: at,
                text,
                x: hit.x + hit.width / 2.0,
                since: Instant::now(),
            })
        });

        let same = match (&self.hover, &found) {
            (Some(a), Some(b)) => (a.bar, a.index) == (b.bar, b.index),
            _ => false,
        };
        match (&mut self.hover, found) {
            (Some(current), Some(next)) if same => {
                // The same module. Leave the clock running and follow it if the
                // bar redrew and moved it; if what it says has changed, the
                // tooltip wants repainting rather than replacing.
                current.x = next.x;
                if current.text != next.text {
                    current.text = next.text;
                    self.tip_stale = true;
                }
            }
            (_, next) => {
                self.hover = next;
                // A different module, or none: whatever is up is about the
                // wrong thing.
                self.hide_tip();
            }
        }
    }

    /// Repaints the tooltip with what its module now says.
    fn refresh_tip(&mut self) {
        self.tip_stale = false;
        let (Some(hover), Some(tip)) = (&self.hover, &mut self.tip) else {
            return;
        };
        let scale = self.bars.get(hover.bar).map_or(1.0, |bar| bar.scale);
        let Some(frame) = bar::tooltip(&self.config, &mut self.text, &hover.text, scale) else {
            return;
        };
        let logical = (
            (frame.pixmap.width() as f32 / scale).round().max(1.0) as i32,
            (frame.pixmap.height() as f32 / scale).round().max(1.0) as i32,
        );
        if logical != tip.logical {
            // A different shape needs a different reservation, and the
            // compositor answers with a configure the paint waits for.
            tip.logical = logical;
            tip.layer
                .set_size(logical.0.max(1) as u32, logical.1.max(1) as u32);
            tip.surface.commit();
        }
        tip.text = hover.text.clone();
        tip.frame = Some(frame);
    }

    /// How long until a waiting tooltip is due, if one is.
    fn tip_due(&self) -> Option<Duration> {
        let hover = self.hover.as_ref()?;
        if self.tip.is_some() {
            return None;
        }
        let delay = Duration::from_millis(self.config.tooltip.delay_ms);
        Some(delay.saturating_sub(hover.since.elapsed()))
    }

    /// Takes down the tooltip, if one is up.
    fn hide_tip(&mut self) {
        if let Some(tip) = self.tip.take() {
            // The pool goes first: its buffers are cut from it and the surface
            // they were attached to is about to be gone.
            drop(tip.pool);
            tip.layer.destroy();
            tip.surface.destroy();
        }
    }

    /// Puts up the tooltip the pointer has been resting on.
    fn show_tip(&mut self, handle: &QueueHandle<State>) {
        let Some(hover) = self.hover.clone() else {
            return;
        };
        let (Some(compositor), Some(shell)) = (
            self.globals.compositor.clone(),
            self.globals.layer_shell.clone(),
        ) else {
            return;
        };
        let Some(bar) = self.bars.get(hover.bar) else {
            return;
        };
        let output = self.globals.outputs[bar.output].0.clone();
        let scale = bar.scale;
        let width = bar.width;

        // Drawn first, because where it goes depends on how large it is.
        let Some(frame) = bar::tooltip(&self.config, &mut self.text, &hover.text, scale) else {
            return;
        };
        let logical = (
            (frame.pixmap.width() as f32 / scale).round() as i32,
            (frame.pixmap.height() as f32 / scale).round() as i32,
        );

        // Centred under the module, and pushed back inside the display rather
        // than hanging off the edge -- which is where the rightmost module's
        // tooltip would always be.
        let centre = hover.x / scale;
        let left = (centre - logical.0 as f32 / 2.0)
            .round()
            .clamp(0.0, (width as i32 - logical.0).max(0) as f32) as i32;

        let surface = compositor.create_surface(handle, ());
        let layer = shell.get_layer_surface(
            &surface,
            Some(&output),
            zwlr_layer_shell_v1::Layer::Overlay,
            "irontile-tooltip".to_owned(),
            handle,
            TOOLTIP,
        );
        let edge = match self.config.position {
            Position::Top => zwlr_layer_surface_v1::Anchor::Top,
            Position::Bottom => zwlr_layer_surface_v1::Anchor::Bottom,
        };
        layer.set_anchor(edge | zwlr_layer_surface_v1::Anchor::Left);
        layer.set_size(logical.0.max(1) as u32, logical.1.max(1) as u32);
        let gap = self.config.height + self.config.tooltip.gap;
        match self.config.position {
            Position::Top => layer.set_margin(gap, 0, 0, left),
            Position::Bottom => layer.set_margin(0, 0, gap, left),
        }
        // Reserving nothing and ignoring what others reserve: a tooltip is
        // measured from the screen edge, and the bar's own zone is already in
        // the margin above.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
        // No input at all. A tooltip that took the pointer would take it off
        // the module it belongs to, which would hide the tooltip, which would
        // give the pointer back: a flicker rather than a tooltip.
        let empty = compositor.create_region(handle, ());
        surface.set_input_region(Some(&empty));
        empty.destroy();

        let viewport = self
            .globals
            .viewporter
            .as_ref()
            .map(|viewporter| viewporter.get_viewport(&surface, handle, ()));
        surface.commit();

        self.tip = Some(Tip {
            surface,
            layer,
            viewport,
            pool: None,
            logical,
            text: hover.text.clone(),
            configured: false,
            frame: Some(frame),
        });
    }

    /// Paints the tooltip once the compositor has sized it.
    fn draw_tip(&mut self, handle: &QueueHandle<State>) {
        let Some(tip) = &mut self.tip else {
            return;
        };
        if !tip.configured {
            return;
        }
        let Some(frame) = &tip.frame else {
            return;
        };
        let scale = self
            .bars
            .get(self.hover.as_ref().map_or(usize::MAX, |h| h.bar))
            .map_or(1.0, |bar| bar.scale);
        let logical = (
            (frame.pixmap.width() as f32 / scale).round() as i32,
            (frame.pixmap.height() as f32 / scale).round() as i32,
        );

        let shm = self.globals.shm.clone();
        let Some(shm) = shm else {
            return;
        };
        let size = (frame.pixmap.width() as i32, frame.pixmap.height() as i32);
        if tip.pool.as_ref().is_none_or(|pool| pool.size != size) {
            tip.pool = None;
            tip.pool = Pool::new(&shm, handle, size);
        }
        let Some(pool) = &tip.pool else {
            return;
        };
        let Some(slot) = pool.free() else {
            return;
        };

        use std::os::unix::fs::FileExt;
        self.scratch.clear();
        for pixel in frame.pixmap.pixels() {
            self.scratch.extend_from_slice(&[
                pixel.blue(),
                pixel.green(),
                pixel.red(),
                pixel.alpha(),
            ]);
        }
        if pool.file.write_all_at(&self.scratch, slot.offset).is_err() {
            return;
        }
        slot.busy.store(true, std::sync::atomic::Ordering::Release);
        if let Some(viewport) = &tip.viewport {
            viewport.set_destination(logical.0.max(1), logical.1.max(1));
        }
        tip.surface.attach(Some(&slot.buffer), 0, 0);
        tip.surface.damage_buffer(0, 0, size.0, size.1);
        tip.surface.commit();
        // Only once it is actually on screen. Dropping it before the steps
        // that can fail would leave a tooltip that could never be painted,
        // since there would be nothing left to paint.
        tip.frame = None;
    }

    /// Acts on a click at the pointer's last position.
    fn click(&mut self, button: Button) {
        let Some(bar) = self.pointer_on.and_then(|index| self.bars.get(index)) else {
            return;
        };
        let Some(frame) = &bar.frame else {
            return;
        };
        let x = self.pointer_at.0 as f32 * bar.scale;
        match bar::hit(&frame.hits, x, button).cloned() {
            Some(Click::Workspace(n)) => {
                if let Ok(mut client) = Client::connect_default() {
                    let _ = client.action(Action::Workspace(n));
                }
            }
            Some(Click::Run(command)) => spawn(&command),
            Some(Click::Toggle(module)) => {
                // Whatever it said before is no longer what it says.
                self.hide_tip();
                if !self.alt.remove(&module) {
                    self.alt.insert(module);
                }
                // The module says something different now, so redraw at once
                // rather than at the next tick: a click that takes a second to
                // show anything reads as a click that missed.
                self.dirty = true;
            }
            None => {}
        }
    }
}

/// Copies a rendered bar into a shared-memory slot and shows it.
fn attach(
    shm: Option<&WlShm>,
    handle: &QueueHandle<State>,
    bar: &mut Bar,
    frame: &Frame,
    logical: (i32, i32),
    scratch: &mut Vec<u8>,
) {
    use std::os::unix::fs::FileExt;
    use std::sync::atomic::Ordering;

    let Some(shm) = shm else {
        return;
    };
    let size = (frame.pixmap.width() as i32, frame.pixmap.height() as i32);
    if bar.pool.as_ref().is_none_or(|pool| pool.size != size) {
        // Dropped first, so its buffers are gone before the replacements are
        // cut; the compositor is told about the old ones either way.
        bar.pool = None;
        bar.pool = Pool::new(shm, handle, size);
    }
    let Some(pool) = &bar.pool else {
        return;
    };
    let Some(slot) = pool.free() else {
        // Both slots are still the compositor's. Ask again when one comes back
        // rather than dropping this frame on the floor.
        bar.pending = true;
        return;
    };

    // tiny-skia stores premultiplied RGBA; wayland's Argb8888 is little-endian
    // BGRA, so the two outer channels swap.
    scratch.clear();
    scratch.reserve(frame.pixmap.pixels().len() * 4);
    for pixel in frame.pixmap.pixels() {
        scratch.extend_from_slice(&[pixel.blue(), pixel.green(), pixel.red(), pixel.alpha()]);
    }
    if pool.file.write_all_at(scratch, slot.offset).is_err() {
        return;
    }

    slot.busy.store(true, Ordering::Release);
    bar.pending = false;
    // What the buffer is in pixels and what it means in logical space are two
    // different numbers whenever the scale is not one, and only the viewport
    // can hold a fraction between them.
    if let Some(viewport) = &bar.viewport {
        viewport.set_destination(logical.0.max(1), logical.1.max(1));
    }
    bar.surface.attach(Some(&slot.buffer), 0, 0);
    bar.surface.damage_buffer(0, 0, size.0, size.1);
    bar.surface.commit();
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
                state.globals.compositor = Some(registry.bind(name, version.min(6), handle, ()));
            }
            "wl_shm" => state.globals.shm = Some(registry.bind(name, version.min(1), handle, ())),
            "zwlr_layer_shell_v1" => {
                state.globals.layer_shell = Some(registry.bind(name, version.min(4), handle, ()));
            }
            "wp_fractional_scale_manager_v1" => {
                state.globals.fractional_scale =
                    Some(registry.bind(name, version.min(1), handle, ()));
            }
            "wp_viewporter" => {
                state.globals.viewporter = Some(registry.bind(name, version.min(1), handle, ()));
            }
            "wl_seat" => {
                let seat: WlSeat = registry.bind(name, version.min(7), handle, ());
                seat.get_pointer(handle, ());
            }
            "wl_output" => {
                let index = state.globals.outputs.len();
                let output: WlOutput = registry.bind(name, version.min(4), handle, index);
                state.globals.outputs.push((output, None, String::new()));
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(entry) = state.globals.outputs.get_mut(*index)
        {
            entry.2 = name;
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for State {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, .. } => {
                layer.ack_configure(serial);
                if *index == TOOLTIP {
                    if let Some(tip) = &mut state.tip {
                        tip.configured = true;
                    }
                    return;
                }
                if let Some(bar) = state.bars.iter_mut().find(|b| b.output == *index) {
                    bar.width = width;
                    bar.configured = true;
                }
                state.dirty = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                if *index == TOOLTIP {
                    state.hide_tip();
                    return;
                }
                state.bars.retain(|b| b.output != *index);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn event(
        state: &mut Self,
        surface: &WlSurface,
        event: wayland_client::protocol::wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Only heeded when there is no fractional scale to be had: this one is
        // a whole number, and rounding 1.3333 to 1 draws a soft bar and
        // rounding it to 2 draws an enormous one.
        if let wayland_client::protocol::wl_surface::Event::PreferredBufferScale { factor } = event
            && let Some(bar) = state.bars.iter_mut().find(|b| &b.surface == surface)
            && bar.fractional.is_none()
        {
            bar.scale = factor as f32;
            // Without a viewport this is the only way to say how large the
            // buffer should appear, and it is why the scale must be whole.
            bar.surface.set_buffer_scale(factor);
            state.dirty = true;
        }
    }
}

/// The exact scale of the display a bar is on, in 120ths.
///
/// This is the number the compositor actually lays things out at. Drawing at
/// the whole number next to it and letting the compositor resample is what
/// makes a bar full of text look soft on a laptop panel.
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
            && let Some(bar) = state.bars.get_mut(*index)
        {
            let scale = scale as f32 / 120.0;
            if bar.scale != scale && scale > 0.0 {
                bar.scale = scale;
                state.dirty = true;
            }
        }
    }
}

/// A released buffer is one the bar may write into again.
impl Dispatch<WlBuffer, std::sync::Arc<std::sync::atomic::AtomicBool>> for State {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wayland_client::protocol::wl_buffer::Event,
        busy: &std::sync::Arc<std::sync::atomic::AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_buffer::Event::Release = event {
            busy.store(false, std::sync::atomic::Ordering::Release);
            // Only when a frame was actually held back: every redraw releases
            // the one before it, and redrawing on that would never stop.
            if state.bars.iter().any(|bar| bar.pending) {
                state.dirty = true;
            }
        }
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
            // Which bar, not just where on it. With a display each, the two
            // bars show the same desktops in different places, so acting on the
            // wrong one's hit map switches to a desktop nobody clicked.
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_on = state.bars.iter().position(|bar| bar.surface == surface);
                state.pointer_at = (surface_x, surface_y);
                state.hovered();
            }
            wl_pointer::Event::Leave { surface, .. } => {
                if state.pointer_on.is_some_and(|index| {
                    state.bars.get(index).is_some_and(|b| b.surface == surface)
                }) {
                    state.pointer_on = None;
                    // Off the bar entirely: nothing is being rested on.
                    state.hover = None;
                    state.hide_tip();
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_at = (surface_x, surface_y);
                // Where the clock on a tooltip starts and stops. Leaving this
                // to the next redraw would mean a tooltip arriving up to a
                // second late, or not at all if the pointer moved on.
                state.hovered();
            }
            wl_pointer::Event::Button {
                button,
                state: pressed,
                ..
            } => {
                if pressed == wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed)
                    && let Some(button) = which(button)
                {
                    state.click(button);
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore ZwlrLayerShellV1);
delegate_noop!(State: ignore WpFractionalScaleManagerV1);
delegate_noop!(State: ignore WpViewporter);
delegate_noop!(State: ignore WpViewport);
delegate_noop!(State: ignore wayland_client::protocol::wl_region::WlRegion);
delegate_noop!(State: ignore WlSeat);
