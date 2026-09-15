//! Drawing the lock screen.
//!
//! Kept apart from the Wayland client so it can be looked at without locking
//! anything: `irontile-lock --dump out.png` renders exactly what a display
//! would show. A lock screen is the one surface that cannot be iterated on
//! while it is doing its job.

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};
use tiny_skia::{Color, LinearGradient, Paint, PixmapMut, Point, Rect, SpreadMode, Transform};

/// How far the entry has got, and what to say about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Waiting for a password. The number is how many keys have landed, which
    /// decides where the bright pip sits and nothing else.
    Typing(usize),
    /// PAM has been asked and has not answered. It takes a couple of seconds
    /// to refuse, so this is a state somebody will actually sit and look at.
    Checking,
    /// The password was wrong, or the account will not have it.
    Denied(String),
    /// On the way out.
    Accepted,
}

/// Everything the screen shows, gathered so drawing takes no decisions of its
/// own and can be tested against a value rather than a running session.
#[derive(Clone, Debug)]
pub struct Screen {
    pub time: String,
    pub seconds: String,
    pub date: String,
    pub user: String,
    pub host: String,
    pub status: Status,
    pub caps: bool,
}

/// Colours, named for what they are rather than where they are used.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub ground: Color,
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
        // The compositor's own colours: the two warm tones are the gradient
        // irontile draws around a focused window, and the muted one is the
        // border it draws around the rest.
        Palette {
            ground: rgb(0x13, 0x11, 0x10),
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

/// The number of pips is fixed and says nothing about the length of what has
/// been typed. A track that grew would shift as it went and would tell anyone
/// watching how long the password is; neither is worth the feedback.
pub const PIPS: usize = 12;

/// Which pip is lit, given how many keys have landed.
///
/// Separated out because it is the whole of the rule and worth stating once:
/// nothing has been typed, nothing is lit; otherwise one pip walks round.
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

    fn shape(&mut self, text: &str, size: f32) -> Buffer {
        let metrics = Metrics::new(size, size * 1.25);
        let mut buffer = Buffer::new(&mut self.fonts, metrics);
        let attrs = match self.families.first() {
            Some(first) => Attrs::new().family(Family::Name(first)),
            None => Attrs::new(),
        };
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);
        buffer
    }

    /// How wide a string will be, so it can be centred before it is drawn.
    pub fn width(&mut self, text: &str, size: f32) -> f32 {
        self.shape(text, size)
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0, f32::max)
    }

    /// Draws `text` with its left edge at `x` and its baseline area at `y`.
    pub fn draw(
        &mut self,
        pixmap: &mut PixmapMut<'_>,
        text: &str,
        size: f32,
        x: f32,
        y: f32,
        color: Color,
    ) {
        let mut buffer = self.shape(text, size);
        let (r, g, b, a) = (
            (color.red() * 255.0) as u8,
            (color.green() * 255.0) as u8,
            (color.blue() * 255.0) as u8,
            (color.alpha() * 255.0) as u8,
        );
        let ink = cosmic_text::Color::rgba(r, g, b, a);
        buffer.draw(
            &mut self.fonts,
            &mut self.cache,
            ink,
            |px, py, w, h, colour| {
                if colour.a() == 0 {
                    return;
                }
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
    }

    /// Draws `text` centred on `centre_x`.
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
        self.draw(pixmap, text, size, centre_x - w / 2.0, y, color);
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

/// Renders the whole screen at `scale`, which is the display's.
///
/// Every measurement below is in logical pixels and multiplied here, so the
/// layout reads the same whatever the display is doing.
pub fn draw(
    pixmap: &mut PixmapMut<'_>,
    screen: &Screen,
    text: &mut Text,
    palette: &Palette,
    scale: f32,
) {
    let (w, h) = (pixmap.width() as f32, pixmap.height() as f32);
    let px = |logical: f32| logical * scale;

    pixmap.fill(palette.ground);

    let gutter = px(24.0);
    let centre = w / 2.0;

    // Corner metadata, the way a rice names the machine it is on.
    let meta = px(11.0);
    text.draw(pixmap, "irontile-lock", meta, gutter, gutter, palette.muted);
    let who = format!("{}@{}", screen.user, screen.host);
    let who_w = text.width(&who, meta);
    text.draw(pixmap, &who, meta, w - gutter - who_w, gutter, palette.dim);

    // The clock, which is the only thing on here anybody looks at on purpose.
    let clock_size = px(96.0);
    let time_w = text.width(&screen.time, clock_size);
    let secs_size = px(52.0);
    let secs_w = text.width(&screen.seconds, secs_size);
    let block = time_w + secs_w;
    let clock_y = h * 0.28;
    text.draw(
        pixmap,
        &screen.time,
        clock_size,
        centre - block / 2.0,
        clock_y,
        palette.text,
    );
    text.draw(
        pixmap,
        &screen.seconds,
        secs_size,
        centre - block / 2.0 + time_w,
        clock_y + px(38.0),
        palette.warm_far,
    );

    let date_y = clock_y + px(124.0);
    text.centred(
        pixmap,
        &screen.date,
        px(13.0),
        centre,
        date_y,
        palette.muted,
    );

    // The entry: a fixed track of pips over a hairline rule.
    let denied = matches!(screen.status, Status::Denied(_));
    let field_w = px(300.0).min(w - gutter * 2.0);
    let field_x = centre - field_w / 2.0;
    let track_y = date_y + px(96.0);

    let typed = match &screen.status {
        Status::Typing(n) => *n,
        // Mid-check the entry is complete but unjudged, so every pip stays up.
        Status::Checking | Status::Accepted => PIPS,
        Status::Denied(_) => 0,
    };
    let lit = lit_pip(typed);
    let radius = px(4.5);
    let step = field_w / (PIPS as f32 + 1.0);
    for i in 0..PIPS {
        let cx = field_x + step * (i as f32 + 1.0);
        let colour = if denied {
            palette.bad
        } else if Some(i) == lit {
            palette.warm_near
        } else if typed > 0 {
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

    // The rule carries the same gradient irontile draws around a focused
    // window, which is most of what makes this look like part of it.
    let rule_y = track_y + px(22.0);
    let rule_h = px(2.0);
    if denied {
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
            // A gradient needs two distinct points; on a display too narrow for
            // that, a flat rule is better than no rule.
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

    let (message, colour) = match &screen.status {
        Status::Typing(0) => ("enter password".to_string(), palette.muted),
        Status::Typing(_) => (String::new(), palette.muted),
        Status::Checking => ("checking".to_string(), palette.muted),
        Status::Denied(why) => (why.clone(), palette.bad),
        Status::Accepted => ("unlocked".to_string(), palette.warm_near),
    };
    if !message.is_empty() {
        text.centred(
            pixmap,
            &message,
            px(12.0),
            centre,
            rule_y + px(22.0),
            colour,
        );
    }

    if screen.caps {
        text.centred(
            pixmap,
            "caps lock",
            px(11.0),
            centre,
            rule_y + px(44.0),
            palette.warm_far,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{PIPS, lit_pip};

    #[test]
    fn nothing_typed_lights_nothing() {
        assert_eq!(lit_pip(0), None);
    }

    #[test]
    fn the_lit_pip_walks_round_without_counting() {
        // The point of the fixed track: after a full turn the lit pip is back
        // where it started, so the screen says a key landed and never says how
        // many have.
        assert_eq!(lit_pip(1), Some(0));
        assert_eq!(lit_pip(PIPS), Some(PIPS - 1));
        assert_eq!(lit_pip(PIPS + 1), Some(0));
        assert_eq!(lit_pip(PIPS * 3 + 5), lit_pip(5));
    }

    #[test]
    fn a_long_password_never_runs_off_the_track() {
        for typed in 1..500 {
            assert!(lit_pip(typed).is_some_and(|i| i < PIPS), "{typed}");
        }
    }
}

#[cfg(test)]
mod font_tests {
    use super::Text;

    /// A monospaced family this machine actually has, whatever it turns out to
    /// be.
    ///
    /// Naming one would make this a test of what is installed rather than of
    /// what the code does: the developer has the lock screen's own font, a CI
    /// runner has whatever the image ships, and a packaging container starts
    /// with no fonts at all until check() pulls one in.
    fn a_monospaced_family() -> Option<String> {
        let fonts = cosmic_text::FontSystem::new();
        fonts
            .db()
            .faces()
            .filter(|face| face.monospaced)
            .find_map(|face| face.families.first().map(|(name, _)| name.clone()))
    }

    #[test]
    fn the_named_font_is_the_one_that_gets_used() {
        // cosmic-text falls back silently when a family is missing, so a lock
        // screen can end up in whatever the system had lying about and nothing
        // says so. Monospace is the tell: ask for a face where every glyph is
        // the same width, and measure whether it is.
        let Some(family) = a_monospaced_family() else {
            // Nothing to ask for. A machine with no monospaced font cannot
            // show the difference either way.
            return;
        };
        let mut named = Text::new(std::slice::from_ref(&family));
        let narrow = named.width("iiiiiiiiii", 32.0);
        let wide = named.width("mmmmmmmmmm", 32.0);
        assert!(
            (narrow - wide).abs() < 1.0,
            "asked for {family}, which is monospaced, and got i={narrow} \
             m={wide} -- something else was substituted without saying so"
        );
    }
}
