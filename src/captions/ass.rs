//! Words -> ASS animated subtitles, TikTok style.
//! Port of AssBuilder.php: 8 presets designed on a 1080x1920 canvas, per-word
//! {\k} karaoke tags burned later with `ffmpeg -vf ass=...`. Other canvases
//! (4:5, 1:1, 16:9) get PlayRes = the output size, scaled type and the
//! caption block moved to the lower third (mid-frame would sit on the face).
//! An optional headline is pinned at the top for the whole clip.

use crate::whisper::Word;

pub const PLAY_W: u32 = 1080;
pub const PLAY_H: u32 = 1920;

pub struct Style {
    pub font: &'static str,
    pub size: u32,
    pub caps: bool,
    pub primary: &'static str,
    pub secondary: &'static str,
    pub outline: &'static str,
    pub back: &'static str,
    pub bold: i32,
    pub alignment: u32,
    pub margin_v: u32,
    pub border: u32,
    pub outline_w: u32,
    pub shadow: u32,
}

pub fn preset(name: &str) -> (&'static str, Style, (usize, usize)) {
    match name {
        "karaoke" => (
            "Karaoke",
            Style {
                font: "Archivo Black",
                size: 84,
                caps: true,
                primary: "&H0035E1FF",
                secondary: "&H00FFFFFF",
                outline: "&H00000000",
                back: "&H80000000",
                bold: 0,
                alignment: 5,
                margin_v: 0,
                border: 1,
                outline_w: 3,
                shadow: 0,
            },
            (3, 14),
        ),
        "hormozi" => (
            "Hormozi",
            Style {
                font: "Anton",
                size: 110,
                caps: true,
                primary: "&H00FFFFFF",
                secondary: "&H00FFFFFF",
                outline: "&H00000000",
                back: "&HCC000000",
                bold: 0,
                alignment: 2,
                margin_v: 450,
                border: 3,
                outline_w: 2,
                shadow: 0,
            },
            (3, 16),
        ),
        "minimal" => (
            "Minimal",
            Style {
                font: "Inter Medium",
                size: 64,
                caps: false,
                primary: "&H00FFFFFF",
                secondary: "&H00FFFFFF",
                outline: "&H00000000",
                back: "&H99000000",
                bold: 0,
                alignment: 2,
                margin_v: 300,
                border: 1,
                outline_w: 2,
                shadow: 1,
            },
            (4, 20),
        ),
        "beast" => (
            "Beast",
            Style {
                font: "Archivo Black",
                size: 96,
                caps: true,
                primary: "&H0000FFFF",
                secondary: "&H00FFFFFF",
                outline: "&H00000000",
                back: "&H80000000",
                bold: 0,
                alignment: 2,
                margin_v: 420,
                border: 1,
                outline_w: 4,
                shadow: 0,
            },
            (2, 12),
        ),
        "neon" => (
            "Neon",
            Style {
                font: "Anton",
                size: 88,
                caps: true,
                primary: "&H00FFFF00",
                secondary: "&H00FFFFFF",
                outline: "&H00000000",
                back: "&H80000000",
                bold: 0,
                alignment: 5,
                margin_v: 0,
                border: 1,
                outline_w: 3,
                shadow: 0,
            },
            (3, 14),
        ),
        "highlight" => (
            "Highlight",
            Style {
                font: "Anton",
                size: 100,
                caps: true,
                primary: "&H00000000",
                secondary: "&H00000000",
                outline: "&H0035E6A3",
                back: "&HCC000000",
                bold: 0,
                alignment: 2,
                margin_v: 450,
                border: 3,
                outline_w: 2,
                shadow: 0,
            },
            (3, 16),
        ),
        "ghost" => (
            "Ghost",
            Style {
                font: "Inter Medium",
                size: 60,
                caps: false,
                primary: "&H00FFFFFF",
                secondary: "&H00FFFFFF",
                outline: "&H00000000",
                back: "&H99000000",
                bold: 0,
                alignment: 2,
                margin_v: 200,
                border: 1,
                outline_w: 2,
                shadow: 1,
            },
            (4, 22),
        ),
        _ => (
            "Tiktok",
            Style {
                font: "Archivo Black",
                size: 84,
                caps: true,
                primary: "&H00FFFFFF",
                secondary: "&H00FFFFFF",
                outline: "&H00000000",
                back: "&H00552CFE",
                bold: 0,
                alignment: 2,
                margin_v: 400,
                border: 1,
                outline_w: 2,
                shadow: 2,
            },
            (3, 14),
        ),
    }
}

/// Keyword sweep color per preset (ASS &HAABBGGRR): the viral look is
/// keywords sweeping in an accent while the rest sweep in the primary.
pub fn accent_for(preset_name: &str) -> &'static str {
    match preset_name {
        "beast" => "&H00FFFF00",     // yellow text -> cyan keywords
        "neon" => "&H00FF00FF",      // cyan text -> magenta keywords
        "highlight" => "&H00FFFFFF", // black text -> white keywords
        _ => "&H0000FFFF",           // everything else -> yellow keywords
    }
}

/// Power words worth popping even mid-sentence (Hormozi-style).
pub const POWER_WORDS: &[&str] = &[
    "free",
    "secret",
    "never",
    "always",
    "new",
    "best",
    "stop",
    "money",
    "win",
    "wins",
    "viral",
    "insane",
    "crazy",
    "easy",
    "fast",
    "proven",
    "truth",
    "lies",
    "lie",
    "mistake",
    "mistakes",
    "hack",
    "million",
    "billion",
    "first",
    "last",
    "warning",
    "exposed",
    "rich",
    "guaranteed",
    "subscribe",
    "subscribed",
    "challenge",
    "challenges",
    "world",
];

/// Keyword test on the ORIGINAL casing (display may uppercase everything):
/// digits and power words always pop; proper nouns pop unless they open
/// the sentence (else every opener would light up).
pub fn is_keyword(raw: &str, sentence_start: bool) -> bool {
    let t = raw
        .trim()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    if t.is_empty() {
        return false;
    }
    if t.chars().any(|c| c.is_ascii_digit()) {
        return true;
    }
    if POWER_WORDS.contains(&t.as_str()) {
        return true;
    }
    if sentence_start {
        return false;
    }
    matches!(raw.trim().chars().next(), Some(c) if c.is_uppercase()) && t.len() > 1
}

fn ends_sentence(w: &str) -> bool {
    w.ends_with(['.', '!', '?', '…'])
}

pub fn valid_preset(name: &str) -> String {
    match name {
        "tiktok" | "karaoke" | "hormozi" | "minimal" | "beast" | "neon" | "highlight" | "ghost" => {
            name.into()
        }
        _ => "tiktok".into(),
    }
}

fn attach_punctuation(words: &[Word]) -> Vec<Word> {
    let mut out: Vec<Word> = Vec::new();
    for w in words {
        let is_punct = !w.w.is_empty() && w.w.chars().all(|c| !c.is_alphanumeric());
        if !out.is_empty() && is_punct {
            if let Some(last) = out.last_mut() {
                last.w.push_str(&w.w);
                last.e = last.e.max(w.e);
                continue;
            }
        }
        out.push(w.clone());
    }
    out
}

pub fn group(
    words: &[Word],
    max_words: usize,
    max_chars: usize,
    max_gap: f64,
    max_dur: f64,
) -> Vec<Vec<Word>> {
    let mut lines: Vec<Vec<Word>> = Vec::new();
    let mut cur: Vec<Word> = Vec::new();
    for w in words {
        let flush = if cur.is_empty() {
            false
        } else {
            let last = cur.last().unwrap();
            let text_len: usize = cur.iter().map(|x| x.w.len() + 1).sum::<usize>() + w.w.len();
            cur.len() >= max_words
                || text_len > max_chars
                || (w.s - last.e) > max_gap
                || (w.e - cur[0].s) > max_dur
        };
        if flush {
            lines.push(std::mem::take(&mut cur));
        }
        cur.push(w.clone());
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// ASS timestamp H:MM:SS.cc (centiseconds).
pub fn stamp(s: f64) -> String {
    let s = s.max(0.0);
    let cs = (s * 100.0).round() as u64;
    format!(
        "{}:{:02}:{:02}.{:02}",
        cs / 360000,
        (cs % 360000) / 6000,
        (cs % 6000) / 100,
        cs % 100
    )
}

/// Canvas and extras for one captions file.
#[derive(Debug, Clone)]
pub struct AssOpts {
    /// Output size (PlayRes).
    pub w: u32,
    pub h: u32,
    /// Headline pinned at the top for `dur` seconds.
    pub headline: Option<String>,
    pub dur: f64,
    /// A corner logo the text keeps clear of.
    pub clear: Option<Clear>,
}

/// Corner space taken by a logo: text on that band (headline at the top,
/// bottom-anchored captions at the bottom) keeps `px` off that side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clear {
    pub top: bool,
    pub left: bool,
    pub px: u32,
}

impl Default for AssOpts {
    fn default() -> Self {
        AssOpts {
            w: PLAY_W,
            h: PLAY_H,
            headline: None,
            dur: 0.0,
            clear: None,
        }
    }
}

/// Headline text: one line-ish, never mid-word (<= `max` chars + "...").
pub fn headline_text(raw: &str, max: usize) -> String {
    let t = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = t.replace(['{', '}', '\\'], "");
    if t.chars().count() <= max {
        return upper_first(&t);
    }
    let mut out = String::new();
    for w in t.split(' ') {
        if out.chars().count() + w.chars().count() + 1 > max {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    let out = out.trim_end_matches([',', ';', ':', '-']).to_string();
    format!("{}\u{2026}", upper_first(&out))
}

/// Sentence-case the first letter (transcript hooks often start lower).
fn upper_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// Build the .ass file for the default 9:16 canvas. `offset` shifts
/// dialogue stamps back (clip cuts reset to 0; full-video mode passes 0.0).
pub fn build(words: &[Word], preset_name: &str, offset: f64) -> String {
    build_for(words, preset_name, offset, &AssOpts::default())
}

/// Build the .ass file for any canvas, with an optional headline.
pub fn build_for(words: &[Word], preset_name: &str, offset: f64, o: &AssOpts) -> String {
    let name = valid_preset(preset_name);
    let (display, style, (max_w, max_c)) = preset(&name);
    let lines = group(&attach_punctuation(words), max_w, max_c, 0.6, 4.0);
    let (pw, ph) = (o.w.max(2), o.h.max(2));
    // Type is designed at 1080 wide; shorter canvases scale it down a bit.
    let k = (pw as f64 / PLAY_W as f64).min(ph as f64 / 1200.0).min(1.0);
    let px = |v: u32| ((v as f64 * k).round() as u32).max(1);
    let tall = (pw as f64 / ph as f64) < 0.6;
    let (alignment, margin_v) = if tall {
        (style.alignment, style.margin_v)
    } else if style.alignment == 5 {
        (2, (ph as f64 * 0.14).round() as u32)
    } else {
        (
            style.alignment,
            ((style.margin_v as f64 * ph as f64 / PLAY_H as f64).round() as u32)
                .max((ph as f64 * 0.1).round() as u32),
        )
    };
    let side = (40.0 * pw as f64 / PLAY_W as f64).round() as u32;
    // (MarginL, MarginR) with a logo's side widened when it shares the band.
    let clear_of = |m: u32, on_top_band: bool| -> (u32, u32) {
        match o.clear {
            Some(c) if c.top == on_top_band => {
                let wide = m.max(c.px);
                if c.left {
                    (wide, m)
                } else {
                    (m, wide)
                }
            }
            _ => (m, m),
        }
    };
    let (cap_l, cap_r) = match alignment {
        1..=3 => clear_of(side, false),
        7..=9 => clear_of(side, true),
        _ => (side, side),
    };
    let mut out = format!(
        "[Script Info]\nTitle: DigiClip {display}\nScriptType: v4.00+\nPlayResX: {pw}\nPlayResY: {ph}\nScaledBorderAndShadow: yes\nWrapStyle: 0\n\n"
    );
    out.push_str("[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n");
    out.push_str(&format!(
        "Style: {},{},{},{},{},{},{},{},0,0,0,100,100,0,0,{},{},{},{},{cap_l},{cap_r},{},1\n",
        display,
        style.font,
        px(style.size),
        style.primary,
        style.secondary,
        style.outline,
        style.back,
        style.bold,
        style.border,
        px(style.outline_w),
        style.shadow,
        alignment,
        margin_v
    ));
    // Headline: a translucent box, top center, clear of the platform's
    // top bar on tall canvases.
    let headline = o
        .headline
        .as_deref()
        .map(|h| headline_text(h, 64))
        .filter(|h| !h.is_empty() && o.dur > 0.0);
    if headline.is_some() {
        let top = (ph as f64 * if tall { 0.085 } else { 0.05 }).round() as u32;
        let (ml, mr) = clear_of((90.0 * pw as f64 / PLAY_W as f64).round() as u32, true);
        out.push_str(&format!(
            "Style: Headline,Archivo Black,{},&H00FFFFFF,&H00FFFFFF,&H38000000,&H38000000,0,0,0,0,100,100,0,0,3,{},0,8,{ml},{mr},{top},1\n",
            px(58),
            px(18),
        ));
    }
    out.push('\n');
    out.push_str("[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n");
    if let Some(h) = &headline {
        out.push_str(&format!(
            "Dialogue: 1,{},{},Headline,,0,0,0,,{{\\fad(250,0)}}{h}\n",
            stamp(0.0),
            stamp(o.dur),
        ));
    }
    let accent = accent_for(&name);
    let mut fresh = true; // next word opens a sentence (tracked across lines)
    for line in &lines {
        let mut text = String::new();
        let mut accented = false; // accent active: restore primary after
        for w in line {
            let word = if style.caps {
                w.w.to_uppercase()
            } else {
                w.w.clone()
            };
            let word = word.replace(['{', '}', '\n', '\r'], "");
            let cs = (((w.e - w.s) * 100.0).round() as i64).max(1);
            if is_keyword(&w.w, fresh) {
                text.push_str(&format!("{{\\k{cs}\\1c{accent}&}}{word} "));
                accented = true;
            } else if accented {
                text.push_str(&format!("{{\\k{cs}\\1c{}&}}{word} ", style.primary));
                accented = false;
            } else {
                text.push_str(&format!("{{\\k{cs}}}{word} "));
            }
            fresh = ends_sentence(&w.w);
        }
        out.push_str(&format!(
            "Dialogue: 0,{},{},{},,0,0,0,,{}\n",
            stamp(line[0].s - offset),
            stamp(line[line.len() - 1].e - offset),
            display,
            text.trim_end()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words() -> Vec<Word> {
        "so this is the part where it gets good"
            .split(' ')
            .enumerate()
            .map(|(i, w)| Word {
                w: w.into(),
                s: i as f64 * 0.4,
                e: i as f64 * 0.4 + 0.35,
                conf: Some(0.9),
            })
            .collect()
    }

    fn style_line<'a>(ass: &'a str, name: &str) -> Vec<&'a str> {
        ass.lines()
            .find(|l| l.starts_with(&format!("Style: {name},")))
            .unwrap_or_else(|| panic!("no {name} style in\n{ass}"))
            .split(',')
            .collect()
    }

    #[test]
    fn headline_text_is_sentence_cased_and_word_bounded() {
        assert_eq!(headline_text("  the  {big}\\ one ", 64), "The big one");
        let long = headline_text("if you guys are not hitting three rate limits a week", 30);
        assert_eq!(long, "If you guys are not hitting\u{2026}");
    }

    #[test]
    fn square_canvas_scales_and_lowers_captions() {
        let ass = build_for(
            &words(),
            "karaoke",
            0.0,
            &AssOpts {
                w: 1080,
                h: 1080,
                headline: Some("Big news".into()),
                dur: 9.0,
                clear: None,
            },
        );
        assert!(ass.contains("PlayResX: 1080\nPlayResY: 1080"));
        let cap = style_line(&ass, "Karaoke");
        // Bottom-anchored on a non-tall canvas.
        assert_eq!(cap[18], "2");
        let h = style_line(&ass, "Headline");
        assert_eq!((h[19], h[20]), ("90", "90"));
        assert!(ass
            .contains("Dialogue: 1,0:00:00.00,0:00:09.00,Headline,,0,0,0,,{\\fad(250,0)}Big news"));
    }

    #[test]
    fn text_keeps_clear_of_a_corner_logo() {
        let opts = |top, left| AssOpts {
            w: 1080,
            h: 1080,
            headline: Some("Big news".into()),
            dur: 9.0,
            clear: Some(Clear { top, left, px: 260 }),
        };
        // Top-right logo: headline's right margin widens, captions untouched.
        let ass = build_for(&words(), "karaoke", 0.0, &opts(true, false));
        let h = style_line(&ass, "Headline");
        assert_eq!((h[19], h[20]), ("90", "260"));
        let cap = style_line(&ass, "Karaoke");
        assert_eq!((cap[19], cap[20]), ("40", "40"));
        // Bottom-left logo: bottom captions' left margin widens instead.
        let ass = build_for(&words(), "karaoke", 0.0, &opts(false, true));
        let h = style_line(&ass, "Headline");
        assert_eq!((h[19], h[20]), ("90", "90"));
        let cap = style_line(&ass, "Karaoke");
        assert_eq!((cap[19], cap[20]), ("260", "40"));
    }
}
