//! Drawing the dialog.
//!
//! Kept apart from the Wayland client for the same reason the lock screen's is:
//! a prompt that only appears when something asks for an administrator is a
//! prompt nobody can iterate on, so `--dump` renders it to a file instead.
//!
//! The same method as everything else in this session -- tiny-skia over a
//! shared-memory buffer, cosmic-text for the type -- and deliberately the same
//! idiom as the lock screen: a fixed track of pips over a hairline rule. Two
//! places in a session ask for a password, and they should not look like they
//! came from different desktops.

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};
use tiny_skia::{Color, LinearGradient, Paint, PixmapMut, Point, Rect, SpreadMode, Transform};

/// How many pips the field shows, however long the password is. Same reasoning
/// as the lock screen: a track that grew would say how much had been typed.
pub const PIPS: usize = 12;

/// What the dialog shows.
#[derive(Clone, Debug)]
pub struct Prompt {
    /// What polkit says is being asked for, in its own words.
    pub message: String,
    /// The action's name, so the dialog says what it is authorising and not
    /// merely that something wants a password.
    pub action: String,
    /// Who the password belongs to.
    pub user: String,
    /// How many keys have landed.
    pub typed: usize,
    /// Set after a refusal, so the second attempt says why there is one.
    pub refused: bool,
}

/// Colours, named for what they are. The lock screen's, so the two agree.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub ground: Color,
    pub edge: Color,
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
            ground: rgb(0x1a, 0x17, 0x16),
            edge: rgb(0x3a, 0x32, 0x2f),
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

/// Which pip is lit, given how many keys have landed.
pub fn lit_pip(typed: usize) -> Option<usize> {
    (typed > 0).then(|| (typed - 1) % PIPS)
}

/// Draws text and keeps the font system between calls.
pub struct Text {
    fonts: FontSystem,
    cache: SwashCache,
    families: Vec<String>,
}

impl std::fmt::Debug for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Text")
            .field("families", &self.families)
            .finish_non_exhaustive()
    }
}

impl Text {
    pub fn new(families: &[String]) -> Text {
        Text {
            fonts: FontSystem::new(),
            cache: SwashCache::new(),
            families: families.to_vec(),
        }
    }

    fn shape(&mut self, text: &str, size: f32, wrap_at: Option<f32>) -> Buffer {
        let metrics = Metrics::new(size, size * 1.35);
        let mut buffer = Buffer::new(&mut self.fonts, metrics);
        let attrs = match self.families.first() {
            Some(first) => Attrs::new().family(Family::Name(first)),
            None => Attrs::new(),
        };
        // A width is what makes cosmic-text wrap; without one a long message
        // runs off the side of the dialog and takes the sentence with it.
        buffer.set_size(wrap_at, None);
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);
        buffer
    }

    pub fn width(&mut self, text: &str, size: f32) -> f32 {
        self.shape(text, size, None)
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0, f32::max)
    }

    /// Draws `text` with its top left at `x`, `y`, wrapped to `wrap_at` if
    /// given. Returns how tall it turned out, so what follows can sit under it.
    pub fn draw(
        &mut self,
        pixmap: &mut PixmapMut<'_>,
        text: &str,
        size: f32,
        at: (f32, f32),
        wrap_at: Option<f32>,
        color: Color,
    ) -> f32 {
        let (x, y) = at;
        let mut buffer = self.shape(text, size, wrap_at);
        let (r, g, b, a) = (
            (color.red() * 255.0) as u8,
            (color.green() * 255.0) as u8,
            (color.blue() * 255.0) as u8,
            (color.alpha() * 255.0) as u8,
        );
        let ink = cosmic_text::Color::rgba(r, g, b, a);
        let mut height = 0.0f32;
        buffer.draw(
            &mut self.fonts,
            &mut self.cache,
            ink,
            |px, py, w, h, colour| {
                if colour.a() == 0 {
                    return;
                }
                height = height.max(py as f32 + h as f32);
                let mut paint = Paint {
                    anti_alias: false,
                    ..Paint::default()
                };
                paint.set_color(Color::from_rgba8(
                    colour.r(),
                    colour.g(),
                    colour.b(),
                    colour.a(),
                ));
                if let Some(rect) =
                    Rect::from_xywh(x + px as f32, y + py as f32, w as f32, h as f32)
                {
                    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
                }
            },
        );
        height
    }

    pub fn centred(
        &mut self,
        pixmap: &mut PixmapMut<'_>,
        text: &str,
        size: f32,
        centre_x: f32,
        y: f32,
        color: Color,
    ) {
        let w = self.width(text, size);
        self.draw(pixmap, text, size, (centre_x - w / 2.0, y), None, color);
    }
}

fn solid(color: Color) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    paint
}

fn box_at(pixmap: &mut PixmapMut<'_>, x: f32, y: f32, w: f32, h: f32, paint: &Paint<'_>) {
    if let Some(rect) = Rect::from_xywh(x, y, w, h) {
        pixmap.fill_rect(rect, paint, Transform::identity(), None);
    }
}

fn circle(pixmap: &mut PixmapMut<'_>, cx: f32, cy: f32, r: f32, paint: &Paint<'_>) {
    let mut path = tiny_skia::PathBuilder::new();
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

/// Renders the whole dialog at `scale`, which is the display's.
pub fn draw(
    pixmap: &mut PixmapMut<'_>,
    prompt: &Prompt,
    text: &mut Text,
    palette: &Palette,
    scale: f32,
) {
    let (w, h) = (pixmap.width() as f32, pixmap.height() as f32);
    let px = |logical: f32| logical * scale;

    pixmap.fill(palette.ground);

    // A border, because this floats over whatever was underneath and its edges
    // are the only thing saying where it stops.
    let edge = px(1.0).max(1.0);
    let border = solid(palette.edge);
    box_at(pixmap, 0.0, 0.0, w, edge, &border);
    box_at(pixmap, 0.0, h - edge, w, edge, &border);
    box_at(pixmap, 0.0, 0.0, edge, h, &border);
    box_at(pixmap, w - edge, 0.0, edge, h, &border);

    let gutter = px(22.0);
    let centre = w / 2.0;
    let wrap = Some(w - gutter * 2.0);

    // What is being authorised, in polkit's words rather than ours.
    let mut y = gutter;
    y += text.draw(
        pixmap,
        &prompt.message,
        px(14.0),
        (gutter, y),
        wrap,
        palette.text,
    ) + px(10.0);

    // And which action it is. The message alone can be vague enough to cover
    // anything; the id is what somebody can look up.
    y += text.draw(
        pixmap,
        &prompt.action,
        px(10.0),
        (gutter, y),
        wrap,
        palette.muted,
    ) + px(14.0);

    text.draw(
        pixmap,
        &format!("password for {}", prompt.user),
        px(11.0),
        (gutter, y),
        wrap,
        palette.dim,
    );

    // The field: the lock screen's, so the two look like one desktop.
    let field_w = (w - gutter * 2.0).min(px(320.0));
    let field_x = centre - field_w / 2.0;
    let track_y = h - gutter - px(34.0);
    let lit = lit_pip(prompt.typed);
    let radius = px(4.5);
    let step = field_w / (PIPS as f32 + 1.0);
    for i in 0..PIPS {
        let cx = field_x + step * (i as f32 + 1.0);
        let colour = if prompt.refused {
            palette.bad
        } else if Some(i) == lit {
            palette.warm_near
        } else if prompt.typed > 0 {
            palette.muted
        } else {
            palette.hairline
        };
        let grown = if Some(i) == lit {
            radius * 1.25
        } else {
            radius
        };
        circle(pixmap, cx, track_y, grown, &solid(colour));
    }

    let rule_y = track_y + px(16.0);
    let rule_h = px(2.0);
    if prompt.refused {
        box_at(
            pixmap,
            field_x,
            rule_y,
            field_w,
            rule_h,
            &solid(palette.bad),
        );
    } else {
        let shader = LinearGradient::new(
            Point::from_xy(field_x, rule_y + rule_h),
            Point::from_xy(field_x + field_w, rule_y),
            vec![
                tiny_skia::GradientStop::new(0.0, palette.warm_near),
                tiny_skia::GradientStop::new(1.0, palette.warm_far),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        );
        match shader {
            Some(shader) => {
                let paint = Paint {
                    shader,
                    anti_alias: true,
                    ..Paint::default()
                };
                box_at(pixmap, field_x, rule_y, field_w, rule_h, &paint);
            }
            None => box_at(
                pixmap,
                field_x,
                rule_y,
                field_w,
                rule_h,
                &solid(palette.warm_near),
            ),
        }
    }

    let (hint, colour) = footer(prompt, palette);
    text.centred(pixmap, &hint, px(10.0), centre, rule_y + px(10.0), colour);
}

/// The line under the field, and the colour to say it in.
///
/// Its own function so what the dialog says can be checked without rendering
/// it: on a machine with no fonts every message draws as nothing, and a test
/// comparing pictures would pass whatever the words were.
pub fn footer(prompt: &Prompt, palette: &Palette) -> (String, Color) {
    if prompt.refused {
        return ("that was not the password".to_string(), palette.bad);
    }
    if prompt.typed == 0 {
        return (
            "enter to confirm, escape to cancel".to_string(),
            palette.muted,
        );
    }
    (String::new(), palette.muted)
}

#[cfg(test)]
mod tests {
    use super::{PIPS, Palette, Prompt, footer, lit_pip};

    fn prompt(typed: usize, refused: bool) -> Prompt {
        Prompt {
            message: "Authentication is required to manage system services".to_string(),
            action: "org.freedesktop.systemd1.manage-units".to_string(),
            user: "andrew".to_string(),
            typed,
            refused,
        }
    }

    #[test]
    fn nothing_typed_lights_nothing() {
        assert_eq!(lit_pip(0), None);
    }

    #[test]
    fn the_lit_pip_walks_round_without_counting() {
        // The same rule as the lock screen: after a full turn it is back where
        // it started, so the track says a key landed and never says how many.
        assert_eq!(lit_pip(1), Some(0));
        assert_eq!(lit_pip(PIPS), Some(PIPS - 1));
        assert_eq!(lit_pip(PIPS + 1), Some(0));
    }

    #[test]
    fn a_refusal_says_so_and_an_empty_field_says_what_to_do() {
        let palette = Palette::default();
        assert_eq!(
            footer(&prompt(0, true), &palette).0,
            "that was not the password"
        );
        assert!(footer(&prompt(0, false), &palette).0.contains("escape"));
    }

    #[test]
    fn typing_says_nothing_over_your_shoulder() {
        let palette = Palette::default();
        assert!(footer(&prompt(4, false), &palette).0.is_empty());
    }
}
