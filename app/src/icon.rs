//! The application icon: a cluster of four F-row keys, drawn in code.
//!
//! Drawn rather than shipped as an image so there is no decoder in the
//! dependency tree, no file to lose, and exactly one definition of what the
//! icon looks like — the build script includes this same file to render the
//! `.ico` embedded in the executable, and the tray and the window render it at
//! runtime. Change it here and every copy follows.
//!
//! Std-only on purpose: the build script compiles it outside the crate.

/// The four keycaps, in the default lane colours — the icon is a lane.
pub const KEY_COLORS: [[u8; 3]; 4] = [
    [80, 170, 255],
    [250, 190, 60],
    [90, 210, 130],
    [225, 110, 200],
];

/// The deck the keys sit on.
const DECK: [u8; 4] = [24, 26, 32, 255];

/// Whether (x, y) lies inside the rectangle (x0..=x1, y0..=y1) with corners
/// rounded by `radius`.
fn inside(x: i32, y: i32, x0: i32, y0: i32, x1: i32, y1: i32, radius: i32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    let dx = (x0 + radius - x).max(x - (x1 - radius)).max(0);
    let dy = (y0 + radius - y).max(y - (y1 - radius)).max(0);
    dx * dx + dy * dy <= radius * radius + radius
}

/// RGBA pixels for the icon at `size` × `size`, transparent outside the deck.
pub fn rgba(size: u32) -> Vec<u8> {
    let s = size as i32;
    let mut pixels = vec![0u8; (size * size * 4) as usize];
    let mut put = |x: i32, y: i32, color: [u8; 4]| {
        let at = ((y * s + x) * 4) as usize;
        pixels[at..at + 4].copy_from_slice(&color);
    };

    // The deck: a dark rounded band, so the keys read on light and dark
    // taskbars alike.
    let deck_top = s * 3 / 16;
    let deck_bottom = s - s * 3 / 16 - 1;
    let deck_radius = (s / 6).max(1);
    for y in 0..s {
        for x in 0..s {
            if inside(x, y, 0, deck_top, s - 1, deck_bottom, deck_radius) {
                put(x, y, DECK);
            }
        }
    }

    // Four keycaps in a row — an F-row cluster, which is also one lane.
    let gap = (s / 16).max(1);
    let key_width = (s - gap * 5) / 4;
    let used = key_width * 4 + gap * 5;
    let left = (s - used) / 2;
    let key_top = deck_top + gap;
    let key_bottom = deck_bottom - gap;
    let key_radius = (s / 12).max(1);
    for (index, color) in KEY_COLORS.iter().enumerate() {
        let x0 = left + gap + index as i32 * (key_width + gap);
        let x1 = x0 + key_width - 1;
        for y in key_top..=key_bottom {
            for x in x0..=x1 {
                if inside(x, y, x0, key_top, x1, key_bottom, key_radius) {
                    put(x, y, [color[0], color[1], color[2], 255]);
                }
            }
        }
    }
    pixels
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn every_size_renders_four_distinct_keys_on_a_deck() {
        for size in [16u32, 24, 32, 48, 64] {
            let pixels = rgba(size);
            assert_eq!(pixels.len(), (size * size * 4) as usize);
            for color in KEY_COLORS {
                let found = pixels
                    .chunks_exact(4)
                    .any(|px| px[0] == color[0] && px[1] == color[1] && px[2] == color[2]);
                assert!(found, "size {size} lost key colour {color:?}");
            }
            // And the corners stay transparent, so it does not render as a
            // solid tile on the taskbar.
            assert_eq!(pixels[3], 0, "size {size} corner is opaque");
        }
    }
}
