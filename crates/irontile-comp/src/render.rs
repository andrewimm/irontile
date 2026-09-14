//! Turning a layout frame into render elements.
//!
//! The only decoration is a solid quad drawn behind each window, one border
//! width larger on every side. Nothing else is drawn, and nothing here decides
//! where a window goes; it just reads the frame.
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
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{ImportAll, ImportMem, Renderer};
use smithay::desktop::layer_map_for_output;
use smithay::render_elements;
use smithay::utils::{Physical, Scale, Transform};
use smithay::wayland::shell::wlr_layer::Layer;

use crate::registry::Registry;
use crate::state::OutputEntry;
use crate::theme::Theme;

render_elements! {
    pub IrontileElement<R> where R: ImportAll + ImportMem;
    Surface = WaylandSurfaceRenderElement<R>,
    Border = SolidColorRenderElement,
    Cursor = MemoryRenderBufferRenderElement<R>,
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
    /// The pointer image and where it is, in global coordinates. `None` when
    /// something else is drawing the pointer, which is the case whenever
    /// irontile is nested.
    pub cursor: Option<(&'a MemoryRenderBuffer, Point)>,
}

/// Builds the element list for one display, topmost element first.
///
/// `draw_render_elements` paints in list order with earlier elements on top, so
/// the list is built from the highest placement down, and within each placement
/// the window's surfaces come before the border quad that sits behind them.
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

    // The pointer is above everything, including a fullscreen window.
    if let Some((buffer, at)) = scene.cursor {
        let local = Point::new(at.x - origin.x, at.y - origin.y);
        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            to_physical(Rect::new(local.x, local.y, 0, 0), scale).to_f64(),
            buffer,
            None,
            None,
            None,
            Kind::Cursor,
        ) {
            Ok(element) => out.push(IrontileElement::Cursor(element)),
            Err(_) => tracing::trace!("could not build a cursor element"),
        }
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

        out.extend(
            AsRenderElements::<R>::render_elements::<IrontileElement<R>>(
                &entry.window,
                renderer,
                to_physical(content, scale),
                scale,
                1.0,
            ),
        );

        if placement.kind == PlacementKind::Fullscreen || scene.theme.border_width <= 0 {
            continue;
        }
        let color = if placement.focused {
            scene.theme.border_focused
        } else {
            scene.theme.border_unfocused
        };
        out.push(IrontileElement::Border(SolidColorRenderElement::new(
            entry.border.clone(),
            physical_rect(cell, scale),
            CommitCounter::default(),
            color,
            Kind::Unspecified,
        )));
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

/// Moves a rectangle from the global coordinate space into one display's.
fn local(rect: Rect, origin: Point) -> Rect {
    Rect::new(rect.x - origin.x, rect.y - origin.y, rect.w, rect.h)
}

fn to_physical(rect: Rect, scale: Scale<f64>) -> smithay::utils::Point<i32, Physical> {
    smithay::utils::Point::<i32, smithay::utils::Logical>::from((rect.x, rect.y))
        .to_physical_precise_round(scale)
}

fn physical_rect(rect: Rect, scale: Scale<f64>) -> smithay::utils::Rectangle<i32, Physical> {
    smithay::utils::Rectangle::<i32, smithay::utils::Logical>::new(
        (rect.x, rect.y).into(),
        (rect.w.max(0), rect.h.max(0)).into(),
    )
    .to_physical_precise_round(scale)
}

/// The transform the nested backend renders with. Winit's surface is upside
/// down relative to the GL convention smithay renders in.
pub const NESTED_TRANSFORM: Transform = Transform::Flipped180;
