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
        let buffer = self.shape(text);
        let text_height: f32 = buffer.layout_runs().map(|run| run.line_height).sum();
        self.paint(
            pixmap,
            buffer,
            x,
            ((height - text_height) / 2.0).max(0.0),
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
                let mut paint = Paint::default();
                paint.set_color_rgba8(
                    glyph_color.r(),
                    glyph_color.g(),
                    glyph_color.b(),
                    glyph_color.a(),
                );
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
