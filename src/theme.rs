//! Accent colours. A theme is just two RGB colours (accent → gradient end); `cover` takes
//! them from the album art of the song that's playing. Drawing reads them from a
//! thread-local set once per frame, so no draw function needs an extra argument.

use std::cell::Cell;

use image::DynamicImage;
use ratatui::style::Color;

type Rgb = [u8; 3];

/// (name, accent, gradient end). Index 0 is the default and the fallback for `cover`.
pub const THEMES: [(&str, Rgb, Rgb); 6] = [
    ("spotify", [30, 215, 96], [0, 175, 235]),
    ("ocean", [0, 180, 255], [130, 100, 255]),
    ("violet", [185, 120, 255], [255, 95, 175]),
    ("sunset", [255, 145, 55], [255, 75, 115]),
    ("rose", [255, 115, 165], [255, 180, 95]),
    ("mono", [225, 225, 225], [130, 130, 140]),
];
/// Theme index meaning "from the album art".
pub const COVER: usize = THEMES.len();

pub fn name(i: usize) -> &'static str {
    THEMES.get(i).map(|t| t.0).unwrap_or("cover")
}

pub fn by_name(n: &str) -> usize {
    if n == "cover" { COVER } else { THEMES.iter().position(|t| t.0 == n).unwrap_or(0) }
}

/// The two colours for theme `i`; `tint` is the current cover's colour (cover theme only).
pub fn colors(i: usize, tint: Option<Rgb>) -> (Rgb, Rgb) {
    match (THEMES.get(i), tint) {
        (Some(t), _) => (t.1, t.2),
        (None, Some(c)) => {
            let (h, s, v) = to_hsv(c);
            // gradient: hue nudged like green→cyan; greys have no hue, so go darker instead
            (c, if s < 0.2 { from_hsv(h, s, v * 0.6) } else { from_hsv(h + 40.0, s, v) })
        }
        (None, None) => (THEMES[0].1, THEMES[0].2),
    }
}

thread_local! {
    static CURRENT: Cell<(Rgb, Rgb)> = const { Cell::new(([30, 215, 96], [0, 175, 235])) };
}

pub fn set(c: (Rgb, Rgb)) {
    CURRENT.with(|t| t.set(c));
}

pub fn accent() -> Color {
    let [r, g, b] = CURRENT.with(Cell::get).0;
    Color::Rgb(r, g, b)
}

/// Text colour that stays readable on an accent background.
pub fn on_accent() -> Color {
    if luma(CURRENT.with(Cell::get).0) > 110.0 { Color::Black } else { Color::White }
}

/// Accent → gradient end, `t` in 0..=1.
pub fn grad(t: f32) -> Color {
    let (a, b) = CURRENT.with(Cell::get);
    let t = t.clamp(0.0, 1.0);
    let mix = |i: usize| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t) as u8;
    Color::Rgb(mix(0), mix(1), mix(2))
}

/// The cover's dominant colour, made readable on a dark terminal. Every pixel votes for its
/// hue, weighted by how colourful it is, so a pastel or off-white cover still gets its
/// tint and a small vivid title on white still wins. Covers with no colour at all get a
/// light grey with their warm/cool cast. A 48 px thumbnail: ~2,300 pixels, ~0.5–1 ms.
pub fn cover_tint(img: &DynamicImage) -> Rgb {
    const BINS: usize = 24; // 15° each
    let small = img.thumbnail(48, 48).to_rgb8();
    let n = small.len() as f32 / 3.0;
    // per hue bin: weight, Σr, Σg, Σb (weighted); plus the plain average for grey covers
    let mut bins = [[0f32; 4]; BINS];
    let mut avg = [0f32; 3];
    let mut colourful = 0f32;
    for p in small.pixels() {
        let [r, g, b] = p.0.map(|c| c as f32);
        avg = [avg[0] + r, avg[1] + g, avg[2] + b];
        let (h, _, v) = to_hsv(p.0);
        let chroma = (r.max(g).max(b) - r.min(g).min(b)) / 255.0;
        // JPEG noise gives greys a few levels of "colour"; near-black hues are noise too
        if chroma < 0.06 || v < 0.12 {
            continue;
        }
        colourful += chroma;
        // squared: a smaller vivid area (a title, green hair) beats a large muted one (skin, sand)
        let w = chroma * chroma;
        let bin = &mut bins[(h / (360.0 / BINS as f32)) as usize % BINS];
        bin[0] += w;
        bin[1] += r * w;
        bin[2] += g * w;
        bin[3] += b * w;
    }
    if colourful < n * 0.012 {
        // black / white / grey cover: keep its cast, faintly
        let (h, s, _) = to_hsv(avg.map(|c| (c / n) as u8));
        return from_hsv(h, s.min(0.12), 0.86);
    }
    // a colour often straddles two bins: score each with its neighbours
    let at = |i: isize| bins[i.rem_euclid(BINS as isize) as usize];
    let best = (0..BINS as isize)
        .max_by(|&a, &b| {
            let score = |i| at(i - 1)[0] * 0.5 + at(i)[0] + at(i + 1)[0] * 0.5;
            score(a).total_cmp(&score(b))
        })
        .unwrap_or(0);
    let sum = [at(best - 1), at(best), at(best + 1)].iter().fold([0f32; 4], |acc, b| std::array::from_fn(|i| acc[i] + b[i]));
    let (h, s, v) = to_hsv([sum[1] / sum[0], sum[2] / sum[0], sum[3] / sum[0]].map(|c| c as u8));
    // pastels stay soft but visible, vivid colours stay vivid, everything bright
    let (mut s, v) = (s.clamp(0.35, 0.85), v.max(0.88));
    // blues and purples are dark even at full value: wash them out until they read on black
    while luma(from_hsv(h, s, v)) < 125.0 && s > 0.2 {
        s -= 0.05;
    }
    from_hsv(h, s, v)
}

fn luma([r, g, b]: Rgb) -> f32 {
    0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32
}

fn to_hsv([r, g, b]: Rgb) -> (f32, f32, f32) {
    let [r, g, b] = [r, g, b].map(|c| c as f32 / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let d = max - min;
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h, if max > 0.0 { d / max } else { 0.0 }, max)
}

fn from_hsv(h: f32, s: f32, v: f32) -> Rgb {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [r, g, b].map(|u| ((u + m) * 255.0).round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb as Px, RgbImage};

    fn tint(f: impl Fn(u32, u32) -> [u8; 3]) -> (f32, f32, f32) {
        to_hsv(cover_tint(&DynamicImage::ImageRgb8(RgbImage::from_fn(48, 48, |x, y| Px(f(x, y))))))
    }

    #[test]
    fn picks_cover_colours() {
        // dark grey with a dark-red block: red, made bright
        let (h, s, v) = tint(|x, _| if x < 12 { [140, 20, 20] } else { [30, 30, 30] });
        assert!((h < 15.0 || h > 345.0) && s >= 0.45 && v >= 0.87, "{h} {s} {v}");
        // off-white with a pink cast (pastel cover): pink, not the green fallback
        let (h, s, _) = tint(|_, _| [250, 228, 236]);
        assert!((h > 300.0 || h < 20.0) && s >= 0.34, "{h} {s}");
        // mostly white with a small vivid blue title: blue
        let (h, ..) = tint(|_, y| if y < 6 { [20, 90, 230] } else { [245, 245, 245] });
        assert!((200.0..=240.0).contains(&h), "{h}");
        // black and white: a light grey, not a colour
        let (_, s, v) = tint(|x, _| if x < 24 { [0, 0, 0] } else { [255, 255, 255] });
        assert!(s <= 0.13 && v > 0.8, "{s} {v}");
        // deep blue is lightened until it reads on a dark terminal
        assert!(luma(cover_tint(&DynamicImage::ImageRgb8(RgbImage::from_fn(48, 48, |_, _| Px([40, 20, 200]))))) >= 125.0);
        assert_eq!(from_hsv(120.0, 1.0, 1.0), [0, 255, 0]);
        assert_eq!(by_name(name(COVER)), COVER);
    }
}
