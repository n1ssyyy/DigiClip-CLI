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
}
