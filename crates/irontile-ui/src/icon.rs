//! Icons, as SVGs rather than as glyphs in a font.
//!
//! An icon font ties a picture to a codepoint in a particular release of a
//! particular font: `` means a processor until the font ships a version
//! where it does not, and then every bar in the world quietly shows the wrong
//! thing. It is also unreadable in a configuration file.
//!
//! Icons here are named, and the names come from the freedesktop icon naming
//! specification -- `battery-level-50-symbolic`, `audio-volume-high-symbolic`
//! -- which is a standard rather than one project's numbering. Any installed
//! icon theme supplies them, they are monochrome so they take the colour of the
//! text beside them, and being vector they are rasterised at whatever size the
//! display actually needs rather than scaled from a fixed one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tiny_skia::{Pixmap, PixmapMut, PixmapPaint, Transform};

use crate::theme::Color;

/// Where icons are found, and what has been drawn already.
pub struct IconSet {
    /// Searched first, so a name can always be overridden locally.
    user: Option<PathBuf>,
    /// Theme directories in inheritance order.
    themes: Vec<PathBuf>,
    /// Built on first use: fifteen thousand files is too many to walk per icon,
    /// and once is quick enough not to notice.
    index: Option<HashMap<String, PathBuf>>,
    /// Rasterised masks, by name and pixel size. Colour is applied afterwards,
    /// so a themed bar does not re-rasterise when a value crosses a threshold.
    cache: HashMap<(String, u32), Option<Pixmap>>,
    /// Scaled tray icons, kept apart because they come as pixels rather than a
    /// name and are keyed by where those pixels live rather than by what they
    /// are called.
    pixmaps: HashMap<(usize, u32), Option<Pixmap>>,
}

impl std::fmt::Debug for IconSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IconSet")
            .field("user", &self.user)
            .field("themes", &self.themes.len())
            .field("indexed", &self.index.as_ref().map(HashMap::len))
            .finish()
    }
}

impl IconSet {
    /// `theme` is an icon theme name such as `Adwaita`; `user` is a directory
    /// of `<name>.svg` files that takes precedence over it.
    pub fn new(theme: &str, user: Option<PathBuf>) -> Self {
        Self {
            user,
            themes: theme_search_path(theme),
            index: None,
            cache: HashMap::new(),
            pixmaps: HashMap::new(),
        }
    }

    /// Draws an icon, tinted to `color`, with its top-left at `x`, `y`.
    /// Returns false when the name could not be found.
    pub fn draw(
        &mut self,
        canvas: &mut PixmapMut<'_>,
        name: &str,
        x: f32,
        y: f32,
        size: u32,
        color: Color,
    ) -> bool {
        let Some(mask) = self.mask(name, size) else {
            return false;
        };
        let tinted = tint(mask, color);
        canvas.draw_pixmap(
            x.round() as i32,
            y.round() as i32,
            tinted.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        true
    }

    /// Draws a tray item's own pixels, scaled to `size`, with its top-left at
    /// `x`, `y`.
    ///
    /// Not tinted: these are an application's own artwork rather than a
    /// symbolic glyph, and recolouring 1Password's key to the bar's foreground
    /// would leave a smudge rather than an icon.
    pub fn draw_pixels(
        &mut self,
        canvas: &mut PixmapMut<'_>,
        pixels: &crate::module::Pixels,
        x: f32,
        y: f32,
        size: u32,
    ) -> bool {
        let Some(scaled) = self.scaled(pixels, size) else {
            return false;
        };
        canvas.draw_pixmap(
            x.round() as i32,
            y.round() as i32,
            scaled.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        true
    }

    fn scaled(&mut self, pixels: &crate::module::Pixels, size: u32) -> Option<&Pixmap> {
        let size = size.max(1);
        // Keyed by where the pixels live: the bytes are shared rather than
        // copied, so an item whose icon has not changed keeps the same address
        // and an item whose icon has changed gets a different one.
        let key = (
            std::sync::Arc::as_ptr(&pixels.argb) as *const u8 as usize,
            size,
        );
        if !self.pixmaps.contains_key(&key) {
            // Bounded: an item that changes its icon constantly would otherwise
            // grow this without limit, and nothing older is worth keeping.
            if self.pixmaps.len() > 64 {
                self.pixmaps.clear();
            }
            self.pixmaps.insert(key, rescale(pixels, size));
        }
        self.pixmaps.get(&key).and_then(Option::as_ref)
    }

    /// Whether a name resolves, so a caller can lay out around a missing icon
    /// rather than leaving a gap where one would have been.
    pub fn has(&mut self, name: &str, size: u32) -> bool {
        self.mask(name, size).is_some()
    }

    fn mask(&mut self, name: &str, size: u32) -> Option<&Pixmap> {
        let size = size.max(1);
        let key = (name.to_owned(), size);
        if !self.cache.contains_key(&key) {
            let rendered = self.path(name).and_then(|path| rasterize(&path, size));
            self.cache.insert(key.clone(), rendered);
        }
        self.cache.get(&key).and_then(Option::as_ref)
    }

    fn path(&mut self, name: &str) -> Option<PathBuf> {
        if let Some(dir) = &self.user {
            let direct = dir.join(format!("{name}.svg"));
            if direct.is_file() {
                return Some(direct);
            }
        }
        if self.index.is_none() {
            self.index = Some(build_index(&self.themes));
        }
        self.index.as_ref()?.get(name).cloned()
    }
}

/// The theme itself, then whatever it inherits, then the themes that are always
/// there, so a name resolves even against a partial theme.
fn theme_search_path(theme: &str) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut wanted = vec![theme.to_owned()];
    let mut seen = Vec::new();

    while let Some(name) = wanted.pop() {
        if name.is_empty() || seen.contains(&name) {
            continue;
        }
        seen.push(name.clone());
        for base in icon_bases() {
            let dir = base.join(&name);
            if !dir.is_dir() {
                continue;
            }
            if let Some(parents) = inherits(&dir.join("index.theme")) {
                wanted.extend(parents);
            }
            roots.push(dir);
        }
    }
    for fallback in ["Adwaita", "hicolor"] {
        for base in icon_bases() {
            let dir = base.join(fallback);
            if dir.is_dir() && !roots.contains(&dir) {
                roots.push(dir);
            }
        }
    }
    roots
}

fn icon_bases() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        bases.push(PathBuf::from(&home).join(".local/share/icons"));
        bases.push(PathBuf::from(home).join(".icons"));
    }
    bases.push(PathBuf::from("/usr/share/icons"));
    bases.push(PathBuf::from("/usr/local/share/icons"));
    bases
}

fn inherits(index: &Path) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(index).ok()?;
    for line in text.lines() {
        if let Some(value) = line.trim().strip_prefix("Inherits=") {
            return Some(value.split(',').map(|s| s.trim().to_owned()).collect());
        }
    }
    None
}

/// Maps every `<name>.svg` under the search path to its file.
///
/// The first theme to supply a name wins, which is what makes the search path
/// an order of preference rather than a set.
fn build_index(roots: &[PathBuf]) -> HashMap<String, PathBuf> {
    let mut index = HashMap::new();
    for root in roots {
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "svg")
                    && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                {
                    index.entry(stem.to_owned()).or_insert(path);
                }
            }
        }
    }
    index
}

/// Rasterises an SVG to a square of `size` pixels.
fn rasterize(path: &Path, size: u32) -> Option<Pixmap> {
    let data = std::fs::read(path).ok()?;
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default()).ok()?;
    let mut pixmap = Pixmap::new(size, size)?;

    // Fit the drawing into the square without distorting it, which matters
    // because plenty of icons are not square.
    let drawn = tree.size();
    let scale = (size as f32 / drawn.width()).min(size as f32 / drawn.height());
    let dx = (size as f32 - drawn.width() * scale) / 2.0;
    let dy = (size as f32 - drawn.height() * scale) / 2.0;
    let transform = Transform::from_translate(dx, dy).pre_scale(scale, scale);

    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(pixmap)
}

/// Replaces an icon's colour while keeping its shape.
///
/// Symbolic icons are a single flat colour, so the coverage is entirely in the
/// alpha channel and recolouring is just writing new colour behind it. That is
/// what lets one icon set follow the bar's palette, and what an icon font
/// cannot do without a second font.
fn tint(mask: &Pixmap, color: Color) -> Pixmap {
    let rgba = color.rgba();
    let mut out = mask.clone();
    for pixel in out.pixels_mut() {
        let alpha = pixel.alpha();
        if alpha == 0 {
            continue;
        }
        // Premultiplied, so each channel is scaled by the coverage already
        // present rather than replacing it.
        let a = alpha as f32 / 255.0;
        *pixel = tiny_skia::PremultipliedColorU8::from_rgba(
            (rgba.red() * a * 255.0).round() as u8,
            (rgba.green() * a * 255.0).round() as u8,
            (rgba.blue() * a * 255.0).round() as u8,
            alpha,
        )
        .unwrap_or(*pixel);
    }
    out
}

/// Turns a tray item's ARGB32 into a square pixmap of the size asked for.
///
/// The specification says most significant byte first, which is A, R, G, B in
/// memory; tiny-skia wants premultiplied RGBA. Applications do send
/// unpremultiplied colour here, so it is multiplied on the way in.
fn rescale(pixels: &crate::module::Pixels, size: u32) -> Option<Pixmap> {
    let (w, h) = (pixels.width, pixels.height);
    if w == 0 || h == 0 || pixels.argb.len() < (w as usize) * (h as usize) * 4 {
        return None;
    }
    let mut source = Pixmap::new(w, h)?;
    for (out, chunk) in source
        .pixels_mut()
        .iter_mut()
        .zip(pixels.argb.chunks_exact(4))
    {
        let (a, r, g, b) = (chunk[0], chunk[1], chunk[2], chunk[3]);
        *out = tiny_skia::ColorU8::from_rgba(r, g, b, a).premultiply();
    }

    let mut out = Pixmap::new(size, size)?;
    let scale = size as f32 / w.max(h) as f32;
    // Centred, so an icon that is not square keeps its proportions rather than
    // being stretched to fill the square the bar lays out for it.
    let left = (size as f32 - w as f32 * scale) / 2.0;
    let top = (size as f32 - h as f32 * scale) / 2.0;
    out.draw_pixmap(
        0,
        0,
        source.as_ref(),
        &PixmapPaint {
            quality: tiny_skia::FilterQuality::Bilinear,
            ..Default::default()
        },
        Transform::from_translate(left, top).pre_scale(scale, scale),
        None,
    );
    Some(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_tray_pixmap_is_squared_off_and_premultiplied() {
        // The specification says ARGB32, most significant byte first, and does
        // not say premultiplied -- so it arrives straight and tiny-skia wants
        // it multiplied. A half-transparent red that came out full strength
        // would look like a different colour rather than a lighter one.
        let pixels = crate::module::Pixels {
            width: 2,
            height: 1,
            argb: vec![
                0x80, 0xff, 0x00, 0x00, // half-transparent red
                0xff, 0x00, 0x00, 0xff, // opaque blue
            ]
            .into(),
        };
        let scaled = super::rescale(&pixels, 8).expect("a pixmap");
        assert_eq!(scaled.width(), 8);
        assert_eq!(scaled.height(), 8, "square, whatever shape it arrived in");

        // Somewhere in the left half, which is the red one.
        let left = scaled.pixel(1, 4).expect("a pixel");
        assert!(
            left.red() <= left.alpha(),
            "premultiplied: no channel may exceed the alpha ({left:?})"
        );
    }

    #[test]
    fn a_pixmap_shorter_than_it_claims_is_refused() {
        // An application that lies about its size would otherwise read past the
        // end of what it sent.
        let pixels = crate::module::Pixels {
            width: 64,
            height: 64,
            argb: vec![0u8; 16].into(),
        };
        assert!(super::rescale(&pixels, 8).is_none());
    }

    use super::*;

    #[test]
    fn the_search_path_always_ends_somewhere_real() {
        // Even a theme that does not exist must resolve against the ones that
        // are always installed, or a typo would mean no icons at all.
        let path = theme_search_path("definitely-not-a-theme");
        assert!(
            path.iter()
                .any(|p| p.ends_with("Adwaita") || p.ends_with("hicolor")),
            "{path:?}"
        );
    }

    #[test]
    fn a_standard_name_resolves() {
        let mut icons = IconSet::new("Adwaita", None);
        // A freedesktop name, not a codepoint in somebody's font release.
        if icons.path("battery-level-50-symbolic").is_none() {
            // No icon theme installed; nothing to assert against.
            return;
        }
        assert!(icons.has("battery-level-50-symbolic", 16));
        assert!(!icons.has("no-such-icon-anywhere", 16));
    }

    #[test]
    fn an_icon_is_rasterised_at_the_size_asked_for() {
        let mut icons = IconSet::new("Adwaita", None);
        let Some(mask) = icons.mask("battery-level-50-symbolic", 32).cloned() else {
            return;
        };
        // Vector, so any size is exact rather than scaled from a fixed one.
        assert_eq!((mask.width(), mask.height()), (32, 32));
        assert!(
            mask.pixels().iter().any(|p| p.alpha() > 0),
            "something should have been drawn"
        );
    }

    #[test]
    fn tinting_keeps_the_shape_and_changes_the_colour() {
        let mut icons = IconSet::new("Adwaita", None);
        let Some(mask) = icons.mask("battery-level-50-symbolic", 32).cloned() else {
            return;
        };
        let tinted = tint(&mask, crate::theme::color("#ff0000"));
        for (before, after) in mask.pixels().iter().zip(tinted.pixels()) {
            assert_eq!(before.alpha(), after.alpha(), "coverage must not change");
        }
        assert!(tinted.pixels().iter().any(|p| p.red() > 0));
    }
}
