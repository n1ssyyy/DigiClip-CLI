//! Text widths for the caption fonts, read straight from the font files (the
//! same bytes libass is given). The word-level caption model lays every word
//! out itself so a word can scale, lift and tilt without moving its
//! neighbours, and for that it needs how wide a word is. Advances only: no
//! kerning, no shaping (the fonts are Latin display faces; a pair kern is a
//! pixel or two and cannot move a word off its slot).
//!
//! The reader serves the bundled fonts and the creator's own (`crate::fonts`),
//! so it treats every file as hostile: each table is checked against the
//! file's length when the font is opened, every later read is bounds-checked,
//! and the loops that walk a table are clamped to what the file can hold. A
//! font it cannot read is an `Err` with a plain reason, never a panic.
//!
//! Outlines: a TrueType font (`glyf`) gives the ink box of every glyph, which
//! the boxes and the headline card are fitted to. An OpenType/CFF font has no
//! `glyf`; reading its charstrings would need a Type 2 interpreter, which the
//! layout does not justify. For those the advances, the line box and the cap
//! height are read exactly as for any font, the ink's side bearings count as
//! zero, and the ink's height is estimated from the cap height (plus one
//! descender allowance when the text has a letter that hangs below the
//! baseline). A box around CFF text is therefore a pixel or two looser at the
//! sides than around TrueType text.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// One font's horizontal metrics, in font units.
pub struct Face {
    data: Cow<'static, [u8]>,
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
    /// Units per em (only the estimate of a CFF font's ink uses it).
    upm: f64,
    /// `usWeightClass` (400 without an OS/2 table).
    weight: u16,
    /// `loca` and `glyf` (TrueType outlines), and whether `loca` is long.
    loca: Option<usize>,
    glyf: Option<usize>,
    loca_long: bool,
    /// Offsets of the cmap subtables read (format 4 / format 12).
    f4: Option<usize>,
    f12: Option<usize>,
    /// Outlines are CFF, not TrueType: no per-glyph ink box.
    cff: bool,
    /// The family libass knows the font by (name id 1).
    family: String,
    cache: Mutex<HashMap<char, f64>>,
}

impl std::fmt::Debug for Face {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Face")
            .field("family", &self.family)
            .field("weight", &self.weight)
            .field("cff", &self.cff)
            .finish()
    }
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

/// A table of the file as (offset, length), checked against the file's size.
fn find_table(b: &[u8], tag: &[u8; 4]) -> Result<Option<(usize, usize)>, String> {
    let n = u16_at(b, 4).unwrap_or(0) as usize;
    for i in 0..n {
        let Some(rec) = b.get(12 + i * 16..12 + i * 16 + 16) else {
            return Err("the font file is truncated (its table directory is cut off)".into());
        };
        if &rec[..4] == tag {
            let off = u32_at(rec, 8).unwrap_or(0) as usize;
            let len = u32_at(rec, 12).unwrap_or(0) as usize;
            if off.checked_add(len).is_none_or(|end| end > b.len()) {
                return Err(format!(
                    "the font file is truncated (its {} table runs past the end)",
                    String::from_utf8_lossy(tag).trim()
                ));
            }
            return Ok(Some((off, len)));
        }
    }
    Ok(None)
}

/// The family name of the `name` table: id 1, else 16, else 4. Windows
/// English records win over other Windows, Unicode and Mac ones.
fn read_family(b: &[u8], (off, len): (usize, usize)) -> Option<String> {
    let count = u16_at(b, off + 2).unwrap_or(0) as usize;
    let str_off = off + u16_at(b, off + 4).unwrap_or(0) as usize;
    let mut best: HashMap<u16, (u8, String)> = HashMap::new();
    for i in 0..count.min(len.saturating_sub(6) / 12) {
        let r = off + 6 + i * 12;
        let (Some(plat), Some(enc), Some(lang), Some(id), Some(l), Some(o)) = (
            u16_at(b, r),
            u16_at(b, r + 2),
            u16_at(b, r + 4),
            u16_at(b, r + 6),
            u16_at(b, r + 8),
            u16_at(b, r + 10),
        ) else {
            break;
        };
        if ![1, 4, 16].contains(&id) {
            continue;
        }
        let (s, e) = (str_off + o as usize, str_off + o as usize + l as usize);
        let Some(raw) = b.get(s..e).filter(|_| e <= off + len) else {
            continue;
        };
        let (rank, text): (u8, String) = match (plat, enc) {
            (3, 0 | 1 | 10) | (0, _) => {
                let units: Vec<u16> = raw
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                let t = char::decode_utf16(units)
                    .map(|c| c.unwrap_or('\u{fffd}'))
                    .collect();
                (if plat == 3 && lang == 0x409 { 3 } else { 2 }, t)
            }
            (1, 0) => (
                1,
                raw.iter()
                    .map(|&c| if c < 128 { c as char } else { '?' })
                    .collect(),
            ),
            _ => continue,
        };
        let text = text.trim().to_string();
        if !text.is_empty() && best.get(&id).is_none_or(|(r0, _)| rank > *r0) {
            best.insert(id, (rank, text));
        }
    }
    let mut get = |id: u16| best.remove(&id).map(|(_, t)| t);
    let (f1, f16) = (get(1), get(16));
    f1.or(f16).or_else(|| get(4))
}

impl Face {
    /// Read a bundled TrueType/OpenType font; `None` for anything it cannot
    /// read.
    pub fn parse(data: &'static [u8]) -> Option<Face> {
        Face::open(Cow::Borrowed(data)).ok()
    }

    /// Read a font file the creator gave us. The error says what is wrong
    /// with it, in words the creator can act on.
    pub fn from_bytes(data: Vec<u8>) -> Result<Face, String> {
        Face::open(Cow::Owned(data))
    }

    fn open(data: Cow<'static, [u8]>) -> Result<Face, String> {
        let b: &[u8] = &data;
        let cff = match u32_at(b, 0) {
            None => return Err("the file is too small to be a font".into()),
            Some(0x0001_0000 | 0x7472_7565) => false,
            Some(0x4F54_544F) => true,
            Some(0x7474_6366) => {
                return Err(
                    "font collections (.ttc) are not supported: add a single .ttf or .otf file"
                        .into(),
                )
            }
            Some(0x774F_4646 | 0x774F_4632) => {
                return Err(
                    "web fonts (.woff, .woff2) are not supported: add a .ttf or .otf file".into(),
                )
            }
            Some(_) => return Err("the file is not a TrueType or OpenType font".into()),
        };
        let n = u16_at(b, 4).unwrap_or(0) as usize;
        if n == 0 || 12 + n * 16 > b.len() {
            return Err("the font file is truncated (its table directory is cut off)".into());
        }
        let need = |tag: &[u8; 4], min: usize| -> Result<(usize, usize), String> {
            let name = String::from_utf8_lossy(tag).trim().to_string();
            match find_table(b, tag)? {
                Some((o, l)) if l >= min => Ok((o, l)),
                Some(_) => Err(format!("the font's {name} table is too short")),
                None => Err(format!("the font has no {name} table")),
            }
        };
        let (head, _) = need(b"head", 54)?;
        let (hhea, _) = need(b"hhea", 36)?;
        let (hmtx, hmtx_len) = need(b"hmtx", 4)?;
        let cmap = need(b"cmap", 4)?;
        let name = need(b"name", 6)?;
        let loca = find_table(b, b"loca")?;
        let glyf = find_table(b, b"glyf")?;
        let has_cff = find_table(b, b"CFF ")?.is_some() || find_table(b, b"CFF2")?.is_some();
        let truetype = glyf.is_some() && loca.is_some();
        if !truetype && !has_cff {
            return Err("the font has no outlines (it needs a glyf or CFF table)".into());
        }
        let metrics = u16_at(b, hhea + 34).unwrap_or(0) as usize;
        if metrics == 0 || metrics * 4 > hmtx_len {
            return Err("the font's hmtx table is too short for its glyph count".into());
        }
        let upm = u16_at(b, head + 18).unwrap_or(0);
        if !(16..=16384).contains(&upm) {
            return Err("the font has an invalid units-per-em".into());
        }
        let i16_at = |o: usize| u16_at(b, o).map(|v| v as i16 as f64);
        let os2 = find_table(b, b"OS/2")?.filter(|t| t.1 >= 78);
        let mut asc = os2
            .and_then(|(o, _)| u16_at(b, o + 74))
            .map_or(0.0, f64::from);
        let mut desc = os2
            .and_then(|(o, _)| u16_at(b, o + 76))
            .map_or(0.0, f64::from);
        if asc + desc <= 0.0 {
            // No usable OS/2 win metrics: libass falls back to hhea.
            asc = i16_at(hhea + 4).unwrap_or(0.0).max(0.0);
            desc = i16_at(hhea + 6).unwrap_or(0.0).abs();
        }
        let win = if asc + desc > 0.0 { asc + desc } else { 1000.0 };
        // sCapHeight is there from OS/2 version 2.
        let cap = os2
            .filter(|&(o, l)| l >= 90 && u16_at(b, o).unwrap_or(0) >= 2)
            .and_then(|(o, _)| i16_at(o + 88))
            .filter(|c| *c > 0.0);
        let weight = os2
            .and_then(|(o, _)| u16_at(b, o + 4))
            .filter(|w| (1..=1000).contains(w))
            .unwrap_or(400);
        let loca_long = u16_at(b, head + 50) == Some(1);
        let (mut f4, mut f12) = (None, None);
        let (cmap_off, cmap_len) = cmap;
        let subtables = u16_at(b, cmap_off + 2).unwrap_or(0) as usize;
        for i in 0..subtables.min(cmap_len.saturating_sub(4) / 8) {
            let r = cmap_off + 4 + i * 8;
            let (Some(plat), Some(enc), Some(rel)) =
                (u16_at(b, r), u16_at(b, r + 2), u32_at(b, r + 4))
            else {
                break;
            };
            let off = cmap_off + rel as usize;
            let Some(format) = u16_at(b, off) else {
                continue;
            };
            match (plat, enc, format) {
                (3, 10, 12) | (0, 4 | 6, 12) => f12 = f12.or(Some(off)),
                (3, 1, 4) | (0, _, 4) => f4 = f4.or(Some(off)),
                _ => {}
            }
        }
        if f4.is_none() && f12.is_none() {
            return Err("the font has no Unicode character map (cmap format 4 or 12)".into());
        }
        let Some(family) = read_family(b, name) else {
            return Err("the font has no family name".into());
        };
        Ok(Face {
            win,
            hmtx,
            metrics,
            asc,
            desc,
            cap,
            upm: upm as f64,
            weight,
            loca: loca.map(|t| t.0),
            glyf: glyf.map(|t| t.0),
            loca_long,
            f4,
            f12,
            cff: cff && !truetype,
            family,
            data,
            cache: Mutex::default(),
        })
    }

    /// The family name libass knows the font by (name id 1).
    pub fn family(&self) -> &str {
        &self.family
    }

    /// `usWeightClass` (400 when the font does not say).
    pub fn weight(&self) -> u16 {
        self.weight
    }

    /// OpenType/CFF outlines: no per-glyph ink box (see the module notes).
    pub fn is_cff(&self) -> bool {
        self.cff
    }

    fn glyph(&self, c: char) -> u16 {
        let b: &[u8] = &self.data;
        let cp = c as u32;
        if let Some(o) = self.f12 {
            // A group is 12 bytes: never walk past the end of the file.
            let room = b.len().saturating_sub(o + 16) / 12;
            let groups = (u32_at(b, o + 12).unwrap_or(0) as usize).min(room);
            for g in 0..groups {
                let r = o + 16 + g * 12;
                let (first, last, id) = (
                    u32_at(b, r).unwrap_or(1),
                    u32_at(b, r + 4).unwrap_or(0),
                    u32_at(b, r + 8).unwrap_or(0),
                );
                if (first..=last).contains(&cp) {
                    return id.wrapping_add(cp - first) as u16;
                }
            }
        }
        if let (Some(o), true) = (self.f4, cp <= 0xFFFF) {
            let segx2 = (u16_at(b, o + 6).unwrap_or(0) as usize).min(b.len().saturating_sub(o));
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
        let a = u16_at(&self.data, self.hmtx + g * 4).unwrap_or(0) as f64;
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
        let b: &[u8] = &self.data;
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
    /// lower case sit in boxes of about the same height. `None` when the text
    /// has no ink. A CFF font has no outlines to read: its ink is the cap
    /// height, and a descender allowance when the text needs one.
    pub fn ink_y(&self, text: &str, size: f64) -> Option<(f64, f64)> {
        let s = self.scale(size);
        if self.glyf.is_none() || self.loca.is_none() {
            if text.chars().all(char::is_whitespace) {
                return None;
            }
            let cap = self.cap.unwrap_or(0.7 * self.upm);
            let hangs = text.chars().any(|c| "gjpqyQ,;()[]{}|/@$".contains(c));
            let low = if hangs { 0.21 * self.upm } else { 0.0 };
            return Some((cap * s, -low * s));
        }
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
    /// of its advance. Negative where the ink overhangs its advance (a slanted
    /// face such as Bangers), so a box fitted to the ink still covers it. Zero
    /// where the font has no outlines to read.
    pub fn ink_x(&self, text: &str, size: f64) -> (f64, f64) {
        let s = self.scale(size);
        let first = text.chars().find(|c| !c.is_whitespace());
        let last = text.chars().rev().find(|c| !c.is_whitespace());
        let l = first.and_then(|c| self.bounds(c)).map_or(0.0, |b| b.0 * s);
        let r = last
            .and_then(|c| Some((self.bounds(c)?, self.advance(c))))
            .map_or(0.0, |(b, a)| (a - b.2) * s);
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

/// The face behind a font family a Look may name: a bundled one, or one the
/// creator added (`fonts`).
pub fn face(font: &str) -> Option<Arc<Face>> {
    static FACES: OnceLock<HashMap<&'static str, Arc<Face>>> = OnceLock::new();
    FACES
        .get_or_init(|| {
            crate::fonts::BUNDLED
                .iter()
                .filter_map(|f| Some((f.family, Arc::new(Face::parse(f.bytes)?))))
                .collect()
        })
        .get(font)
        .cloned()
        .or_else(|| crate::fonts::user_face(font))
}

/// A tiny synthetic TrueType/CFF font for tests: three glyphs (notdef, `A`..`Z`
/// as one box glyph, space) in a format 4 cmap.
#[cfg(test)]
pub(crate) mod testfont {
    fn be16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_be_bytes());
    }

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
    }

    /// `family` goes in name ids 1 and 4. `cff` swaps `glyf`/`loca` for a
    /// stub `CFF ` table under an `OTTO` header.
    pub(crate) fn build(family: &str, cff: bool) -> Vec<u8> {
        let mut head = vec![0u8; 54];
        head[..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        head[18..20].copy_from_slice(&1000u16.to_be_bytes());
        let mut hhea = vec![0u8; 36];
        hhea[4..6].copy_from_slice(&800i16.to_be_bytes());
        hhea[6..8].copy_from_slice(&(-200i16).to_be_bytes());
        hhea[34..36].copy_from_slice(&3u16.to_be_bytes());
        let mut hmtx = Vec::new();
        for adv in [500u16, 600, 300] {
            be16(&mut hmtx, adv);
            be16(&mut hmtx, 0);
        }
        let mut maxp = vec![0u8; 6];
        maxp[..4].copy_from_slice(&0x0000_5000u32.to_be_bytes());
        maxp[4..6].copy_from_slice(&3u16.to_be_bytes());
        let mut os2 = vec![0u8; 96];
        os2[..2].copy_from_slice(&4u16.to_be_bytes());
        os2[4..6].copy_from_slice(&700u16.to_be_bytes());
        os2[74..76].copy_from_slice(&900u16.to_be_bytes());
        os2[76..78].copy_from_slice(&250u16.to_be_bytes());
        os2[88..90].copy_from_slice(&700u16.to_be_bytes());
        // cmap: one (3, 1) format 4 subtable with three segments.
        let mut cmap = Vec::new();
        for x in [0u16, 1, 3, 1] {
            be16(&mut cmap, x);
        }
        cmap.extend_from_slice(&12u32.to_be_bytes());
        let segs: [(u16, u16, u16); 3] = [
            (32, 32, 2u16.wrapping_sub(32)),
            (65, 90, 1u16.wrapping_sub(65)),
            (0xFFFF, 0xFFFF, 1),
        ];
        for x in [4u16, 14 + 8 * 3 + 2, 0, 6, 4, 1, 2] {
            be16(&mut cmap, x);
        }
        for s in segs {
            be16(&mut cmap, s.1);
        }
        be16(&mut cmap, 0);
        for s in segs {
            be16(&mut cmap, s.0);
        }
        for s in segs {
            be16(&mut cmap, s.2);
        }
        for _ in segs {
            be16(&mut cmap, 0);
        }
        let mut name = Vec::new();
        for x in [0u16, 2, 6 + 24] {
            be16(&mut name, x);
        }
        let fam = utf16(family);
        for (id, off) in [(1u16, 0usize), (4, fam.len())] {
            for x in [3u16, 1, 0x409, id, fam.len() as u16, off as u16] {
                be16(&mut name, x);
            }
        }
        name.extend_from_slice(&fam);
        name.extend_from_slice(&fam);
        let mut tables: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"OS/2", os2),
            (b"cmap", cmap),
            (b"head", head),
            (b"hhea", hhea),
            (b"hmtx", hmtx),
            (b"maxp", maxp),
            (b"name", name),
        ];
        if cff {
            tables.push((b"CFF ", vec![1, 0, 4, 1]));
        } else {
            let mut glyf = Vec::new();
            for x in [1i16, 50, 0, 550, 700, 0] {
                glyf.extend_from_slice(&x.to_be_bytes());
            }
            let mut loca = Vec::new();
            for x in [0u16, 0, 6, 6] {
                be16(&mut loca, x);
            }
            tables.push((b"glyf", glyf));
            tables.push((b"loca", loca));
        }
        tables.sort_by_key(|t| *t.0);
        let mut out = Vec::new();
        out.extend_from_slice(if cff { b"OTTO" } else { &[0, 1, 0, 0] });
        be16(&mut out, tables.len() as u16);
        out.extend_from_slice(&[0u8; 6]);
        let mut pos = 12 + 16 * tables.len();
        let mut body = Vec::new();
        for (tag, data) in &tables {
            out.extend_from_slice(*tag);
            out.extend_from_slice(&[0u8; 4]);
            out.extend_from_slice(&(pos as u32).to_be_bytes());
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            body.extend_from_slice(data);
            while body.len() % 4 != 0 {
                body.push(0);
            }
            pos = 12 + 16 * tables.len() + body.len();
        }
        out.extend_from_slice(&body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_caption_font_reads_and_text_has_width() {
        for name in crate::fonts::bundled_families() {
            let f = face(name).unwrap_or_else(|| panic!("{name} unreadable"));
            let w = f.width("Hello world", 100.0);
            assert!((100.0..1100.0).contains(&w), "{name}: {w}");
            assert!(f.width("iiii", 80.0) <= f.width("MMMM", 80.0), "{name}");
            // The name table agrees with the name a Look uses.
            assert_eq!(f.family(), name);
        }
        // Monospace: every glyph the same width.
        for name in ["JetBrains Mono", "Space Mono"] {
            let m = face(name).unwrap();
            assert!((m.width("iiii", 100.0) - m.width("MMMM", 100.0)).abs() < 1e-6);
        }
    }

    #[test]
    fn ink_and_line_box_come_from_the_outlines() {
        for name in crate::fonts::bundled_families() {
            let f = face(name).unwrap();
            assert!(!f.is_cff(), "{name}");
            let (asc, desc) = f.line_box(100.0);
            assert!(asc > 40.0 && desc > 0.0 && asc + desc < 200.0, "{name}");
            let (top, bottom) = f.ink_y("HELLO", 100.0).unwrap();
            // Capitals: from about the baseline up to the cap height, inside the line box.
            assert!((40.0..asc).contains(&top), "{name}: top {top}");
            assert!(bottom <= 0.0 && bottom > -20.0, "{name}: bottom {bottom}");
            // A descender reaches below the baseline further than capitals do.
            // (Bebas Neue, Bangers, Luckiest Guy and Permanent Marker draw lower case as capitals: no descenders.)
            let (_, low) = f.ink_y("gyp", 100.0).unwrap();
            let caps_only =
                ["Bebas Neue", "Bangers", "Luckiest Guy", "Permanent Marker"].contains(&name);
            assert!(caps_only || low < bottom - 5.0, "{name}: {low} {bottom}");
            // Bearings are small (an overhang is a negative one); a space has no ink.
            let (l, r) = f.ink_x("HELLO", 100.0);
            assert!(
                (-15.0..15.0).contains(&l) && (-15.0..15.0).contains(&r),
                "{name}: {l} {r}"
            );
            assert!(f.ink_y("   ", 100.0).is_none());
        }
    }

    /// What libass draws: the ink width in px of `HIMN OMNH` at a font size
    /// of 100 px, measured on a rendered still of each bundled font
    /// (`cargo run --example font_check`). The reader's advances and bearings
    /// must land within 1 % of it, which keeps the word-level layout and the
    /// boxes true for every family, whatever its units per em or metrics.
    const LIBASS_INK_WIDTH: [(&str, f64); 15] = [
        ("Anton", 253.0),
        ("Bebas Neue", 265.0),
        ("Oswald", 280.0),
        ("Archivo Black", 492.0),
        ("Lilita One", 491.0),
        ("Bangers", 229.0),
        ("Luckiest Guy", 436.0),
        ("Inter Medium", 494.0),
        ("Montserrat ExtraBold", 415.0),
        ("Poppins", 340.0),
        ("Space Grotesk", 381.0),
        ("DM Serif Display", 395.0),
        ("Permanent Marker", 443.0),
        ("JetBrains Mono", 333.0),
        ("Space Mono", 365.0),
    ];

    #[test]
    fn every_bundled_family_measures_what_libass_draws() {
        for name in crate::fonts::bundled_families() {
            assert!(LIBASS_INK_WIDTH.iter().any(|(n, _)| *n == name), "{name}");
        }
        for (name, drawn) in LIBASS_INK_WIDTH {
            let f = face(name).unwrap();
            let (l, r) = f.ink_x("HIMN OMNH", 100.0);
            let ink = f.width("HIMN OMNH", 100.0) - l - r;
            assert!(
                (ink - drawn).abs() / drawn < 0.01,
                "{name}: reader {ink:.1}, libass {drawn}"
            );
        }
    }

    #[test]
    fn a_synthetic_truetype_font_reads_as_built() {
        let f = Face::from_bytes(testfont::build("Test Sans", false)).unwrap();
        assert_eq!(
            (f.family(), f.weight(), f.is_cff()),
            ("Test Sans", 700, false)
        );
        // win = 900 + 250: 115 px at 100 px is 0.8696 px a unit.
        assert!((f.width("A", 1150.0) - 600.0).abs() < 1e-6);
        assert!((f.width("A A", 1150.0) - 1500.0).abs() < 1e-6);
        // Not in the cmap: glyph 0.
        assert!((f.width("a", 1150.0) - 500.0).abs() < 1e-6);
        let (top, bottom) = f.ink_y("A", 1150.0).unwrap();
        assert!(
            (top - 700.0).abs() < 1e-6 && bottom == 0.0,
            "{top} {bottom}"
        );
        let (l, r) = f.ink_x("A", 1150.0);
        assert!(
            (l - 50.0).abs() < 1e-6 && (r - 50.0).abs() < 1e-6,
            "{l} {r}"
        );
        assert!(f.ink_y(" ", 100.0).is_none());
    }

    #[test]
    fn a_cff_font_is_laid_out_without_ink_boxes() {
        let f = Face::from_bytes(testfont::build("Test CFF", true)).unwrap();
        assert!(f.is_cff());
        assert!((f.width("A", 1150.0) - 600.0).abs() < 1e-6);
        // Cap height for the top; a descender allowance only where one is needed.
        let (top, bottom) = f.ink_y("HAT", 1150.0).unwrap();
        assert!((top - 700.0).abs() < 1e-6 && bottom == 0.0);
        let (_, low) = f.ink_y("gyp", 1150.0).unwrap();
        assert!(low < -100.0, "{low}");
        assert_eq!(f.ink_x("HAT", 100.0), (0.0, 0.0));
        assert!(f.ink_y("  ", 100.0).is_none());
    }

    #[test]
    fn unreadable_fonts_are_errors_not_panics() {
        let good = testfont::build("Good", false);
        // Every prefix of a good font, byte-flipped copies and plain garbage.
        for cut in (0..good.len()).step_by(7) {
            let _ = Face::from_bytes(good[..cut].to_vec());
        }
        for i in (0..good.len()).step_by(3) {
            let mut bad = good.clone();
            bad[i] ^= 0xFF;
            if let Ok(f) = Face::from_bytes(bad) {
                let _ = (
                    f.width("AB Hg", 80.0),
                    f.ink_y("AB Hg", 80.0),
                    f.ink_x("AB", 80.0),
                );
            }
        }
        let mut seed = 0x1234_5678u32;
        for len in [0usize, 1, 3, 4, 11, 12, 100, 4096] {
            let junk: Vec<u8> = (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 24) as u8
                })
                .collect();
            assert!(Face::from_bytes(junk).is_err());
        }
        // A header that promises a huge table directory or table.
        let mut huge = good.clone();
        huge[4..6].copy_from_slice(&0xFFFFu16.to_be_bytes());
        assert!(Face::from_bytes(huge).unwrap_err().contains("truncated"));
        for (what, bytes) in [
            ("collection", b"ttcf\0\x01\0\0\0\0\0\x01".to_vec()),
            ("web font", b"wOFF\0\x01\0\0\0\0\0\0".to_vec()),
            ("garbage", b"MZ this is not a font at all".to_vec()),
            ("empty", Vec::new()),
        ] {
            let e = Face::from_bytes(bytes).unwrap_err();
            assert!(!e.is_empty(), "{what}");
        }
        assert!(Face::from_bytes(b"ttcf\0\x01\0\0\0\0\0\x01".to_vec())
            .unwrap_err()
            .contains(".ttc"));
    }

    #[test]
    fn a_cmap_that_lies_about_its_size_cannot_hang_the_reader() {
        // A format 12 map claiming four billion groups, and a format 4 map
        // claiming 64 KB of segments, in a 1 KB file.
        let mut f = testfont::build("Liar", false);
        let at = f.windows(4).position(|w| w == [0, 4, 0, 0x28]).unwrap();
        f[at..at + 2].copy_from_slice(&12u16.to_be_bytes());
        let face = Face::from_bytes(f.clone());
        if let Ok(face) = face {
            assert!(face.width("ABC xyz", 80.0) >= 0.0);
        }
        let mut g = testfont::build("Liar", false);
        let at = g.windows(4).position(|w| w == [0, 4, 0, 0x28]).unwrap();
        g[at + 6..at + 8].copy_from_slice(&0xFFFFu16.to_be_bytes());
        let face = Face::from_bytes(g).unwrap();
        assert!(face.width("ABC xyz", 80.0) >= 0.0);
    }
}
