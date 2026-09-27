//! Drawing the popups and the panel.
//!
//! Everything here takes logical sizes and a scale, the way the bar does, so
//! one renderer serves displays that do not share one. Geometry is worked out
//! by functions that draw nothing, because where a button is matters to the
//! pointer as much as to the eye, and the two must not be able to disagree.

use tiny_skia::{
    Color, GradientStop, LinearGradient, Paint, PathBuilder, PixmapMut, Point, Rect, SpreadMode,
    Transform,
};

use crate::draw::TextRenderer;
use crate::icon::IconSet;
use crate::notify::model::{Centre, Hit, Notification, Urgency, ago};

/// The colours, which are the lock screen's and the compositor's.
///
/// Copied rather than shared: these three programs are separate binaries with
/// separate lifetimes, and a colour table is a cheaper thing to repeat than a
/// dependency between them. The two warm tones are the gradient irontile draws
/// around a focused window.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub ground: Color,
    pub raised: Color,
    pub hairline: Color,
    pub muted: Color,
    pub dim: Color,
    pub text: Color,
    pub warm_near: Color,
    pub warm_far: Color,
    pub bad: Color,
}

impl Default for Palette {
    fn default() -> Self {
        Palette {
            ground: rgb(0x13, 0x11, 0x10),
            raised: rgb(0x1c, 0x19, 0x18),
            hairline: rgb(0x2a, 0x24, 0x22),
            muted: rgb(0x69, 0x59, 0x59),
            dim: rgb(0x8f, 0x7f, 0x76),
            text: rgb(0xe8, 0xdd, 0xd5),
            warm_near: rgb(0xdd, 0xbb, 0xa8),
            warm_far: rgb(0xc6, 0x7f, 0x5f),
            bad: rgb(0xc4, 0x48, 0x3d),
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgba8(r, g, b, 255)
}

fn shaded(color: Color, alpha: f32) -> Color {
    Color::from_rgba(color.red(), color.green(), color.blue(), alpha).unwrap_or(color)
}

/// Logical sizes. Named rather than scattered, because a card and a panel have
/// to agree about them or the panel's cards sit differently from the popups.
pub mod size {
    /// How wide a popup is, and how wide the panel's cards are drawn.
    pub const CARD_W: f32 = 380.0;
    /// The least a card can be, before any text.
    pub const CARD_MIN_H: f32 = 78.0;
    pub const PANEL_W: f32 = 420.0;
    pub const PAD: f32 = 14.0;
    pub const GAP: f32 = 10.0;
    pub const ICON: f32 = 28.0;
    pub const RADIUS: f32 = 8.0;
    /// The accent stripe down the left edge of a card.
    pub const STRIPE: f32 = 3.0;
    pub const TITLE: f32 = 13.0;
    pub const BODY: f32 = 12.0;
    pub const SMALL: f32 = 11.0;
    /// The session buttons along the bottom of the panel.
    pub const BUTTON_H: f32 = 62.0;
    /// The switches in the header.
    pub const CHIP_H: f32 = 22.0;
    pub const CHIP_ICON: f32 = 12.0;
    pub const BUTTON_ICON: f32 = 20.0;
    /// At most two lines of body text: a notification is a summary, and one
    /// that grows to fit an essay pushes the others off the screen.
    pub const BODY_LINES: usize = 2;
}

/// How tall a card is, given its text.
///
/// Worked out without drawing so that a surface can be sized before a single
/// pixel is laid down -- a layer surface has to ask for its size before it has
/// anything to put in it.
pub fn card_height(text: &mut TextRenderer, card: &Notification, scale: f32) -> f32 {
    let px = |v: f32| v * scale;
    let body_w = px(size::CARD_W) - px(size::PAD) * 2.0 - px(size::ICON) - px(size::GAP);
    text.set_size(px(size::BODY));
    let body = if card.body.is_empty() {
        0.0
    } else {
        text.wrapped_height(&card.body, body_w, size::BODY_LINES) + px(4.0)
    };
    let actions = if card.actions.is_empty() {
        0.0
    } else {
        px(28.0)
    };
    (px(size::PAD) * 2.0 + px(20.0) + body + actions).max(px(size::CARD_MIN_H))
}

/// Draws one notification, with its top-left at `x`, `y`. Returns its height.
pub fn card(
    pixmap: &mut PixmapMut<'_>,
    icons: &mut IconSet,
    text: &mut TextRenderer,
    card: &Notification,
    palette: &Palette,
    at: (f32, f32),
    scale: f32,
) -> f32 {
    let px = |v: f32| v * scale;
    let (x, y) = at;
    let w = px(size::CARD_W);
    let h = card_height(text, card, scale);

    rounded(pixmap, x, y, w, h, px(size::RADIUS), &solid(palette.raised));
    // The stripe says how loudly this asked to be seen, in the one place a
    // colour can say it without competing with the words.
    let stripe = match card.urgency {
        Urgency::Critical => solid(palette.bad),
        Urgency::Low => solid(palette.hairline),
        Urgency::Normal => warm(palette, x, y + h, x + px(size::STRIPE), y),
    };
    rounded(
        pixmap,
        x,
        y,
        px(size::STRIPE) * 2.0,
        h,
        px(size::RADIUS),
        &stripe,
    );
    box_at(
        pixmap,
        x + px(size::STRIPE),
        y,
        px(size::RADIUS),
        h,
        &solid(palette.raised),
    );

    let left = x + px(size::PAD) + px(size::STRIPE);
    let mut cursor = y + px(size::PAD);

    // The application's icon, or its initial in a soft disc when it named one
    // nobody has. Something always sits in this column, so the text below it
    // starts at the same place whatever arrived.
    // A critical notification's icon is said in the colour the stripe uses,
    // because the one thing worth noticing about it is that it is not
    // ordinary.
    let icon_colour = match card.urgency {
        Urgency::Critical => palette.bad,
        _ => palette.dim,
    };
    let drawn = card.icon.as_deref().is_some_and(|name| {
        icons.draw(
            pixmap,
            name,
            left,
            cursor,
            px(size::ICON).round() as u32,
            crate::theme::Color(icon_colour),
        )
    });
    if !drawn {
        let r = px(size::ICON) / 2.0;
        circle(
            pixmap,
            left + r,
            cursor + r,
            r,
            &solid(shaded(palette.warm_far, 0.25)),
        );
        if let Some(initial) = card.app.chars().next() {
            text.set_size(px(size::TITLE));
            let letter = initial.to_uppercase().to_string();
            let lw = text.width(&letter);
            text.draw_at(
                pixmap,
                &letter,
                left + r - lw / 2.0,
                cursor + r - px(size::TITLE) * 0.7,
                crate::theme::Color(palette.warm_near),
            );
        }
    }

    let body_x = left + px(size::ICON) + px(size::GAP);
    let body_w = x + w - px(size::PAD) - body_x;

    // The application and how long ago, along the top in the quietest colour
    // on the card: useful when looked for, invisible when not.
    text.set_size(px(size::SMALL));
    let stamp = ago(card.age);
    let stamp_w = text.width(&stamp);
    text.draw_at(
        pixmap,
        &card.app,
        body_x,
        cursor,
        crate::theme::Color(palette.muted),
    );
    text.draw_at(
        pixmap,
        &stamp,
        x + w - px(size::PAD) - stamp_w,
        cursor,
        crate::theme::Color(palette.muted),
    );
    cursor += px(size::SMALL) * 1.5;

    text.set_size(px(size::TITLE));
    text.draw_wrapped(
        pixmap,
        &card.summary,
        (body_x, cursor),
        (body_w, 1),
        crate::theme::Color(palette.text),
    );
    cursor += px(size::TITLE) * 1.5;

    if !card.body.is_empty() {
        text.set_size(px(size::BODY));
        let height = text.draw_wrapped(
            pixmap,
            &card.body,
            (body_x, cursor),
            (body_w, size::BODY_LINES),
            crate::theme::Color(palette.dim),
        );
        cursor += height + px(4.0);
    }

    if !card.actions.is_empty() {
        text.set_size(px(size::SMALL));
        let mut button_x = body_x;
        for action in &card.actions {
            let label_w = text.width(&action.label);
            let button_w = label_w + px(16.0);
            if button_x + button_w > x + w - px(size::PAD) {
                break;
            }
            rounded(
                pixmap,
                button_x,
                cursor,
                button_w,
                px(20.0),
                px(4.0),
                &solid(palette.hairline),
            );
            text.draw_at(
                pixmap,
                &action.label,
                button_x + px(8.0),
                cursor + px(4.0),
                crate::theme::Color(palette.warm_near),
            );
            button_x += button_w + px(6.0);
        }
    }

    h
}

/// Where each session button sits, in the panel's own pixels.
///
/// Free of any drawing so the pointer and the picture read the same table. The
/// row divides the width evenly: five buttons is what there are, and a row
/// that sized each to its label would shuffle as the labels changed.
pub fn button_rects(
    count: usize,
    width: f32,
    bottom: f32,
    scale: f32,
) -> Vec<(f32, f32, f32, f32)> {
    if count == 0 {
        return Vec::new();
    }
    let px = |v: f32| v * scale;
    let pad = px(size::PAD);
    let inner = width - pad * 2.0;
    let each = inner / count as f32;
    let top = bottom - px(size::BUTTON_H) - pad;
    (0..count)
        .map(|i| (pad + each * i as f32, top, each, px(size::BUTTON_H)))
        .collect()
}

/// Which button a point is in, if any.
pub fn button_at(rects: &[(f32, f32, f32, f32)], point: (f32, f32)) -> Option<usize> {
    rects.iter().position(|(x, y, w, h)| {
        point.0 >= *x && point.0 < x + w && point.1 >= *y && point.1 < y + h
    })
}

/// A rectangle in the panel's own pixels, and what pressing it does.
pub type Spot = (Hit, (f32, f32, f32, f32));

/// Every place in the panel a pointer can land, in the order they are drawn.
///
/// The one table both the drawing and the pointer read. A panel where the
/// picture and the hit testing were worked out separately is a panel with
/// buttons that are a few pixels away from where they look, which is the kind
/// of wrong nobody can quite describe.
pub fn spots(text: &mut TextRenderer, state: &Centre, (w, h): (f32, f32), scale: f32) -> Vec<Spot> {
    let px = |v: f32| v * scale;
    let pad = px(size::PAD);
    let mut spots = Vec::new();

    // The header's two switches, right-aligned, quiet hours outermost.
    text.set_size(px(size::SMALL));
    let clear_w = text.width("Clear all") + px(size::CHIP_ICON) + px(18.0);
    let quiet_w = text.width("Quiet hours") + px(size::CHIP_ICON) + px(18.0);
    let chip_h = px(size::CHIP_H);
    let chip_y = pad - px(2.0);
    let quiet_x = w - pad - quiet_w;
    spots.push((Hit::Quiet, (quiet_x, chip_y, quiet_w, chip_h)));
    if !state.notifications.is_empty() {
        spots.push((
            Hit::Clear,
            (quiet_x - clear_w - px(6.0), chip_y, clear_w, chip_h),
        ));
    }

    // The session buttons, sharing the width of the row evenly.
    for (index, rect) in button_rects(state.buttons.len(), w, h, scale)
        .into_iter()
        .enumerate()
    {
        spots.push((Hit::Power(index), rect));
    }

    // The cards, from the rule down to whatever the buttons left.
    let floor = spots
        .iter()
        .filter_map(|(hit, (_, y, _, _))| matches!(hit, Hit::Power(_)).then_some(*y))
        .fold(h - pad, f32::min)
        - px(size::GAP);
    let mut cursor = pad + px(26.0) + px(18.0);
    let card_x = pad + (w - pad * 2.0 - px(size::CARD_W)) / 2.0;
    for note in &state.notifications {
        let height = card_height(text, note, scale);
        if cursor + height > floor {
            break;
        }
        spots.push((
            Hit::Card(note.id),
            (card_x, cursor, px(size::CARD_W), height),
        ));
        cursor += height + px(size::GAP);
    }

    spots
}

/// What is under a point, if anything.
pub fn spot_at(spots: &[Spot], point: (f32, f32)) -> Option<Hit> {
    spots
        .iter()
        .find(|(_, (x, y, w, h))| {
            point.0 >= *x && point.0 < x + w && point.1 >= *y && point.1 < y + h
        })
        .map(|(hit, _)| *hit)
}

/// Draws the whole panel into a buffer of `w` by `h` real pixels.
pub fn centre(
    pixmap: &mut PixmapMut<'_>,
    icons: &mut IconSet,
    text: &mut TextRenderer,
    state: &Centre,
    palette: &Palette,
    (w, h): (f32, f32),
    scale: f32,
) {
    let px = |v: f32| v * scale;
    let pad = px(size::PAD);
    pixmap.fill(shaded(palette.ground, 0.96));

    text.set_size(px(15.0));
    text.draw_at(
        pixmap,
        "Notifications",
        pad,
        pad,
        crate::theme::Color(palette.text),
    );

    let spots = spots(text, state, (w, h), scale);

    // The same rule the lock screen draws under its field, for the same
    // reason: it is most of what makes these look like one desktop.
    let rule_y = pad + px(26.0);
    let rule_h = px(1.5);
    let paint = warm(palette, pad, rule_y + rule_h, w - pad, rule_y);
    box_at(pixmap, pad, rule_y, w - pad * 2.0, rule_h, &paint);

    if state.notifications.is_empty() {
        text.set_size(px(size::BODY));
        let message = "Nothing to report";
        let width = text.width(message);
        text.draw_at(
            pixmap,
            message,
            (w - width) / 2.0,
            rule_y + px(42.0),
            crate::theme::Color(palette.muted),
        );
    }

    for (hit, rect) in &spots {
        let hovered = state.hovered == Some(*hit);
        match hit {
            Hit::Quiet => chip(
                pixmap,
                icons,
                text,
                rect,
                if state.quiet {
                    "notifications-disabled-symbolic"
                } else {
                    "preferences-system-notifications-symbolic"
                },
                "Quiet hours",
                // Lit when it is on, because a switch that looks the same
                // either way is a switch nobody trusts.
                if state.quiet {
                    palette.warm_near
                } else if hovered {
                    palette.dim
                } else {
                    palette.muted
                },
                hovered || state.quiet,
                palette,
                scale,
            ),
            Hit::Clear => chip(
                pixmap,
                icons,
                text,
                rect,
                "edit-clear-all-symbolic",
                "Clear all",
                if hovered { palette.dim } else { palette.muted },
                hovered,
                palette,
                scale,
            ),
            Hit::Card(id) => {
                if let Some(note) = state.notifications.iter().find(|n| n.id == *id) {
                    card(pixmap, icons, text, note, palette, (rect.0, rect.1), scale);
                }
            }
            Hit::Power(index) => {
                let Some(button) = state.buttons.get(*index) else {
                    continue;
                };
                let (bx, by, bw, bh) = *rect;
                if hovered {
                    rounded(
                        pixmap,
                        bx + px(2.0),
                        by,
                        bw - px(4.0),
                        bh,
                        px(size::RADIUS),
                        &solid(palette.hairline),
                    );
                }
                let colour = if hovered {
                    palette.warm_near
                } else {
                    palette.dim
                };
                let icon_px = px(size::BUTTON_ICON).round() as u32;
                let icon_x = bx + (bw - icon_px as f32) / 2.0;
                let icon_y = by + px(12.0);
                if !icons.draw(
                    pixmap,
                    &button.icon,
                    icon_x,
                    icon_y,
                    icon_px,
                    crate::theme::Color(colour),
                ) {
                    // No icon theme installed, or a name this one does not
                    // carry. A disc is not a picture of anything, but it keeps
                    // the row aligned and the word underneath still says what
                    // the button does.
                    circle(
                        pixmap,
                        icon_x + icon_px as f32 / 2.0,
                        icon_y + icon_px as f32 / 2.0,
                        icon_px as f32 / 2.0,
                        &solid(shaded(colour, 0.35)),
                    );
                }
                text.set_size(px(size::SMALL));
                let label_w = text.width(&button.label);
                text.draw_at(
                    pixmap,
                    &button.label,
                    bx + (bw - label_w) / 2.0,
                    by + px(12.0) + icon_px as f32 + px(8.0),
                    crate::theme::Color(colour),
                );
            }
        }
    }
}

/// One of the header's switches: an icon, a word, and a soft back when it is
/// lit or under the pointer.
#[allow(clippy::too_many_arguments)]
fn chip(
    pixmap: &mut PixmapMut<'_>,
    icons: &mut IconSet,
    text: &mut TextRenderer,
    rect: &(f32, f32, f32, f32),
    icon: &str,
    label: &str,
    colour: Color,
    lit: bool,
    palette: &Palette,
    scale: f32,
) {
    let px = |v: f32| v * scale;
    let (x, y, w, h) = *rect;
    if lit {
        rounded(pixmap, x, y, w, h, h / 2.0, &solid(palette.hairline));
    }
    let icon_px = px(size::CHIP_ICON).round() as u32;
    let icon_x = x + px(8.0);
    icons.draw(
        pixmap,
        icon,
        icon_x,
        y + (h - icon_px as f32) / 2.0,
        icon_px,
        crate::theme::Color(colour),
    );
    text.set_size(px(size::SMALL));
    text.draw_at(
        pixmap,
        label,
        icon_x + icon_px as f32 + px(5.0),
        y + (h - px(size::SMALL) * 1.4) / 2.0,
        crate::theme::Color(colour),
    );
}

fn solid(color: Color) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    paint
}

/// The focused-window gradient, from one point to another.
fn warm(palette: &Palette, x0: f32, y0: f32, x1: f32, y1: f32) -> Paint<'static> {
    let shader = LinearGradient::new(
        Point::from_xy(x0, y0),
        Point::from_xy(x1, y1),
        vec![
            GradientStop::new(0.0, palette.warm_near),
            GradientStop::new(1.0, palette.warm_far),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    );
    match shader {
        Some(shader) => Paint {
            shader,
            anti_alias: true,
            ..Paint::default()
        },
        // A gradient needs two distinct points; flat is better than nothing.
        None => solid(palette.warm_near),
    }
}

fn box_at(pixmap: &mut PixmapMut<'_>, x: f32, y: f32, w: f32, h: f32, paint: &Paint<'_>) {
    if let Some(rect) = Rect::from_xywh(x, y, w, h) {
        pixmap.fill_rect(rect, paint, Transform::identity(), None);
    }
}

fn circle(pixmap: &mut PixmapMut<'_>, cx: f32, cy: f32, r: f32, paint: &Paint<'_>) {
    let mut path = PathBuilder::new();
    path.push_circle(cx, cy, r);
    if let Some(path) = path.finish() {
        pixmap.fill_path(
            &path,
            paint,
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn rounded(pixmap: &mut PixmapMut<'_>, x: f32, y: f32, w: f32, h: f32, r: f32, paint: &Paint<'_>) {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let mut path = PathBuilder::new();
    path.move_to(x + r, y);
    path.line_to(x + w - r, y);
    path.quad_to(x + w, y, x + w, y + r);
    path.line_to(x + w, y + h - r);
    path.quad_to(x + w, y + h, x + w - r, y + h);
    path.line_to(x + r, y + h);
    path.quad_to(x, y + h, x, y + h - r);
    path.line_to(x, y + r);
    path.quad_to(x, y, x + r, y);
    path.close();
    if let Some(path) = path.finish() {
        pixmap.fill_path(
            &path,
            paint,
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buttons_divide_the_row_evenly_and_sit_above_the_bottom() {
        let rects = button_rects(5, 420.0, 900.0, 1.0);
        assert_eq!(rects.len(), 5);
        let widths: Vec<f32> = rects.iter().map(|(_, _, w, _)| *w).collect();
        assert!(
            widths
                .windows(2)
                .all(|pair| (pair[0] - pair[1]).abs() < 0.01)
        );
        // Inside the panel, with the same gutter as everything else.
        assert!((rects[0].0 - size::PAD).abs() < 0.01);
        let (x, _, w, _) = rects[4];
        assert!((x + w - (420.0 - size::PAD)).abs() < 0.01);
        assert!(rects[0].1 + rects[0].3 < 900.0);
    }

    #[test]
    fn a_point_finds_the_button_under_it_and_nothing_between_them() {
        let rects = button_rects(5, 420.0, 900.0, 1.0);
        let (x, y, w, h) = rects[2];
        assert_eq!(button_at(&rects, (x + w / 2.0, y + h / 2.0)), Some(2));
        // Above the row is the list, which is not a button.
        assert_eq!(button_at(&rects, (x + w / 2.0, y - 1.0)), None);
        assert_eq!(button_at(&rects, (x + w / 2.0, y + h + 1.0)), None);
    }

    #[test]
    fn a_row_of_no_buttons_has_no_rectangles() {
        assert!(button_rects(0, 420.0, 900.0, 1.0).is_empty());
        assert_eq!(button_at(&[], (10.0, 10.0)), None);
    }
}
