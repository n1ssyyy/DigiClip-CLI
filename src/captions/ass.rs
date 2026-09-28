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

/// A caption line never runs across a sentence end: "TEACHER. RAISE"
/// reads as one thought. Splits after any word that ends a sentence.
fn split_sentences(lines: Vec<Vec<Word>>) -> Vec<Vec<Word>> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let mut cur = Vec::new();
        for w in line {
            let end = ends_sentence(&w.w);
            cur.push(w);
            if end {
                out.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

/// A line too short to read joins a neighbor from the same sentence
/// ("I'M" + "KINDERGARTEN"), forward first, if the pair stays compact
/// (a little over the preset's width is fine: it wraps, it doesn't flash).
fn merge_flashes(lines: Vec<Vec<Word>>, max_c: usize) -> Vec<Vec<Word>> {
    let chars = |l: &[Word]| l.iter().map(|w| w.w.chars().count() + 1).sum::<usize>();
    let span = |l: &[Word]| l[l.len() - 1].e - l[0].s;
    let fits = |a: &[Word], b: &[Word]| {
        !ends_sentence(&a[a.len() - 1].w)
            && b[0].s - a[a.len() - 1].e < 0.6
            && chars(a) + chars(b) <= max_c + 8
    };
    let mut out: Vec<Vec<Word>> = Vec::with_capacity(lines.len());
    let mut it = lines.into_iter().peekable();
    while let Some(mut line) = it.next() {
        if span(&line) < MIN_LINE_S {
            if let Some(next) = it.peek() {
                if fits(&line, next) {
                    line.extend(it.next().unwrap_or_default());
                    out.push(line);
                    continue;
                }
            }
            if let Some(prev) = out.last_mut() {
                if fits(prev, &line) {
                    prev.extend(line);
                    continue;
                }
            }
        }
        out.push(line);
    }
    out
}

/// Shortest time a caption line stays up (s): a flash is unreadable and
/// its pop/fade would flicker.
const MIN_LINE_S: f64 = 0.45;
/// Gaps shorter than this between lines are bridged (s): the line holds
/// until the next one replaces it instead of blinking off.
const BRIDGE_S: f64 = 0.35;

/// On-screen span per line: its words' span, held through short gaps and
/// to at least `MIN_LINE_S`, never overlapping the next line or `end`.
fn hold_lines(lines: &[Vec<Word>], end: f64) -> Vec<(f64, f64)> {
    (0..lines.len())
        .map(|i| {
            let (s, e) = (lines[i][0].s, lines[i][lines[i].len() - 1].e);
            let next = lines.get(i + 1).map_or(end, |l| l[0].s).min(end);
            let e = if next - e < BRIDGE_S {
                next
            } else {
                e.max(s + MIN_LINE_S).min(next)
            };
            (s, e.max(lines[i][lines[i].len() - 1].e.min(end)))
        })
        .collect()
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

/// Caption motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anim {
    /// Each line pops in (quick scale overshoot + fade), keywords bump as
    /// they're spoken.
    #[default]
    Pop,
    /// Pop, and words appear one by one as they're spoken.
    Words,
    /// No motion: lines cut in and out (the pre-2.5 look).
    Static,
}

impl Anim {
    /// `pop` | `words` | `none` (unknown values fall back to `pop`).
    pub fn parse(s: &str) -> Anim {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "static" => Anim::Static,
            "words" | "word" | "reveal" => Anim::Words,
            _ => Anim::Pop,
        }
    }
}

/// Headline budget (chars): two short lines on the card.
const HEADLINE_MAX: usize = 44;
/// Headline type and accent on the white card (&HAABBGGRR).
const HEADLINE_INK: &str = "&H00111111";
const HEADLINE_ACCENT: &str = "&H001E3CFF";
/// Caption line entrance: fade in/out (ms) and a scale pop that settles by
/// `POP_MS` — every word's scale is relative to this.
const LINE_IN: i64 = 80;
const LINE_OUT: i64 = 60;
const POP_MS: i64 = 200;
const LINE_POP: &str =
    "\\fscx84\\fscy84\\t(0,110,\\fscx105\\fscy105)\\t(110,200,\\fscx100\\fscy100)";

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
    /// Caption motion.
    pub anim: Anim,
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
            anim: Anim::default(),
        }
    }
}

/// Words a headline never ends on (a cut there reads as unfinished).
const DANGLING: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "so", "to", "of", "in", "on", "at", "for", "with",
    "from", "by", "as", "is", "are", "was", "were", "be", "that", "this", "my", "your", "our",
    "their", "his", "her", "its", "if", "when", "than", "then", "because", "about", "into", "i",
    "you", "we", "they", "he", "she", "it", "not", "just", "very", "really", "like", "all",
];

fn bare(w: &str) -> String {
    w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'')
        .to_lowercase()
}

/// Headline text: clean, sentence-cased, at most `max` chars and never
/// cut mid-thought — a long line is cut at its last clause break that
/// fits, else at a word, minus any dangling tail ("…of the"). No
/// ellipsis, no trailing full stop.
pub fn headline_text(raw: &str, max: usize) -> String {
    let t = raw
        .replace(['{', '}', '\\', '*', '"', '\u{201C}', '\u{201D}'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let words: Vec<&str> = t.split(' ').filter(|w| !w.is_empty()).collect();
    let mut n = 0;
    let mut len = 0;
    for w in &words {
        let l = w.chars().count() + usize::from(n > 0);
        if len + l > max {
            break;
        }
        len += l;
        n += 1;
    }
    let mut keep = &words[..n];
    if n < words.len() {
        // Last clause break that keeps at least 3 words.
        if let Some(i) = (3..=n)
            .rev()
            .find(|&i| keep[i - 1].ends_with([',', ';', ':', '.', '!', '?', '\u{2014}']))
        {
            keep = &keep[..i];
        } else {
            while keep.len() > 2 && DANGLING.contains(&bare(keep[keep.len() - 1]).as_str()) {
                keep = &keep[..keep.len() - 1];
            }
        }
    }
    let out = keep.join(" ");
    let out = out
        .trim_end_matches([',', ';', ':', '.', '-', '\u{2014}', '\u{2026}'])
        .trim();
    upper_first(out)
}

/// Headline as ASS text: two balanced lines once it's long enough to
/// wrap, one keyword in the accent color.
fn headline_markup(h: &str, ink: &str, accent: &str) -> String {
    let words: Vec<&str> = h.split(' ').collect();
    // Accent: a keyword, else the longest content word.
    let pick = (1..words.len())
        .find(|&i| is_keyword(words[i], false))
        .or_else(|| {
            (0..words.len())
                .filter(|&i| {
                    let b = bare(words[i]);
                    b.chars().count() >= 5 && !DANGLING.contains(&b.as_str())
                })
                .max_by_key(|&i| (words[i].chars().count(), usize::MAX - i))
        });
    // Balanced break: the split that minimizes the longer line.
    let total = h.chars().count();
    let brk = if total > 18 && words.len() > 1 {
        (1..words.len()).min_by_key(|&i| {
            let a = words[..i].join(" ").chars().count();
            a.max(total - a - 1)
        })
    } else {
        None
    };
    let mut out = String::new();
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            out.push_str(if Some(i) == brk { "\\N" } else { " " });
        }
        if Some(i) == pick {
            out.push_str(&format!("{{\\1c{accent}&}}{w}{{\\1c{ink}&}}"));
        } else {
            out.push_str(w);
        }
    }
    out
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
    let moving = o.anim != Anim::Static;
    let mut lines = group(&attach_punctuation(words), max_w, max_c, 0.6, 4.0);
    if moving {
        lines = merge_flashes(split_sentences(lines), max_c);
    }
    let spans = if moving {
        hold_lines(
            &lines,
            if o.dur > 0.0 {
                o.dur + offset
            } else {
                f64::INFINITY
            },
        )
    } else {
        lines.iter().map(|l| (l[0].s, l[l.len() - 1].e)).collect()
    };
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
    // Headline: one solid white card (a single box behind both lines —
    // BorderStyle 4) with dark type and one accented word, top center,
    // clear of the platform's top bar on tall canvases.
    let headline = o
        .headline
        .as_deref()
        .map(|h| headline_text(h, HEADLINE_MAX))
        .filter(|h| !h.is_empty() && o.dur > 0.0);
    if headline.is_some() {
        let top = (ph as f64 * if tall { 0.085 } else { 0.05 }).round() as u32;
        let (ml, mr) = clear_of((90.0 * pw as f64 / PLAY_W as f64).round() as u32, true);
        out.push_str(&format!(
            "Style: Headline,Archivo Black,{},{HEADLINE_INK},{HEADLINE_INK},&H00FFFFFF,&H00FFFFFF,0,0,0,0,100,100,0,0,4,{},0,8,{ml},{mr},{top},1\n",
            px(64),
            px(24),
        ));
    }
    out.push('\n');
    out.push_str("[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n");
    if let Some(h) = &headline {
        // Pops in: a quick overshoot, then settles.
        out.push_str(&format!(
            "Dialogue: 1,{},{},Headline,,0,0,0,,{{\\fad(160,0)\\fscx72\\fscy72\\t(0,200,\\fscx106\\fscy106)\\t(200,340,\\fscx100\\fscy100)}}{}\n",
            stamp(0.0),
            stamp(o.dur),
            headline_markup(h, HEADLINE_INK, HEADLINE_ACCENT),
        ));
    }
    let accent = accent_for(&name);
    let mut fresh = true; // next word opens a sentence (tracked across lines)
    for (line, &(t0, t1)) in lines.iter().zip(&spans) {
        let mut text = String::new();
        let mut accented = false; // accent active: restore primary after
        let mut bumped = false; // a keyword bump is in effect: re-base the scale
        for (i, w) in line.iter().enumerate() {
            let word = if style.caps {
                w.w.to_uppercase()
            } else {
                w.w.clone()
            };
            let word = word.replace(['{', '}', '\n', '\r'], "");
            let cs = (((w.e - w.s) * 100.0).round() as i64).max(1);
            let key = is_keyword(&w.w, fresh);
            // Offset into the line (ms): \t times are event-relative.
            let dt = ((w.s - line[0].s) * 1000.0).round().max(0.0) as i64;
            let mut tags = format!("\\k{cs}");
            if moving && i == 0 {
                tags.insert_str(0, &format!("\\fad({LINE_IN},{LINE_OUT}){LINE_POP}"));
            }
            if moving && bumped {
                // Scale state carries to later text: restore the line's own.
                tags.push_str(LINE_POP);
                bumped = false;
            }
            if key {
                tags.push_str(&format!("\\1c{accent}&"));
                accented = true;
            } else if accented {
                tags.push_str(&format!("\\1c{}&", style.primary));
                accented = false;
            }
            if moving && key && dt >= POP_MS {
                tags.push_str(&format!(
                    "\\t({dt},{},\\fscx114\\fscy114)\\t({},{},\\fscx100\\fscy100)",
                    dt + 90,
                    dt + 90,
                    dt + 240
                ));
                bumped = true;
            }
            if o.anim == Anim::Words && i > 0 {
                tags.push_str(&format!("\\alpha&HFF&\\t({dt},{},\\alpha&H00&)", dt + 80));
            }
            text.push_str(&format!("{{{tags}}}{word} "));
            fresh = ends_sentence(&w.w);
        }
        out.push_str(&format!(
            "Dialogue: 0,{},{},{},,0,0,0,,{}\n",
            stamp(t0 - offset),
            stamp(t1 - offset),
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
        assert_eq!(headline_text("  the  {big}\\ *one*. ", 64), "The big one");
        // Cut at a word, minus the dangling tail; never an ellipsis.
        let long = headline_text("the secret to growing fast is doing less of it", 30);
        assert_eq!(long, "The secret to growing fast");
        // Cut at the last clause break that fits.
        let long = headline_text("nobody talks about this, but it changes everything", 40);
        assert_eq!(long, "Nobody talks about this");
        assert_eq!(
            headline_text("Why is nobody doing this?", 44),
            "Why is nobody doing this?"
        );
    }

    #[test]
    fn headline_is_one_card_on_two_balanced_lines() {
        let m = headline_markup("The secret to growing fast", "&H00111111", "&H001E3CFF");
        assert_eq!(
            m,
            "The {\\1c&H001E3CFF&}secret{\\1c&H00111111&} to\\Ngrowing fast"
        );
        let m = headline_markup("I made $1 million", "&H00111111", "&H001E3CFF");
        assert_eq!(m, "I made {\\1c&H001E3CFF&}$1{\\1c&H00111111&} million");
        let ass = build_for(
            &words(),
            "karaoke",
            0.0,
            &AssOpts {
                headline: Some("Big news".into()),
                dur: 9.0,
                ..AssOpts::default()
            },
        );
        let h = style_line(&ass, "Headline");
        assert_eq!(h[15], "4"); // one box behind the whole event
    }

    fn first_line(ws: &[Word], anim: Anim) -> String {
        let ass = build_for(
            ws,
            "minimal",
            0.0,
            &AssOpts {
                anim,
                ..AssOpts::default()
            },
        );
        ass.lines()
            .find(|l| l.starts_with("Dialogue: 0,"))
            .unwrap()
            .to_string()
    }

    #[test]
    fn caption_motion_modes() {
        // Static: the classic karaoke line, untouched.
        let l = first_line(&words(), Anim::Static);
        assert!(
            l.ends_with(",,{\\k35}so {\\k35}this {\\k35}is {\\k35}the"),
            "{l}"
        );
        // Pop: the line pops in and fades.
        let l = first_line(&words(), Anim::Pop);
        assert!(
            l.contains(",,{\\fad(80,60)\\fscx84\\fscy84\\t(0,110,"),
            "{l}"
        );
        assert!(!l.contains("alpha"));
        // Words: later words appear as they're spoken.
        let l = first_line(&words(), Anim::Words);
        assert!(
            l.contains("{\\k35\\alpha&HFF&\\t(400,480,\\alpha&H00&)}this"),
            "{l}"
        );
        assert_eq!(Anim::parse("none"), Anim::Static);
        assert_eq!(Anim::parse("WORDS"), Anim::Words);
        assert_eq!(Anim::parse("?"), Anim::Pop);
    }

    #[test]
    fn animated_lines_split_at_sentences_and_never_flash() {
        let w = |t: &str, s: f64, e: f64| Word {
            w: t.into(),
            s,
            e,
            conf: None,
        };
        // "teacher." ends a sentence; "raise" starts the next line.
        let ws = vec![
            w("kindergarten", 0.0, 0.4),
            w("teacher.", 0.45, 0.6),
            w("raise", 0.65, 0.7),
            w("hands", 2.0, 2.1),
        ];
        let ass = build_for(&ws, "minimal", 0.0, &AssOpts::default());
        let d: Vec<&str> = ass
            .lines()
            .filter(|l| l.starts_with("Dialogue: 0,"))
            .collect();
        assert_eq!(d.len(), 3, "{ass}");
        // Short gap bridged; a lone flash held to the minimum.
        assert!(
            d[0].starts_with("Dialogue: 0,0:00:00.00,0:00:00.65,"),
            "{}",
            d[0]
        );
        assert!(
            d[1].starts_with("Dialogue: 0,0:00:00.65,0:00:01.10,"),
            "{}",
            d[1]
        );
        assert!(
            d[2].starts_with("Dialogue: 0,0:00:02.00,0:00:02.45,"),
            "{}",
            d[2]
        );
        // Static keeps the words' own spans.
        let ass = build_for(
            &ws,
            "minimal",
            0.0,
            &AssOpts {
                anim: Anim::Static,
                ..AssOpts::default()
            },
        );
        assert!(ass.contains("Dialogue: 0,0:00:00.45,0:00:00.70,"), "{ass}");
    }

    #[test]
    fn keywords_bump_and_restore_the_line_scale() {
        let ws: Vec<Word> = "made a million now"
            .split(' ')
            .enumerate()
            .map(|(i, w)| Word {
                w: w.into(),
                s: i as f64 * 0.4,
                e: i as f64 * 0.4 + 0.35,
                conf: Some(0.9),
            })
            .collect();
        let l = first_line(&ws, Anim::Pop);
        assert!(
            l.contains(
                "\\1c&H0000FFFF&\\t(800,890,\\fscx114\\fscy114)\\t(890,1040,\\fscx100\\fscy100)}million"
            ),
            "{l}"
        );
        assert!(
            l.contains(&format!("{{\\k35{LINE_POP}\\1c&H00FFFFFF&}}now")),
            "{l}"
        );
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
                anim: Anim::Pop,
            },
        );
        assert!(ass.contains("PlayResX: 1080\nPlayResY: 1080"));
        let cap = style_line(&ass, "Karaoke");
        // Bottom-anchored on a non-tall canvas.
        assert_eq!(cap[18], "2");
        let h = style_line(&ass, "Headline");
        assert_eq!((h[19], h[20]), ("90", "90"));
        assert!(ass.contains(
            "Dialogue: 1,0:00:00.00,0:00:09.00,Headline,,0,0,0,,{\\fad(160,0)\\fscx72\\fscy72\\t(0,200,\\fscx106\\fscy106)\\t(200,340,\\fscx100\\fscy100)}Big news"
        ));
    }

    #[test]
    fn text_keeps_clear_of_a_corner_logo() {
        let opts = |top, left| AssOpts {
            w: 1080,
            h: 1080,
            headline: Some("Big news".into()),
            dur: 9.0,
            clear: Some(Clear { top, left, px: 260 }),
            anim: Anim::Pop,
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
