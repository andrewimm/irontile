//! Drawing the lock screen.
//!
//! Kept apart from the Wayland client so it can be looked at without locking
//! anything: `irontile-lock --dump out.png` renders exactly what a display
//! would show. A lock screen is the one surface that cannot be iterated on
//! while it is doing its job.

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};
use irontile_power::Battery;
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
    /// What the battery is doing, or `None` on a machine without one, which
    /// draws nothing rather than a zero.
    pub battery: Option<Battery>,
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

/// How wide the battery glyph is, including its terminal, in logical pixels.
const BATTERY_W: f32 = 17.5;
/// How tall its body is. Chosen against the 11px corner text: a shade shorter
/// than the letters beside it, so it sits in the line rather than on it.
const BATTERY_H: f32 = 8.0;
/// Below this, unplugged, the number is the point rather than the decoration.
const BATTERY_LOW: f64 = 15.0;

/// Draws the battery glyph with its top left at `x`, `y`, sized in real pixels.
///
/// Drawn from rectangles rather than set in a font. Every glyph on this screen
/// comes from whatever the machine happens to have installed, and a battery is
/// exactly the character a bare machine turns out not to have -- the lock
/// screen is the last place to find that out. Rectangles are always there.
fn battery_at(
    pixmap: &mut PixmapMut<'_>,
    x: f32,
    y: f32,
    scale: f32,
    state: &Battery,
    palette: &Palette,
) {
    let px = |logical: f32| logical * scale;
    // Snapped to whole pixels, all of it. The case is a single pixel thick,
    // and a single pixel laid down at half past a pixel is drawn as two grey
    // ones -- a soft grey box beside type that is pin sharp, which looks like
    // a mistake even to somebody who could not say what was wrong with it.
    let x = x.round();
    let y = y.round();
    let body_w = px(16.0).round();
    let body_h = px(BATTERY_H).round();
    let edge = px(1.0).round().max(1.0);

    let shell = if state.charging {
        palette.warm_near
    } else if state.percent <= BATTERY_LOW {
        palette.bad
    } else {
        palette.dim
    };
    let paint = solid(shell);

    // The case, as four strips rather than a stroked path: at this size a
    // stroke lands on half pixels and comes out a different weight on each
    // side, which reads as a wonky box next to type that is pin sharp.
    box_at(pixmap, x, y, body_w, edge, &paint);
    box_at(pixmap, x, y + body_h - edge, body_w, edge, &paint);
    box_at(pixmap, x, y, edge, body_h, &paint);
    box_at(pixmap, x + body_w - edge, y, edge, body_h, &paint);

    // The terminal on the positive end, which is what makes a rounded oblong
    // read as a battery and not as a progress bar.
    let nub_h = px(3.5).round();
    box_at(
        pixmap,
        x + body_w,
        y + ((body_h - nub_h) / 2.0).round(),
        px(BATTERY_W - 16.0).round().max(1.0),
        nub_h,
        &paint,
    );

    // The charge inside, inset by the case and a gap so the two never touch.
    let inset = edge * 2.0;
    let room = body_w - inset * 2.0;
    let level = (state.percent.clamp(0.0, 100.0) / 100.0) as f32;
    // On the charger the case fills regardless of level: the bolt has to sit
    // on something, and the number beside it says how full it is anyway.
    let filled = if state.charging { room } else { room * level };
    // Anything left at all shows as a sliver rather than rounding away to an
    // empty case: "nearly flat" and "flat" are different enough to be worth a
    // pixel, and the case is drawn either way so nothing is lost by it.
    let filled = if filled > 0.0 {
        filled.round().max(1.0)
    } else {
        0.0
    };
    box_at(
        pixmap,
        x + inset,
        y + inset,
        filled,
        body_h - inset * 2.0,
        &paint,
    );

    if state.charging {
        bolt(pixmap, x, y, body_w, body_h, palette.ground);
    }
}

/// Cuts a lightning bolt out of a filled battery, in the ground colour.
///
/// Drawn as a hole rather than a mark so it reads at eleven pixels: a bolt
/// drawn *on* the case competes with the case, while a bolt taken out of it
/// has the whole fill behind it.
fn bolt(pixmap: &mut PixmapMut<'_>, x: f32, y: f32, w: f32, h: f32, colour: Color) {
    // Proportions of the body, so the shape holds at any scale.
    let at = |fx: f32, fy: f32| Point::from_xy(x + w * fx, y + h * fy);
    let points = [
        at(0.58, 0.12),
        at(0.34, 0.56),
        at(0.47, 0.56),
        at(0.40, 0.88),
        at(0.66, 0.42),
        at(0.52, 0.42),
    ];
    let mut path = tiny_skia::PathBuilder::new();
    path.move_to(points[0].x, points[0].y);
    for point in &points[1..] {
        path.line_to(point.x, point.y);
    }
    path.close();
    if let Some(path) = path.finish() {
        pixmap.fill_path(
            &path,
            &solid(colour),
            tiny_skia::FillRule::Winding,
            Transform::identity(),
            None,
        );
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

    // The battery under it, sharing that right edge. This corner is where the
    // screen says what machine it is; how much of it is left belongs with
    // that, and it is the one thing on here that cannot be found out any
    // other way -- the bar that would otherwise say it is behind this.
    if let Some(state) = &screen.battery {
        let reading = format!("{}%", state.percent.round());
        let reading_w = text.width(&reading, meta);
        let gap = px(5.0);
        let group_w = px(BATTERY_W) + gap + reading_w;
        let group_x = w - gutter - group_w;
        // Far enough below the name to be its own line, close enough to read
        // as the same corner.
        let row_y = gutter + px(19.0);
        // Text sits in a box a quarter taller than the type; the glyph is
        // centred on that box rather than on the text's own top edge.
        let middle = row_y + meta * 1.25 / 2.0;
        battery_at(
            pixmap,
            group_x,
            middle - px(BATTERY_H) / 2.0,
            scale,
            state,
            palette,
        );
        let ink = if state.charging {
            palette.warm_near
        } else if state.percent <= BATTERY_LOW {
            palette.bad
        } else {
            palette.dim
        };
        text.draw(
            pixmap,
            &reading,
            meta,
            group_x + px(BATTERY_W) + gap,
            row_y,
            ink,
        );
    }

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

#[cfg(test)]
mod battery_tests {
    use super::{Battery, Palette, Screen, Status, Text, draw};
    use irontile_power::Battery as Power;

    /// Renders the corner and hands back its pixels.
    ///
    /// Everything asserted below is drawn from rectangles, so these say the
    /// same thing on a machine with a full set of fonts and on one with none
    /// at all -- which is the state a build container is in.
    fn corner(battery: Option<Power>) -> Vec<[u8; 4]> {
        let mut pixmap = tiny_skia::Pixmap::new(600, 200).expect("a pixmap that size");
        let screen = Screen {
            time: "17:30".to_string(),
            seconds: ":46".to_string(),
            date: "tuesday, 15 september".to_string(),
            user: "andrew".to_string(),
            host: "enlil".to_string(),
            status: Status::Typing(0),
            caps: false,
            battery,
        };
        let mut text = Text::new(&[]);
        draw(
            &mut pixmap.as_mut(),
            &screen,
            &mut text,
            &Palette::default(),
            1.0,
        );
        let mut out = Vec::new();
        for y in 0..70 {
            for x in 400..600 {
                let p = pixmap.pixel(x, y).expect("inside the pixmap");
                out.push([p.red(), p.green(), p.blue(), p.alpha()]);
            }
        }
        out
    }

    fn holds(pixels: &[[u8; 4]], colour: tiny_skia::Color) -> bool {
        let want = [
            (colour.red() * 255.0).round() as u8,
            (colour.green() * 255.0).round() as u8,
            (colour.blue() * 255.0).round() as u8,
        ];
        pixels
            .iter()
            .any(|p| p[0] == want[0] && p[1] == want[1] && p[2] == want[2])
    }

    #[test]
    fn a_machine_without_a_battery_is_not_a_machine_at_nought_percent() {
        let none = corner(None);
        let some = corner(Some(Power {
            percent: 45.0,
            charging: false,
        }));
        assert_ne!(
            none, some,
            "a desktop should draw no battery at all, not an empty one"
        );
    }

    #[test]
    fn a_battery_running_out_is_drawn_in_the_colour_of_a_warning() {
        let palette = Palette::default();
        let low = corner(Some(Power {
            percent: 8.0,
            charging: false,
        }));
        let fine = corner(Some(Power {
            percent: 80.0,
            charging: false,
        }));
        assert!(
            holds(&low, palette.bad),
            "a battery this low is the one thing in that corner worth looking at"
        );
        assert!(
            !holds(&fine, palette.bad),
            "and a battery that is fine should not be shouting"
        );
    }

    #[test]
    fn the_charger_shows_in_the_glyph_and_not_only_in_the_number() {
        let charging = corner(Some(Power {
            percent: 45.0,
            charging: true,
        }));
        let not = corner(Some(Power {
            percent: 45.0,
            charging: false,
        }));
        assert_ne!(
            charging, not,
            "plugging in at the same percentage has to change the picture: \
             the number is identical, so the glyph is the whole signal"
        );
    }

    #[test]
    fn a_battery_at_nought_still_draws_its_case() {
        // The case is the frame the fill goes in. An empty battery that drew
        // nothing would read as no battery at all, which is a different fact.
        let empty = corner(Some(Power {
            percent: 0.0,
            charging: false,
        }));
        let none = corner(None);
        assert_ne!(empty, none);
    }

    /// The type the screen holds is the shared one, not a copy of it.
    #[allow(dead_code)]
    fn types_line_up(power: Power) -> Battery {
        power
    }
}
