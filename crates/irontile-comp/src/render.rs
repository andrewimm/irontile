//! Turning a layout frame into render elements.
//!
//! The only decoration is a border drawn as four strips around each window.
//! Nothing else is drawn, and nothing here decides where a window goes; it just
//! reads the frame.
//!
//! Everything is emitted in coordinates relative to the display being drawn.
//! A nested window and a DRM connector both render their own display starting
//! at the origin, so the global coordinate space the layout engine works in is
//! translated away here rather than in either backend.

use irontile_layout::{Frame, Layout, OutputId, PlacementKind, Point, Rect};
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{AsRenderElements, Kind};
use smithay::backend::renderer::{ImportAll, ImportMem, Renderer};
use smithay::desktop::layer_map_for_output;
use smithay::render_elements;
use smithay::utils::{Physical, Scale, Transform};
use smithay::wayland::shell::wlr_layer::Layer;

use crate::registry::Registry;
use crate::state::OutputEntry;
use crate::theme::{Paint, Theme};

render_elements! {
    pub IrontileElement<R> where R: ImportAll + ImportMem;
    Surface = WaylandSurfaceRenderElement<R>,
    Border = SolidColorRenderElement,
    /// Any image the compositor rasterised itself: the pointer, and the mark
    /// that says the screen is being copied. One variant rather than two
    /// because the element type is what the list holds, and the macro derives
    /// a conversion per type.
    Image = MemoryRenderBufferRenderElement<R>,
}

/// Everything drawing one display needs.
///
/// A borrow of each piece rather than of the whole compositor, because the
/// renderer now lives in the compositor too and the two have to be borrowed at
/// once.
pub struct Scene<'a> {
    pub frame: &'a Frame,
    pub windows: &'a Registry,
    pub outputs: &'a [OutputEntry],
    pub layout: &'a Layout,
    pub theme: &'a Theme,
    /// What to draw for the pointer, and where it is in global coordinates.
    /// `None` when something else draws the pointer, which is the case whenever
    /// irontile is nested.
    pub cursor: Option<Cursor<'a>>,
    /// Set while the session is locked. Nothing behind it is drawn.
    pub lock: Option<&'a crate::lock::Lock>,
    /// The capture indicator, when something is copying the screen. Drawn over
    /// everything including the lock screen, and included in the copy itself.
    pub capture: Option<&'a MemoryRenderBuffer>,
}

/// The pointer, ready to draw.
pub enum Cursor<'a> {
    /// A themed or built-in image. Hotspot and size are both logical.
    Image {
        buffer: &'a MemoryRenderBuffer,
        hotspot: (i32, i32),
        /// Logical size to draw at.
        size: (i32, i32),
        /// The image's own size in pixels, which is the region to sample from.
        source: (i32, i32),
        at: Point,
    },
    /// A surface the client is drawing itself.
    Surface {
        surface: &'a smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        hotspot: (i32, i32),
        at: Point,
    },
}

/// Builds the element list for one display, topmost element first.
///
/// `draw_render_elements` paints in list order with earlier elements on top, so
/// the list is built from the highest placement down, and within each placement
/// the window's surfaces come before the border strips around them.
pub fn elements<R>(
    scene: &Scene<'_>,
    renderer: &mut R,
    output: OutputId,
    scale: f64,
) -> Vec<IrontileElement<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let scale = Scale::from(scale);
    let origin = scene
        .layout
        .output(output)
        .map(|o| o.logical.origin())
        .unwrap_or_default();

    let mut out = Vec::new();

    // Above everything except the pointer, and above it in every sense: over a
    // fullscreen window, over the lock screen, over a bar. A client copying the
    // screen cannot cover the mark that says so, and because this is in the
    // list handed to screencopy, the copy carries it too.
    if let Some(buffer) = scene.capture
        && let Some(display) = scene.layout.output(output)
    {
        let size = crate::indicator::SIZE;
        let inset = crate::indicator::INSET;
        let at = Rect::new(display.logical.w - inset - size, inset, 0, 0);
        let pixels = ((f64::from(size) * scale.x).round() as i32).max(4);
        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            to_physical(at, scale).to_f64(),
            buffer,
            None,
            // Drawn at this display's scale already, so it is placed at its own
            // size rather than scaled a second time. Same reasoning as the
            // cursor below, and the same trap if the source is left out.
            Some(smithay::utils::Rectangle::from_size(
                (f64::from(pixels), f64::from(pixels)).into(),
            )),
            Some((pixels, pixels).into()),
            Kind::Unspecified,
        ) {
            Ok(element) => out.push(IrontileElement::Image(element)),
            Err(_) => tracing::trace!("could not build the capture indicator"),
        }
    }

    // The pointer is above everything, including a fullscreen window. The
    // hotspot is subtracted here, so the pixel the client nominated is the one
    // under the pointer rather than the image's top left corner.
    match &scene.cursor {
        Some(Cursor::Image {
            buffer,
            hotspot,
            size,
            source,
            at,
        }) => {
            let local = Point::new(at.x - origin.x - hotspot.0, at.y - origin.y - hotspot.1);
            match MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                to_physical(Rect::new(local.x, local.y, 0, 0), scale).to_f64(),
                buffer,
                None,
                // Both rectangles are given, and both are needed. The image was
                // chosen for this display's scale, so its destination size is
                // stated rather than derived from the buffer, which would scale
                // it a second time. The source must then be stated as well:
                // left out, it defaults to the *destination* size rather than
                // to the whole image, which crops the cursor instead of
                // resizing it.
                Some(smithay::utils::Rectangle::from_size(
                    (f64::from(source.0), f64::from(source.1)).into(),
                )),
                Some((size.0, size.1).into()),
                Kind::Cursor,
            ) {
                Ok(element) => out.push(IrontileElement::Image(element)),
                Err(_) => tracing::trace!("could not build a cursor element"),
            }
        }
        Some(Cursor::Surface {
            surface,
            hotspot,
            at,
        }) => {
            let local = Point::new(at.x - origin.x - hotspot.0, at.y - origin.y - hotspot.1);
            out.extend(
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree::<
                    R,
                    IrontileElement<R>,
                >(
                    renderer,
                    surface,
                    to_physical(Rect::new(local.x, local.y, 0, 0), scale),
                    scale,
                    1.0,
                    Kind::Cursor,
                ),
            );
        }
        None => {}
    }

    // A locked session shows its lock screen and nothing else. Not the windows,
    // not the panels, not even a bar that reserved space -- the guarantee is
    // that what was on screen is not on screen, and a compositor that draws it
    // underneath is only pretending.
    //
    // A display with no lock surface is left as the background colour, which is
    // what a locker that has died looks like: blank, and still locked.
    if let Some(lock) = scene.lock {
        if let Some(surface) = lock.surface_for(output) {
            out.extend(
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree::<
                    R,
                    IrontileElement<R>,
                >(
                    renderer,
                    surface.wl_surface(),
                    to_physical(Rect::new(0, 0, 0, 0), scale),
                    scale,
                    1.0,
                    Kind::Unspecified,
                ),
            );
        }
        return out;
    }

    // Overlay and top layers go above every window; background and bottom go
    // below. These bracket the windows in the list.
    out.extend(layer_elements(
        scene,
        renderer,
        output,
        origin,
        scale,
        &[Layer::Overlay, Layer::Top],
    ));

    let mut ordered: Vec<_> = scene
        .frame
        .placements
        .iter()
        .filter(|p| p.output == output)
        .collect();
    ordered.sort_by_key(|p| std::cmp::Reverse(p.z));

    for placement in ordered {
        let Some(entry) = scene.windows.get(placement.window) else {
            continue;
        };
        let cell = local(placement.rect, origin);
        let content = match placement.kind {
            // A fullscreen window covers the display outright; a border would
            // make it not fullscreen.
            PlacementKind::Fullscreen => cell,
            _ => cell.inset(scene.theme.border_width),
        };

        // The cell is where the *window* goes, and a window is not the whole
        // buffer it arrives in. A client drawing its own decorations puts its
        // shadows outside the region it named with `set_window_geometry`, so
        // its buffer starts above and to the left of anything visible. Drawing
        // the buffer at the cell's corner therefore pushes the window itself
        // down and right by however wide those shadows are -- ten or twenty
        // pixels for a GTK application, and nothing at all for one that draws
        // no shadows, which is why it looks like only some programs are wrong.
        //
        // The size sent in the configure is a window geometry size too, so the
        // client is already sized correctly; only the origin needs moving back.
        let inset = entry.window.geometry().loc;
        let buffer = Rect::new(
            content.x - inset.x,
            content.y - inset.y,
            content.w,
            content.h,
        );

        out.extend(
            AsRenderElements::<R>::render_elements::<IrontileElement<R>>(
                &entry.window,
                renderer,
                to_physical(buffer, scale),
                scale,
                1.0,
            ),
        );

        if placement.kind == PlacementKind::Fullscreen || scene.theme.border_width <= 0 {
            continue;
        }
        // Size and colour live on the buffers, kept in step by `sync_borders`,
        // so that changing either advances the commit counter damage tracking
        // reads. Passing them here instead would repaint nothing.
        let paint = if placement.focused {
            &scene.theme.border_focused
        } else {
            &scene.theme.border_unfocused
        };
        let rects = border_segments(cell, scene.theme.border_width, !paint.is_solid());
        for (buffer, rect) in entry.border.iter().zip(rects) {
            out.push(IrontileElement::Border(
                SolidColorRenderElement::from_buffer(
                    buffer,
                    to_physical(rect, scale),
                    scale,
                    1.0,
                    Kind::Unspecified,
                ),
            ));
        }
    }

    out.extend(layer_elements(
        scene,
        renderer,
        output,
        origin,
        scale,
        &[Layer::Bottom, Layer::Background],
    ));
    out
}

/// Render elements for the layer-shell surfaces in the given layers, topmost
/// first.
fn layer_elements<R>(
    scene: &Scene<'_>,
    renderer: &mut R,
    output: OutputId,
    origin: Point,
    scale: Scale<f64>,
    layers: &[Layer],
) -> Vec<IrontileElement<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Send + Clone + 'static,
{
    let Some(entry) = scene.outputs.iter().find(|e| e.id == output) else {
        return Vec::new();
    };
    let _ = origin;
    let map = layer_map_for_output(&entry.output);
    let mut out = Vec::new();
    for layer in map.layers().rev() {
        if !layers.contains(&layer.layer()) {
            continue;
        }
        let Some(geometry) = map.layer_geometry(layer) else {
            continue;
        };
        // Layer geometry is already relative to its own display.
        let at = Rect::new(
            geometry.loc.x,
            geometry.loc.y,
            geometry.size.w,
            geometry.size.h,
        );
        out.extend(
            AsRenderElements::<R>::render_elements::<IrontileElement<R>>(
                layer,
                renderer,
                to_physical(at, scale),
                scale,
                1.0,
            ),
        );
    }
    out
}

/// The four strips making up a window's border: top, bottom, left, right.
///
/// A ring rather than a filled quad behind the window. The difference only
/// shows while a client's buffer is smaller than its cell -- during a resize,
/// or in a new window's first frames -- and then a backdrop paints that gap
/// border-coloured, which reads as a bright flash. A ring leaves the gap as
/// background, which is both less jarring and closer to the truth: there is
/// nothing there yet.
pub fn border_rects(cell: Rect, width: i32) -> [Rect; 4] {
    let width = width.max(0).min(cell.w / 2).min(cell.h / 2);
    let inner_h = (cell.h - 2 * width).max(0);
    [
        Rect::new(cell.x, cell.y, cell.w, width),
        Rect::new(cell.x, cell.bottom() - width, cell.w, width),
        Rect::new(cell.x, cell.y + width, width, inner_h),
        Rect::new(cell.right() - width, cell.y + width, width, inner_h),
    ]
}

/// How finely a gradient border is cut up.
///
/// Each strip becomes at most this many quads and each is filled with the
/// gradient sampled at its own centre. A real per-pixel gradient would want a
/// shader, and on a strip a couple of pixels thick the difference does not
/// survive being looked at: across a full-width window these are steps of
/// around one part in 255 per band.
const GRADIENT_SLICES: i32 = 32;

/// The quads making up a window's border.
///
/// The same list whether it is being sized in the global coordinate space or
/// drawn in one display's, because every piece is measured as an offset from
/// the cell rather than from the origin. That is what lets `sync_borders` own
/// the buffers and the renderer place them without the two having to agree on
/// anything but the cell's size.
pub fn border_segments(cell: Rect, width: i32, sliced: bool) -> Vec<Rect> {
    let strips = border_rects(cell, width);
    if !sliced {
        return strips.to_vec();
    }
    let mut out = Vec::new();
    for strip in strips {
        let along_x = strip.w >= strip.h;
        let length = if along_x { strip.w } else { strip.h };
        let pieces = length.clamp(1, GRADIENT_SLICES);
        for i in 0..pieces {
            // Cumulative fractions rather than a fixed step, so the pieces
            // exactly tile the strip however the division falls out.
            let from = (i * length) / pieces;
            let to = ((i + 1) * length) / pieces;
            out.push(if along_x {
                Rect::new(strip.x + from, strip.y, to - from, strip.h)
            } else {
                Rect::new(strip.x, strip.y + from, strip.w, to - from)
            });
        }
    }
    out
}

/// The colour one piece of a border is filled with.
///
/// Sampled at the piece's own centre, measured across the whole window rather
/// than along the strip it belongs to, so the four sides meet at the corners
/// instead of each running through the colours on its own.
pub fn segment_color(paint: &Paint, cell: Rect, piece: Rect) -> [f32; 4] {
    paint.at(paint.position(
        (piece.x - cell.x) as f32 + piece.w as f32 / 2.0,
        (piece.y - cell.y) as f32 + piece.h as f32 / 2.0,
        cell.w as f32,
        cell.h as f32,
    ))
}

/// Moves a rectangle from the global coordinate space into one display's.
fn local(rect: Rect, origin: Point) -> Rect {
    Rect::new(rect.x - origin.x, rect.y - origin.y, rect.w, rect.h)
}

fn to_physical(rect: Rect, scale: Scale<f64>) -> smithay::utils::Point<i32, Physical> {
    smithay::utils::Point::<i32, smithay::utils::Logical>::from((rect.x, rect.y))
        .to_physical_precise_round(scale)
}

/// The transform the nested backend renders with. Winit's surface is upside
/// down relative to the GL convention smithay renders in.
pub const NESTED_TRANSFORM: Transform = Transform::Flipped180;

#[cfg(test)]
mod tests {
    use super::{GRADIENT_SLICES, border_rects, border_segments, segment_color};
    use crate::theme::Paint;
    use irontile_layout::Rect;

    /// The area a ring of the given width covers, counted the long way round.
    fn ring_area(cell: Rect, width: i32) -> i32 {
        border_rects(cell, width).iter().map(|r| r.w * r.h).sum()
    }

    #[test]
    fn a_solid_border_is_the_four_strips_and_nothing_more() {
        let cell = Rect::new(10, 20, 300, 200);
        assert_eq!(
            border_segments(cell, 2, false),
            border_rects(cell, 2).to_vec()
        );
    }

    #[test]
    fn slicing_covers_the_same_pixels_the_strips_did() {
        // The pieces are what gets painted, so anything they fail to cover is a
        // gap in the border and anything they cover twice is a seam.
        let cell = Rect::new(10, 20, 301, 199);
        let sliced = border_segments(cell, 3, true);
        let area: i32 = sliced.iter().map(|r| r.w * r.h).sum();
        assert_eq!(area, ring_area(cell, 3));

        for strip in border_rects(cell, 3) {
            let mut covered: Vec<&Rect> = sliced
                .iter()
                .filter(|r| r.w > 0 && r.h > 0 && strip.contains_rect(**r))
                .collect();
            covered.sort_by_key(|r| (r.x, r.y));
            // Each piece starts exactly where the last one ended.
            for pair in covered.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                assert!(
                    a.right() == b.x || a.bottom() == b.y,
                    "{a:?} and {b:?} neither meet nor overlap"
                );
            }
        }
    }

    #[test]
    fn a_piece_is_the_same_size_wherever_the_window_sits() {
        // `sync_borders` measures in the global space and the renderer measures
        // in one display's, and the buffers they share carry only sizes. If
        // those two disagreed, a window on a second display would be painted
        // with the wrong border and nothing would say so.
        let here = border_segments(Rect::new(0, 0, 640, 480), 2, true);
        let there = border_segments(Rect::new(1731, 407, 640, 480), 2, true);
        let sizes = |v: &[Rect]| v.iter().map(|r| (r.w, r.h)).collect::<Vec<_>>();
        assert_eq!(sizes(&here), sizes(&there));
    }

    #[test]
    fn a_strip_shorter_than_the_slice_count_is_not_cut_into_empty_pieces() {
        // Otherwise a window shrunk to nothing would still cost a full set of
        // quads, every one of them zero pixels wide.
        let sliced = border_segments(Rect::new(0, 0, 8, 400), 1, true);
        let widest = sliced.iter().map(|r| r.w).max().unwrap();
        assert!(sliced.iter().all(|r| r.w > 0 && r.h > 0), "{sliced:?}");
        assert!(widest <= 8);
        assert!(sliced.len() <= 4 * GRADIENT_SLICES as usize);
    }

    #[test]
    fn a_diagonal_gradient_reaches_its_stops_at_opposite_corners() {
        // A sign error here is the difference between a border that runs the
        // way the configuration says and one that runs backwards, and both
        // look deliberate.
        let start = [1.0, 0.0, 0.0, 1.0];
        let end = [0.0, 0.0, 1.0, 1.0];
        let paint = Paint::gradient(vec![start, end], 45.0);
        let cell = Rect::new(400, 300, 800, 600);
        let pieces = border_segments(cell, 2, true);

        let nearest = |x: i32, y: i32| {
            let piece = pieces
                .iter()
                .min_by_key(|p| (p.center().x - x).abs() + (p.center().y - y).abs())
                .unwrap();
            segment_color(&paint, cell, *piece)
        };
        // Bottom-left is where 45 degrees starts; top-right is where it ends.
        let low = nearest(cell.x, cell.bottom());
        let high = nearest(cell.right(), cell.y);
        assert!(
            low[0] > 0.9 && low[2] < 0.1,
            "{low:?} should be the red end"
        );
        assert!(
            high[2] > 0.9 && high[0] < 0.1,
            "{high:?} should be the blue end"
        );
    }

    #[test]
    fn a_solid_border_is_one_colour_on_every_side() {
        let paint = Paint::solid([0.2, 0.4, 0.6, 1.0]);
        let cell = Rect::new(0, 0, 640, 480);
        for piece in border_segments(cell, 2, false) {
            assert_eq!(segment_color(&paint, cell, piece), [0.2, 0.4, 0.6, 1.0]);
        }
    }
}
