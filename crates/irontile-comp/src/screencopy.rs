//! Copying a display's pixels to a client, as wlr-screencopy.
//!
//! This is how a screenshot is taken: a client asks for a display, is told what
//! shape of buffer to provide, hands one over, and is told when it has been
//! filled. Everything that captures the screen is built on it -- `grim`, and
//! the screen-sharing portal after it.
//!
//! The copy is made by rendering the display again into an offscreen buffer
//! rather than reading back whatever the hardware last scanned out. That costs
//! a frame's worth of work for something that happens when somebody presses a
//! key, and in exchange it is the same code path on every backend and needs
//! nothing from the display hardware at all.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Physical, Rectangle};

use crate::state::Irontile;
use irontile_layout::OutputId;

/// The version of the protocol this speaks.
///
/// Two adds `copy_with_damage`, for a client following a display rather than
/// taking one picture. Three adds `buffer_done`, which ends a list of buffer
/// shapes the client may choose from, and an optional `linux_dmabuf` entry in
/// that list -- optional because a compositor offers it only if it can fill
/// one, and this fills shared memory. A screen recorder asks for three and
/// nothing less, so two is not "everything a screenshot needs" as long as
/// recording counts.
const VERSION: u32 = 3;

/// What a frame object is waiting for.
#[derive(Debug)]
pub struct Pending {
    pub frame: ZwlrScreencopyFrameV1,
    /// The buffer the client handed over, once it has.
    pub buffer: Option<smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer>,
    pub output: OutputId,
    /// The part of the display asked for, in its own pixels.
    pub region: Rectangle<i32, Physical>,
    pub overlay_cursor: bool,
    /// Set when the client asked with `copy_with_damage`, which is a client
    /// watching a display: it is owed a `damage` event saying what moved before
    /// it is told the copy is ready.
    pub wants_damage: bool,
}

/// How long the indicator stays up after the last frame was handed over.
///
/// It is also the shortest time it is ever shown, which is the point of it: a
/// screenshot is one frame and would otherwise light the indicator for a
/// sixtieth of a second, which is the same as not showing it at all. A
/// recorder keeps handing frames over and holds it lit.
const INDICATE_FOR: Duration = Duration::from_millis(1500);

/// The frames that have been asked for and not yet filled.
#[derive(Debug, Default)]
pub struct Screencopy {
    pending: Vec<Pending>,
    /// When a copy was last actually handed to a client. Not when one was
    /// asked for: a request that fails copies nothing and is nothing to
    /// report.
    last_served: Option<Instant>,
}

impl Screencopy {
    pub fn new(display: &DisplayHandle) -> Screencopy {
        display.create_global::<Irontile, ZwlrScreencopyManagerV1, _>(VERSION, ());
        Screencopy::default()
    }

    /// Whether a client has been given a picture of the screen just now.
    ///
    /// The compositor cannot stop a client that can reach the socket from
    /// copying the screen, and on a desktop where nothing is sandboxed it
    /// would be pretending to try. What it can do is refuse to let it happen
    /// quietly.
    pub fn capturing(&self) -> bool {
        self.last_served
            .is_some_and(|at| at.elapsed() < INDICATE_FOR)
    }

    /// Takes everything waiting on a display, so it can be filled.
    pub fn take_for(&mut self, output: OutputId) -> Vec<Pending> {
        let mut taken = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].output == output {
                taken.push(self.pending.remove(i));
            } else {
                i += 1;
            }
        }
        taken
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// What a frame remembers between being created and being copied into.
#[derive(Debug, Default)]
pub struct FrameState {
    /// Set once `copy` has been answered, so a second one is refused rather
    /// than filling a buffer the client may already have taken back.
    used: Mutex<bool>,
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for Irontile {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _data: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for Irontile {
    fn request(
        state: &mut Self,
        _client: &Client,
        _manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let (frame, output, region, overlay_cursor) = match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => (frame, output, None, overlay_cursor != 0),
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => (
                frame,
                output,
                Some((x, y, width, height)),
                overlay_cursor != 0,
            ),
            zwlr_screencopy_manager_v1::Request::Destroy => return,
            _ => return,
        };

        let frame = init.init(frame, FrameState::default());
        let Some(id) = state.output_id_of(&output) else {
            // A display that is not here cannot be copied, and saying so is
            // better than leaving the client waiting for a frame that is never
            // coming.
            frame.failed();
            return;
        };

        let size = state.output_pixels(id);
        let whole = Rectangle::from_size(size);
        let region = match region {
            Some((x, y, w, h)) => {
                // Asked for in logical coordinates, taken in the display's own.
                let scale = state.output_scale(id);
                let at = |value: i32| (f64::from(value) * scale).round() as i32;
                Rectangle::new((at(x), at(y)).into(), (at(w), at(h)).into())
            }
            None => whole,
        };
        let Some(region) = region.intersection(whole) else {
            frame.failed();
            return;
        };
        if region.size.w <= 0 || region.size.h <= 0 {
            frame.failed();
            return;
        }

        // What shape of buffer to bring. Shared memory only: a screenshot is
        // taken once and read on the processor, and offering a GPU buffer as
        // well would mean supporting both for no gain.
        frame.buffer(
            wl_shm::Format::Xrgb8888,
            region.size.w as u32,
            region.size.h as u32,
            (region.size.w * 4) as u32,
        );
        if frame.version() >= 3 {
            frame.buffer_done();
        }

        state.screencopy.pending.push(Pending {
            frame,
            output: id,
            region,
            overlay_cursor,
            buffer: None,
            wants_damage: false,
        });
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, FrameState> for Irontile {
    fn request(
        state: &mut Self,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameState,
        _handle: &DisplayHandle,
        _init: &mut DataInit<'_, Self>,
    ) {
        // Read before the match, which takes the request apart.
        let wants_damage = matches!(
            request,
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { .. }
        );
        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer }
            | zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                let already = {
                    let mut used = data.used.lock().unwrap();
                    std::mem::replace(&mut *used, true)
                };
                if already {
                    frame.post_error(
                        zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                        "this frame has already been copied",
                    );
                    return;
                }
                if let Some(waiting) = state
                    .screencopy
                    .pending
                    .iter_mut()
                    .find(|pending| &pending.frame == frame)
                {
                    waiting.buffer = Some(buffer);
                    waiting.wants_damage = wants_damage;
                    // Filled on the next pass over this display.
                    state.dirty = true;
                } else {
                    frame.failed();
                }
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {
                state
                    .screencopy
                    .pending
                    .retain(|pending| &pending.frame != frame);
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        frame: &ZwlrScreencopyFrameV1,
        _data: &FrameState,
    ) {
        state
            .screencopy
            .pending
            .retain(|pending| &pending.frame != frame);
    }
}

/// The display a copy is being made of.
#[derive(Clone, Copy, Debug)]
pub struct Display {
    pub id: OutputId,
    /// Its size in its own pixels.
    pub size: smithay::utils::Size<i32, Physical>,
    pub scale: f64,
    /// How the backend orients what it draws. A copy has to be made the same
    /// way round as what is on screen, or the screenshot comes out upside down.
    pub transform: smithay::utils::Transform,
    /// What an empty part of it looks like.
    pub clear: [f32; 4],
}

/// Fills every frame waiting on a display.
///
/// Rendered fresh rather than read back from the hardware: the same code on
/// every backend, and nothing asked of the display controller.
pub fn serve<R>(
    screencopy: &mut Screencopy,
    renderer: &mut R,
    elements: &[crate::render::IrontileElement<R>],
    display: Display,
    now: std::time::Duration,
) where
    R: smithay::backend::renderer::Renderer
        + smithay::backend::renderer::ImportAll
        + smithay::backend::renderer::ImportMem
        + smithay::backend::renderer::ExportMem
        + smithay::backend::renderer::Offscreen<smithay::backend::renderer::gles::GlesRenderbuffer>,
    R::TextureId: Send + Clone + 'static,
    R::Error: Send + Sync + 'static,
{
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::damage::OutputDamageTracker;

    let Display {
        id,
        size,
        scale,
        transform,
        clear,
    } = display;
    let waiting = screencopy.take_for(id);
    if waiting.is_empty() {
        return;
    }

    for pending in waiting {
        let Some(buffer) = &pending.buffer else {
            // Asked for but never handed a buffer. It stays on the client's
            // side of the bargain; put it back for when it does.
            screencopy.pending.push(pending);
            continue;
        };

        let filled = (|| -> Option<()> {
            let mut target: smithay::backend::renderer::gles::GlesRenderbuffer = renderer
                .create_buffer(Fourcc::Argb8888, (size.w, size.h).into())
                .ok()?;
            let mut framebuffer = renderer.bind(&mut target).ok()?;
            let mut damage = OutputDamageTracker::new(size, scale, transform);
            damage
                .render_output(renderer, &mut framebuffer, 0, elements, clear)
                .ok()?;

            let mapping = renderer
                .copy_framebuffer(
                    &framebuffer,
                    Rectangle::new(
                        (pending.region.loc.x, pending.region.loc.y).into(),
                        (pending.region.size.w, pending.region.size.h).into(),
                    ),
                    Fourcc::Xrgb8888,
                )
                .ok()?;
            let pixels = renderer.map_texture(&mapping).ok()?;
            write_into(buffer, pixels, pending.region.size.w, pending.region.size.h)?;
            Some(())
        })();

        match filled {
            Some(()) => {
                // A picture of the screen has just left the compositor. This
                // is the one place that is true, so it is the one place that
                // records it.
                screencopy.last_served = Some(Instant::now());
                // Nothing about the copy is upside down or shuffled, which is
                // what an empty flags means.
                pending
                    .frame
                    .flags(zwlr_screencopy_frame_v1::Flags::empty());
                // The whole region, every time. Working out what actually
                // changed would let a recorder skip work, but claiming less
                // than moved would have it draw a stale frame -- and the copy
                // itself is already a full one.
                if pending.wants_damage {
                    pending.frame.damage(
                        0,
                        0,
                        pending.region.size.w as u32,
                        pending.region.size.h as u32,
                    );
                }
                let secs = now.as_secs();
                pending.frame.ready(
                    (secs >> 32) as u32,
                    (secs & 0xffff_ffff) as u32,
                    now.subsec_nanos(),
                );
            }
            None => pending.frame.failed(),
        }
    }
}

/// Copies pixels into a client's shared-memory buffer.
fn write_into(
    buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    pixels: &[u8],
    width: i32,
    height: i32,
) -> Option<()> {
    use smithay::wayland::shm;

    let wanted = (width as usize) * (height as usize) * 4;
    if pixels.len() < wanted {
        return None;
    }
    shm::with_buffer_contents_mut(buffer, |slice, len, data| {
        if data.width != width || data.height != height || len < wanted {
            return None;
        }
        // The client's buffer may have a longer stride than the copy does.
        let stride = data.stride as usize;
        let row_bytes = (width as usize) * 4;
        if data.offset as usize + (height as usize - 1) * stride + row_bytes > len {
            return None;
        }
        // SAFETY: the client's buffer is mapped for the life of this closure
        // and `len` is its length, which the bounds check above stays inside.
        // Taken as a slice once rather than written through the pointer, so
        // every copy below is an ordinary checked one.
        #[allow(unsafe_code)]
        let target = unsafe { std::slice::from_raw_parts_mut(slice, len) };
        for row in 0..height as usize {
            let from = row * row_bytes;
            let to = data.offset as usize + row * stride;
            target[to..to + row_bytes].copy_from_slice(&pixels[from..from + row_bytes]);
        }
        Some(())
    })
    .ok()
    .flatten()
}
