//! Drawing a bar into a pixel buffer.
//!
//! Software rasterisation into shared memory. A bar redraws when something
//! changes rather than continuously, so it is nowhere near needing the GPU, and
//! staying off it means the bar never competes with the compositor for one.
//!
//! Text is laid out by cosmic-text, which matters more than it sounds: a status
//! line mixes ordinary text with glyphs from an icon font, and getting those
//! from a second family requires real font fallback rather than a single face.

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};
use tiny_skia::{Paint, PixmapMut, Rect, Transform};

use crate::theme::Color;

/// Holds the font machinery, which is expensive to build and cheap to reuse.
pub struct TextRenderer {
    fonts: FontSystem,
    cache: SwashCache,
    families: Vec<String>,
    /// Whether a colour glyph is drained to the colour of the text around it.
    monochrome: bool,
    /// The size to rasterize at, in pixels of the buffer being drawn into --
    /// not the size written in the configuration, which is in logical pixels.
    /// The two differ by the display's scale, and one renderer serves bars on
    /// displays that do not share one.
    size: f32,
}

impl std::fmt::Debug for TextRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextRenderer")
            .field("families", &self.families)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl TextRenderer {
    /// Whether colour glyphs are drained to the text colour. On by default: a
    /// status line is one row of one colour, and a glyph that arrived from an
    /// emoji font because the text font did not have it looks like a mistake
    /// rather than a decision.
    pub fn set_monochrome(&mut self, yes: bool) {
        self.monochrome = yes;
    }

    /// Sets the size to rasterize at, in buffer pixels.
    ///
    /// Everything else a bar draws is multiplied by the display's scale, and
    /// text has to be too, or it comes out the right number of pixels in a
    /// buffer that is larger than it thinks -- which reads as text that shrank.
    pub fn set_size(&mut self, size: f32) {
        self.size = size.max(1.0);
    }

    /// `families` is tried in order, so an icon font listed first supplies the
    /// glyphs it has and everything else falls through to the text font.
    pub fn new(families: &[String], size: f32) -> Self {
        Self {
            fonts: FontSystem::new(),
            cache: SwashCache::new(),
            families: families.to_vec(),
            monochrome: true,
            size,
        }
    }

    fn shape(&mut self, text: &str) -> Buffer {
        // Line height a little over the size, which is what keeps descenders
        // from clipping against the bar's edge.
        let metrics = Metrics::new(self.size, self.size * 1.4);
        let mut buffer = Buffer::new(&mut self.fonts, metrics);
        let attrs = {
            let mut attrs = Attrs::new();
            if let Some(first) = self.families.first() {
                attrs = attrs.family(Family::Name(first));
            }
            attrs
        };
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);
        buffer
    }

    /// How wide a string will be, so a bar can place it before drawing it.
    pub fn width(&mut self, text: &str) -> f32 {
        let buffer = self.shape(text);
        buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0_f32, f32::max)
    }

    /// Draws text with its left edge at `x` and vertically centred in `height`.
    pub fn draw(
        &mut self,
        pixmap: &mut PixmapMut<'_>,
        text: &str,
        x: f32,
        height: f32,
        color: Color,
    ) {
        self.draw_shifted(pixmap, text, x, height, 0.0, color);
    }

    /// Centred in `height` as [`TextRenderer::draw`] does it, then moved `dy`
    /// from there -- negative upwards.
    ///
    /// For text part way through being replaced: one line rising out of the
    /// space while another rises into it. Anything drawn past the edge of the
    /// pixmap is clipped, which is what makes a line able to leave.
    pub fn draw_shifted(
        &mut self,
        pixmap: &mut PixmapMut<'_>,
        text: &str,
        x: f32,
        height: f32,
        dy: f32,
        color: Color,
    ) {
        let buffer = self.shape(text);
        let text_height: f32 = buffer.layout_runs().map(|run| run.line_height).sum();
        self.paint(
            pixmap,
            buffer,
            x,
            ((height - text_height) / 2.0).max(0.0) + dy,
            color,
        );
    }

    /// Draws text with its top-left corner at `x`, `top`.
    ///
    /// What a stack of lines needs, where each one's position is already known
    /// and centring each within its own box would space them unevenly.
    pub fn draw_at(
        &mut self,
        pixmap: &mut PixmapMut<'_>,
        text: &str,
        x: f32,
        top: f32,
        color: Color,
    ) {
        let buffer = self.shape(text);
        self.paint(pixmap, buffer, x, top, color);
    }

    fn paint(
        &mut self,
        pixmap: &mut PixmapMut<'_>,
        buffer: Buffer,
        x: f32,
        top: f32,
        color: Color,
    ) {
        let rgba = color.rgba();
        let fill = cosmic_text::Color::rgba(
            (rgba.red() * 255.0) as u8,
            (rgba.green() * 255.0) as u8,
            (rgba.blue() * 255.0) as u8,
            (rgba.alpha() * 255.0) as u8,
        );
        let monochrome = self.monochrome;
        // cosmic-text throws away the alpha of the colour it is handed -- its
        // own source says `TODO: blend base alpha?` -- and reports coverage
        // alone, so a colour asking to be half transparent arrives fully
        // opaque. Kept here and multiplied in below, because a caller fading
        // text out has no way of knowing that.
        let opacity = rgba.alpha().clamp(0.0, 1.0);

        let mut buffer = buffer;
        buffer.draw(
            &mut self.fonts,
            &mut self.cache,
            fill,
            |px, py, w, h, glyph_color| {
                // cosmic-text hands back coverage as a colour per pixel block;
                // anything fully transparent is outside the glyph.
                if glyph_color.a() == 0 {
                    return;
                }
                let glyph_color = if monochrome {
                    drained(glyph_color, fill)
                } else {
                    glyph_color
                };
                let alpha = (f32::from(glyph_color.a()) * opacity).round() as u8;
                if alpha == 0 {
                    return;
                }
                let mut paint = Paint::default();
                paint.set_color_rgba8(glyph_color.r(), glyph_color.g(), glyph_color.b(), alpha);
                paint.anti_alias = false;
                if let Some(rect) = Rect::from_xywh(
                    x + px as f32,
                    top + py as f32,
                    w.max(1) as f32,
                    h.max(1) as f32,
                ) {
                    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
                }
            },
        );
    }
}

/// A coloured glyph, said in the colour of the text around it.
///
/// Only a colour font ever gets here: an ordinary glyph comes back in exactly
/// the colour it was asked for, and is left alone. A colour one -- an emoji,
/// usually, and often one nobody chose, since a font fallback picks it up for
/// a symbol the text font happens not to have -- arrives in its own palette,
/// which on a status line reads as one word painted green among a row of
/// off-white ones.
///
/// Brightness is kept and colour is not: the glyph's luminance picks how much
/// of the text colour to use, so shape and shading survive and nothing comes
/// out invisible. The floor is what stops a dark emoji vanishing into a dark
/// bar.
fn drained(glyph: cosmic_text::Color, fill: cosmic_text::Color) -> cosmic_text::Color {
    if glyph.r() == fill.r() && glyph.g() == fill.g() && glyph.b() == fill.b() {
        return glyph;
    }
    let luminance = (0.2126 * f32::from(glyph.r())
        + 0.7152 * f32::from(glyph.g())
        + 0.0722 * f32::from(glyph.b()))
        / 255.0;
    let weight = 0.35 + 0.65 * luminance;
    let channel = |value: u8| (f32::from(value) * weight).round().clamp(0.0, 255.0) as u8;
    cosmic_text::Color::rgba(
        channel(fill.r()),
        channel(fill.g()),
        channel(fill.b()),
        glyph.a(),
    )
}

/// Paints a solid rectangle.
pub fn fill(pixmap: &mut PixmapMut<'_>, x: f32, y: f32, w: f32, h: f32, color: Color) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let mut paint = Paint::default();
    paint.set_color(color.rgba());
    paint.anti_alias = false;
    if let Some(rect) = Rect::from_xywh(x, y, w, h) {
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
}

impl TextRenderer {
    /// Lays text out to a width and draws it, returning how tall it came out.
    ///
    /// The bar never needs this -- a status line that wrapped would be a bar
    /// that changed height -- but a notification body is somebody else's prose
    /// and arrives at whatever length it likes.
    pub fn draw_wrapped(
        &mut self,
        pixmap: &mut PixmapMut<'_>,
        text: &str,
        at: (f32, f32),
        within: (f32, usize),
        color: Color,
    ) -> f32 {
        let (x, top) = at;
        let (width, lines) = within;
        let mut buffer = self.shape_wrapped(text, width);
        // Anything past the line budget is dropped rather than shrunk: a
        // notification is a summary, and one that grows to fit an essay pushes
        // every other notification off the screen.
        let kept: Vec<_> = buffer.layout_runs().take(lines).collect();
        let height: f32 = kept.iter().map(|run| run.line_height).sum();
        drop(kept);
        buffer.set_size(Some(width), Some(height.max(1.0)));
        buffer.shape_until_scroll(&mut self.fonts, false);
        self.paint(pixmap, buffer, x, top, color);
        height
    }

    /// How tall wrapped text will be, without drawing it.
    pub fn wrapped_height(&mut self, text: &str, width: f32, lines: usize) -> f32 {
        let buffer = self.shape_wrapped(text, width);
        buffer
            .layout_runs()
            .take(lines)
            .map(|run| run.line_height)
            .sum()
    }

    fn shape_wrapped(&mut self, text: &str, width: f32) -> Buffer {
        let metrics = Metrics::new(self.size, self.size * 1.4);
        let mut buffer = Buffer::new(&mut self.fonts, metrics);
        let attrs = {
            let mut attrs = Attrs::new();
            if let Some(first) = self.families.first() {
                attrs = attrs.family(Family::Name(first));
            }
            attrs
        };
        buffer.set_size(Some(width), None);
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);
        buffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::color;

    /// How much ink a run of text put on a fresh pixmap.
    fn ink(alpha: f32, dy: f32) -> u32 {
        let mut text = TextRenderer::new(&["Noto Sans".to_string()], 14.0);
        let mut pixmap = tiny_skia::Pixmap::new(200, 30).expect("a pixmap");
        text.draw_shifted(
            &mut pixmap.as_mut(),
            "a title",
            4.0,
            30.0,
            dy,
            color("#ffffff").faded(alpha),
        );
        pixmap
            .pixels()
            .iter()
            .map(|pixel| u32::from(pixel.alpha()))
            .sum()
    }

    #[test]
    fn asking_for_faded_text_gets_faded_text() {
        // cosmic-text reports coverage and throws away the alpha of the colour
        // it was handed, so this has to be applied on the way to the pixmap. It
        // was not, once: text asked to be half transparent came out opaque,
        // which made one line crossing another look like one line on top of
        // another.
        let solid = ink(1.0, 0.0);
        let faint = ink(0.35, 0.0);
        assert!(solid > 0, "nothing was drawn at all");
        assert!(
            faint * 2 < solid,
            "faded text was barely fainter: {faint} against {solid}"
        );
        assert_eq!(ink(0.0, 0.0), 0, "fully transparent text drew something");
    }

    #[test]
    fn text_pushed_off_the_top_is_clipped_rather_than_wrapped_round() {
        // What makes a line able to leave: it is drawn where it would not fit
        // and the part outside is simply not there.
        let settled = ink(1.0, 0.0);
        let leaving = ink(1.0, -14.0);
        assert!(
            leaving < settled,
            "a line pushed halfway out should show less of itself: {leaving} against {settled}"
        );
        assert_eq!(ink(1.0, -200.0), 0, "a line pushed right out still drew");
    }
}
