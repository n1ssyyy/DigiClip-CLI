//! Words -> ASS animated subtitles, TikTok style.
//! Port of AssBuilder.php: PlayRes 1080x1920, 8 presets, per-word {\k}
//! karaoke tags burned later with `ffmpeg -vf ass=...`.

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

/// Build the .ass file. `offset` shifts dialogue stamps back (clip cuts
/// reset to 0; full-video mode passes 0.0).
pub fn build(words: &[Word], preset_name: &str, offset: f64) -> String {
    let name = valid_preset(preset_name);
    let (display, style, (max_w, max_c)) = preset(&name);
    let lines = group(&attach_punctuation(words), max_w, max_c, 0.6, 4.0);
    let mut out = format!(
        "[Script Info]\nTitle: DigiClip {display}\nScriptType: v4.00+\nPlayResX: {PLAY_W}\nPlayResY: {PLAY_H}\nScaledBorderAndShadow: yes\nWrapStyle: 0\n\n"
    );
    out.push_str("[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n");
    out.push_str(&format!(
        "Style: {},{},{},{},{},{},{},{},0,0,0,100,100,0,0,{},{},{},{},40,40,{},1\n\n",
        display,
        style.font,
        style.size,
        style.primary,
        style.secondary,
        style.outline,
        style.back,
        style.bold,
        style.border,
        style.outline_w,
        style.shadow,
        style.alignment,
        style.margin_v
    ));
    out.push_str("[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n");
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
