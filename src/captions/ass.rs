//! Words -> ASS animated subtitles, TikTok style.
//! Port of AssBuilder.php: 8 presets designed on a 1080x1920 canvas, per-word
//! {\k} karaoke tags burned later with `ffmpeg -vf ass=...`. Other canvases
//! (4:5, 1:1, 16:9) get PlayRes = the output size, scaled type and the
//! caption block moved to the lower third (mid-frame would sit on the face).
//! An optional headline is pinned at the top for the whole clip.

use crate::look::{BoxLook, CaptionsLook, Case, HeadlineAnim, HeadlineLook};
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
/// `word_cap` is a hard words-per-line limit set by a Look.
fn merge_flashes(lines: Vec<Vec<Word>>, max_c: usize, word_cap: Option<usize>) -> Vec<Vec<Word>> {
    let chars = |l: &[Word]| l.iter().map(|w| w.w.chars().count() + 1).sum::<usize>();
    let span = |l: &[Word]| l[l.len() - 1].e - l[0].s;
    let fits = |a: &[Word], b: &[Word]| {
        !ends_sentence(&a[a.len() - 1].w)
            && b[0].s - a[a.len() - 1].e < 0.6
            && chars(a) + chars(b) <= max_c + 8
            && word_cap.is_none_or(|n| a.len() + b.len() <= n)
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
    /// Lines fade in and out: no scale pop, no keyword bump.
    Fade,
    /// Lines rise a short distance into place while fading in; no pop.
    Slide,
    /// Pop with a bigger overshoot and a second small settle.
    Bounce,
}

impl Anim {
    /// `pop` | `words` | `none` | `fade` | `slide` | `bounce`; `None` for
    /// anything else.
    pub fn from_name(s: &str) -> Option<Anim> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pop" => Some(Anim::Pop),
            "none" | "off" | "static" => Some(Anim::Static),
            "words" | "word" | "reveal" => Some(Anim::Words),
            "fade" => Some(Anim::Fade),
            "slide" => Some(Anim::Slide),
            "bounce" => Some(Anim::Bounce),
            _ => None,
        }
    }

    /// Like [`Anim::from_name`]; unknown values fall back to `pop`.
    pub fn parse(s: &str) -> Anim {
        Anim::from_name(s).unwrap_or_default()
    }

    /// The scale entrance (tags, ms until it has settled), for the motions
    /// that have one.
    fn scale_in(self) -> Option<(&'static str, i64)> {
        match self {
            Anim::Pop | Anim::Words => Some((LINE_POP, POP_MS)),
            Anim::Bounce => Some((LINE_BOUNCE, BOUNCE_MS)),
            _ => None,
        }
    }

    /// Line fade in / out (ms).
    fn fade(self) -> (i64, i64) {
        match self {
            Anim::Fade => (200, 140),
            Anim::Slide => (160, LINE_OUT),
            _ => (LINE_IN, LINE_OUT),
        }
    }
}

/// Headline budget (chars): two short lines on the card.
const HEADLINE_MAX: usize = 48;
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
/// Bounce entrance: a deeper start, a bigger overshoot, an undershoot and a
/// last small settle, done by `BOUNCE_MS`.
const LINE_BOUNCE: &str = "\\fscx70\\fscy70\\t(0,120,\\fscx122\\fscy122)\\t(120,210,\\fscx94\\fscy94)\\t(210,280,\\fscx104\\fscy104)\\t(280,340,\\fscx100\\fscy100)";
const BOUNCE_MS: i64 = 340;
/// Slide entrance: the line starts this share of the frame height lower
/// and rises into place over `SLIDE_MS`.
const SLIDE_RISE: f64 = 0.022;
const SLIDE_MS: i64 = 260;
/// Padding (px at 1080 wide) of a box a Look adds to a style without one.
const BOX_PAD: f64 = 14.0;
/// Headline type size and card padding (px at 1080 wide, before `size`).
const HEADLINE_PX: f64 = 64.0;
const HEADLINE_PAD: f64 = 24.0;
/// Outline (px at 1080 wide, before `size`) of a headline without a card.
const HEADLINE_EDGE: f64 = 5.0;
/// Line height of the headline face as a share of its size, as libass lays
/// it out (measured on a render; only used to find the middle of today's
/// card and to keep a placed card inside the frame).
const HEADLINE_LINE: f64 = 1.0;
/// A headline's fade in / out (ms) when the Look asks for one.
const HEADLINE_FADE_IN: i64 = 200;
const HEADLINE_FADE_OUT: i64 = 200;

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
    /// Split screen: captions sit on the seam between the halves.
    pub seam: bool,
    /// The Look's captions section: overrides over the style (position,
    /// size, font, colours, box, words per line, motion).
    pub captions: Option<CaptionsLook>,
    /// The Look's headline section (position, size, colours, card, motion,
    /// time on screen). Only matters while `headline` is set.
    pub headline_look: Option<HeadlineLook>,
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
            seam: false,
            captions: None,
            headline_look: None,
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
    // "I", "I'm"… are capitalized but never the point.
    let pick = (1..words.len())
        .find(|&i| {
            is_keyword(words[i], false)
                && !bare(words[i]).starts_with("i'")
                && bare(words[i]) != "i"
        })
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

/// Where a Look puts the caption block: its centre in output pixels and the
/// left/right margin that keeps the wrapped text inside the frame.
struct Place {
    x: i64,
    y: i64,
    margin: u32,
}

/// A number for an ASS style field: whole values print bare ("3"), others
/// to at most two decimals.
fn ass_num(v: f64) -> String {
    let r = (v * 100.0).round() / 100.0;
    if r.fract() == 0.0 {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

/// The colour with its alpha byte replaced (`&HAABBGGRR`).
fn with_alpha(c: &str, alpha: u8) -> String {
    format!("&H{alpha:02X}{}", c.get(4..).unwrap_or("000000"))
}

/// Build the .ass file for any canvas, with an optional headline.
pub fn build_for(words: &[Word], preset_name: &str, offset: f64, o: &AssOpts) -> String {
    let name = valid_preset(preset_name);
    let (display, style, (max_w, max_c)) = preset(&name);
    let cap = o.captions.as_ref();
    // The Look's motion wins over the flat option.
    let anim = cap.and_then(|c| c.anim).unwrap_or(o.anim);
    let moving = anim != Anim::Static;
    // Words per line: the style's character budget stretches with the word
    // limit so a higher limit is reachable.
    let (max_w, max_c) = match cap.and_then(|c| c.max_words) {
        Some(n) if n > max_w => (n, (max_c * n).div_ceil(max_w)),
        Some(n) => (n, max_c),
        None => (max_w, max_c),
    };
    let mut lines = group(&attach_punctuation(words), max_w, max_c, 0.6, 4.0);
    if cap.is_some_and(|c| c.show == Some(false)) {
        lines.clear();
    }
    if moving {
        lines = merge_flashes(split_sentences(lines), max_c, cap.and_then(|c| c.max_words));
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
    let px = |v: f64| ((v * k).round() as u32).max(1);
    let tall = (pw as f64 / ph as f64) < 0.6;
    let (alignment, margin_v) = if o.seam {
        (5, 0)
    } else if tall {
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
    // An explicit position wins over the style's alignment, the seam rule
    // and the logo clearance. The block is centred on the point and wraps
    // inside the nearer frame edge; very close to an edge the centre is
    // nudged inward so a few words still fit.
    let place = cap.filter(|c| c.x.is_some() || c.y.is_some()).map(|c| {
        let (w, s) = (pw as f64, side as f64);
        let cx = c.x.unwrap_or(0.5) * w;
        let half = (cx.min(w - cx) - s).max(0.2 * w);
        let cx = cx.max(half + s).min(w - half - s);
        Place {
            x: cx.round() as i64,
            y: (c.y.unwrap_or(0.5) * ph as f64).round() as i64,
            margin: (w / 2.0 - half).round().max(0.0) as u32,
        }
    });
    let (alignment, margin_v, cap_l, cap_r) = match &place {
        Some(p) => (5, 0, p.margin, p.margin),
        None => (alignment, margin_v, cap_l, cap_r),
    };
    // Style values: the preset's, with the Look's overrides on top.
    let font = cap.and_then(|c| c.font).unwrap_or(style.font);
    let font_px = px(style.size as f64 * cap.and_then(|c| c.size).unwrap_or(1.0));
    let caps = match cap.and_then(|c| c.case) {
        Some(Case::Upper) => true,
        Some(Case::AsIs) => false,
        None => style.caps,
    };
    // Sung colour: `active`, else `color`; unsung colour: `color`.
    let primary = cap
        .and_then(|c| c.active.or(c.color))
        .map_or_else(|| style.primary.to_string(), |c| c.ass(0));
    let secondary = cap
        .and_then(|c| c.color)
        .map_or_else(|| style.secondary.to_string(), |c| c.ass(0));
    // Box (BorderStyle 3 draws it in the outline colour, `outline_w` wide).
    let box_alpha = |op: f64| ((1.0 - op.clamp(0.0, 1.0)) * 255.0).round() as u8;
    let mut border = style.border;
    let mut outline = style.outline.to_string();
    let mut outline_w = px(style.outline_w as f64) as f64;
    match cap.and_then(|c| c.box_) {
        Some(BoxLook::None) => border = 1,
        Some(BoxLook::Color(c)) => {
            let op = cap.and_then(|c| c.box_opacity).unwrap_or(1.0);
            outline = c.ass(box_alpha(op));
            if border != 3 {
                border = 3;
                outline_w = px(BOX_PAD) as f64;
            }
        }
        None => {
            if let (3, Some(op)) = (border, cap.and_then(|c| c.box_opacity)) {
                outline = with_alpha(&outline, box_alpha(op));
            }
        }
    }
    if border != 3 {
        if let Some(c) = cap.and_then(|c| c.outline) {
            outline = c.ass(0);
        }
    }
    if let Some(v) = cap.and_then(|c| c.outline_w) {
        outline_w = v * k;
    }
    let shadow = cap
        .and_then(|c| c.shadow)
        .map_or(style.shadow as f64, |v| v * k);
    let mut out = format!(
        "[Script Info]\nTitle: DigiClip {display}\nScriptType: v4.00+\nPlayResX: {pw}\nPlayResY: {ph}\nScaledBorderAndShadow: yes\nWrapStyle: 0\n\n"
    );
    out.push_str("[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n");
    let row = |name: &str, prim: &str, sec: &str, outl: &str, border: u32, ow: f64, sh: f64| {
        format!(
            "Style: {},{},{},{},{},{},{},{},0,0,0,100,100,0,0,{},{},{},{},{cap_l},{cap_r},{},1\n",
            name,
            font,
            font_px,
            prim,
            sec,
            outl,
            style.back,
            style.bold,
            border,
            ass_num(ow),
            ass_num(sh),
            alignment,
            margin_v
        )
    };
    // libass draws a translucent box once per run of text, and the karaoke
    // runs overlap by the padding: a dark seam between words. So a see-through
    // box gets its own style and one event per line (a single run of
    // invisible text under the real one); an opaque box needs neither.
    let soft_box = border == 3 && !outline.starts_with("&H00");
    let box_name = format!("{display}Box");
    if soft_box {
        out.push_str(&row(
            display,
            &primary,
            &secondary,
            "&H00000000",
            1,
            0.0,
            0.0,
        ));
        out.push_str(&row(
            &box_name,
            &with_alpha(&primary, 0xFF),
            &with_alpha(&secondary, 0xFF),
            &outline,
            3,
            outline_w,
            shadow,
        ));
    } else {
        out.push_str(&row(
            display, &primary, &secondary, &outline, border, outline_w, shadow,
        ));
    }
    // Headline: one solid white card (a single box behind both lines —
    // BorderStyle 4) with dark type and one accented word, top center,
    // clear of the platform's top bar on tall canvases. A Look can move it,
    // resize it, recolour it, drop the card, change the entrance and end it
    // early.
    let headline = o
        .headline
        .as_deref()
        .map(|h| headline_text(h, HEADLINE_MAX))
        .filter(|h| !h.is_empty() && o.dur > 0.0);
    let mut headline_event = None;
    if let Some(h) = &headline {
        let hl = o.headline_look.clone().unwrap_or_default();
        let size = hl.size.unwrap_or(1.0);
        let top = (ph as f64 * if tall { 0.085 } else { 0.05 }).round() as u32;
        let side_h = (90.0 * pw as f64 / PLAY_W as f64).round() as u32;
        let no_card = hl.card == Some(BoxLook::None);
        let ink_rgb = hl
            .ink
            .or(no_card.then_some(crate::look::Rgb(255, 255, 255)));
        let ink = ink_rgb.map_or_else(|| HEADLINE_INK.to_string(), |c| c.ass(0));
        let accent = hl
            .accent
            .map_or_else(|| HEADLINE_ACCENT.to_string(), |c| c.ass(0));
        let (border, edge, edge_w) = if no_card {
            // No card: a dark outline keeps the type readable on footage (a
            // light one when the ink itself is dark).
            let dark_ink = ink_rgb.is_some_and(|c| {
                (0.2126 * c.0 as f64 + 0.7152 * c.1 as f64 + 0.0722 * c.2 as f64) < 90.0
            });
            let e = if dark_ink { "&H00FFFFFF" } else { "&H00000000" };
            (1, e.to_string(), px(HEADLINE_EDGE * size))
        } else {
            let e = match hl.card {
                Some(BoxLook::Color(c)) => c.ass(0),
                _ => "&H00FFFFFF".to_string(),
            };
            (4, e, px(HEADLINE_PAD * size))
        };
        let font_px = px(HEADLINE_PX * size);
        let markup = headline_markup(h, &ink, &accent);
        // Where: today's top-centre, or anchored on a point by its middle.
        let place = (hl.x.is_some() || hl.y.is_some()).then(|| {
            let (w, s) = (pw as f64, side_h as f64);
            let cx = hl.x.unwrap_or(0.5) * w;
            let half = (cx.min(w - cx) - s).max(0.2 * w);
            let cx = cx.max(half + s).min(w - half - s);
            let lines = 1 + markup.matches("\\N").count();
            let block = lines as f64 * font_px as f64 * HEADLINE_LINE;
            let cy = hl.y.map_or(top as f64 + block / 2.0, |y| y * ph as f64);
            // Whole card inside the frame.
            let reach = block / 2.0 + edge_w as f64;
            let cy = cy.max(reach).min((ph as f64 - reach).max(reach));
            Place {
                x: cx.round() as i64,
                y: cy.round() as i64,
                margin: (w / 2.0 - half).round().max(0.0) as u32,
            }
        });
        let (ml, mr, align, margin_v) = match &place {
            Some(p) => (p.margin, p.margin, 5, 0),
            None => {
                let (ml, mr) = clear_of(side_h, true);
                (ml, mr, 8, top)
            }
        };
        out.push_str(&format!(
            "Style: Headline,Archivo Black,{font_px},{ink},{ink},{edge},{edge},0,0,0,0,100,100,0,0,{border},{edge_w},0,{align},{ml},{mr},{margin_v},1\n",
        ));
        // Time on screen: the whole clip, or `seconds` of it with a short
        // fade out, never past the clip.
        let (end, fout) = match hl.seconds.filter(|&s| s > 0.0) {
            Some(s) if s < o.dur => (s, HEADLINE_FADE_OUT.min((s * 1000.0) as i64)),
            _ => (o.dur, 0),
        };
        let mut tags = String::new();
        if let Some(p) = &place {
            tags.push_str(&format!("\\pos({},{})", p.x, p.y));
        }
        match hl.anim.unwrap_or(HeadlineAnim::Pop) {
            // Pops in: a quick overshoot, then settles.
            HeadlineAnim::Pop => tags.push_str(&format!(
                "\\fad(160,{fout})\\fscx72\\fscy72\\t(0,200,\\fscx106\\fscy106)\\t(200,340,\\fscx100\\fscy100)"
            )),
            HeadlineAnim::Fade => tags.push_str(&format!("\\fad({HEADLINE_FADE_IN},{fout})")),
            HeadlineAnim::None if fout > 0 => tags.push_str(&format!("\\fad(0,{fout})")),
            HeadlineAnim::None => {}
        }
        let tags = if tags.is_empty() {
            tags
        } else {
            format!("{{{tags}}}")
        };
        headline_event = Some(format!(
            "Dialogue: 1,{},{},Headline,,0,0,0,,{tags}{markup}\n",
            stamp(0.0),
            stamp(end),
        ));
    }
    out.push('\n');
    out.push_str("[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n");
    if let Some(e) = &headline_event {
        out.push_str(e);
    }
    // What opens every line: its placement and entrance. A slide moves
    // the line, so it always needs a point: the requested one, else where
    // the style's alignment and margins put the block.
    let scale = anim.scale_in();
    let mut lead = String::new();
    if anim == Anim::Slide {
        let (x, y) = place.as_ref().map_or_else(
            || {
                let (w, h) = (pw as i64, ph as i64);
                let x = match alignment {
                    1 | 4 | 7 => cap_l as i64,
                    3 | 6 | 9 => w - cap_r as i64,
                    _ => cap_l as i64 + (w - cap_l as i64 - cap_r as i64) / 2,
                };
                let y = match alignment {
                    1..=3 => h - margin_v as i64,
                    4..=6 => h / 2,
                    _ => margin_v as i64,
                };
                (x, y)
            },
            |p| (p.x, p.y),
        );
        let rise = (ph as f64 * SLIDE_RISE).round() as i64;
        lead.push_str(&format!("\\move({x},{},{x},{y},0,{SLIDE_MS})", y + rise));
    } else if let Some(p) = &place {
        lead.push_str(&format!("\\pos({},{})", p.x, p.y));
    }
    if moving {
        let (fin, fout) = anim.fade();
        lead.push_str(&format!("\\fad({fin},{fout})"));
        if let Some((tags, _)) = scale {
            lead.push_str(tags);
        }
    }
    let accent = cap
        .and_then(|c| c.accent)
        .map_or_else(|| accent_for(&name).to_string(), |c| c.ass(0));
    let mut fresh = true; // next word opens a sentence (tracked across lines)
    for (line, &(t0, t1)) in lines.iter().zip(&spans) {
        let mut text = String::new();
        let mut plain = String::new(); // the line without tags, for a box event
        let mut accented = false; // accent active: restore primary after
        let mut bumped = false; // a keyword bump is in effect: re-base the scale
        for (i, w) in line.iter().enumerate() {
            let word = if caps {
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
            if i == 0 && !lead.is_empty() {
                tags.insert_str(0, &lead);
            }
            if let (true, Some((reset, _))) = (bumped, scale) {
                // Scale state carries to later text: restore the line's own.
                tags.push_str(reset);
                bumped = false;
            }
            if key {
                tags.push_str(&format!("\\1c{accent}&"));
                accented = true;
            } else if accented {
                tags.push_str(&format!("\\1c{primary}&"));
                accented = false;
            }
            if let Some((_, settled)) = scale {
                if key && dt >= settled {
                    tags.push_str(&format!(
                        "\\t({dt},{},\\fscx114\\fscy114)\\t({},{},\\fscx100\\fscy100)",
                        dt + 90,
                        dt + 90,
                        dt + 240
                    ));
                    bumped = true;
                }
            }
            if anim == Anim::Words && i > 0 {
                tags.push_str(&format!("\\alpha&HFF&\\t({dt},{},\\alpha&H00&)", dt + 80));
            }
            text.push_str(&format!("{{{tags}}}{word} "));
            plain.push_str(&format!("{word} "));
            fresh = ends_sentence(&w.w);
        }
        if soft_box {
            let head = if lead.is_empty() {
                String::new()
            } else {
                format!("{{{lead}}}")
            };
            out.push_str(&format!(
                "Dialogue: 0,{},{},{box_name},,0,0,0,,{head}{}\n",
                stamp(t0 - offset),
                stamp(t1 - offset),
                plain.trim_end()
            ));
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
        // "I'm" is capitalized, not a keyword.
        let m = headline_markup("Feel like I'm in a coffin", "&H00111111", "&H001E3CFF");
        assert!(m.contains("{\\1c&H001E3CFF&}coffin"), "{m}");
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
                seam: false,
                ..AssOpts::default()
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
    fn split_screen_captions_sit_on_the_seam() {
        let ass = build_for(
            &words(),
            "karaoke",
            0.0,
            &AssOpts {
                w: 1080,
                h: 1920,
                seam: true,
                ..Default::default()
            },
        );
        let cap = style_line(&ass, "Karaoke");
        assert_eq!((cap[18], cap[21]), ("5", "0"));
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
            seam: false,
            ..AssOpts::default()
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

    // ---- Look: captions section -------------------------------------

    fn fnv(s: &str) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in s.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    fn sample() -> Vec<Word> {
        "so I made 3 million dollars last year. Never stop building"
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

    fn headline_of(json: &str) -> Option<HeadlineLook> {
        crate::look::Look::parse(&format!(r##"{{"headline":{json}}}"##)).headline
    }

    fn captions_of(json: &str) -> Option<CaptionsLook> {
        crate::look::Look::parse(&format!(r##"{{"captions":{json}}}"##)).captions
    }

    fn with(json: &str) -> AssOpts {
        AssOpts {
            captions: captions_of(json),
            ..AssOpts::default()
        }
    }

    fn build_look(style: &str, json: &str) -> String {
        build_for(&sample(), style, 0.0, &with(json))
    }

    fn dialogues(ass: &str) -> Vec<&str> {
        ass.lines()
            .filter(|l| l.starts_with("Dialogue: 0,"))
            .collect()
    }

    #[test]
    fn no_look_and_empty_look_are_byte_identical_to_before() {
        // Hash and length of the output of the code before the Look existed,
        // for six styles on five canvases.
        let tr = |top, left| Some(Clear { top, left, px: 260 });
        let base: Vec<(&str, AssOpts, (u64, usize))> = vec![
            ("karaoke", AssOpts::default(), (0xc5102f6c4955005f, 1626)),
            (
                "hormozi",
                AssOpts {
                    w: 1080,
                    h: 1080,
                    headline: Some("Big news".into()),
                    dur: 9.0,
                    clear: tr(true, false),
                    ..AssOpts::default()
                },
                (0x8675867d6d103399, 1879),
            ),
            (
                "minimal",
                AssOpts {
                    w: 1920,
                    h: 1080,
                    anim: Anim::Words,
                    ..AssOpts::default()
                },
                (0x54befa8e5b35d800, 1793),
            ),
            (
                "tiktok",
                AssOpts {
                    w: 1080,
                    h: 1350,
                    anim: Anim::Static,
                    ..AssOpts::default()
                },
                (0x28c1ea7e0c51d6b4, 1037),
            ),
            (
                "highlight",
                AssOpts {
                    seam: true,
                    ..AssOpts::default()
                },
                (0xd3868e02dafc904d, 1631),
            ),
            (
                "beast",
                AssOpts {
                    w: 1080,
                    h: 1080,
                    clear: tr(false, true),
                    ..AssOpts::default()
                },
                (0x683788510c5d2b47, 1742),
            ),
        ];
        for (style, o, (hash, len)) in base {
            for (cap, head) in [(None, None), (captions_of("{}"), headline_of("{}"))] {
                let o = AssOpts {
                    captions: cap,
                    headline_look: head,
                    ..o.clone()
                };
                let a = build_for(&sample(), style, 0.0, &o);
                assert_eq!((fnv(&a), a.len()), (hash, len), "{style}\n{a}");
            }
        }
        // A headline look with no headline on is nothing to draw.
        let o = AssOpts {
            headline_look: headline_of(r#"{"x":0.3,"seconds":2,"card":"none"}"#),
            ..AssOpts::default()
        };
        let a = build_for(&sample(), "karaoke", 0.0, &o);
        assert_eq!((fnv(&a), a.len()), (0xc5102f6c4955005f, 1626));
        // Sections that are not captions change nothing either.
        let l =
            crate::look::Look::parse(r#"{"v":1,"bar":{"pos":"top"},"camera":{"feel":"lively"}}"#);
        assert!(l.captions.is_none());
    }

    #[test]
    fn caption_position_centres_the_block_on_the_point() {
        let ass = build_look("karaoke", r#"{"x":0.25,"y":0.8}"#);
        let s = style_line(&ass, "Karaoke");
        // Middle-centre, no vertical margin, wrap width = twice the room
        // to the nearer edge (270 - 40 side margin = 230 each way).
        assert_eq!((s[18], s[19], s[20], s[21]), ("5", "310", "310", "0"));
        assert!(dialogues(&ass)[0].contains(",,{\\pos(270,1536)\\fad(80,60)\\fscx84"));
        // A missing x or y is the middle.
        let ass = build_look("karaoke", r#"{"y":0.25}"#);
        let s = style_line(&ass, "Karaoke");
        assert_eq!((s[19], s[20]), ("40", "40"));
        assert!(dialogues(&ass)[0].contains("{\\pos(540,480)"));
        let ass = build_look("karaoke", r#"{"x":0.5}"#);
        assert!(dialogues(&ass)[0].contains("{\\pos(540,960)"));
        // Hard against an edge the centre is nudged in so words still fit.
        let ass = build_look("karaoke", r#"{"x":0.02,"y":0.5}"#);
        let s = style_line(&ass, "Karaoke");
        assert_eq!((s[19], s[20]), ("324", "324"));
        assert!(dialogues(&ass)[0].contains("{\\pos(256,960)"));
        let ass = build_look("karaoke", r#"{"x":1}"#);
        assert!(dialogues(&ass)[0].contains("{\\pos(824,960)"));
        // Every canvas, and it beats the style alignment, the seam and the logo.
        let o = AssOpts {
            w: 1080,
            h: 1080,
            seam: true,
            clear: Some(Clear {
                top: false,
                left: true,
                px: 400,
            }),
            ..with(r#"{"x":0.5,"y":0.9}"#)
        };
        let ass = build_for(&sample(), "minimal", 0.0, &o);
        let s = style_line(&ass, "Minimal");
        assert_eq!((s[18], s[19], s[20], s[21]), ("5", "40", "40", "0"));
        assert!(dialogues(&ass)[0].contains("{\\pos(540,972)"));
        // Static lines get the position too.
        let ass = build_for(
            &sample(),
            "minimal",
            0.0,
            &AssOpts {
                anim: Anim::Static,
                ..with(r#"{"x":0.5,"y":0.5}"#)
            },
        );
        assert!(dialogues(&ass)[0].contains(",,{\\pos(540,960)\\k35}so"));
    }

    #[test]
    fn caption_size_font_and_case() {
        let size = |ass: &str| style_line(ass, "Karaoke")[2].to_string();
        assert_eq!(size(&build_look("karaoke", r#"{"size":1.5}"#)), "126");
        assert_eq!(size(&build_look("karaoke", r#"{"size":0.5}"#)), "42");
        // Square canvas scales by 0.9 on top.
        let sq = AssOpts {
            w: 1080,
            h: 1080,
            ..with(r#"{"size":1.5}"#)
        };
        assert_eq!(size(&build_for(&sample(), "karaoke", 0.0, &sq)), "113");
        let sq = AssOpts {
            w: 1080,
            h: 1080,
            ..AssOpts::default()
        };
        assert_eq!(size(&build_for(&sample(), "karaoke", 0.0, &sq)), "76");
        assert_eq!(
            style_line(&build_look("karaoke", r#"{"font":"Anton"}"#), "Karaoke")[1],
            "Anton"
        );
        assert_eq!(
            style_line(
                &build_look("minimal", r#"{"font":"JetBrains Mono"}"#),
                "Minimal"
            )[1],
            "JetBrains Mono"
        );
        assert_eq!(
            style_line(
                &build_look("karaoke", r#"{"font":"Comic Sans"}"#),
                "Karaoke"
            )[1],
            "Archivo Black"
        );
        // Case: upper on a lower-case style, asis on an upper-case one.
        let a = build_look("minimal", r#"{"case":"upper"}"#);
        assert!(
            dialogues(&a)[0].contains("}SO ") && dialogues(&a)[0].contains("}MADE"),
            "{a}"
        );
        let a = build_look("karaoke", r#"{"case":"asis"}"#);
        assert!(
            dialogues(&a)[0].contains("}so ") && !dialogues(&a)[0].contains("}SO"),
            "{a}"
        );
        let a = build_look("karaoke", r#"{"case":"upper"}"#);
        assert!(dialogues(&a)[0].contains("}SO "));
    }

    #[test]
    fn caption_colours_convert_to_ass_order() {
        let cols = |json: &str| {
            let ass = build_look("karaoke", json);
            let s = style_line(&ass, "Karaoke");
            (s[3].to_string(), s[4].to_string(), ass)
        };
        // Style default: primary yellow, secondary white.
        let (p, s, _) = cols("{}");
        assert_eq!((p.as_str(), s.as_str()), ("&H0035E1FF", "&H00FFFFFF"));
        // `color` alone sets both (#102030 -> BGR 302010).
        let (p, s, _) = cols(r##"{"color":"#102030"}"##);
        assert_eq!((p.as_str(), s.as_str()), ("&H00302010", "&H00302010"));
        // `active` alone sets only the sung colour.
        let (p, s, _) = cols(r##"{"active":"#FF3B30"}"##);
        assert_eq!((p.as_str(), s.as_str()), ("&H00303BFF", "&H00FFFFFF"));
        // Both: active is sung, color is unsung.
        let (p, s, a) = cols(r##"{"color":"#FFFFFF","active":"#FF3B30","accent":"#00FF00"}"##);
        assert_eq!((p.as_str(), s.as_str()), ("&H00303BFF", "&H00FFFFFF"));
        // Keywords sweep in the accent; the next word restores the sung colour.
        let l = dialogues(&a).join("\n");
        assert!(l.contains("\\1c&H0000FF00&"), "{l}");
        assert!(l.contains("\\1c&H00303BFF&"), "{l}");
        assert!(!l.contains("&H0000FFFF&"), "{l}");
        // Without a look the keyword colour is the style's.
        let (_, _, a) = cols("{}");
        assert!(dialogues(&a)[1].contains("\\1c&H0000FFFF&"));
        // Outline colour, width and shadow (scaled like the type).
        let ass = build_look(
            "karaoke",
            r##"{"outline":"#FF0000","outline_w":5,"shadow":4}"##,
        );
        let s = style_line(&ass, "Karaoke");
        assert_eq!((s[5], s[15], s[16], s[17]), ("&H000000FF", "1", "5", "4"));
        let sq = AssOpts {
            w: 1080,
            h: 1080,
            ..with(r#"{"outline_w":5,"shadow":4}"#)
        };
        let s = build_for(&sample(), "karaoke", 0.0, &sq);
        let s = style_line(&s, "Karaoke");
        assert_eq!((s[16], s[17]), ("4.5", "3.6"));
        let ass = build_look("tiktok", r#"{"outline_w":0,"shadow":0}"#);
        let s = style_line(&ass, "Tiktok");
        assert_eq!((s[16], s[17]), ("0", "0"));
    }

    #[test]
    fn caption_box_on_and_off() {
        // A see-through box on a style without one: its own BorderStyle 3
        // style in the box colour at 60% opacity (alpha 0x66), padded, with
        // invisible text; the caption style itself stays box-free. One box
        // event per caption line, in a single run.
        let ass = build_look("minimal", r##"{"box":"#000000","box_opacity":0.6}"##);
        let s = style_line(&ass, "MinimalBox");
        assert_eq!(
            (s[3], s[5], s[15], s[16]),
            ("&HFFFFFFFF", "&H66000000", "3", "14")
        );
        assert_eq!(style_line(&ass, "Minimal")[15], "1");
        let (boxes, texts): (Vec<&str>, Vec<&str>) = dialogues(&ass)
            .into_iter()
            .partition(|l| l.contains(",MinimalBox,"));
        assert_eq!(boxes.len(), texts.len());
        assert!(
            boxes[0].contains(",,{\\fad(80,60)") && boxes[0].ends_with("}so I made 3"),
            "{}",
            boxes[0]
        );
        assert!(!boxes[0].contains("\\k"));
        // Opaque: just the one style.
        let ass = build_look("minimal", r##"{"box":"#000000","box_opacity":1}"##);
        assert!(!ass.contains("MinimalBox"));
        assert_eq!(style_line(&ass, "Minimal")[15], "3");
        let ass = build_look("minimal", r##"{"box":"#336699"}"##);
        let s = style_line(&ass, "Minimal");
        assert_eq!((s[5], s[15]), ("&H00996633", "3"));
        // An explicit outline_w is the padding.
        let ass = build_look("minimal", r##"{"box":"#336699","outline_w":6}"##);
        assert_eq!(style_line(&ass, "Minimal")[16], "6");
        // Removing a style's box.
        for style in ["hormozi", "highlight"] {
            let name = if style == "hormozi" {
                "Hormozi"
            } else {
                "Highlight"
            };
            assert_eq!(style_line(&build_look(style, ""), name)[15], "3");
            for off in [r#"{"box":null}"#, r#"{"box":"none"}"#] {
                let ass = build_look(style, off);
                assert_eq!(style_line(&ass, name)[15], "1", "{style} {off}");
            }
        }
        // Opacity alone fades the style's own box.
        let ass = build_look("hormozi", r#"{"box_opacity":0.5}"#);
        let s = style_line(&ass, "HormoziBox");
        assert_eq!((s[5], s[15]), ("&H80000000", "3"));
        // ... and does nothing to a style with no box.
        let ass = build_look("minimal", r#"{"box_opacity":0.5}"#);
        assert_eq!(style_line(&ass, "Minimal")[15], "1");
        // A new colour keeps the style's box.
        let ass = build_look("highlight", r##"{"box":"#FF00FF"}"##);
        let s = style_line(&ass, "Highlight");
        assert_eq!((s[5], s[15], s[16]), ("&H00FF00FF", "3", "2"));
    }

    #[test]
    fn caption_words_per_line_regroups() {
        let words_in = |l: &str| l.matches("\\k").count();
        let a = build_look("minimal", r#"{"max_words":2}"#);
        assert!(dialogues(&a).iter().all(|l| words_in(l) <= 2), "{a}");
        assert!(dialogues(&a).len() > dialogues(&build_look("minimal", "{}")).len());
        // 8 words need more than the style's 20 characters: relaxed.
        let a = build_look("minimal", r#"{"max_words":8}"#);
        assert_eq!(words_in(dialogues(&a)[0]), 8, "{a}");
        // Out of range clamps.
        assert_eq!(build_look("minimal", r#"{"max_words":50}"#), a);
        assert_eq!(
            build_look("minimal", r#"{"max_words":0}"#),
            build_look("minimal", r#"{"max_words":1}"#)
        );
    }

    #[test]
    fn hiding_captions_keeps_the_headline() {
        let o = AssOpts {
            headline: Some("Big news".into()),
            dur: 9.0,
            ..with(r#"{"show":false}"#)
        };
        let ass = build_for(&sample(), "karaoke", 0.0, &o);
        assert!(dialogues(&ass).is_empty(), "{ass}");
        assert!(ass.contains("Dialogue: 1,0:00:00.00,0:00:09.00,Headline,"));
        let shown = AssOpts {
            captions: captions_of(r#"{"show":true}"#),
            ..o
        };
        assert!(!dialogues(&build_for(&sample(), "karaoke", 0.0, &shown)).is_empty());
    }

    #[test]
    fn new_caption_motions() {
        let first = |style: &str, json: &str| dialogues(&build_look(style, json))[0].to_string();
        // Fade: fade only, no scale, no bump.
        let ass = build_look("karaoke", r#"{"anim":"fade"}"#);
        for l in dialogues(&ass) {
            assert!(!l.contains("fsc") && !l.contains("\\t("), "{l}");
        }
        assert!(dialogues(&ass)[0].contains(",,{\\fad(200,140)\\k"));
        // Slide with the style's own anchor (karaoke: centre of a tall frame),
        // rising 42 px (2.2% of 1920) into place.
        assert!(first("karaoke", r#"{"anim":"slide"}"#)
            .contains(",,{\\move(540,1002,540,960,0,260)\\fad(160,60)\\k"));
        // Slide on a bottom-anchored style: the bottom margin line.
        let o = AssOpts {
            w: 1080,
            h: 1080,
            ..with(r#"{"anim":"slide"}"#)
        };
        let ass = build_for(&sample(), "minimal", 0.0, &o);
        assert!(
            dialogues(&ass)[0].contains("{\\move(540,935,540,911,0,260)"),
            "{ass}"
        );
        // Slide with a requested point ends exactly there.
        let l = first("karaoke", r#"{"anim":"slide","x":0.3,"y":0.8}"#);
        assert!(
            l.contains("{\\move(324,1578,324,1536,0,260)\\fad(160,60)"),
            "{l}"
        );
        assert!(!l.contains("\\pos"), "{l}");
        // Slide, fade and bounce have no keyword bump except bounce's.
        let l = first("karaoke", r#"{"anim":"slide","x":0.3,"y":0.8}"#);
        assert!(!l.contains("fscx114"));
        // Bounce: deeper start, overshoot, undershoot, settle; and a pos.
        let l = first("karaoke", r#"{"anim":"bounce","x":0.5,"y":0.25}"#);
        assert!(
            l.contains(&format!("{{\\pos(540,480)\\fad(80,60){LINE_BOUNCE}\\k")),
            "{l}"
        );
        // Its keyword bump waits for the entrance to finish, then re-bases.
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
        let ass = build_for(&ws, "minimal", 0.0, &with(r#"{"anim":"bounce"}"#));
        let l = dialogues(&ass)[0];
        assert!(l.contains("\\t(800,890,\\fscx114\\fscy114)"), "{l}");
        assert!(
            l.contains(&format!("{{\\k35{LINE_BOUNCE}\\1c&H00FFFFFF&}}now")),
            "{l}"
        );
        // Pop is still exactly pop.
        let l = first("karaoke", r#"{"anim":"pop"}"#);
        assert!(l.contains(&format!(",,{{\\fad(80,60){LINE_POP}\\k")), "{l}");
        // The Look's motion beats the flat option, both ways.
        let o = AssOpts {
            anim: Anim::Static,
            ..with(r#"{"anim":"fade"}"#)
        };
        assert!(dialogues(&build_for(&sample(), "karaoke", 0.0, &o))[0].contains("\\fad(200,140)"));
        let o = AssOpts {
            anim: Anim::Fade,
            ..with("{}")
        };
        assert!(dialogues(&build_for(&sample(), "karaoke", 0.0, &o))[0].contains("\\fad(200,140)"));
        let o = AssOpts {
            anim: Anim::Pop,
            ..with(r#"{"anim":"none"}"#)
        };
        assert!(dialogues(&build_for(&sample(), "karaoke", 0.0, &o))[0].contains(",,{\\k35}"));
        assert_eq!(Anim::parse("Bounce"), Anim::Bounce);
        assert_eq!(Anim::from_name("spin"), None);
    }

    #[test]
    fn out_of_range_and_bad_caption_values_fall_back() {
        let same = |bad: &str, good: &str| {
            assert_eq!(
                build_look("karaoke", bad),
                build_look("karaoke", good),
                "{bad}"
            );
        };
        same(r#"{"size":9}"#, r#"{"size":2}"#);
        same(r#"{"size":0.01}"#, r#"{"size":0.5}"#);
        same(r#"{"x":7,"y":-2}"#, r#"{"x":1,"y":0}"#);
        same(
            r#"{"outline_w":99,"shadow":99}"#,
            r#"{"outline_w":8,"shadow":6}"#,
        );
        same(r#"{"font":"Papyrus","case":"title","anim":"spin"}"#, "{}");
        same(
            r##"{"color":"red","active":"#12345","accent":"#GGGGGG","outline":7}"##,
            "{}",
        );
        same(
            r#"{"box":"teal","box_opacity":"half","size":"big","show":"no"}"#,
            "{}",
        );
        same(r#"{"unknown":1,"x":null}"#, "{}");
    }

    // ---- Look: headline section --------------------------------------

    const HEAD: &str = "Why most founders quit too early";

    /// `H:MM:SS.cc` -> seconds.
    fn stamp_secs(t: &str) -> f64 {
        let p: Vec<f64> = t.split(':').map(|v| v.parse().unwrap()).collect();
        p[0] * 3600.0 + p[1] * 60.0 + p[2]
    }

    fn head_ass(json: &str, w: u32, h: u32, dur: f64) -> String {
        build_for(
            &sample(),
            "karaoke",
            0.0,
            &AssOpts {
                w,
                h,
                dur,
                headline: Some(HEAD.into()),
                headline_look: headline_of(json),
                ..AssOpts::default()
            },
        )
    }

    fn tall(json: &str) -> String {
        head_ass(json, 1080, 1920, 9.0)
    }

    fn head_event(ass: &str) -> &str {
        ass.lines()
            .find(|l| l.starts_with("Dialogue: 1,"))
            .unwrap_or_else(|| panic!("no headline event in\n{ass}"))
    }

    #[test]
    fn headline_today_is_a_white_card_popping_in_at_the_top() {
        let ass = tall("{}");
        let h = style_line(&ass, "Headline");
        assert_eq!(
            h[1..].join(","),
            "Archivo Black,64,&H00111111,&H00111111,&H00FFFFFF,&H00FFFFFF,0,0,0,0,100,100,0,0,4,24,0,8,90,90,163,1"
        );
        assert!(head_event(&ass).starts_with(
            "Dialogue: 1,0:00:00.00,0:00:09.00,Headline,,0,0,0,,{\\fad(160,0)\\fscx72\\fscy72\\t(0,200,"
        ));
    }

    #[test]
    fn headline_size_scales_type_and_card_padding_together() {
        let h = style_line(&tall(r#"{"size":1.5}"#), "Headline").join(",");
        assert!(h.contains(",Archivo Black,96,"), "{h}");
        assert!(h.contains(",0,0,4,36,0,8,"), "{h}");
        let h = style_line(&tall(r#"{"size":0.5}"#), "Headline").join(",");
        assert!(h.contains(",Archivo Black,32,"), "{h}");
        assert!(h.contains(",0,0,4,12,0,8,"), "{h}");
    }

    #[test]
    fn headline_ink_card_and_accent_colours() {
        let ass = tall(r##"{"ink":"#FFFFFF","card":"#111111","accent":"#FFD400"}"##);
        let h = style_line(&ass, "Headline");
        assert_eq!(
            (h[3], h[4], h[5], h[6]),
            ("&H00FFFFFF", "&H00FFFFFF", "&H00111111", "&H00111111")
        );
        assert_eq!(h[15], "4");
        let e = head_event(&ass);
        // The accent word is yellow, and the type goes back to the ink after.
        assert!(e.contains("{\\1c&H0000D4FF&}"), "{e}");
        assert!(e.contains("{\\1c&H00FFFFFF&}"), "{e}");
        assert!(!e.contains("&H001E3CFF"), "{e}");
        // Each colour on its own.
        let h = style_line(&tall(r##"{"ink":"#00FF00"}"##), "Headline").join(",");
        assert!(
            h.contains(",&H0000FF00,&H0000FF00,&H00FFFFFF,&H00FFFFFF,"),
            "{h}"
        );
        let h = style_line(&tall(r##"{"card":"#102030"}"##), "Headline").join(",");
        assert!(
            h.contains(",&H00111111,&H00111111,&H00302010,&H00302010,"),
            "{h}"
        );
        assert!(head_event(&tall(r##"{"accent":"#00FF00"}"##)).contains("{\\1c&H0000FF00&}"));
    }

    #[test]
    fn a_headline_without_a_card_gets_a_dark_outline() {
        for card in [r#""none""#, "null", r#""NONE""#] {
            let ass = tall(&format!(r#"{{"card":{card}}}"#));
            let h = style_line(&ass, "Headline");
            // Plain outline text: white ink, black edge, no box.
            assert_eq!(
                (h[3], h[5], h[15], h[16]),
                ("&H00FFFFFF", "&H00000000", "1", "5"),
                "{card}"
            );
            let e = head_event(&ass);
            assert!(e.contains("{\\1c&H001E3CFF&}"), "{e}");
            assert!(
                e.contains("{\\1c&HFFFFFF&}") || e.contains("{\\1c&H00FFFFFF&}"),
                "{e}"
            );
        }
        // The edge scales with the type.
        let ass = tall(r#"{"card":"none","size":2}"#);
        let h = style_line(&ass, "Headline");
        assert_eq!((h[2], h[16]), ("128", "10"));
        // Your own ink is kept; a dark ink gets a light edge instead.
        let ass = tall(r##"{"card":"none","ink":"#FFD400"}"##);
        let h = style_line(&ass, "Headline");
        assert_eq!((h[3], h[5]), ("&H0000D4FF", "&H00000000"));
        let ass = tall(r##"{"card":"none","ink":"#101010"}"##);
        let h = style_line(&ass, "Headline");
        assert_eq!((h[3], h[5]), ("&H00101010", "&H00FFFFFF"));
    }

    #[test]
    fn headline_position_anchors_the_card_by_its_centre() {
        let ass = tall(r#"{"x":0.5,"y":0.7}"#);
        let h = style_line(&ass, "Headline");
        // Middle-centre, no vertical margin, the usual side margins.
        assert_eq!((h[18], h[19], h[20], h[21]), ("5", "90", "90", "0"));
        assert!(head_event(&ass).contains(",,{\\pos(540,1344)\\fad(160,0)\\fscx72"));
        // Off-centre: wrap width is twice the room to the nearer edge.
        let ass = tall(r#"{"x":0.35,"y":0.2}"#);
        let h = style_line(&ass, "Headline");
        assert_eq!((h[19], h[20]), ("252", "252"));
        assert!(head_event(&ass).contains("{\\pos(378,384)"));
        // Hard against an edge the centre is nudged in so words still fit.
        let ass = tall(r#"{"x":0.02,"y":0.5}"#);
        let h = style_line(&ass, "Headline");
        assert_eq!((h[19], h[20]), ("324", "324"));
        assert!(head_event(&ass).contains("{\\pos(306,960)"));
        let ass = tall(r#"{"x":1,"y":0.5}"#);
        assert!(head_event(&ass).contains("{\\pos(774,960)"));
        // Never lets the wrapped text leave the frame, wherever x is.
        for i in 0..=20 {
            let x = i as f64 / 20.0;
            let ass = tall(&format!(r#"{{"x":{x},"y":0.5}}"#));
            let h = style_line(&ass, "Headline");
            let m: f64 = h[19].parse().unwrap();
            let pos = head_event(&ass).split("\\pos(").nth(1).unwrap();
            let cx: f64 = pos.split(',').next().unwrap().parse().unwrap();
            let half = 540.0 - m;
            assert!(cx - half >= 89.0 && cx + half <= 991.0, "x {x}: {cx} {m}");
        }
        // One coordinate: the other is the middle (x) or today's spot (y).
        let ass = tall(r#"{"y":0.25}"#);
        assert!(head_event(&ass).contains("{\\pos(540,480)"));
        let ass = tall(r#"{"x":0.5}"#);
        assert!(
            head_event(&ass).contains("{\\pos(540,227)"),
            "{}",
            head_event(&ass)
        );
    }

    #[test]
    fn a_placed_headline_stays_in_the_frame_and_beats_the_logo() {
        // The card (two lines, padding) never pokes out of the top or bottom.
        let ass = tall(r#"{"x":0.5,"y":0}"#);
        assert!(
            head_event(&ass).contains("{\\pos(540,88)"),
            "{}",
            head_event(&ass)
        );
        let ass = tall(r#"{"x":0.5,"y":1}"#);
        assert!(
            head_event(&ass).contains("{\\pos(540,1832)"),
            "{}",
            head_event(&ass)
        );
        // A corner logo widens the headline's margin only while the headline
        // sits where it always did.
        let clear = Some(Clear {
            top: true,
            left: false,
            px: 260,
        });
        let run = |json: &str| {
            build_for(
                &sample(),
                "karaoke",
                0.0,
                &AssOpts {
                    headline: Some(HEAD.into()),
                    dur: 9.0,
                    clear,
                    headline_look: headline_of(json),
                    ..AssOpts::default()
                },
            )
        };
        let h = style_line(&run("{}"), "Headline").join(",");
        assert!(h.contains(",8,90,260,163,1"), "{h}");
        let h = style_line(&run(r#"{"size":1.2}"#), "Headline").join(",");
        assert!(h.contains(",8,90,260,163,1"), "{h}");
        let h = style_line(&run(r#"{"x":0.5,"y":0.12}"#), "Headline").join(",");
        assert!(h.contains(",5,90,90,0,1"), "{h}");
    }

    #[test]
    fn headline_placement_on_a_square_canvas() {
        let ass = head_ass(
            r##"{"x":0.5,"y":0.7,"size":1.3,"ink":"#FFFFFF","card":"#111111","accent":"#FFD400","anim":"fade"}"##,
            1080,
            1080,
            9.0,
        );
        let h = style_line(&ass, "Headline");
        assert_eq!((h[2], h[16]), ("75", "28"));
        assert!(
            head_event(&ass).contains("{\\pos(540,756)\\fad(200,0)}"),
            "{}",
            head_event(&ass)
        );
    }

    #[test]
    fn headline_motion_pop_fade_none() {
        let tags = |json: &str| {
            let e = head_event(&tall(json)).to_string();
            let t = e.split(",,").nth(2).unwrap().to_string();
            t.split('}')
                .next()
                .unwrap()
                .trim_start_matches('{')
                .to_string()
        };
        assert!(tags(r#"{"anim":"pop"}"#).starts_with("\\fad(160,0)\\fscx72\\fscy72\\t(0,200,"));
        assert_eq!(tags("{}"), tags(r#"{"anim":"pop"}"#));
        // Fade: fades in, never scales.
        assert_eq!(tags(r#"{"anim":"fade"}"#), "\\fad(200,0)");
        // None: cuts in, so there is no tag block at all...
        let ass = tall(r#"{"anim":"none"}"#);
        assert!(
            head_event(&ass).contains(",Headline,,0,0,0,,Why "),
            "{}",
            head_event(&ass)
        );
        // ...unless it is placed or ends early.
        assert_eq!(tags(r#"{"anim":"none","x":0.4}"#), "\\pos(432,227)");
        assert_eq!(tags(r#"{"anim":"none","seconds":3}"#), "\\fad(0,200)");
        assert!(!tags(r#"{"anim":"fade","x":0.5,"y":0.5}"#).contains("fscx"));
    }

    #[test]
    fn headline_seconds_shortens_the_event_and_never_passes_the_clip() {
        let end = |json: &str, dur: f64| {
            let ass = head_ass(json, 1080, 1920, dur);
            let e = head_event(&ass).to_string();
            let mut f = e.split(',').skip(1);
            let (a, b) = (f.next().unwrap().to_string(), f.next().unwrap().to_string());
            let fad = e
                .split("\\fad(")
                .nth(1)
                .map(|t| t.split(')').next().unwrap().to_string());
            (a, b, fad)
        };
        // Whole clip: absent, zero.
        assert_eq!(
            end("{}", 9.0),
            (
                "0:00:00.00".into(),
                "0:00:09.00".into(),
                Some("160,0".into())
            )
        );
        assert_eq!(end(r#"{"seconds":0}"#, 9.0), end("{}", 9.0));
        // 3 s then a 200 ms fade out inside it.
        assert_eq!(
            end(r#"{"seconds":3}"#, 9.0),
            (
                "0:00:00.00".into(),
                "0:00:03.00".into(),
                Some("160,200".into())
            )
        );
        assert_eq!(end(r#"{"seconds":2.5,"anim":"fade"}"#, 9.0).1, "0:00:02.50");
        // Longer than the clip, or the same: the clip.
        assert_eq!(end(r#"{"seconds":30}"#, 9.0), end("{}", 9.0));
        assert_eq!(end(r#"{"seconds":9}"#, 9.0), end("{}", 9.0));
        assert_eq!(end(r#"{"seconds":3}"#, 2.0), end("{}", 2.0));
        // Very short: the fade out cannot be longer than the headline.
        assert_eq!(end(r#"{"seconds":0.1}"#, 9.0).2, Some("160,100".into()));
        // It moves with the clip length, not the other way round.
        for dur in [1.0, 4.0, 60.0] {
            let (_, b, _) = end(r#"{"seconds":5}"#, dur);
            let secs = stamp_secs(&b);
            assert!(secs <= dur + 1e-9, "{b} vs {dur}");
        }
        // The captions do not follow it.
        let ass = tall(r#"{"seconds":1}"#);
        assert_eq!(dialogues(&ass), dialogues(&tall("{}")));
    }

    #[test]
    fn a_headline_look_without_a_headline_draws_nothing() {
        let ass = build_for(
            &sample(),
            "karaoke",
            0.0,
            &AssOpts {
                dur: 9.0,
                headline_look: headline_of(r#"{"x":0.2,"y":0.2}"#),
                ..AssOpts::default()
            },
        );
        assert!(!ass.contains("Headline"));
    }
}
