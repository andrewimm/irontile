//! The pointer image.
//!
//! Wayland gives the compositor two different jobs here, and they need
//! different machinery:
//!
//! - A client may hand over its own cursor surface with a hotspot, through
//!   `wl_pointer.set_cursor`. No theme is involved; the surface is composited
//!   where the pointer is, offset by the hotspot. That is handled at render
//!   time and does not come through this module.
//! - A client may instead *name* a shape through `wp_cursor_shape_v1`, and then
//!   the compositor has to supply the image. That is what this module is for.
//!
//! Shapes are resolved against an XCursor theme, the format every installed
//! cursor theme on every distribution already uses. A built-in arrow is kept as
//! the last resort so that a machine with no themes installed still has a
//! visible pointer -- which is not hypothetical: it is what the first run on
//! bare hardware looks like.

use std::collections::HashMap;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::input::pointer::CursorIcon;
use smithay::utils::Transform;

/// Side length of the built-in arrow, in pixels.
const BUILTIN_SIZE: i32 = 24;

/// The built-in arrow, as an indent and a run length per row. A compact way to
/// write a shape that would otherwise be a wall of hex.
const ARROW: [(i32, i32); 17] = [
    (0, 1),
    (0, 2),
    (0, 3),
    (0, 4),
    (0, 5),
    (0, 6),
    (0, 7),
    (0, 8),
    (0, 9),
    (0, 10),
    (0, 11),
    (0, 12),
    (0, 9),
    (0, 6),
    (5, 4),
    (6, 4),
    (7, 4),
];

/// A pointer image ready to draw.
#[derive(Clone, Debug)]
pub struct CursorImage {
    pub buffer: MemoryRenderBuffer,
    /// The point under the pointer, in logical pixels.
    pub hotspot: (i32, i32),
    /// Size to draw at, in logical pixels.
    ///
    /// A themed image is loaded at the display's scale, so the renderer must be
    /// told the logical size explicitly or it will scale the image a second
    /// time and the pointer comes out twice as big as it should be.
    pub logical_size: (i32, i32),
    /// The image's own size in pixels. A hardware cursor plane usually demands
    /// one particular size, so this is the first thing to check when a display
    /// refuses to take the cursor onto its own plane.
    pub size: (i32, i32),
}

/// Where pointer images come from.
#[derive(Debug)]
pub struct CursorSource {
    theme: Option<xcursor::CursorTheme>,
    theme_name: String,
    base_size: i32,
    /// Resolved images, keyed by shape and the size actually asked for.
    /// Decoding an XCursor file on every pointer motion would be absurd.
    cache: HashMap<(CursorIcon, i32), Option<CursorImage>>,
    builtin: CursorImage,
}

impl CursorSource {
    /// Loads a theme by name, falling back to the built-in arrow for anything
    /// it cannot supply.
    pub fn new(theme_name: &str, base_size: i32) -> Self {
        let theme = (!theme_name.is_empty()).then(|| xcursor::CursorTheme::load(theme_name));
        Self {
            theme,
            theme_name: theme_name.to_owned(),
            base_size: base_size.max(1),
            cache: HashMap::new(),
            builtin: builtin_arrow(),
        }
    }

    pub fn theme_name(&self) -> &str {
        &self.theme_name
    }

    pub fn base_size(&self) -> i32 {
        self.base_size
    }

    /// The image for a shape at a given display scale.
    pub fn image(&mut self, icon: CursorIcon, scale: f64) -> CursorImage {
        // A cursor is sized in logical pixels but drawn in physical ones, so a
        // display at twice the scale wants twice the image rather than the same
        // one stretched.
        let size = ((f64::from(self.base_size) * scale).round() as i32).max(1);
        if !self.cache.contains_key(&(icon, size)) {
            let loaded = self.load(icon, size);
            self.cache.insert((icon, size), loaded);
        }
        match self.cache.get(&(icon, size)).and_then(Clone::clone) {
            Some(mut image) => {
                // The image was chosen for this display's scale, so undo that
                // scale to get the size it should occupy in logical pixels.
                let logical = |px: i32| ((f64::from(px) / scale).round() as i32).max(1);
                image.logical_size = (logical(image.size.0), logical(image.size.1));
                image.hotspot = (logical(image.hotspot.0), logical(image.hotspot.1));
                image
            }
            // The built-in arrow has one fixed size, so it is simply drawn at
            // its nominal logical size and scaled up like any other image.
            None => self.builtin.clone(),
        }
    }

    fn load(&self, icon: CursorIcon, size: i32) -> Option<CursorImage> {
        let theme = self.theme.as_ref()?;
        // A shape has several historical names and themes vary in which they
        // ship, so every alternative is tried before giving up.
        let path = icon
            .alt_names()
            .iter()
            .chain(std::iter::once(&icon.name()))
            .find_map(|name| theme.load_icon(name))?;
        let bytes = std::fs::read(path).ok()?;
        let images = xcursor::parser::parse_xcursor(&bytes)?;

        // Nearest nominal size, preferring the larger on a tie: scaling a
        // cursor down looks better than scaling one up.
        let image = images.iter().min_by_key(|image| {
            ((image.size as i32) - size).abs() * 2 + i32::from(image.size as i32 > size)
        })?;

        Some(CursorImage {
            buffer: MemoryRenderBuffer::from_slice(
                &image.pixels_rgba,
                Fourcc::Argb8888,
                (image.width as i32, image.height as i32),
                1,
                Transform::Normal,
                None,
            ),
            // Both in image pixels here; converted to logical by the caller,
            // which is the only place the display's scale is known.
            hotspot: (image.xhot as i32, image.yhot as i32),
            logical_size: (image.width as i32, image.height as i32),
            size: (image.width as i32, image.height as i32),
        })
    }
}

/// The arrow drawn when no theme can supply one.
///
/// White with a black outline, so it stays visible over both light and dark
/// windows, and with its tip at the origin so the hotspot is simply (0, 0).
fn builtin_arrow() -> CursorImage {
    let size = BUILTIN_SIZE as usize;
    let mut filled = vec![vec![false; size]; size];
    for (row, (indent, run)) in ARROW.iter().enumerate() {
        for x in *indent..(*indent + *run) {
            if (x as usize) < size && row < size {
                filled[row][x as usize] = true;
            }
        }
    }

    let mut pixels = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            // The outline is every empty pixel touching a filled one, which
            // gives the arrow a border without a second hand-drawn shape.
            let (r, g, b, a) = if filled[y][x] {
                (255u8, 255u8, 255u8, 255u8)
            } else if touches_filled(&filled, x, y) {
                (0, 0, 0, 255)
            } else {
                (0, 0, 0, 0)
            };
            let i = (y * size + x) * 4;
            // Argb8888 is little-endian in memory: blue, green, red, alpha.
            pixels[i] = b;
            pixels[i + 1] = g;
            pixels[i + 2] = r;
            pixels[i + 3] = a;
        }
    }

    CursorImage {
        buffer: MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Argb8888,
            (BUILTIN_SIZE, BUILTIN_SIZE),
            1,
            Transform::Normal,
            None,
        ),
        hotspot: (0, 0),
        logical_size: (BUILTIN_SIZE, BUILTIN_SIZE),
        size: (BUILTIN_SIZE, BUILTIN_SIZE),
    }
}

fn touches_filled(filled: &[Vec<bool>], x: usize, y: usize) -> bool {
    let size = filled.len();
    for dy in -1i32..=1 {
        for dx in -1i32..=1 {
            let (nx, ny) = (x as i32 + dx, y as i32 + dy);
            if nx < 0 || ny < 0 || nx >= size as i32 || ny >= size as i32 {
                continue;
            }
            if filled[ny as usize][nx as usize] {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape() -> Vec<Vec<bool>> {
        let size = BUILTIN_SIZE as usize;
        let mut filled = vec![vec![false; size]; size];
        for (row, (indent, run)) in ARROW.iter().enumerate() {
            for x in *indent..(*indent + *run) {
                filled[row][x as usize] = true;
            }
        }
        filled
    }

    #[test]
    fn the_builtin_arrow_has_a_tip_at_its_hotspot() {
        let filled = shape();
        // The hotspot is (0, 0), so that pixel must be part of the arrow or the
        // pointer would appear offset from what it is pointing at.
        assert!(filled[0][0]);
        assert!(!filled[0][1]);
        assert!(filled[5][5], "and it widens");
        assert_eq!(builtin_arrow().hotspot, (0, 0));
    }

    #[test]
    fn a_missing_theme_still_yields_a_pointer() {
        // A machine with no cursor themes installed must still have a visible
        // pointer; on a session backend nothing else draws one.
        let mut source = CursorSource::new("definitely-not-a-theme", 24);
        let image = source.image(CursorIcon::Default, 1.0);
        assert_eq!(image.hotspot, (0, 0));
    }

    #[test]
    fn an_empty_theme_name_means_the_builtin() {
        let mut source = CursorSource::new("", 24);
        assert_eq!(source.image(CursorIcon::Text, 1.0).hotspot, (0, 0));
    }

    #[test]
    fn a_themed_image_is_drawn_at_its_logical_size() {
        // The image loaded for a scaled display is bigger in pixels but must
        // occupy the same logical space, or the pointer doubles in size on a
        // HiDPI screen.
        let mut source = CursorSource::new("Adwaita", 24);
        let one = source.image(CursorIcon::Default, 1.0);
        let two = source.image(CursorIcon::Default, 2.0);
        if one.size == two.size {
            // No theme installed, or it ships a single size; nothing to compare.
            return;
        }
        assert!(
            two.size.0 > one.size.0,
            "a scaled display wants more pixels"
        );
        let difference = (two.logical_size.0 - one.logical_size.0).abs();
        assert!(
            difference <= 2,
            "logical size should barely move: {one:?} vs {two:?}"
        );
    }

    #[test]
    fn scale_changes_the_size_asked_for() {
        // Cache keys are per size, so a second display at another scale gets
        // its own image rather than a stretched copy of the first.
        let mut source = CursorSource::new("", 24);
        source.image(CursorIcon::Default, 1.0);
        source.image(CursorIcon::Default, 2.0);
        assert_eq!(source.cache.len(), 2);
    }
}
