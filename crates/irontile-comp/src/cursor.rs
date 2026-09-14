//! The pointer image.
//!
//! On real hardware nothing else draws a cursor, so the compositor has to. This
//! is a built-in arrow rather than a themed one: an unthemed pointer that is
//! always present beats a themed one that is missing whenever a theme cannot be
//! found, and the shape is the part a user needs.
//!
//! The nested backend leaves this alone, because the compositor it runs inside
//! is already drawing a pointer and two would be worse than one.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;

/// Side length of the cursor bitmap, in pixels.
const SIZE: i32 = 24;

/// The classic arrow, as a column per row: how many pixels of the row are
/// filled, starting at the row's own indent. A compact way to write a shape
/// that would otherwise be a wall of hex.
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

/// Builds the pointer image: a white arrow with a black outline, so it stays
/// visible over both light and dark windows.
pub fn arrow() -> MemoryRenderBuffer {
    let mut pixels = vec![0u8; (SIZE * SIZE * 4) as usize];

    let mut filled = vec![vec![false; SIZE as usize]; SIZE as usize];
    for (row, (indent, run)) in ARROW.iter().enumerate() {
        for x in *indent..(*indent + *run) {
            if x < SIZE && (row as i32) < SIZE {
                filled[row][x as usize] = true;
            }
        }
    }

    // The outline is every empty pixel touching a filled one, which gives the
    // arrow a border without needing a second hand-drawn shape.
    for y in 0..SIZE as usize {
        for x in 0..SIZE as usize {
            let (r, g, b, a) = if filled[y][x] {
                (255u8, 255u8, 255u8, 255u8)
            } else if neighbours(&filled, x, y) {
                (0, 0, 0, 255)
            } else {
                (0, 0, 0, 0)
            };
            let i = (y * SIZE as usize + x) * 4;
            // Argb8888 is little-endian in memory: blue, green, red, alpha.
            pixels[i] = b;
            pixels[i + 1] = g;
            pixels[i + 2] = r;
            pixels[i + 3] = a;
        }
    }

    MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Argb8888,
        (SIZE, SIZE),
        1,
        smithay::utils::Transform::Normal,
        // The arrow is mostly transparent, so none of it is opaque.
        None,
    )
}

fn neighbours(filled: &[Vec<bool>], x: usize, y: usize) -> bool {
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

    #[test]
    fn the_arrow_has_a_tip_and_an_outline() {
        let mut filled = vec![vec![false; SIZE as usize]; SIZE as usize];
        for (row, (indent, run)) in ARROW.iter().enumerate() {
            for x in *indent..(*indent + *run) {
                filled[row][x as usize] = true;
            }
        }
        // The tip is a single pixel at the top left, which is what makes a
        // pointer point at something.
        assert!(filled[0][0]);
        assert!(!filled[0][1]);
        // And it widens.
        assert!(filled[5][5]);
        // Every filled pixel has an unfilled neighbour somewhere outside, so
        // the outline pass has something to draw.
        assert!(neighbours(&filled, 1, 0));
    }
}
