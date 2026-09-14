//! Colours and fonts.

use serde::Deserialize;

/// A colour, written the way a stylesheet writes one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color(pub tiny_skia::Color);

impl Color {
    pub fn rgba(&self) -> tiny_skia::Color {
        self.0
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        parse(&text)
            .map(Color)
            .ok_or_else(|| serde::de::Error::custom(format!("{text:?} is not a colour")))
    }
}

/// Parses `#rgb`, `#rrggbb` or `#rrggbbaa`.
fn parse(text: &str) -> Option<tiny_skia::Color> {
    let hex = text.trim().strip_prefix('#')?;
    let channel = |i: usize, width: usize| -> Option<u8> {
        let slice = hex.get(i * width..(i + 1) * width)?;
        let value = u8::from_str_radix(slice, 16).ok()?;
        // A single digit means the nibble is repeated, so #f0a reads as #ff00aa.
        Some(if width == 1 { value * 17 } else { value })
    };
    let (r, g, b, a) = match hex.len() {
        3 => (channel(0, 1)?, channel(1, 1)?, channel(2, 1)?, 255),
        6 => (channel(0, 2)?, channel(1, 2)?, channel(2, 2)?, 255),
        8 => (
            channel(0, 2)?,
            channel(1, 2)?,
            channel(2, 2)?,
            channel(3, 2)?,
        ),
        _ => return None,
    };
    Some(tiny_skia::Color::from_rgba8(r, g, b, a))
}

pub fn color(text: &str) -> Color {
    Color(parse(text).unwrap_or(tiny_skia::Color::BLACK))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_parse_in_each_width() {
        assert_eq!(
            parse("#000000"),
            Some(tiny_skia::Color::from_rgba8(0, 0, 0, 255))
        );
        assert_eq!(parse("#fff"), parse("#ffffff"));
        assert_eq!(
            parse("#25222180"),
            Some(tiny_skia::Color::from_rgba8(0x25, 0x22, 0x21, 0x80))
        );
        assert_eq!(parse("252221"), None);
        assert_eq!(parse("#12345"), None);
    }
}
