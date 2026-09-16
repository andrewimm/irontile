//! The mark that says the screen is being copied.
//!
//! A compositor cannot stop a client that can reach its socket from taking a
//! picture of the screen, and on a desktop where nothing is sandboxed it would
//! be pretending to try: anything that can talk to the compositor already runs
//! as the user and can read their files. What it can do is refuse to let it
//! happen quietly. This is drawn by the compositor, above everything -- above a
//! fullscreen window, above the lock screen, above the bar -- so the client
//! doing the copying has no way to cover it, and it appears in the copy it is
//! warning about.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::Transform;

/// How wide the indicator is, in logical pixels.
///
/// Small enough to sit in a corner unremarked, large enough not to be taken
/// for a dead pixel.
pub const SIZE: i32 = 12;

/// How far it sits from the corner of the display, in logical pixels.
pub const INSET: i32 = 10;

/// The colour. Not from the theme, and not configurable.
///
/// Everything else about how irontile looks is the user's to set. An indicator
/// that can be turned off, made transparent, or coloured to match the
/// background is one that says nothing the moment somebody wants it to say
/// nothing, and the someone wanting that is not always the person at the
/// keyboard. This is the one mark on screen that is not decoration.
const COLOUR: (u32, u32, u32) = (0xf2, 0x6e, 0x2e);

/// Holds the drawn circle, rebuilt only when a display's scale changes.
#[derive(Debug, Default)]
pub struct Indicator {
    cached: Option<(i32, MemoryRenderBuffer)>,
}

impl Indicator {
    /// How many pixels across the circle it is holding, if any.
    #[cfg(test)]
    fn drawn_at(&self) -> Option<i32> {
        self.cached.as_ref().map(|(pixels, _)| *pixels)
    }

    /// The circle at this display's scale.
    ///
    /// Drawn at the scale rather than drawn once and resampled, so the rim is
    /// smooth on a fractional display. Displays change scale about never, so
    /// this rasterises approximately once.
    pub fn buffer(&mut self, scale: f64) -> &MemoryRenderBuffer {
        let pixels = ((f64::from(SIZE) * scale).round() as i32).max(4);
        match &self.cached {
            Some((have, _)) if *have == pixels => {}
            _ => self.cached = Some((pixels, draw(pixels))),
        }
        &self.cached.as_ref().expect("just filled").1
    }
}

/// Rasterises a filled circle, premultiplied, at `size` pixels square.
///
/// Rasterised rather than loaded from a file: a compositor that had to find an
/// icon on disk to tell you the screen was being recorded would fail to tell
/// you on the machine where the icon was missing.
fn draw(size: i32) -> MemoryRenderBuffer {
    MemoryRenderBuffer::from_slice(
        &circle(size),
        Fourcc::Argb8888,
        (size, size),
        1,
        Transform::Normal,
        None,
    )
}

/// The circle's pixels, premultiplied, `size` square.
fn circle(size: i32) -> Vec<u8> {
    let side = size as usize;
    let mut pixels = vec![0u8; side * side * 4];

    let centre = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 / 2.0 - 0.5;
    let (r, g, b) = COLOUR;

    for y in 0..side {
        for x in 0..side {
            let dx = x as f32 - centre;
            let dy = y as f32 - centre;
            let distance = (dx * dx + dy * dy).sqrt();
            // A pixel of falloff at the rim, which is all the smoothing a
            // circle this small needs to stop reading as a lozenge.
            let coverage = (radius - distance + 0.5).clamp(0.0, 1.0);
            let alpha = (coverage * 255.0).round() as u32;
            let at = (y * side + x) * 4;
            // Premultiplied and in the order Argb8888 wants the bytes.
            pixels[at] = ((b * alpha) / 255) as u8;
            pixels[at + 1] = ((g * alpha) / 255) as u8;
            pixels[at + 2] = ((r * alpha) / 255) as u8;
            pixels[at + 3] = alpha as u8;
        }
    }

    pixels
}

#[cfg(test)]
mod tests {
    use super::{Indicator, SIZE, circle};

    /// The four bytes at `x`, `y` in a `size`-square image.
    fn pixel(pixels: &[u8], size: i32, x: i32, y: i32) -> [u8; 4] {
        let at = ((y * size + x) * 4) as usize;
        [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
    }

    #[test]
    fn it_is_a_circle_and_not_a_square() {
        // The corners are what tells the two apart, and a square would be
        // taken for a rendering fault rather than for a warning.
        let size = 24;
        let pixels = circle(size);
        let centre = pixel(&pixels, size, size / 2, size / 2);
        assert_eq!(centre[3], 255, "the middle should be solid");
        for (x, y) in [(0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1)] {
            assert_eq!(
                pixel(&pixels, size, x, y)[3],
                0,
                "the corner at {x},{y} should be empty"
            );
        }
    }

    #[test]
    fn the_mark_is_the_colour_it_is_meant_to_be() {
        // Stated rather than assumed: this colour is the whole message, and
        // nothing else on screen is allowed to set it.
        let size = 24;
        let pixels = circle(size);
        let [b, g, r, a] = pixel(&pixels, size, size / 2, size / 2);
        assert_eq!((r, g, b, a), (0xf2, 0x6e, 0x2e, 255));
    }

    #[test]
    fn a_display_scale_makes_it_bigger_rather_than_blurrier() {
        let mut indicator = Indicator::default();
        indicator.buffer(1.0);
        assert_eq!(indicator.drawn_at(), Some(SIZE));
        indicator.buffer(2.0);
        assert_eq!(
            indicator.drawn_at(),
            Some(SIZE * 2),
            "a doubled display should get a circle drawn at twice the size, \
             not one drawn small and stretched"
        );
    }

    #[test]
    fn a_nonsense_scale_cannot_shrink_it_away() {
        // The one mark that is supposed to be noticed must not vanish because
        // a display reported something daft.
        let mut indicator = Indicator::default();
        for scale in [0.0, 0.01, 0.1] {
            indicator.buffer(scale);
            assert!(
                indicator.drawn_at().is_some_and(|pixels| pixels >= 4),
                "a scale of {scale} left nothing to see"
            );
        }
    }
}
