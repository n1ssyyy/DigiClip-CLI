//! Text widths for the caption fonts, read straight from the embedded font
//! files (the same bytes libass is given). The word-level caption model lays
//! every word out itself so a word can scale, lift and tilt without moving
//! its neighbours, and for that it needs how wide a word is. Advances only:
//! no kerning, no shaping (the fonts are Latin display faces; a pair kern is a
//! pixel or two and cannot move a word off its slot).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// One font's horizontal metrics, in font units.
pub struct Face {
    data: &'static [u8],
    /// OS/2 winAscent + winDescent: libass sizes a font so this height is its
    /// size, which also makes one line of text exactly `size` px tall.
    win: f64,
    hmtx: usize,
    metrics: usize,
    /// OS/2 winAscent / winDescent (libass lays a line out on these) and the
    /// cap height, in font units.
    asc: f64,
    desc: f64,
    cap: Option<f64>,
    /// `loca` and `glyf` (TrueType outlines), and whether `loca` is long.
    loca: Option<usize>,
    glyf: Option<usize>,
    loca_long: bool,
    /// Offsets of the cmap subtables read (format 4 / format 12).
    f4: Option<usize>,
    f12: Option<usize>,
    cache: Mutex<HashMap<char, f64>>,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(o)?, *b.get(o + 1)?]))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *b.get(o)?,
        *b.get(o + 1)?,
        *b.get(o + 2)?,
        *b.get(o + 3)?,
    ]))
}

impl Face {
    /// Read a TrueType/OpenType font; `None` for anything it cannot read.
    pub fn parse(data: &'static [u8]) -> Option<Face> {
        let b = data;
        let n = u16_at(b, 4)? as usize;
        let table = |tag: &[u8; 4]| -> Option<usize> {
            (0..n).find_map(|i| {
                let e = 12 + i * 16;
                if b.get(e..e + 4)? == tag {
                    Some(u32_at(b, e + 8)? as usize)
                } else {
                    None
                }
            })
        };
        let hhea = table(b"hhea")?;
        let hmtx = table(b"hmtx")?;
        let cmap = table(b"cmap")?;
        let win = table(b"OS/2")
            .and_then(|o| Some(u16_at(b, o + 74)? as f64 + u16_at(b, o + 76)? as f64))
            .filter(|w| *w > 0.0)
            .unwrap_or(1000.0);
        let metrics = u16_at(b, hhea + 34)? as usize;
        let i16_at = |o: usize| u16_at(b, o).map(|v| v as i16 as f64);
        let os2 = table(b"OS/2");
        let asc = os2.and_then(|o| u16_at(b, o + 74)).map_or(0.0, f64::from);
        let desc = os2.and_then(|o| u16_at(b, o + 76)).map_or(0.0, f64::from);
        // sCapHeight is there from OS/2 version 2.
        let cap = os2
            .filter(|&o| u16_at(b, o).unwrap_or(0) >= 2)
            .and_then(|o| i16_at(o + 88))
            .filter(|c| *c > 0.0);
        let loca_long = table(b"head").and_then(|h| u16_at(b, h + 50)) == Some(1);
        let (mut f4, mut f12) = (None, None);
        for i in 0..u16_at(b, cmap + 2)? as usize {
            let r = cmap + 4 + i * 8;
            let (plat, enc) = (u16_at(b, r)?, u16_at(b, r + 2)?);
            let off = cmap + u32_at(b, r + 4)? as usize;
            match (plat, enc, u16_at(b, off)?) {
                (3, 10, 12) | (0, 4 | 6, 12) => f12 = f12.or(Some(off)),
                (3, 1, 4) | (0, _, 4) => f4 = f4.or(Some(off)),
                _ => {}
            }
        }
        (f4.is_some() || f12.is_some()).then_some(Face {
            data,
            win,
            hmtx,
            metrics,
            asc,
            desc,
            cap,
            loca: table(b"loca"),
            glyf: table(b"glyf"),
            loca_long,
            f4,
            f12,
            cache: Mutex::default(),
        })
    }

    fn glyph(&self, c: char) -> u16 {
        let b = self.data;
        let cp = c as u32;
        if let Some(o) = self.f12 {
            let groups = u32_at(b, o + 12).unwrap_or(0) as usize;
            for g in 0..groups {
                let r = o + 16 + g * 12;
                let (first, last, id) = (
                    u32_at(b, r).unwrap_or(1),
                    u32_at(b, r + 4).unwrap_or(0),
                    u32_at(b, r + 8).unwrap_or(0),
                );
                if (first..=last).contains(&cp) {
                    return (id + cp - first) as u16;
                }
            }
        }
        if let (Some(o), true) = (self.f4, cp <= 0xFFFF) {
            let segx2 = u16_at(b, o + 6).unwrap_or(0) as usize;
            let ends = o + 14;
            let starts = ends + segx2 + 2;
            let deltas = starts + segx2;
            let ranges = deltas + segx2;
            for s in 0..segx2 / 2 {
                let end = u16_at(b, ends + s * 2).unwrap_or(0) as u32;
                if cp > end {
                    continue;
                }
                let start = u16_at(b, starts + s * 2).unwrap_or(1) as u32;
                if cp < start {
                    return 0;
                }
                let delta = u16_at(b, deltas + s * 2).unwrap_or(0) as u32;
                let ro = u16_at(b, ranges + s * 2).unwrap_or(0) as usize;
                if ro == 0 {
                    return ((cp + delta) & 0xFFFF) as u16;
                }
                let at = ranges + s * 2 + ro + 2 * (cp - start) as usize;
                return match u16_at(b, at) {
                    Some(0) | None => 0,
                    Some(g) => ((g as u32 + delta) & 0xFFFF) as u16,
                };
            }
        }
        0
    }

    /// Advance of one character, in font units.
    fn advance(&self, c: char) -> f64 {
        if let Some(v) = self.cache.lock().ok().and_then(|m| m.get(&c).copied()) {
            return v;
        }
        let g = (self.glyph(c) as usize).min(self.metrics.saturating_sub(1));
        let a = u16_at(self.data, self.hmtx + g * 4).unwrap_or(0) as f64;
        if let Ok(mut m) = self.cache.lock() {
            m.insert(c, a);
        }
        a
    }

    /// Box of one glyph in font units: (x min, y min, x max, y max), `None`
    /// for a glyph with no outline (a space) or a font without `glyf`.
    fn bounds(&self, c: char) -> Option<(f64, f64, f64, f64)> {
        let (loca, glyf) = (self.loca?, self.glyf?);
        let g = self.glyph(c) as usize;
        let b = self.data;
        let (a0, a1) = if self.loca_long {
            (
                u32_at(b, loca + g * 4)? as usize,
                u32_at(b, loca + g * 4 + 4)? as usize,
            )
        } else {
            (
                u16_at(b, loca + g * 2)? as usize * 2,
                u16_at(b, loca + g * 2 + 2)? as usize * 2,
            )
        };
        if a1 <= a0 {
            return None;
        }
        let o = glyf + a0;
        let v = |d: usize| u16_at(b, o + d).map(|v| v as i16 as f64);
        Some((v(2)?, v(4)?, v(6)?, v(8)?))
    }

    /// libass' line box above and below the baseline, in px at `size`: the
    /// ascent and the descent (both positive).
    pub fn line_box(&self, size: f64) -> (f64, f64) {
        let s = self.scale(size);
        (self.asc * s, self.desc * s)
    }

    /// How far the ink of `text` reaches above the baseline and below it, in
    /// px at `size` (`(top, bottom)`, both measured upward, so a descender
    /// makes `bottom` negative). The top is never below the cap height and the
    /// bottom never above the baseline, so one line of capitals and one of
    /// lower case sit in boxes of about the same height. `None` when the font
    /// has no outlines to read or the text has no ink.
    pub fn ink_y(&self, text: &str, size: f64) -> Option<(f64, f64)> {
        let s = self.scale(size);
        let mut top: Option<f64> = None;
        let mut bottom = 0.0f64;
        for c in text.chars() {
            if let Some((_, y0, _, y1)) = self.bounds(c) {
                top = Some(top.map_or(y1, |t| t.max(y1)));
                bottom = bottom.min(y0);
            }
        }
        let cap = self
            .cap
            .or_else(|| self.bounds('H').map(|b| b.3))
            .unwrap_or(0.0);
        top.map(|t| (t.max(cap) * s, bottom * s))
    }

    /// The side bearings of `text` in px at `size`: how far the first glyph's
    /// ink starts after the pen and how far the last glyph's ink stops short
    /// of its advance. Zero where the font has no outlines to read.
    pub fn ink_x(&self, text: &str, size: f64) -> (f64, f64) {
        let s = self.scale(size);
        let first = text.chars().find(|c| !c.is_whitespace());
        let last = text.chars().rev().find(|c| !c.is_whitespace());
        let l = first
            .and_then(|c| self.bounds(c))
            .map_or(0.0, |b| b.0.max(0.0) * s);
        let r = last
            .and_then(|c| Some((self.bounds(c)?, self.advance(c))))
            .map_or(0.0, |(b, a)| (a - b.2).max(0.0) * s);
        (l, r)
    }

    /// Pixels per font unit at a libass font size of `size` px.
    pub fn scale(&self, size: f64) -> f64 {
        size / self.win
    }

    /// Width of `text` in px at a libass font size of `size` px.
    pub fn width(&self, text: &str, size: f64) -> f64 {
        text.chars().map(|c| self.advance(c)).sum::<f64>() * self.scale(size)
    }
}

/// The face behind one of the caption fonts (`look::FONTS` and the presets).
pub fn face(font: &str) -> Option<&'static Face> {
    static FACES: OnceLock<HashMap<&'static str, Face>> = OnceLock::new();
    FACES
        .get_or_init(|| {
            let mut m = HashMap::new();
            for (name, file) in [
                ("Anton", "Anton-Regular.ttf"),
                ("Archivo Black", "ArchivoBlack-Regular.ttf"),
                ("Inter Medium", "Inter-Medium.ttf"),
                ("JetBrains Mono", "JetBrainsMono-Variable.ttf"),
            ] {
                let bytes = crate::provision::FONTS
                    .iter()
                    .find(|(n, _)| *n == file)
                    .map(|(_, b)| *b);
                if let Some(f) = bytes.and_then(Face::parse) {
                    m.insert(name, f);
                }
            }
            m
        })
        .get(font)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_caption_font_reads_and_text_has_width() {
        for name in crate::look::FONTS {
            let f = face(name).unwrap_or_else(|| panic!("{name} unreadable"));
            let w = f.width("Hello world", 100.0);
            assert!((100.0..1100.0).contains(&w), "{name}: {w}");
            assert!(f.width("iiii", 80.0) <= f.width("MMMM", 80.0), "{name}");
        }
        // Monospace: every glyph the same width.
        let m = face("JetBrains Mono").unwrap();
        assert!((m.width("iiii", 100.0) - m.width("MMMM", 100.0)).abs() < 1e-6);
    }

    #[test]
    fn ink_and_line_box_come_from_the_outlines() {
        for name in crate::look::FONTS {
            let f = face(name).unwrap();
            let (asc, desc) = f.line_box(100.0);
            assert!(asc > 40.0 && desc > 0.0 && asc + desc < 200.0, "{name}");
            let (top, bottom) = f.ink_y("HELLO", 100.0).unwrap();
            // Capitals: from about the baseline up to the cap height, inside the line box.
            assert!((40.0..asc).contains(&top), "{name}: top {top}");
            assert!(bottom <= 0.0 && bottom > -20.0, "{name}: bottom {bottom}");
            // A descender reaches below the baseline further than capitals do.
            let (_, low) = f.ink_y("gyp", 100.0).unwrap();
            assert!(low < bottom - 5.0, "{name}: {low} {bottom}");
            // Bearings are small and never negative; a space has no ink.
            let (l, r) = f.ink_x("HELLO", 100.0);
            assert!(
                (0.0..15.0).contains(&l) && (0.0..15.0).contains(&r),
                "{name}"
            );
            assert!(f.ink_y("   ", 100.0).is_none());
        }
    }
}
