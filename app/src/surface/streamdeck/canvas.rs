//! What one Stream Deck key looks like: one colour, with a label over it.
//!
//! Pure pixels and no device. The deck reads like the keyboard — a row is a
//! lane, as many keys wide as the deck has columns, like the F-row — so a key
//! is one colour from the palette, as an LED would be. What an LED cannot do
//! is the label: the lane's name on the first key, its state and how long on
//! the last — in Waiting, how long over its state — and in Waiting an up, a
//! down and an Enter on the three between.
//! The words are ink, not light, and the triangles are drawn, not typeset.
//!
//! The font is Segoe UI Bold, read from the Windows font folder: every
//! Windows carries it, so nothing is shipped, and a bold face is what a
//! small LCD wants — the window's light Ubuntu, tried first, was hard to
//! read at arm's length. Where the file is not there, the window's own
//! Ubuntu from egui's defaults stands in; where that is missing too — a
//! build of egui with no fonts, not a machine — a key is its colour alone,
//! never a panic.

use std::sync::OnceLock;

use ab_glyph::{Font, FontArc, PxScale, ScaleFont, point};

use crate::settings::Rgb;
use crate::surface::palette;

/// The face on the keys, as the Windows font folder names it.
pub const FONT: &str = "segoeuib.ttf";

/// The stand-in, by egui's name for it: the window's own face.
pub const FALLBACK_FONT: &str = "Ubuntu-Light";

/// Ink on a black key: white. On every other lit key, that much darker.
const INK: [u8; 3] = [255, 255, 255];

/// Ink on a dark key — an empty or idle lane. A label, not a light: grey is
/// neither a lane's colour nor red, and dim enough not to take the eye.
const QUIET_INK: [u8; 3] = [120, 120, 120];

/// How much of the key a triangle takes, each way: about the height of the
/// text's capitals, so an arrow and a word sit as one size.
const TRIANGLE: f32 = 0.20;

/// Packed RGB, row-major, top-left first — what the deck's encoder takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
}

impl Canvas {
    fn filled(width: usize, height: usize, colour: Rgb) -> Self {
        let mut rgb = Vec::with_capacity(width * height * 3);
        for _ in 0..width * height {
            rgb.extend_from_slice(&[colour.r, colour.g, colour.b]);
        }
        Self { width, height, rgb }
    }

    pub fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
        let at = (y * self.width + x) * 3;
        match self.rgb.get(at..at + 3) {
            Some(px) => [px[0], px[1], px[2]],
            None => [0, 0, 0],
        }
    }

    /// Lays `ink` over the pixel at `coverage`, 0.0 leaving it and 1.0
    /// replacing it. Off the canvas is ignored, not an error.
    fn blend(&mut self, x: usize, y: usize, ink: [u8; 3], coverage: f32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let c = coverage.clamp(0.0, 1.0);
        let at = (y * self.width + x) * 3;
        if let Some(px) = self.rgb.get_mut(at..at + 3) {
            for (channel, over) in px.iter_mut().zip(ink) {
                *channel = (f32::from(*channel) * (1.0 - c) + f32::from(over) * c).round() as u8;
            }
        }
    }
}

/// What a key says, if anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Label {
    None,
    /// The lane's name, on its first key.
    Name(String),
    /// The state word over how long, on its last key. An empty `elapsed`
    /// draws nothing on its line — a preview has no clock.
    Status {
        state: &'static str,
        elapsed: String,
    },
    /// A Waiting lane's last key: how long, as the headline, under the word
    /// as its caption. Once the colour and the answer keys have said the
    /// word, the number is the news.
    Wait {
        state: &'static str,
        held: String,
    },
    Up,
    Down,
    Enter,
    /// A number the lane can show — context used, a limit used — as its
    /// short name over the value, or over a dash while it is unknown.
    Gauge {
        name: &'static str,
        value: Option<u8>,
    },
}

/// How the label is drawn: quietly on a key that is meant to be dark, and
/// otherwise in the ink that reads on the key — decided for the lane, not
/// the instant. A lane's keys only ever run between its glow and its full
/// colour, so the ink is chosen at both ends: if the same ink reads on
/// both, that is the ink, steady; if the full colour wants black where the
/// glow wants white, the ink fades between the two along the same ramp the
/// key is on. No shadow, and never a flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    /// Grey — for a key that is dark on purpose: an empty or idle lane.
    Quiet,
    /// On a lit key: how far from white (0) towards black (255).
    Lit(u8),
}

impl Ink {
    /// White, for a dark key.
    pub const BRIGHT: Self = Self::Lit(0);
    /// Black, for a light key.
    pub const DARK: Self = Self::Lit(255);

    /// The ink for a key showing `colour` on a lane whose colour is `lane`.
    pub fn on(colour: Rgb, lane: Rgb) -> Self {
        let low = palette::base(lane);
        let (at_low, at_full) = (reads_on(low), reads_on(lane));
        if at_low == at_full {
            return Self::Lit(at_low);
        }
        // Where the key is between the glow and the full colour, by
        // brightness — the ramp the palette dims along.
        let (from, to) = (f32::from(brightness(low)), f32::from(brightness(lane)));
        let t = if to > from {
            ((f32::from(brightness(colour)) - from) / (to - from)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let darkness = f32::from(at_low) + (f32::from(at_full) - f32::from(at_low)) * t;
        Self::Lit(darkness.round() as u8)
    }
}

/// Which of black (255) and white (0) reads on `colour`: black above the
/// luminance where the two contrast equally against a background — 0.179
/// by the WCAG arithmetic, the one number in here that is not taste.
fn reads_on(colour: Rgb) -> u8 {
    if luminance(colour) > 0.179 { 255 } else { 0 }
}

/// Relative luminance, 0.0 black to 1.0 white, from sRGB.
fn luminance(colour: Rgb) -> f32 {
    let linear = |value: u8| {
        let c = f32::from(value) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(colour.r) + 0.7152 * linear(colour.g) + 0.0722 * linear(colour.b)
}

/// How bright a colour looks, 0 black to 255 white: the usual weights on
/// the channels as the key is sent them — the ramp the palette dims along,
/// so the ink and the key move together.
fn brightness(colour: Rgb) -> u8 {
    (0.2126 * f32::from(colour.r) + 0.7152 * f32::from(colour.g) + 0.0722 * f32::from(colour.b))
        .round() as u8
}

#[derive(Clone, Copy)]
struct Paint {
    colour: [u8; 3],
}

impl Paint {
    fn of(ink: Ink) -> Self {
        match ink {
            Ink::Quiet => Paint { colour: QUIET_INK },
            Ink::Lit(darkness) => Paint {
                colour: INK.map(|channel| channel - darkness),
            },
        }
    }
}

/// A key with nothing on it.
pub fn blank(size: (usize, usize)) -> Canvas {
    Canvas::filled(size.0, size.1, palette::OFF)
}

/// One key: `colour` edge to edge, the label over it. Deterministic — the
/// same inputs are the same bytes, which is what lets a surface skip a key
/// whose inputs have not changed.
pub fn render_key(size: (usize, usize), colour: Rgb, label: &Label, ink: Ink) -> Canvas {
    let mut canvas = Canvas::filled(size.0, size.1, colour);
    let paint = Paint::of(ink);
    // Sized for a 72 px key and scaled with the height for the larger ones.
    let height = canvas.height as f32;
    let scale = height / 72.0;
    let rows = |from: f32, to: f32| {
        (
            (height * from).round() as usize,
            (height * to).round() as usize,
        )
    };
    match label {
        Label::None => {}
        Label::Name(name) => {
            if let Some(font) = font() {
                draw_name(
                    &mut canvas,
                    font,
                    name,
                    rows(0.08, 0.92),
                    20.0 * scale,
                    paint,
                );
            }
        }
        Label::Status { state, elapsed } => {
            if let Some(font) = font() {
                let mut lines = vec![(*state).to_owned()];
                if !elapsed.trim().is_empty() {
                    lines.push(elapsed.clone());
                }
                draw_lines(
                    &mut canvas,
                    font,
                    &lines,
                    rows(0.08, 0.92),
                    20.0 * scale,
                    paint,
                );
            }
        }
        Label::Wait { state, held } => {
            if let Some(font) = font() {
                draw_stack(
                    &mut canvas,
                    font,
                    &[((*state).to_owned(), CAPTION), (held.clone(), 1.0)],
                    rows(0.08, 0.92),
                    HEADLINE * scale,
                    paint,
                );
            }
        }
        Label::Up => draw_triangle(&mut canvas, true, paint),
        Label::Down => draw_triangle(&mut canvas, false, paint),
        Label::Enter => {
            if let Some(font) = font() {
                draw_lines(
                    &mut canvas,
                    font,
                    &["Enter".to_owned()],
                    rows(0.08, 0.92),
                    20.0 * scale,
                    paint,
                );
            }
        }
        Label::Gauge { name, value } => {
            if let Some(font) = font() {
                let shown = match value {
                    Some(value) => format!("{value}%"),
                    None => "—".to_owned(),
                };
                draw_lines(
                    &mut canvas,
                    font,
                    &[(*name).to_owned(), shown],
                    rows(0.08, 0.92),
                    20.0 * scale,
                    paint,
                );
            }
        }
    }
    canvas
}

/// The keys' font, parsed once: Segoe UI Bold from the system, else the
/// window's Ubuntu. `None` only when neither is there.
fn font() -> Option<&'static FontArc> {
    static FONT_DATA: OnceLock<Option<FontArc>> = OnceLock::new();
    FONT_DATA
        .get_or_init(|| system_font().or_else(embedded_font))
        .as_ref()
}

fn system_font() -> Option<FontArc> {
    let windir = std::env::var_os("WINDIR")?;
    let path = std::path::Path::new(&windir).join("Fonts").join(FONT);
    let bytes = std::fs::read(path).ok()?;
    FontArc::try_from_vec(bytes).ok()
}

fn embedded_font() -> Option<FontArc> {
    let definitions = eframe::egui::FontDefinitions::default();
    let data = definitions.font_data.get(FALLBACK_FONT)?;
    FontArc::try_from_vec(data.font.to_vec()).ok()
}

/// Where a triangle sits: `(x0, y0, width, height)` of its box, centred.
fn triangle_box(size: (usize, usize)) -> (usize, usize, usize, usize) {
    let width = ((size.0 as f32) * TRIANGLE).round().max(3.0) as usize;
    let height = ((size.1 as f32) * TRIANGLE).round().max(3.0) as usize;
    ((size.0 - width) / 2, (size.1 - height) / 2, width, height)
}

/// A filled triangle pointing up or down, in the middle of the key. Shapes
/// rather than glyphs: the font need not carry an arrow, and a triangle
/// reads at any size.
fn draw_triangle(canvas: &mut Canvas, up: bool, paint: Paint) {
    let (x0, y0, width, height) = triangle_box((canvas.width, canvas.height));
    let centre = x0 as f32 + (width as f32 - 1.0) / 2.0;
    let half_base = (width as f32 - 1.0) / 2.0;
    let fill = |canvas: &mut Canvas, colour: [u8; 3], dx: usize, dy: usize| {
        for row in 0..height {
            // 0 at the apex, 1 at the base.
            let t = if height > 1 {
                row as f32 / (height - 1) as f32
            } else {
                1.0
            };
            let t = if up { t } else { 1.0 - t };
            let half = (half_base * t).round();
            let from = (centre - half).round().max(0.0) as usize;
            let to = (centre + half).round() as usize;
            for x in from..=to {
                canvas.blend(x + dx, y0 + row + dy, colour, 1.0);
            }
        }
    };
    fill(canvas, paint.colour, 0, 0);
}

/// The words of a name, for the deck's eyes only: a project is called
/// `agent-frow` or `ai.agent.keeb`, and on a key an inch wide those are
/// better read as words, one per line, than as one long word cut short.
/// Dots and dashes break like spaces; nothing else is touched.
pub fn name_words(name: &str) -> Vec<String> {
    name.split(|c: char| c.is_whitespace() || c == '.' || c == '-')
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The most lines a name is spread over.
const NAME_LINES: usize = 3;

/// `name`'s words wrapped greedily into lines that fit `room` at `scale`, or
/// `None` when a single word does not.
fn wrap(font: &FontArc, words: &[String], room: f32, scale: f32) -> Option<Vec<String>> {
    let mut lines: Vec<String> = Vec::new();
    for word in words {
        match lines.last_mut() {
            Some(line) if measure(font, scale, &format!("{line} {word}")) <= room => {
                line.push(' ');
                line.push_str(word);
            }
            _ => {
                if measure(font, scale, word) > room {
                    return None;
                }
                lines.push(word.clone());
            }
        }
    }
    Some(lines)
}

/// The largest size, from `px` down to a floor, at which every one of
/// `lines` fits `room` wide and all of them fit `height` tall. At the floor,
/// whatever still does not fit is cut when drawn, as a single line is.
fn fit_lines(font: &FontArc, lines: &[String], room: f32, height: f32, px: f32) -> f32 {
    let floor = (px * 0.45).max(7.0);
    let mut scale = px;
    while scale > floor {
        let scaled = font.as_scaled(PxScale::from(scale));
        let line_height = scaled.ascent() - scaled.descent();
        let wide = lines.iter().any(|line| measure(font, scale, line) > room);
        if !wide && line_height * lines.len() as f32 <= height {
            return scale;
        }
        scale -= 1.0;
    }
    floor
}

/// The largest size at which `name` fits `room` wide and `height` tall on at
/// most [`NAME_LINES`] lines, and the lines it makes: fewer, larger lines
/// win, and a word that fits no line at the floor is cut like a single
/// line would be. `None` when there are no words.
fn name_layout(
    font: &FontArc,
    name: &str,
    room: f32,
    height: f32,
    px: f32,
) -> Option<(f32, Vec<String>)> {
    let words = name_words(name);
    if words.is_empty() {
        return None;
    }
    let floor = (px * 0.45).max(7.0);
    let mut scale = px;
    while scale >= floor {
        if let Some(lines) = wrap(font, &words, room, scale)
            && lines.len() <= NAME_LINES
        {
            let scaled = font.as_scaled(PxScale::from(scale));
            let line_height = scaled.ascent() - scaled.descent();
            if line_height * lines.len() as f32 <= height {
                return Some((scale, lines));
            }
        }
        scale -= 1.0;
    }
    // Nothing fits whole: one line at the floor, cut, as any long word is.
    Some((floor, vec![words.join(" ")]))
}

/// A name over up to [`NAME_LINES`] lines, as large as the key allows, the
/// block centred in `band`.
fn draw_name(
    canvas: &mut Canvas,
    font: &FontArc,
    name: &str,
    band: (usize, usize),
    px: f32,
    paint: Paint,
) {
    let (room, height) = room_in(canvas, band);
    let Some((scale, lines)) = name_layout(font, name, room, height, px) else {
        return;
    };
    draw_block(canvas, font, &lines, band, scale, paint);
}

/// Fixed lines — a state word over a time — as large as the key allows, the
/// same rule the name is set by, so the two read as one size.
fn draw_lines(
    canvas: &mut Canvas,
    font: &FontArc,
    lines: &[String],
    band: (usize, usize),
    px: f32,
    paint: Paint,
) {
    let (room, height) = room_in(canvas, band);
    let scale = fit_lines(font, lines, room, height, px);
    draw_block(canvas, font, lines, band, scale, paint);
}

/// The headline's size on a 72 px key — larger than a state word, since it
/// is one short number — and the caption's size as a share of it.
const HEADLINE: f32 = 26.0;
const CAPTION: f32 = 0.6;

/// A line's height at `scale`: ascent to descent.
fn line_height(font: &FontArc, scale: f32) -> f32 {
    let scaled = font.as_scaled(PxScale::from(scale));
    scaled.ascent() - scaled.descent()
}

/// The largest size, from `px` down to a floor, at which every line of a
/// stack — each `(text, share)` set at its share of the size — fits `room`
/// wide, and all of them together fit `height` tall. At the floor, whatever
/// still does not fit is cut when drawn, as a single line is.
fn fit_stack(font: &FontArc, lines: &[(String, f32)], room: f32, height: f32, px: f32) -> f32 {
    let floor = (px * 0.45).max(7.0);
    let mut scale = px;
    while scale > floor {
        let wide = lines
            .iter()
            .any(|(line, share)| measure(font, scale * share, line) > room);
        let tall: f32 = lines
            .iter()
            .map(|(_, share)| line_height(font, scale * share))
            .sum();
        if !wide && tall <= height {
            return scale;
        }
        scale -= 1.0;
    }
    floor
}

/// Lines at their own shares of one size — a caption over a headline — as
/// large as the key allows, one under the other, the block centred in
/// `band`. Each line is drawn in its own band, so it is centred in its own
/// height rather than the block's.
fn draw_stack(
    canvas: &mut Canvas,
    font: &FontArc,
    lines: &[(String, f32)],
    band: (usize, usize),
    px: f32,
    paint: Paint,
) {
    let (room, height) = room_in(canvas, band);
    let scale = fit_stack(font, lines, room, height, px);
    let block: f32 = lines
        .iter()
        .map(|(_, share)| line_height(font, scale * share))
        .sum();
    let mut top = band.0 as f32 + ((height - block) / 2.0).max(0.0);
    for (line, share) in lines {
        let size = scale * share;
        let bottom = top + line_height(font, size);
        let rows = (top.round() as usize, (bottom.round() as usize).min(band.1));
        draw_text(canvas, font, line, rows, size, paint);
        top = bottom;
    }
}

/// How wide and tall the text may be inside `band`.
fn room_in(canvas: &Canvas, band: (usize, usize)) -> (f32, f32) {
    let margin = (canvas.width as f32 * 0.06).max(2.0);
    (
        canvas.width as f32 - 2.0 * margin,
        band.1.saturating_sub(band.0) as f32,
    )
}

/// `lines` at `scale`, one under the other, the block centred in `band`.
fn draw_block(
    canvas: &mut Canvas,
    font: &FontArc,
    lines: &[String],
    band: (usize, usize),
    scale: f32,
    paint: Paint,
) {
    let height = band.1.saturating_sub(band.0) as f32;
    let scaled = font.as_scaled(PxScale::from(scale));
    let line_height = scaled.ascent() - scaled.descent();
    let block = line_height * lines.len() as f32;
    let top = band.0 as f32 + ((height - block) / 2.0).max(0.0);
    for (index, line) in lines.iter().enumerate() {
        let y0 = (top + line_height * index as f32).round() as usize;
        let y1 = (top + line_height * (index + 1) as f32).round() as usize;
        draw_text(canvas, font, line, (y0, y1.min(band.1)), scale, paint);
    }
}

/// One line, centred in the rows `band`, at `px` or as much smaller as it
/// takes to fit — down to a floor, past which the end is cut rather than
/// spilled. Nothing is drawn outside the band.
fn draw_text(
    canvas: &mut Canvas,
    font: &FontArc,
    text: &str,
    band: (usize, usize),
    px: f32,
    paint: Paint,
) {
    let text = text.trim();
    if text.is_empty() || band.1 <= band.0 {
        return;
    }
    let margin = (canvas.width as f32 * 0.06).max(2.0);
    let room = canvas.width as f32 - 2.0 * margin;
    let floor = (px * 0.6).max(7.0);
    let mut scale = px;
    let mut shown = text.to_owned();
    while measure(font, scale, &shown) > room && scale - 1.0 >= floor {
        scale -= 1.0;
    }
    while measure(font, scale, &shown) > room && shown.chars().count() > 1 {
        shown.pop();
    }
    let scaled = font.as_scaled(PxScale::from(scale));
    let width = measure(font, scale, &shown);
    let x = (canvas.width as f32 - width) / 2.0;
    let text_height = scaled.ascent() - scaled.descent();
    let baseline = band.0 as f32 + ((band.1 - band.0) as f32 - text_height) / 2.0 + scaled.ascent();
    draw_run(canvas, font, scale, &shown, x, baseline, band, paint.colour);
}

/// How wide `text` is at `scale`, advances and kerning included.
fn measure(font: &FontArc, scale: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(scale));
    let mut width = 0.0;
    let mut previous = None;
    for ch in text.chars() {
        let id = font.glyph_id(ch);
        if let Some(before) = previous {
            width += scaled.kern(before, id);
        }
        width += scaled.h_advance(id);
        previous = Some(id);
    }
    width
}

#[allow(clippy::too_many_arguments)]
fn draw_run(
    canvas: &mut Canvas,
    font: &FontArc,
    scale: f32,
    text: &str,
    x: f32,
    baseline: f32,
    band: (usize, usize),
    colour: [u8; 3],
) {
    let scaled = font.as_scaled(PxScale::from(scale));
    let mut cursor = x;
    let mut previous = None;
    for ch in text.chars() {
        let id = font.glyph_id(ch);
        if let Some(before) = previous {
            cursor += scaled.kern(before, id);
        }
        let glyph = id.with_scale_and_position(PxScale::from(scale), point(cursor, baseline));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                if px < 0 || py < 0 {
                    return;
                }
                let py = py as usize;
                if py < band.0 || py >= band.1 {
                    return;
                }
                canvas.blend(px as usize, py, colour, coverage);
            });
        }
        cursor += scaled.h_advance(id);
        previous = Some(id);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const RED: Rgb = Rgb::new(200, 0, 0);
    const SIZE: (usize, usize) = (72, 72);

    fn status() -> Label {
        Label::Status {
            state: "Waiting",
            elapsed: "1m 20s".to_owned(),
        }
    }

    fn is(px: [u8; 3], colour: Rgb) -> bool {
        px == [colour.r, colour.g, colour.b]
    }

    #[test]
    fn a_key_is_exactly_the_size_asked_for() {
        for size in [(72, 72), (80, 80), (96, 96), (120, 120)] {
            let key = render_key(
                size,
                RED,
                &Label::Name("agent-frow".to_owned()),
                Ink::BRIGHT,
            );
            assert_eq!((key.width, key.height), size);
            assert_eq!(key.rgb.len(), size.0 * size.1 * 3);
        }
    }

    #[test]
    fn rendering_is_deterministic() {
        let once = render_key(SIZE, RED, &status(), Ink::BRIGHT);
        let twice = render_key(SIZE, RED, &status(), Ink::BRIGHT);
        assert_eq!(once, twice);
    }

    #[test]
    fn a_blank_key_is_black() {
        let key = blank(SIZE);
        assert!(key.rgb.iter().all(|&channel| channel == 0));
    }

    #[test]
    fn a_font_loads() {
        assert!(
            font().is_some(),
            "{FONT} on Windows, else egui's {FALLBACK_FONT}"
        );
    }

    #[test]
    fn a_key_is_one_colour_under_its_words() {
        let key = render_key(SIZE, RED, &status(), Ink::BRIGHT);
        for y in 0..72 {
            assert!(is(key.pixel(0, y), RED), "left edge, row {y}");
            assert!(is(key.pixel(71, y), RED), "right edge, row {y}");
        }
        for x in 0..72 {
            for y in [0, 1, 2, 69, 70, 71] {
                assert!(is(key.pixel(x, y), RED), "row {y} column {x}");
            }
        }
    }

    #[test]
    fn an_up_triangle_points_up() {
        let (x0, y0, width, height) = triangle_box(SIZE);
        // The apex pixel: the middle column, which for an even width is the
        // right-hand one of the two, as the drawing rounds.
        let apex_x = x0 + width / 2;
        let up = render_key(SIZE, RED, &Label::Up, Ink::BRIGHT);
        assert_eq!(up.pixel(apex_x, y0), INK, "the apex is ink");
        assert!(is(up.pixel(x0, y0), RED), "the top-left corner is bare");
        assert!(
            is(up.pixel(x0 + width - 1, y0), RED),
            "the top-right corner is bare"
        );
        assert_eq!(
            up.pixel(x0, y0 + height - 1),
            INK,
            "the base runs to the left corner"
        );

        let down = render_key(SIZE, RED, &Label::Down, Ink::BRIGHT);
        let bottom = y0 + height - 1;
        assert_eq!(down.pixel(apex_x, bottom), INK, "the apex is at the bottom");
        assert!(
            is(down.pixel(x0, bottom), RED),
            "the bottom-left corner is bare"
        );
        assert_eq!(down.pixel(x0, y0), INK, "the base runs along the top");
        assert_ne!(up, down);
    }

    #[test]
    fn status_shows_two_lines_set_like_a_name() {
        let full = render_key(SIZE, RED, &status(), Ink::BRIGHT);
        let no_clock = render_key(
            SIZE,
            RED,
            &Label::Status {
                state: "Waiting",
                elapsed: String::new(),
            },
            Ink::BRIGHT,
        );
        let none = render_key(SIZE, RED, &Label::None, Ink::BRIGHT);
        assert_ne!(no_clock, none, "the state line shows");
        assert_ne!(full, no_clock, "the elapsed line shows");
        // Two lines are a taller block: its first line sits higher than a
        // lone one does.
        let first_ink = |key: &Canvas| {
            (0..key.height).find(|&y| (0..key.width).any(|x| !is(key.pixel(x, y), RED)))
        };
        assert!(first_ink(&full) < first_ink(&no_clock));
        // And it is set at the name's size: a lone "Waiting" is as tall as a
        // lone name of the same word.
        let as_name = render_key(SIZE, RED, &Label::Name("Waiting".to_owned()), Ink::BRIGHT);
        assert_eq!(no_clock, as_name, "one word reads the same on either key");
    }

    #[test]
    fn a_long_name_is_cut_rather_than_overflowing() {
        let long = "a-project-folder-with-a-name-that-goes-on-and-on-and-on";
        let key = render_key(SIZE, RED, &Label::Name(long.to_owned()), Ink::BRIGHT);
        for y in 0..72 {
            assert!(is(key.pixel(0, y), RED));
            assert!(is(key.pixel(71, y), RED));
        }
    }

    #[test]
    fn ink_is_chosen_for_the_lane_and_fades_only_when_its_ends_disagree() {
        // A dark lane: white reads on its full colour as on its glow, so
        // the ink is white throughout — no fade.
        let navy = Rgb::new(0, 60, 160);
        assert_eq!(Ink::on(navy, navy), Ink::BRIGHT);
        assert_eq!(Ink::on(palette::base(navy), navy), Ink::BRIGHT);
        assert_eq!(
            Ink::on(Rgb::new(0, 36, 96), navy),
            Ink::BRIGHT,
            "halfway up, still white"
        );
        // A light lane: black on its full colour, white on its glow, and
        // the greys between as the key moves from one to the other.
        let blue = Rgb::new(80, 170, 255);
        assert_eq!(Ink::on(blue, blue), Ink::DARK);
        assert_eq!(Ink::on(palette::base(blue), blue), Ink::BRIGHT);
        let Ink::Lit(mid) = Ink::on(Rgb::new(48, 102, 153), blue) else {
            panic!("a lit key");
        };
        assert!(0 < mid && mid < 255, "halfway up: {mid}");
        // Error's dark red on a light lane sits at the dark end of the ramp.
        assert_eq!(Ink::on(palette::DARK_RED, blue), Ink::BRIGHT);
        let white = Rgb::new(255, 255, 255);
        assert_eq!(Ink::on(white, white), Ink::DARK);
    }

    #[test]
    fn the_ink_is_one_straight_ramp_with_no_shadow() {
        assert_eq!(Paint::of(Ink::Lit(128)).colour, [127, 127, 127]);
        assert_eq!(Paint::of(Ink::BRIGHT).colour, INK);
        assert_eq!(Paint::of(Ink::DARK).colour, [0, 0, 0]);
        // A white triangle on a black key and nothing else: no shadow.
        let key = render_key(SIZE, Rgb::new(0, 0, 0), &Label::Up, Ink::BRIGHT);
        let pixels: Vec<[u8; 3]> = key.rgb.chunks(3).map(|px| [px[0], px[1], px[2]]).collect();
        assert!(
            pixels.iter().all(|px| *px == [0, 0, 0] || *px == INK),
            "black key, white ink, nothing between"
        );
    }

    #[test]
    fn dark_ink_is_black_without_a_shadow() {
        let key = render_key(SIZE, Rgb::new(80, 170, 255), &Label::Up, Ink::DARK);
        let pixels: Vec<[u8; 3]> = key.rgb.chunks(3).map(|px| [px[0], px[1], px[2]]).collect();
        assert!(
            !pixels.contains(&[255, 255, 255]),
            "no white anywhere on a light key"
        );
        assert!(
            pixels.contains(&[0, 0, 0]),
            "the black ink is there — a solid triangle, not a thin glyph"
        );
    }

    #[test]
    fn a_name_breaks_at_dots_and_dashes() {
        assert_eq!(name_words("agent-frow"), ["agent", "frow"]);
        assert_eq!(
            name_words("ai.agent.keeb v2"),
            ["ai", "agent", "keeb", "v2"]
        );
        assert_eq!(name_words("--x--"), ["x"]);
        assert_eq!(
            name_words("snake_case"),
            ["snake_case"],
            "underscores are left alone"
        );
        assert!(name_words(" . - ").is_empty());
    }

    #[test]
    fn a_broken_name_is_set_larger_over_more_lines() {
        let font = font().unwrap();
        let (scale, lines) = name_layout(font, "agent-frow", 63.0, 60.0, 20.0).unwrap();
        assert_eq!(lines, ["agent", "frow"]);
        assert!(scale > 15.0, "larger than the one-line size, got {scale}");
        let (one, lines) = name_layout(font, "frow", 63.0, 60.0, 20.0).unwrap();
        assert_eq!(lines, ["frow"]);
        assert_eq!(one, 20.0, "a short name takes the full size");
        let (_, lines) = name_layout(font, "one two three four", 63.0, 60.0, 20.0).unwrap();
        assert!(lines.len() <= NAME_LINES);
        let (floor, lines) =
            name_layout(font, "anunbreakablenameoverflowsthekey", 63.0, 60.0, 20.0).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(floor, 9.0, "the floor, and the line is cut when drawn");
    }

    #[test]
    fn enter_is_set_like_the_name() {
        let enter = render_key(SIZE, RED, &Label::Enter, Ink::BRIGHT);
        let name = render_key(SIZE, RED, &Label::Name("Enter".to_owned()), Ink::BRIGHT);
        assert_eq!(enter, name);
    }

    #[test]
    fn a_triangle_is_a_fifth_of_the_key() {
        let (_, _, width, height) = triangle_box(SIZE);
        assert_eq!((width, height), (14, 14));
    }

    #[test]
    fn waiting_puts_how_long_first_and_larger() {
        let wait = Label::Wait {
            state: "Waiting",
            held: "12m".to_owned(),
        };
        let key = render_key(SIZE, RED, &wait, Ink::BRIGHT);
        let as_status = render_key(
            SIZE,
            RED,
            &Label::Status {
                state: "Waiting",
                elapsed: "12m".to_owned(),
            },
            Ink::BRIGHT,
        );
        assert_ne!(key, as_status, "not the state key's two lines of one size");
        assert_ne!(key, render_key(SIZE, RED, &Label::None, Ink::BRIGHT));
        assert_eq!(
            key,
            render_key(SIZE, RED, &wait, Ink::BRIGHT),
            "deterministic"
        );
        // The ink falls in two bands, a caption over a headline, and the
        // lower one — the count — is the taller.
        let inked: Vec<bool> = (0..key.height)
            .map(|y| (0..key.width).any(|x| !is(key.pixel(x, y), RED)))
            .collect();
        let mut bands: Vec<(usize, usize)> = Vec::new();
        for (y, &ink) in inked.iter().enumerate() {
            match (ink, bands.last_mut()) {
                (true, Some(band)) if band.1 == y => band.1 = y + 1,
                (true, _) => bands.push((y, y + 1)),
                (false, _) => {}
            }
        }
        assert_eq!(bands.len(), 2, "a caption and a headline: {bands:?}");
        let (caption, headline) = (bands[0], bands[1]);
        assert!(
            headline.1 - headline.0 > caption.1 - caption.0,
            "the count is the larger line: {bands:?}"
        );
    }

    #[test]
    fn a_gauge_is_its_label_over_its_value() {
        let gauge = render_key(
            SIZE,
            RED,
            &Label::Gauge {
                name: "ctx",
                value: Some(42),
            },
            Ink::BRIGHT,
        );
        let as_status = render_key(
            SIZE,
            RED,
            &Label::Status {
                state: "ctx",
                elapsed: "42%".to_owned(),
            },
            Ink::BRIGHT,
        );
        assert_eq!(gauge, as_status, "two lines by the one rule");
        let unknown = render_key(
            SIZE,
            RED,
            &Label::Gauge {
                name: "ctx",
                value: None,
            },
            Ink::BRIGHT,
        );
        assert_ne!(gauge, unknown);
    }

    #[test]
    fn an_unknown_gauge_is_a_dash() {
        let unknown = render_key(
            SIZE,
            RED,
            &Label::Gauge {
                name: "5h",
                value: None,
            },
            Ink::BRIGHT,
        );
        let dash = render_key(
            SIZE,
            RED,
            &Label::Status {
                state: "5h",
                elapsed: "—".to_owned(),
            },
            Ink::BRIGHT,
        );
        assert_eq!(unknown, dash);
    }

    #[test]
    fn quiet_ink_never_outshines_the_key() {
        let key = render_key(
            SIZE,
            palette::OFF,
            &Label::Name("quiet".to_owned()),
            Ink::Quiet,
        );
        let brightest = key.rgb.iter().copied().max().unwrap();
        assert!(
            brightest <= QUIET_INK[0],
            "no white on a dark key, got {brightest}"
        );
        assert!(brightest > 40, "but the words are there");
    }
}
