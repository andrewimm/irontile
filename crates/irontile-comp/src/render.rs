//! Turning a layout frame into render elements.
//!
//! The only decoration is a solid quad drawn behind each window, one border
//! width larger on every side. Nothing else is drawn, and nothing here decides
//! where a window goes; it just reads the frame.

use irontile_layout::{PlacementKind, Rect};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{ImportAll, Renderer};
use smithay::render_elements;
use smithay::utils::{Physical, Point, Rectangle, Scale, Transform};

use crate::state::Irontile;

render_elements! {
    pub IrontileElement<R> where R: ImportAll;
    Surface = WaylandSurfaceRenderElement<R>,
    Border = SolidColorRenderElement,
}

/// Builds the element list for one frame, topmost element first.
///
/// `draw_render_elements` paints in list order with earlier elements on top, so
/// the list is built from the highest placement down, and within each placement
/// the window's surfaces come before the border quad that sits behind them.
pub fn elements<R>(state: &Irontile, renderer: &mut R, scale: f64) -> Vec<IrontileElement<R>>
where
    R: Renderer + ImportAll,
    R::TextureId: Clone + 'static,
{
    let mut ordered: Vec<_> = state.placements.placements.iter().collect();
    ordered.sort_by_key(|p| std::cmp::Reverse(p.z));

    let scale = Scale::from(scale);
    let mut out = Vec::new();

    for placement in ordered {
        let Some(entry) = state.windows.get(placement.window) else {
            continue;
        };
        let content = state.content_rect(placement.rect, placement.kind);
        let location = to_physical(content, scale);

        out.extend(
            smithay::backend::renderer::element::AsRenderElements::<R>::render_elements::<
                IrontileElement<R>,
            >(&entry.window, renderer, location, scale, 1.0),
        );

        // A fullscreen window has no border, so there is no quad to put behind
        // it, and drawing one would show through at the display edges.
        if placement.kind == PlacementKind::Fullscreen || state.config.theme.border_width <= 0 {
            continue;
        }
        let color = if placement.focused {
            state.config.theme.border_focused
        } else {
            state.config.theme.border_unfocused
        };
        out.push(IrontileElement::Border(SolidColorRenderElement::new(
            entry.border.clone(),
            physical_rect(placement.rect, scale),
            CommitCounter::default(),
            color,
            Kind::Unspecified,
        )));
    }

    out
}

fn to_physical(rect: Rect, scale: Scale<f64>) -> Point<i32, Physical> {
    Point::<i32, smithay::utils::Logical>::from((rect.x, rect.y)).to_physical_precise_round(scale)
}

fn physical_rect(rect: Rect, scale: Scale<f64>) -> Rectangle<i32, Physical> {
    Rectangle::<i32, smithay::utils::Logical>::new(
        (rect.x, rect.y).into(),
        (rect.w.max(0), rect.h.max(0)).into(),
    )
    .to_physical_precise_round(scale)
}

/// The transform the nested backend renders with. Winit's surface is upside
/// down relative to the GL convention smithay renders in.
pub const NESTED_TRANSFORM: Transform = Transform::Flipped180;
