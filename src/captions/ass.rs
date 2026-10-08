//! Words -> ASS animated subtitles, TikTok style.
//! Port of AssBuilder.php: 8 presets designed on a 1080x1920 canvas, per-word
//! {\k} karaoke tags burned later with `ffmpeg -vf ass=...`. Other canvases
//! (4:5, 1:1, 16:9) get PlayRes = the output size, scaled type and the
//! caption block moved to the lower third (mid-frame would sit on the face).
//! An optional headline is pinned at the top for the whole clip.

use super::motion;
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

pub(super) fn ends_sentence(w: &str) -> bool {
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
    group_by(words, max_words, max_chars, max_gap, max_dur, false)
}

/// [`group`], counting the characters of a block as characters (a Look's
/// `max_chars`) rather than as the bytes the styles' own budgets were tuned in.
fn group_by(
    words: &[Word],
    max_words: usize,
    max_chars: usize,
    max_gap: f64,
    max_dur: f64,
    by_chars: bool,
) -> Vec<Vec<Word>> {
    let len = |w: &Word| {
        if by_chars {
            w.w.chars().count()
        } else {
            w.w.len()
        }
    };
    let mut lines: Vec<Vec<Word>> = Vec::new();
    let mut cur: Vec<Word> = Vec::new();
    for w in words {
        let flush = if cur.is_empty() {
            false
        } else {
            let last = cur.last().unwrap();
            let text_len: usize = cur.iter().map(|x| len(x) + 1).sum::<usize>() + len(w);
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
/// `word_cap` is a hard words-per-line limit set by a Look; `hard_chars` says
/// the character budget is a Look's `max_chars`, which a merge never exceeds
/// (the style's own is stretched by a few characters).
fn merge_flashes(
    lines: Vec<Vec<Word>>,
    max_c: usize,
    word_cap: Option<usize>,
    hard_chars: bool,
) -> Vec<Vec<Word>> {
    let chars = |l: &[Word]| l.iter().map(|w| w.w.chars().count() + 1).sum::<usize>();
    let span = |l: &[Word]| l[l.len() - 1].e - l[0].s;
    // Two blocks joined read as `chars(a) + chars(b) - 1` characters.
    let room = if hard_chars { max_c + 1 } else { max_c + 8 };
    let fits = |a: &[Word], b: &[Word]| {
        !ends_sentence(&a[a.len() - 1].w)
            && b[0].s - a[a.len() - 1].e < 0.6
            && chars(a) + chars(b) <= room
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
    // Word looks, an entrance or an exit, or any of the text-dressing fields
    // (stroke, shadow, glow, box objects, spacing, line gap, lines, max_chars,
    // align, rotate) in the Look: the word-level writer (`motion`) draws the
    // captions. Without them the line writer below runs, exactly as it always
    // did.
    let word_level = cap.filter(|c| c.positioned());
    let moving = match word_level {
        Some(c) => {
            let (en, ex) = motion::effective_motion(c, anim);
            en.kind != crate::look::EnterKind::None || ex.kind != crate::look::ExitKind::None
        }
        None => anim != Anim::Static,
    };
    // Words per line: the style's character budget stretches with the word
    // limit so a higher limit is reachable.
    let (max_w, max_c) = match cap.and_then(|c| c.max_words) {
        Some(n) if n > max_w => (n, (max_c * n).div_ceil(max_w)),
        Some(n) => (n, max_c),
        None => (max_w, max_c),
    };
    // A Look's `max_chars` replaces the style's character budget; the word
    // cap is then its own `max_words`, or the contract's top (8): the style's
    // own few words would otherwise cut every block short of it.
    let hard_chars = cap.and_then(|c| c.max_chars);
    let (max_w, max_c) = match hard_chars {
        Some(n) => (cap.and_then(|c| c.max_words).unwrap_or(8), n),
        None => (max_w, max_c),
    };
    let mut lines = group_by(
        &attach_punctuation(words),
        max_w,
        max_c,
        0.6,
        4.0,
        hard_chars.is_some(),
    );
    if cap.is_some_and(|c| c.show == Some(false)) {
        lines.clear();
    }
    if moving {
        lines = merge_flashes(
            split_sentences(lines),
            max_c,
            cap.and_then(|c| c.max_words),
            hard_chars.is_some(),
        );
    }
    let spans_of = |lines: &[Vec<Word>]| -> Vec<(f64, f64)> {
        if moving {
            hold_lines(
                lines,
                if o.dur > 0.0 {
                    o.dur + offset
                } else {
                    f64::INFINITY
                },
            )
        } else {
            lines.iter().map(|l| (l[0].s, l[l.len() - 1].e)).collect()
        }
    };
    let spans = spans_of(&lines);
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
    // A box object is drawn as a shape by the word-level writer: the style's
    // own libass box (and a v1 box colour) step aside for it.
    let vector_box = cap.is_some_and(|c| c.box_fx.is_some());
    // The box the style or a v1 `box` colour gives (the shape's default colour).
    let own_box = match cap.and_then(|c| c.box_) {
        Some(BoxLook::Color(c)) => Some(c.ass(0)),
        Some(BoxLook::None) => None,
        None => (style.border == 3).then(|| style.outline.to_string()),
    };
    match cap.and_then(|c| c.box_) {
        _ if vector_box => border = 1,
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
    if vector_box && style.border == 3 {
        // The style's outline width was its box padding: no stroke.
        outline_w = 0.0;
    } else if let Some(v) = cap.and_then(|c| c.outline_w) {
        outline_w = v * k;
    }
    // The text's own stroke: what its style row draws, the Look's `stroke` on
    // top (it wins over `outline` / `outline_w`). Under a libass box the style
    // row has no stroke (its outline slot is the box).
    let stroke_look = cap.and_then(|c| c.stroke.as_ref());
    let mut stroke_c = if border == 3 {
        "&H00000000".to_string()
    } else {
        outline.clone()
    };
    let mut stroke_w = if border == 3 { 0.0 } else { outline_w };
    if let Some(st) = stroke_look {
        if let Some(c) = st.color {
            stroke_c = c.ass(0);
        }
        if let Some(w) = st.width {
            stroke_w = w * k;
        }
        if border != 3 {
            outline = stroke_c.clone();
            outline_w = stroke_w;
        }
    }
    // A shadow object replaces the style's (and a v1 number): it is drawn as
    // a copy under the text.
    let shadow = if cap.is_some_and(|c| c.shadow_fx.is_some()) {
        0.0
    } else {
        cap.and_then(|c| c.shadow)
            .map_or(style.shadow as f64, |v| v * k)
    };
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
    // libass draws a box once per run of text, and the karaoke runs overlap by
    // the padding: where the box is see-through a dark seam shows between
    // words. A box is see-through when its colour is, and for the eighty
    // milliseconds a line fades in or out (every motion but `none`, which cuts,
    // and `words`, whose box is meant to grow word by word). So such a box
    // gets its own style and one event per line (a single run of invisible
    // text under the real one); a box that is opaque all the time needs neither.
    let fading = moving && anim != Anim::Words;
    let soft_box = border == 3 && (!outline.starts_with("&H00") || fading);
    let box_name = format!("{display}Box");
    // The word-level writer always draws the box as its own event(s).
    let split_box = if word_level.is_some() {
        border == 3
    } else {
        soft_box
    };
    if split_box {
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
    if let Some(c) = word_level {
        let cfg = motion::Cfg::resolve(
            c,
            anim,
            motion::col_of_ass(&primary),
            motion::col_of_ass(&secondary),
            motion::col_of_ass(&accent),
            k,
            &motion::Base {
                stroke: motion::Stroke {
                    col: motion::col_of_ass(&stroke_c),
                    w: stroke_w,
                },
                box_col: own_box.as_deref().map(motion::col_of_ass),
                box_opacity: c.box_opacity.unwrap_or(1.0),
                font_px: font_px as f64,
            },
        );
        let alpha_of = |s: &str| u8::from_str_radix(s.get(2..4).unwrap_or("00"), 16).unwrap_or(0);
        let geo = motion::Geo {
            pw,
            ph,
            k,
            alignment,
            margin_v,
            cap_l,
            cap_r,
            place: place.as_ref().map(|p| (p.x, p.y)),
            font,
            font_px,
        };
        let draw = motion::Draw {
            text_style: display,
            box_style: split_box.then(|| {
                (
                    box_name.as_str(),
                    alpha_of(&outline),
                    (shadow > 0.0).then(|| alpha_of(style.back)),
                )
            }),
            caps,
        };
        // At most `lines` rows in a block: longer blocks are cut into more.
        let (lines, spans) = match c.lines {
            Some(n) => {
                let l = motion::fit_rows(&cfg, &geo, caps, lines, n);
                let s = spans_of(&l);
                (l, s)
            }
            None => (lines, spans),
        };
        out.push_str(&motion::events(&cfg, &geo, &draw, &lines, &spans, offset));
        return out;
    }
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
                (0xec7cabc2b102f4b4, 2585),
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
                (0xee2916c46b0b8e44, 2346),
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
        // Opaque and cut in (nothing fades): just the one style.
        let ass = build_look(
            "minimal",
            r##"{"box":"#000000","box_opacity":1,"anim":"none"}"##,
        );
        assert!(!ass.contains("MinimalBox"));
        assert_eq!(style_line(&ass, "Minimal")[15], "3");
        let ass = build_look("minimal", r##"{"box":"#336699","anim":"none"}"##);
        let s = style_line(&ass, "Minimal");
        assert_eq!((s[5], s[15]), ("&H00996633", "3"));
        // An explicit outline_w is the padding.
        let ass = build_look(
            "minimal",
            r##"{"box":"#336699","outline_w":6,"anim":"none"}"##,
        );
        assert_eq!(style_line(&ass, "Minimal")[16], "6");
        // Opaque but fading in and out: the box is its own event per line, or
        // the boxes of the runs would overlap into a dark seam while it fades.
        let ass = build_look("minimal", r##"{"box":"#336699"}"##);
        let s = style_line(&ass, "MinimalBox");
        assert_eq!((s[5], s[15], s[16]), ("&H00996633", "3", "14"));
        assert_eq!(style_line(&ass, "Minimal")[15], "1");
        let (boxes, texts): (Vec<&str>, Vec<&str>) = dialogues(&ass)
            .into_iter()
            .partition(|l| l.contains(",MinimalBox,"));
        assert_eq!(boxes.len(), texts.len());
        assert!(!boxes[0].contains("\\k"));
        // `words` grows the box with the words, as it always did.
        let ass = build_look("minimal", r##"{"box":"#336699","anim":"words"}"##);
        assert!(!ass.contains("MinimalBox"));
        // Removing a style's box.
        for style in ["hormozi", "highlight"] {
            let name = if style == "hormozi" {
                "Hormozi"
            } else {
                "Highlight"
            };
            let cut = |j: &str| build_look(style, j);
            assert_eq!(style_line(&cut(r#"{"anim":"none"}"#), name)[15], "3");
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
        let ass = build_look("highlight", r##"{"box":"#FF00FF","anim":"none"}"##);
        let s = style_line(&ass, "Highlight");
        assert_eq!((s[5], s[15], s[16]), ("&H00FF00FF", "3", "2"));
        let ass = build_look("highlight", r##"{"box":"#FF00FF"}"##);
        let s = style_line(&ass, "HighlightBox");
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

    // ---- Look: words, enter, exit -------------------------------------

    /// The events of one word (the text after the tags), in file order.
    fn ev<'a>(ass: &'a str, w: &str) -> Vec<&'a str> {
        ass.lines()
            .filter(|l| l.starts_with("Dialogue: 0,") && l.ends_with(&format!("}}{w}")))
            .collect()
    }

    /// The override tags of an event.
    fn tg(e: &str) -> &str {
        let body = e.split_once(",,0,0,0,,{").map_or("", |x| x.1);
        body.rsplit_once('}').map_or("", |x| x.0)
    }

    fn start(e: &str) -> &str {
        e.split(',').nth(1).unwrap()
    }

    fn end(e: &str) -> &str {
        e.split(',').nth(2).unwrap()
    }

    /// Where an event puts its word (the start of a move).
    fn at(e: &str) -> (f64, f64) {
        let t = tg(e);
        let i = t
            .find("\\pos(")
            .map(|i| i + 5)
            .or_else(|| t.find("\\move(").map(|i| i + 6));
        let r = &t[i.expect("no position")..];
        let mut p = r.split([',', ')']);
        (
            p.next().unwrap().parse().unwrap(),
            p.next().unwrap().parse().unwrap(),
        )
    }

    /// Where a move ends.
    fn to(e: &str) -> (f64, f64) {
        let r = tg(e).split("\\move(").nth(1).expect("no move");
        let p: Vec<f64> = r
            .split(')')
            .next()
            .unwrap()
            .split(',')
            .map(|v| v.parse().unwrap())
            .collect();
        (p[2], p[3])
    }

    /// A static look (lines keep the words' own spans) with word settings.
    fn st(json: &str) -> String {
        build_look("minimal", &format!(r#"{{"anim":"none",{json}}}"#))
    }

    #[test]
    fn default_ass_is_byte_identical_for_every_style_and_empty_word_sections() {
        // Hash and length of what HEAD wrote before the word model existed.
        let ws: Vec<Word> =
            "so I made 3 million dollars last year. Never stop building the best thing"
                .split(' ')
                .enumerate()
                .map(|(i, w)| Word {
                    w: w.into(),
                    s: i as f64 * 0.4,
                    e: i as f64 * 0.4 + 0.35,
                    conf: Some(0.9),
                })
                .collect();
        let anims = [
            Anim::Pop,
            Anim::Words,
            Anim::Static,
            Anim::Fade,
            Anim::Slide,
            Anim::Bounce,
        ];
        let head: &[(&str, usize, u64, usize)] = &[
            ("karaoke", 0, 0x5f8e454ce390ec78, 1940),
            ("karaoke", 1, 0x5a5249cb3a7606cd, 2246),
            ("karaoke", 2, 0xfa03f6e607c4a1ed, 1153),
            ("karaoke", 3, 0xd36bc11fc5008783, 1182),
            ("karaoke", 4, 0x62070ba5fe3cadc3, 1322),
            ("karaoke", 5, 0xddb2b8fa274b9d28, 2372),
            ("hormozi", 0, 0xa06e82056cdaa933, 2848),
            ("hormozi", 1, 0xa5a5c4574fcf22cd, 2130),
            ("hormozi", 2, 0x58625c6da271ec68, 1134),
            ("hormozi", 3, 0x97ab22c27e6d5eb8, 1820),
            ("hormozi", 4, 0x7ff9a358b07168b2, 2168),
            ("hormozi", 5, 0x3a41694e7c9bf372, 3550),
            ("minimal", 0, 0x613bbc0c9b432d08, 1750),
            ("minimal", 1, 0x8aaca1953ddb57a6, 2096),
            ("minimal", 2, 0x85c09e8fbc1e94d8, 1090),
            ("minimal", 3, 0x5d5be388c1b446ca, 1120),
            ("minimal", 4, 0xada9c9624185c51e, 1236),
            ("minimal", 5, 0xb208782ded1a09ba, 2074),
            ("beast", 0, 0xb3edaf485e211054, 1974),
            ("beast", 1, 0xd74e839c6dda1ff8, 2246),
            ("beast", 2, 0xe6eeef5d9ee1204f, 1221),
            ("beast", 3, 0xb432cbeee4652a4d, 1217),
            ("beast", 4, 0xdbd9e131e27380b7, 1391),
            ("beast", 5, 0x087d3cb00f36d5d2, 2406),
            ("neon", 0, 0xa2af98a0d854dd29, 1911),
            ("neon", 1, 0x371f1c82269cbf9a, 2217),
            ("neon", 2, 0xb46aee36f630bcb2, 1121),
            ("neon", 3, 0xf242d9731e26d878, 1153),
            ("neon", 4, 0x4189686735e5c330, 1293),
            ("neon", 5, 0x31bc34f84f278527, 2343),
            ("highlight", 0, 0x08014178d679867c, 2878),
            ("highlight", 1, 0xb2a072fde39550f1, 2146),
            ("highlight", 2, 0xcf5bd369ed64bad4, 1150),
            ("highlight", 3, 0xae45d0b96277f717, 1850),
            ("highlight", 4, 0xbdd8fcfab527a0d9, 2198),
            ("highlight", 5, 0x68c79bd7fc445bf1, 3580),
            ("ghost", 0, 0x69c5fde3c802c709, 1738),
            ("ghost", 1, 0xc57647296bd6af97, 2084),
            ("ghost", 2, 0xfcde295efb4010d9, 1076),
            ("ghost", 3, 0x989ffce9172be6ff, 1108),
            ("ghost", 4, 0x6fc30d3f553a5d1f, 1224),
            ("ghost", 5, 0x5fc0877808a4b0bf, 2062),
            ("tiktok", 0, 0x7c85f1b8be168fd4, 1935),
            ("tiktok", 1, 0xea74d6b2e0f45e23, 2241),
            ("tiktok", 2, 0xb6a7ca024b3a9f21, 1147),
            ("tiktok", 3, 0xe60d553e4287686d, 1177),
            ("tiktok", 4, 0x2f0178aa484049dd, 1322),
            ("tiktok", 5, 0xaec712a6209a45c2, 2367),
        ];
        // No look, and every way of saying "nothing": the empty sections, and
        // sections whose every value is junk.
        let nothing = [
            None,
            captions_of("{}"),
            captions_of(r#"{"words":{}}"#),
            captions_of(r#"{"enter":{},"exit":{}}"#),
            captions_of(
                r##"{"words":{"mode":"sideways","fill":"wipe","upcoming":{"color":"red","opacity":"half"},
                    "keyword":{"scale":"big"},"attack_ease":"wobble","hold_ms":"x"},
                    "enter":{"kind":"spin","ms":"x","ease":"wobble"},"exit":{"kind":"slide_left","ms":null}}"##,
            ),
        ];
        assert_eq!(head.len(), 48);
        for &(style, a, hash, len) in head {
            for cap in &nothing {
                let o = AssOpts {
                    anim: anims[a],
                    captions: cap.clone(),
                    ..AssOpts::default()
                };
                let s = build_for(&ws, style, 0.0, &o);
                assert_eq!((fnv(&s), s.len()), (hash, len), "{style} {a} {cap:?}\n{s}");
            }
        }
    }

    #[test]
    fn each_state_field_switches_at_the_words_start_and_back_at_its_end() {
        // "I" is spoken from 0.40 to 0.75, in a static line that runs 0 to 1.55.
        let one = |json: &str| {
            let a = st(json);
            let e = ev(&a, "I");
            assert_eq!(e.len(), 1, "{a}");
            tg(e[0]).to_string()
        };
        // Colour: unsung colour first, the active colour at the start, the
        // spoken colour at the end.
        let t = one(r##""words":{"active":{"color":"#FF0000"},"spoken":{"color":"#00FF00"}}"##);
        assert!(t.contains("\\1c&HFFFFFF&"), "{t}");
        assert!(t.contains("\\t(400,400,\\1c&H0000FF&)"), "{t}");
        assert!(t.contains("\\t(750,750,\\1c&H00FF00&)"), "{t}");
        // Upcoming colour.
        let t = one(r##""words":{"upcoming":{"color":"#112233"}}"##);
        assert!(t.contains("\\1c&H332211&"), "{t}");
        // Opacity (0.5 -> alpha 0x80, 0.25 -> 0xBF).
        let t = one(r#""words":{"upcoming":{"opacity":0.5},"spoken":{"opacity":0.25}}"#);
        assert!(t.contains("\\alpha&H80&"), "{t}");
        assert!(t.contains("\\t(400,400,\\alpha&H00&)"), "{t}");
        assert!(t.contains("\\t(750,750,\\alpha&HBF&)"), "{t}");
        // Active opacity under an opaque upcoming word.
        let t = one(r#""words":{"active":{"opacity":0.5}}"#);
        assert!(t.contains("\\t(400,400,\\alpha&H80&)"), "{t}");
        // Scale.
        let t = one(r#""words":{"upcoming":{"scale":0.8},"active":{"scale":1.2}}"#);
        assert!(t.contains("\\fscx80\\fscy80"), "{t}");
        assert!(t.contains("\\t(400,400,\\fscx120\\fscy120)"), "{t}");
        assert!(t.contains("\\t(750,750,\\fscx100\\fscy100)"), "{t}");
        let t = one(r#""words":{"spoken":{"scale":1.5}}"#);
        assert!(t.contains("\\t(750,750,\\fscx150\\fscy150)"), "{t}");
        // Blur (px at 1080 wide, a 1080-wide canvas here).
        let t = one(r#""words":{"upcoming":{"blur":3},"spoken":{"blur":2}}"#);
        assert!(t.contains("\\blur3"), "{t}");
        assert!(t.contains("\\t(400,400,\\blur0)"), "{t}");
        assert!(t.contains("\\t(750,750,\\blur2)"), "{t}");
    }

    #[test]
    fn lift_and_rotate_move_only_the_active_word() {
        // Lift 0.1 em of a 64 px face is 6.4 px up while "I" is active: the
        // word sits at its slot, then 6.4 px higher from 0.40 to 0.75, then back.
        let a = st(r#""words":{"active":{"lift":0.1}}"#);
        let e = ev(&a, "I");
        assert_eq!(e.len(), 3, "{a}");
        let ys: Vec<f64> = e.iter().map(|l| at(l).1).collect();
        assert!((ys[0] - ys[1] - 6.4).abs() < 0.11, "{ys:?}");
        assert!((ys[0] - ys[2]).abs() < 0.11, "{ys:?}");
        assert!(
            start(e[1]) == "0:00:00.40" && end(e[1]) == "0:00:00.75",
            "{a}"
        );
        // "so" lifts for its own span and then sits down: two pieces, the
        // second at the slot, like the one before "I".
        let so = ev(&a, "so");
        assert_eq!(so.len(), 2, "{a}");
        assert!((at(so[1]).1 - ys[0]).abs() < 0.11, "{a}");
        // Negative lift goes down.
        let a = st(r#""words":{"active":{"lift":-0.1}}"#);
        let e = ev(&a, "I");
        assert!(at(e[1]).1 > at(e[0]).1 + 6.0);
        // Rotate is clockwise: ASS turns the other way.
        // Tilt moves nothing, so it is a step inside the one event.
        let a = st(r#""words":{"active":{"rotate":5}}"#);
        let e = ev(&a, "I");
        assert_eq!(e.len(), 1, "{a}");
        assert!(tg(e[0]).contains("\\t(400,400,\\frz-5)"), "{a}");
        assert!(tg(e[0]).contains("\\t(750,750,\\frz0)"), "{a}");
        let a = st(r#""words":{"active":{"rotate":-3}}"#);
        assert!(tg(ev(&a, "I")[0]).contains("\\t(400,400,\\frz3)"), "{a}");
        // A word that starts the line is tilted from the first frame, and a
        // keyword (which lights in its own colour) puts its tilt down too.
        let a = st(r#""words":{"active":{"rotate":5}}"#);
        assert!(tg(ev(&a, "so")[0]).contains("\\frz-5"), "{a}");
        assert!(
            tg(ev(&a, "million")[0]).contains("\\t(350,350,\\frz0)"),
            "{a}"
        );
        // A gentle attack lifts in steps along a straight move.
        let a = st(r#""words":{"attack_ms":200,"attack_ease":"linear","active":{"lift":0.1}}"#);
        let e = ev(&a, "I");
        assert!(tg(e[1]).starts_with("\\an5\\q2\\move("), "{a}");
        let (y0, y1) = (at(e[1]).1, to(e[1]).1);
        assert!(y1 < y0, "{y0} {y1}");
        assert!(start(e[1]) == "0:00:00.40", "{a}");
    }

    #[test]
    fn attack_hold_and_release_land_on_the_expected_centiseconds() {
        // "I": starts 0.40, ends 0.75. Attack 200 ms from the start: 400..600.
        // Hold 100 ms after the end: 850. Release 500 ms: 850..1350.
        let json = r#""words":{"upcoming":{"opacity":0.5},"spoken":{"opacity":0.25},
            "attack_ms":200,"attack_ease":"linear","hold_ms":100,"release_ms":500,"release_ease":"linear"}"#;
        let a = st(json);
        let e = ev(&a, "I");
        assert_eq!(e.len(), 1, "{a}");
        let t = tg(e[0]);
        assert!(t.contains("\\t(400,600,\\alpha&H00&)"), "{t}");
        assert!(t.contains("\\t(850,1350,\\alpha&HBF&)"), "{t}");
        // The line (and its event) is cut at 1.55: a release past that is cut.
        let e3 = ev(&a, "3");
        assert!(end(e3[0]) == "0:00:01.55", "{a}");
        assert!(!tg(e3[0]).contains("\\t(1550"), "{a}");
        // "3" is the last word: its attack runs from its own start, and its
        // release would only begin at the line's end, so it is never drawn.
        assert!(tg(e3[0]).contains("\\t(1200,1400,\\alpha&H00&"), "{a}");
        assert!(!tg(e3[0]).contains("&HBF&"), "{a}");
        // The hold keeps the look past the word's end: no hold, release at 750.
        let a = st(
            r#""words":{"upcoming":{"opacity":0.5},"spoken":{"opacity":0.25},"release_ms":500,"release_ease":"linear"}"#,
        );
        assert!(tg(ev(&a, "I")[0]).contains("\\t(750,1250,\\alpha&HBF&)"));
        // An attack longer than the word holds the release back until it is done.
        let a = st(
            r#""words":{"upcoming":{"opacity":0.5},"spoken":{"opacity":0.25},"attack_ms":400,"attack_ease":"linear","release_ms":100,"release_ease":"linear"}"#,
        );
        let t = tg(ev(&a, "I")[0]).to_string();
        assert!(t.contains("\\t(400,800,\\alpha&H00&)"), "{t}");
        assert!(t.contains("\\t(800,900,\\alpha&HBF&)"), "{t}");
    }

    #[test]
    fn a_release_overlaps_the_next_words_attack() {
        // "so" ends at 0.35 and trails for 600 ms; "I" is lit from 0.40.
        let a = st(
            r##""words":{"active":{"color":"#FF0000"},"spoken":{"color":"#00FF00"},"release_ms":600,"release_ease":"linear"}"##,
        );
        let so = tg(ev(&a, "so")[0]).to_string();
        let i = tg(ev(&a, "I")[0]).to_string();
        assert!(so.contains("\\t(350,950,\\1c&H00FF00&)"), "{so}");
        assert!(i.contains("\\t(400,400,\\1c&H0000FF&)"), "{i}");
        // Both are on screen over the same moments: events share the line's span.
        assert_eq!(start(ev(&a, "so")[0]), start(ev(&a, "I")[0]));
    }

    #[test]
    fn every_ease_is_an_acceleration_or_a_two_step_back() {
        let run = |ease: &str| {
            let a = st(&format!(
                r##""words":{{"active":{{"color":"#FF0000","scale":1.2}},"spoken":{{"color":"#00FF00","scale":1}},
                    "hold_ms":100,"release_ms":500,"release_ease":"{ease}"}}"##
            ));
            tg(ev(&a, "I")[0]).to_string()
        };
        let lin = run("linear");
        assert!(
            lin.contains("\\t(850,1350,\\fscx100\\fscy100\\1c&H00FF00&)"),
            "{lin}"
        );
        let out = run("out");
        assert!(
            out.contains("\\t(850,1350,0.5,\\fscx100\\fscy100\\1c&H00FF00&)"),
            "{out}"
        );
        let inn = run("in");
        assert!(
            inn.contains("\\t(850,1350,2,\\fscx100\\fscy100\\1c&H00FF00&)"),
            "{inn}"
        );
        // Back: colour just eases out; the scale runs 10% past its target (98
        // for 120 -> 100), 58% of the way, and settles.
        let back = run("back");
        assert!(back.contains("\\t(850,1350,0.5,\\1c&H00FF00&)"), "{back}");
        assert!(
            back.contains("\\t(850,1140,0.4,\\fscx98\\fscy98)"),
            "{back}"
        );
        assert!(back.contains("\\t(1140,1350,\\fscx100\\fscy100)"), "{back}");
        // The same on the way in.
        let att = |ease: &str| {
            let a = st(&format!(
                r#""words":{{"upcoming":{{"opacity":0.5}},"attack_ms":200,"attack_ease":"{ease}"}}"#
            ));
            tg(ev(&a, "I")[0]).to_string()
        };
        assert!(att("linear").contains("\\t(400,600,\\alpha&H00&)"));
        assert!(att("out").contains("\\t(400,600,0.5,\\alpha&H00&)"));
        assert!(att("in").contains("\\t(400,600,2,\\alpha&H00&)"));
        assert!(att("back").contains("\\t(400,600,0.5,\\alpha&H00&)"));
        // Defaults: out on both.
        assert!(att("wobble").contains("\\t(400,600,0.5,\\alpha&H00&)"));
    }

    #[test]
    fn snap_turns_active_at_once_and_sweep_runs_across_the_word() {
        // "I" is 350 ms long and starts 400 ms into its line.
        let snap = st(r##""words":{"active":{"color":"#FF0000"}}"##);
        assert!(!snap.contains("\\kf"), "{snap}");
        let sweep = st(r##""words":{"fill":"sweep","active":{"color":"#FF0000"}}"##);
        let t = tg(ev(&sweep, "I")[0]).to_string();
        assert!(t.contains("\\1c&H0000FF&\\2c&HFFFFFF&\\k40\\kf35"), "{t}");
        // The first word of a line starts its sweep at once.
        assert!(
            tg(ev(&sweep, "so")[0]).contains("\\2c&HFFFFFF&\\kf35"),
            "{sweep}"
        );
        // Nothing to sweep when the colour does not change.
        let same = st(r##""words":{"fill":"sweep","active":{"color":"#FFFFFF"}}"##);
        let t = tg(ev(&same, "I")[0]).to_string();
        assert!(!t.contains("\\kf") && !t.contains("\\2c"), "{t}");
        // A sweep that is cut into moving slices keeps its place in the word.
        let a = st(
            r##""words":{"fill":"sweep","attack_ms":200,"attack_ease":"linear","active":{"color":"#FF0000","lift":0.1}}"##,
        );
        let e = ev(&a, "I");
        // The piece after the 200 ms lift starts 200 ms into the sweep: it began
        // 20 centiseconds ago.
        assert!(e.iter().any(|l| tg(l).contains("\\k-20\\kf35")), "{a}");
    }

    #[test]
    fn modes_all_build_and_single() {
        let a = st(r#""words":{"mode":"all","upcoming":{"opacity":0.5}}"#);
        assert_eq!(start(ev(&a, "I")[0]), "0:00:00.00");
        // Build: a word has no event before it is spoken.
        let b = st(r#""words":{"mode":"build"}"#);
        assert_eq!(start(ev(&b, "so")[0]), "0:00:00.00");
        assert_eq!(start(ev(&b, "I")[0]), "0:00:00.40");
        assert_eq!(start(ev(&b, "made")[0]), "0:00:00.80");
        assert_eq!(end(ev(&b, "made")[0]), "0:00:01.55");
        // ...and it comes in at its own start (default attack: 80 ms from nothing).
        assert!(tg(ev(&b, "I")[0]).contains("\\alpha&HFF&"), "{b}");
        assert!(
            tg(ev(&b, "I")[0]).contains("\\t(0,80,0.5,\\alpha&H00&)"),
            "{b}"
        );
        // The slots are the line's whole layout from the start: "so" does not
        // move when later words arrive.
        let x_so: Vec<f64> = ev(&b, "so").iter().map(|l| at(l).0).collect();
        assert!(x_so.iter().all(|x| (x - x_so[0]).abs() < 0.11), "{x_so:?}");
        // Single: one word at a time, each up until the next one starts, all at
        // the same spot.
        let s = st(r#""words":{"mode":"single"}"#);
        let (so, i, made) = (ev(&s, "so"), ev(&s, "I"), ev(&s, "made"));
        assert_eq!((start(so[0]), end(so[0])), ("0:00:00.00", "0:00:00.40"));
        assert_eq!((start(i[0]), end(i[0])), ("0:00:00.40", "0:00:00.80"));
        assert_eq!(start(made[0]), "0:00:00.80");
        assert_eq!(end(ev(&s, "3")[0]), "0:00:01.55");
        assert!((at(so[0]).0 - at(i[0]).0).abs() < 0.11);
        assert!((at(so[0]).1 - at(i[0]).1).abs() < 0.11);
        // A spoken look that is invisible clears the word when its release ends.
        let s = st(
            r#""words":{"mode":"single","spoken":{"opacity":0},"release_ms":100,"release_ease":"linear"}"#,
        );
        assert_eq!(end(ev(&s, "so")[0]), "0:00:00.40");
        let s = st(
            r#""words":{"mode":"single","spoken":{"opacity":0},"release_ms":20,"release_ease":"linear"}"#,
        );
        assert_eq!(end(ev(&s, "so")[0]), "0:00:00.37");
        // Only one word is ever on screen.
        let s = st(r#""words":{"mode":"single"}"#);
        let mut spans: Vec<(f64, f64)> = dialogues(&s)
            .iter()
            .map(|l| (stamp_secs(start(l)), stamp_secs(end(l))))
            .collect();
        spans.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for w in spans.windows(2) {
            assert!(w[1].0 >= w[0].1 - 1e-9, "{w:?}");
        }
    }

    #[test]
    fn keywords_light_in_their_colour_and_scale_by_the_keyword_scale() {
        // "million" opens line 2 (spoken 1.60 to 1.95) and is a keyword.
        let hd = |json: &str| {
            let a = st(json);
            tg(ev(&a, "million")[0]).to_string()
        };
        // The keyword colour (the style's accent: yellow) replaces the active
        // colour, and is what stays once it is spoken.
        let t = hd(r##""words":{"active":{"color":"#FF0000"}}"##);
        assert!(t.contains("\\1c&H00FFFF&"), "{t}");
        assert!(!t.contains("\\t("), "{t}");
        // An ordinary word takes the active colour.
        let a = st(r##""words":{"active":{"color":"#FF0000"}}"##);
        assert!(
            tg(ev(&a, "dollars")[0]).contains("\\t(400,400,\\1c&H0000FF&)"),
            "{a}"
        );
        // The v1 accent is the default keyword colour; the v2 colour beats it.
        let t = hd(r##""accent":"#00FF00","words":{"active":{"color":"#FF0000"}}"##);
        assert!(t.contains("\\1c&H00FF00&"), "{t}");
        let t = hd(
            r##""accent":"#00FF00","words":{"keyword":{"color":"#FF00FF"},"active":{"color":"#FF0000"}}"##,
        );
        assert!(t.contains("\\1c&HFF00FF&"), "{t}");
        // An explicit spoken colour wins over the keyword colour once spoken.
        let t = hd(r##""words":{"spoken":{"color":"#888888"}}"##);
        assert!(t.contains("\\t(350,350,\\1c&H888888&)"), "{t}");
        // Keyword scale multiplies the active and spoken scales.
        let t = hd(r#""words":{"keyword":{"scale":1.3}}"#);
        assert!(t.contains("\\fscx130\\fscy130"), "{t}");
        let t = hd(
            r#""words":{"keyword":{"scale":1.5},"active":{"scale":1.2},"spoken":{"scale":0.8}}"#,
        );
        assert!(t.contains("\\fscx180\\fscy180"), "{t}");
        assert!(t.contains("\\t(350,350,\\fscx120\\fscy120)"), "{t}");
        // Today's bump stays with the pop entrance: 14% up, then back, when the
        // word starts after the entrance has settled (and not with a keyword scale).
        let bump = |json: &str| {
            let a = build_look("minimal", json);
            ev(&a, "3").iter().any(|l| tg(l).contains("\\fscx114"))
        };
        let a = build_look("minimal", r#"{"words":{"upcoming":{"opacity":0.9}}}"#);
        // "3" starts 1200 ms in, and its event began at 200 ms: 90 up, 150 back.
        assert!(
            tg(ev(&a, "3").last().unwrap())
                .contains("\\t(1000,1090,\\fscx114\\fscy114)\\t(1090,1240,\\fscx100\\fscy100)"),
            "{a}"
        );
        assert!(!bump(r#"{"words":{"keyword":{"scale":1.1}}}"#));
        assert!(!bump(
            r#"{"anim":"fade","words":{"upcoming":{"opacity":0.9}}}"#
        ));
    }

    #[test]
    fn out_of_range_and_bad_word_values_fall_back() {
        let same = |bad: &str, good: &str| {
            assert_eq!(
                build_look("minimal", &format!(r#"{{"words":{bad}}}"#)),
                build_look("minimal", &format!(r#"{{"words":{good}}}"#)),
                "{bad}"
            );
        };
        same(
            r#"{"upcoming":{"opacity":3}}"#,
            r#"{"upcoming":{"opacity":1}}"#,
        );
        same(
            r#"{"upcoming":{"opacity":-1}}"#,
            r#"{"upcoming":{"opacity":0}}"#,
        );
        same(r#"{"active":{"scale":9}}"#, r#"{"active":{"scale":1.5}}"#);
        same(r#"{"active":{"scale":0.1}}"#, r#"{"active":{"scale":0.5}}"#);
        same(r#"{"upcoming":{"blur":99}}"#, r#"{"upcoming":{"blur":10}}"#);
        same(
            r#"{"active":{"lift":5,"rotate":99}}"#,
            r#"{"active":{"lift":0.3,"rotate":10}}"#,
        );
        same(
            r#"{"active":{"lift":-5,"rotate":-99}}"#,
            r#"{"active":{"lift":-0.3,"rotate":-10}}"#,
        );
        same(r#"{"attack_ms":9999}"#, r#"{"attack_ms":400}"#);
        same(
            r#"{"hold_ms":9999,"release_ms":99999}"#,
            r#"{"hold_ms":600,"release_ms":2000}"#,
        );
        same(r#"{"hold_ms":-4}"#, r#"{"hold_ms":0}"#);
        // Fields a state does not have are ignored, and bad enums and colours
        // count as absent.
        same(
            r#"{"upcoming":{"lift":0.2,"rotate":4,"opacity":0.8},"active":{"blur":5}}"#,
            r#"{"upcoming":{"opacity":0.8}}"#,
        );
        same(
            r##"{"mode":"sideways","fill":"wipe","attack_ease":"wobble","active":{"color":"#12345"},"upcoming":{"opacity":0.8}}"##,
            r#"{"upcoming":{"opacity":0.8}}"#,
        );
        let ent = |bad: &str, good: &str| {
            assert_eq!(
                build_look("minimal", &format!(r#"{{"enter":{bad}}}"#)),
                build_look("minimal", &format!(r#"{{"enter":{good}}}"#)),
                "{bad}"
            );
        };
        ent(
            r#"{"kind":"slide_up","ms":99999}"#,
            r#"{"kind":"slide_up","ms":800}"#,
        );
        ent(r#"{"kind":"zoom","ms":-5}"#, r#"{"kind":"zoom","ms":0}"#);
        ent(r#"{"kind":"fade","ease":"wobble"}"#, r#"{"kind":"fade"}"#);
        let ex = |bad: &str, good: &str| {
            assert_eq!(
                build_look("minimal", &format!(r#"{{"exit":{bad}}}"#)),
                build_look("minimal", &format!(r#"{{"exit":{good}}}"#)),
                "{bad}"
            );
        };
        ex(
            r#"{"kind":"zoom","ms":99999}"#,
            r#"{"kind":"zoom","ms":600}"#,
        );
        // A kind with no time is no motion.
        assert_eq!(
            build_look(
                "minimal",
                r#"{"enter":{"kind":"zoom","ms":0},"exit":{"kind":"fade","ms":0}}"#
            ),
            build_look(
                "minimal",
                r#"{"enter":{"kind":"none"},"exit":{"kind":"none"}}"#
            ),
        );
    }

    #[test]
    fn v1_anim_maps_onto_mode_and_motion_and_v2_wins() {
        let nudge = r#""upcoming":{"opacity":0.9}"#;
        let look = |anim: &str, extra: &str| {
            build_look(
                "minimal",
                &format!(r#"{{"anim":"{anim}","words":{{{nudge}{extra}}}}}"#),
            )
        };
        // words -> build + pop
        let w = look("words", "");
        assert_eq!(start(ev(&w, "I")[0]), "0:00:00.40", "{w}");
        assert!(tg(ev(&w, "so")[0]).contains("\\fscx84\\fscy84"), "{w}");
        // An explicit mode beats it; the entrance stays.
        let w = look("words", r#","mode":"all""#);
        assert_eq!(start(ev(&w, "I")[0]), "0:00:00.00", "{w}");
        // The flat option means the same: the Look's own words beat it.
        let o = AssOpts {
            anim: Anim::Words,
            ..with(r#"{"words":{"mode":"all","upcoming":{"opacity":0.9}}}"#)
        };
        let w = build_for(&sample(), "minimal", 0.0, &o);
        assert_eq!(start(ev(&w, "I")[0]), "0:00:00.00", "{w}");
        let o = AssOpts {
            anim: Anim::Words,
            ..with(r#"{"words":{"upcoming":{"opacity":0.9}}}"#)
        };
        let w = build_for(&sample(), "minimal", 0.0, &o);
        assert_eq!(start(ev(&w, "I")[0]), "0:00:00.40", "{w}");
        // pop -> pop in, a 60 ms fade out
        let p = look("pop", "");
        let e = ev(&p, "so");
        assert!(tg(e[0]).contains("\\fscx84\\fscy84"), "{p}");
        assert!(tg(e.last().unwrap()).contains("\\t("), "{p}");
        assert!(tg(e.last().unwrap()).contains("\\alpha&HFF&)"), "{p}");
        // none -> no entrance, no exit
        let n = look("none", "");
        let t = tg(ev(&n, "so")[0]).to_string();
        assert!(
            !t.contains("fsc") && !t.contains("\\move") && !t.contains("\\alpha&HFF&"),
            "{t}"
        );
        // fade -> fades in, no scale
        let f = look("fade", "");
        let t = tg(ev(&f, "so")[0]).to_string();
        assert!(t.contains("\\alpha&HFF&") && !t.contains("fsc"), "{t}");
        // slide -> rises
        let s = look("slide", "");
        assert!(tg(ev(&s, "so")[0]).contains("\\move("), "{s}");
        // bounce -> deeper start
        let b = look("bounce", "");
        assert!(tg(ev(&b, "so")[0]).contains("\\fscx70\\fscy70"), "{b}");
        // v2 enter beats the shorthand's, kind by kind.
        let s = build_look(
            "minimal",
            &format!(r#"{{"anim":"slide","enter":{{"kind":"fade"}},"words":{{{nudge}}}}}"#),
        );
        let t = tg(ev(&s, "so")[0]).to_string();
        assert!(t.contains("\\alpha&HFF&") && !t.contains("\\move"), "{t}");
        // A time alone keeps the shorthand's kind.
        let s = build_look(
            "minimal",
            r#"{"anim":"slide","enter":{"ms":400},"exit":{"ms":300}}"#,
        );
        assert!(tg(ev(&s, "so")[0]).contains("\\move("), "{s}");
        // A static shorthand with word looks keeps the static grouping: the
        // lines run exactly the words' span.
        let st_ = look("none", "");
        assert!(
            dialogues(&st_)
                .iter()
                .any(|l| l.contains("0:00:00.00,0:00:01.55,")),
            "{st_}"
        );
    }

    fn secs_of(s: &str) -> f64 {
        stamp_secs(s)
    }

    #[test]
    fn every_entrance_kind_starts_where_it_should() {
        let first = |kind: &str| {
            let a = build_look(
                "minimal",
                &format!(r#"{{"enter":{{"kind":"{kind}","ms":300,"ease":"linear"}}}}"#),
            );
            let e: Vec<String> = ev(&a, "so").iter().map(|s| s.to_string()).collect();
            e
        };
        let last_y = |e: &[String]| at(e.last().unwrap()).1;
        let last_x = |e: &[String]| at(e.last().unwrap()).0;
        // none: the line is just there.
        let n = first("none");
        assert!(
            !tg(&n[0]).contains("\\move") && !tg(&n[0]).contains("\\t(0,"),
            "{n:?}"
        );
        // pop 0.84, zoom 0.6, bounce 0.7: the line starts smaller.
        assert!(tg(&first("pop")[0]).contains("\\fscx84\\fscy84"));
        assert!(tg(&first("zoom")[0]).contains("\\fscx60\\fscy60"));
        assert!(tg(&first("bounce")[0]).contains("\\fscx70\\fscy70"));
        // fade and blur start clear; blur starts soft.
        assert!(tg(&first("fade")[0]).contains("\\alpha&HFF&"));
        let b = first("blur");
        assert!(
            tg(&b[0]).contains("\\blur8") && tg(&b[0]).contains("\\alpha&HFF&"),
            "{b:?}"
        );
        // slides start 2.2% of the height lower / higher, 4% of the width right / left.
        let e = first("slide_up");
        assert!((at(&e[0]).1 - last_y(&e) - 42.2).abs() < 0.3, "{e:?}");
        let e = first("slide_down");
        assert!((at(&e[0]).1 - last_y(&e) + 42.2).abs() < 0.3, "{e:?}");
        let e = first("slide_left");
        assert!((at(&e[0]).0 - last_x(&e) - 43.2).abs() < 0.3, "{e:?}");
        let e = first("slide_right");
        assert!((at(&e[0]).0 - last_x(&e) + 43.2).abs() < 0.3, "{e:?}");
        // drop starts 6% of the height above.
        let e = first("drop");
        assert!((at(&e[0]).1 - last_y(&e) + 115.2).abs() < 0.3, "{e:?}");
        // The entrance is over in the time given: the word is at rest after 300 ms.
        let e = first("slide_up");
        let rest = e.iter().find(|l| secs_of(start(l)) >= 0.3 - 1e-9).unwrap();
        assert!(tg(rest).contains("\\pos("), "{rest}");
        // Every kind ends at rest, at the same place.
        for k in [
            "pop",
            "fade",
            "slide_up",
            "slide_down",
            "slide_left",
            "slide_right",
            "zoom",
            "bounce",
            "blur",
            "drop",
        ] {
            let e = first(k);
            let l = e.last().unwrap();
            assert!(tg(l).contains("\\pos("), "{k}: {l}");
        }
    }

    #[test]
    fn entrance_ease_shapes_the_travel() {
        let ys = |ease: &str| {
            let a = build_look(
                "minimal",
                &format!(r#"{{"enter":{{"kind":"slide_up","ms":400,"ease":"{ease}"}}}}"#),
            );
            ev(&a, "so").iter().map(|l| at(l).1).collect::<Vec<_>>()
        };
        let rest = |v: &[f64]| *v.last().unwrap();
        // How far the line has come at 100 ms (a quarter of the time).
        let travelled = |ease: &str| {
            let a = build_look(
                "minimal",
                &format!(r#"{{"enter":{{"kind":"slide_up","ms":400,"ease":"{ease}"}}}}"#),
            );
            let e = ev(&a, "so");
            let r = at(e.last().unwrap()).1;
            let l = e
                .iter()
                .find(|l| secs_of(start(l)) >= 0.1 - 1e-9)
                .copied()
                .unwrap_or(e[0]);
            (at(e[0]).1 - at(l).1) / (at(e[0]).1 - r)
        };
        let (lin, out, inn) = (travelled("linear"), travelled("out"), travelled("in"));
        assert!(out > lin + 0.1 && inn < lin - 0.1, "{lin} {out} {inn}");
        // Back runs past the end and settles: some slice goes above the rest
        // position... for a rising slide, past it is higher (smaller y).
        let b = ys("back");
        assert!(b.iter().any(|y| *y < rest(&b) - 0.5), "{b:?}");
        let l = ys("linear");
        assert!(l.iter().all(|y| *y >= rest(&l) - 0.11), "{l:?}");
    }

    #[test]
    fn every_exit_kind_ends_where_it_should() {
        let last = |kind: &str, ms: u32| {
            let a = build_look(
                "minimal",
                &format!(r#"{{"exit":{{"kind":"{kind}","ms":{ms}}}}}"#),
            );
            let e = ev(&a, "year.");
            (a.clone(), e.last().unwrap().to_string(), e.len())
        };
        // none: a hard cut, nothing happens at the end.
        let (_, e, _) = last("none", 200);
        assert!(!tg(&e).contains("\\t("), "{e}");
        // fade: 100 ms of fading out, ending clear exactly at the line's end.
        let (a, e, _) = last("fade", 100);
        let ends = secs_of(end(&e));
        assert!(tg(&e).contains("\\alpha&HFF&)"), "{a}\n{e}");
        let from = tg(&e)
            .split("\\t(")
            .nth(1)
            .unwrap()
            .split(',')
            .next()
            .unwrap()
            .parse::<f64>()
            .unwrap();
        let line_ms = (ends - secs_of(start(&e))) * 1000.0;
        assert!(
            (line_ms - from - 100.0).abs() <= 10.0,
            "{line_ms} {from}\n{e}"
        );
        // zoom shrinks to 60%, blur to 8, slides 2.2% of the height.
        let (_, e, _) = last("zoom", 200);
        assert!(tg(&e).contains("\\fscx60\\fscy60"), "{e}");
        let (_, e, _) = last("blur", 200);
        assert!(
            tg(&e).contains("\\blur8") && tg(&e).contains("\\alpha&HFF&"),
            "{e}"
        );
        let (_, e, _) = last("slide_up", 200);
        let (_, y1) = to(&e);
        let (a0, _, _) = last("none", 200);
        let rest_y = at(ev(&a0, "year.").last().unwrap()).1;
        assert!((rest_y - y1 - 42.2).abs() < 0.3, "{rest_y} {y1}");
        let (_, e, _) = last("slide_down", 200);
        assert!((to(&e).1 - rest_y - 42.2).abs() < 0.3, "{e}");
        // A longer exit starts earlier.
        let (_, long, n_long) = last("fade", 400);
        let (_, short, n_short) = last("fade", 100);
        assert!(secs_of(start(&long)) <= secs_of(start(&short)) + 1e-9);
        assert!(n_long >= 1 && n_short >= 1);
    }

    #[test]
    fn the_line_stays_put_when_a_word_scales_lifts_or_tilts() {
        // Wherever a word goes in time, its slot's x never changes and, without
        // lift, neither does its y: the neighbours do not move.
        let a = st(
            r#""words":{"active":{"scale":1.4,"rotate":3},"spoken":{"scale":1.2},"attack_ms":200,"release_ms":300}"#,
        );
        for w in ["so", "I", "made", "3"] {
            let e = ev(&a, w);
            let (x0, y0) = at(e[0]);
            for l in &e {
                let (x, y) = at(l);
                assert!((x - x0).abs() < 0.11 && (y - y0).abs() < 0.11, "{w}: {l}");
            }
        }
        // With lift only the y moves.
        let a =
            st(r#""words":{"active":{"lift":0.2,"scale":1.3},"attack_ms":160,"release_ms":300}"#);
        for w in ["so", "I", "made", "3"] {
            let e = ev(&a, w);
            let x0 = at(e[0]).0;
            assert!(e.iter().all(|l| (at(l).0 - x0).abs() < 0.11), "{w}");
        }
        // A line that wraps wraps the same way for its whole life.
        let a = build_look(
            "minimal",
            r#"{"anim":"none","size":2,"words":{"active":{"scale":1.5},"spoken":{"scale":1.25},"release_ms":200}}"#,
        );
        for w in ["million", "dollars", "last", "year."] {
            let e = ev(&a, w);
            let (x0, y0) = at(e[0]);
            assert!(
                e.iter()
                    .all(|l| (at(l).0 - x0).abs() < 0.11 && (at(l).1 - y0).abs() < 0.11),
                "{w}"
            );
        }
        let ys: std::collections::BTreeSet<i64> = ["million", "dollars", "last", "year."]
            .iter()
            .map(|w| at(ev(&a, w)[0]).1.round() as i64)
            .collect();
        assert!(ys.len() >= 2, "a 128 px line of four words wraps: {ys:?}");
    }

    #[test]
    fn a_wrapped_line_is_a_block_centred_on_its_anchor() {
        // Two rows of four slots: the block's middle is where one row's would be.
        let one = build_look("minimal", r#"{"anim":"none","words":{"mode":"all"}}"#);
        let two = build_look(
            "minimal",
            r#"{"anim":"none","size":2,"words":{"mode":"all"}}"#,
        );
        let mid = |a: &str| {
            let ys: Vec<f64> = ["so", "I", "made", "3"]
                .iter()
                .map(|w| at(ev(a, w)[0]).1)
                .collect();
            (ys.iter().cloned().fold(f64::MAX, f64::min)
                + ys.iter().cloned().fold(f64::MIN, f64::max))
                / 2.0
        };
        // Bottom-anchored: the block's bottom edge stays at the margin, so two
        // rows sit higher than one by half a row.
        assert!(mid(&one) > mid(&two), "{} {}", mid(&one), mid(&two));
    }

    #[test]
    fn a_box_is_drawn_once_per_row_and_follows_the_line() {
        let a = build_look("hormozi", r#"{"words":{"upcoming":{"opacity":0.5}}}"#);
        let style = style_line(&a, "HormoziBox");
        assert_eq!(style[15], "3");
        // The text style has no box.
        assert_eq!(style_line(&a, "Hormozi")[15], "1");
        let boxes: Vec<&str> = a.lines().filter(|l| l.contains(",HormoziBox,")).collect();
        assert!(!boxes.is_empty());
        // The box event comes before its words and carries the line's fade.
        let first_box = a.find(",HormoziBox,").unwrap();
        let first_word = a.find(",Hormozi,,0,0,0,,{").unwrap();
        assert!(first_box < first_word, "{a}");
        assert!(boxes[0].contains("\\3a&H"), "{}", boxes[0]);
        // Single mode: a box per word, around that word only.
        let s = build_look("hormozi", r#"{"words":{"mode":"single"}}"#);
        let n_box = s.lines().filter(|l| l.contains(",HormoziBox,")).count();
        let n_word = s
            .lines()
            .filter(|l| l.contains(",Hormozi,,0,0,0,,{"))
            .count();
        assert_eq!(n_box, n_word, "{s}");
    }

    #[test]
    fn the_word_model_works_on_every_style_and_canvas() {
        let look = r##"{"words":{"mode":"build","upcoming":{"opacity":0.4,"blur":2},
            "active":{"color":"#FFD400","scale":1.1,"lift":0.05,"rotate":-2},"spoken":{"opacity":0.8},
            "keyword":{"scale":1.2},"fill":"sweep","release_ms":300},
            "enter":{"kind":"drop","ms":300},"exit":{"kind":"blur","ms":150}}"##;
        for style in [
            "karaoke",
            "hormozi",
            "minimal",
            "beast",
            "neon",
            "highlight",
            "ghost",
            "tiktok",
        ] {
            for (w, h) in [(1080, 1920), (1080, 1350), (1080, 1080), (1920, 1080)] {
                let o = AssOpts { w, h, ..with(look) };
                let a = build_for(&sample(), style, 0.0, &o);
                assert!(
                    a.lines().filter(|l| l.starts_with("Dialogue: 0,")).count() > 10,
                    "{style}"
                );
                // Every override block closes, every time is ordered.
                for l in a.lines().filter(|l| l.starts_with("Dialogue: 0,")) {
                    assert_eq!(l.matches('{').count(), l.matches('}').count(), "{l}");
                    assert!(secs_of(end(l)) > secs_of(start(l)), "{l}");
                    assert!(!l.contains("NaN") && !l.contains("inf"), "{l}");
                }
            }
        }
    }

    #[test]
    fn a_minute_of_captions_stays_a_reasonable_event_count() {
        // 12 words, two lines of six.
        let ws: Vec<Word> = (0..12)
            .map(|i| Word {
                w: [
                    "Most", "people", "never", "learn", "how", "to", "speak", "on", "camera.",
                    "Try", "it", "today.",
                ][i]
                    .into(),
                s: 0.3 + i as f64 * 0.4,
                e: 0.3 + i as f64 * 0.4 + 0.35,
                conf: None,
            })
            .collect();
        let count = |json: &str| {
            let o = with(json);
            let a = build_for(&ws, "karaoke", 0.0, &o);
            a.lines().filter(|l| l.starts_with("Dialogue: 0,")).count()
        };
        let before = count("{}");
        let after = count(r#"{"words":{"release_ms":400,"spoken":{"opacity":0.6}}}"#);
        assert!(before <= 6, "{before}");
        // The word model costs events, but stays in the tens per line.
        assert!(after < 12 * 14, "{after}");
        let lift = count(r#"{"words":{"active":{"lift":0.1},"attack_ms":120,"release_ms":300}}"#);
        assert!(lift < 12 * 24, "{lift}");
    }

    // ---- the text-dressing fields (look.captions.fx / look.captions.type) ----

    fn layer(ass: &str, n: u8) -> Vec<&str> {
        let p = format!("Dialogue: {n},");
        ass.lines().filter(|l| l.starts_with(&p)).collect()
    }

    /// The text of an event: what follows its first override block.
    fn body(e: &str) -> &str {
        let at = e.find(",,0,0,0,,").map_or(0, |i| i + 9);
        let rest = &e[at..];
        rest.find('}').map_or(rest, |i| &rest[i + 1..])
    }

    /// Numbers after a tag name in one event, e.g. `\\blur` -> [8.0].
    fn tag_nums(e: &str, tag: &str) -> Vec<f64> {
        e.split(tag)
            .skip(1)
            .filter_map(|r| {
                let n: String = r
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
                    .collect();
                n.parse().ok()
            })
            .collect()
    }

    /// (start, end) pairs of events, in order.
    fn times(evs: &[&str]) -> Vec<(String, String)> {
        evs.iter()
            .map(|e| (start(e).to_string(), end(e).to_string()))
            .collect()
    }

    fn karaoke_px() -> f64 {
        84.0
    }

    fn face() -> &'static crate::captions::metrics::Face {
        crate::captions::metrics::face("Archivo Black").unwrap()
    }

    #[test]
    fn any_new_field_switches_the_caption_to_the_positioned_writer() {
        let base = build_look("karaoke", "{}");
        assert!(base.contains("\\k"), "the line writer");
        for j in [
            r#"{"spacing":0.1}"#,
            r#"{"line_gap":1.2}"#,
            r#"{"lines":2}"#,
            r#"{"max_chars":20}"#,
            r#"{"align":"left"}"#,
            r#"{"rotate":3}"#,
            r##"{"stroke":{"width":4}}"##,
            r##"{"shadow":{"x":3}}"##,
            r##"{"glow":{"size":10}}"##,
            r##"{"box":{"radius":1}}"##,
            r##"{"words":{"active":{"glow":{"size":9}}}}"##,
            r##"{"words":{"keyword":{"glow":{"size":9}}}}"##,
        ] {
            let a = build_look("karaoke", j);
            assert!(!a.contains("\\k"), "{j}: {a}");
            assert!(a.contains("\\an5"), "{j}");
        }
        // The v1 forms alone keep the line writer.
        for j in [
            r##"{"box":"#101010"}"##,
            r#"{"box":"none"}"#,
            r#"{"shadow":4}"#,
            r##"{"outline":"#FF0000","outline_w":5}"##,
            r#"{"max_words":2}"#,
        ] {
            assert!(build_look("karaoke", j).contains("\\k"), "{j}");
        }
        // Junk in the new fields is nothing.
        for j in [
            r#"{"glow":{"size":"big"}}"#,
            r#"{"glow":{}}"#,
            r#"{"box":{"radius":"round"}}"#,
            r#"{"stroke":5}"#,
            r#"{"align":"middle"}"#,
            r#"{"spacing":"wide"}"#,
        ] {
            assert_eq!(build_look("karaoke", j), base, "{j}");
        }
    }

    #[test]
    fn letter_spacing_is_a_fraction_of_the_type_size_and_widens_the_line() {
        let ws = &sample();
        let a = build_look("karaoke", r#"{"spacing":0.1}"#);
        let e = &layer(&a, 0)[0];
        assert!(
            tag_nums(e, "\\fsp")
                .iter()
                .all(|v| (v - 0.1 * karaoke_px()).abs() < 0.06),
            "{e}"
        );
        assert!(e.contains("\\fsp8.4"), "{e}");
        // Clamped to its range, a bad value is absent, zero says nothing.
        assert_eq!(
            build_look("karaoke", r#"{"spacing":9}"#),
            build_look("karaoke", r#"{"spacing":0.3}"#)
        );
        assert!(!build_look("karaoke", r#"{"spacing":0}"#).contains("\\fsp"));
        // Wider: the first line's first and last words sit further apart.
        let span = |j: &str| {
            let j = j.replace('}', r#","anim":"none"}"#);
            let a = build_for(ws, "karaoke", 0.0, &with(&j));
            let l = layer(&a, 0);
            (at(l[2]).0 - at(l[0]).0).abs()
        };
        assert!(span(r#"{"spacing":0.2}"#) > span(r#"{"spacing":0.01}"#) + 20.0);
        // Tighter is allowed.
        assert!(build_look("karaoke", r#"{"spacing":-0.05}"#).contains("\\fsp-4.2"));
    }

    #[test]
    fn line_gap_scales_the_row_pitch_and_clamps() {
        let rows_y = |j: &str| {
            let a = build_look("karaoke", &format!(r#"{{"max_words":8,"size":2,{j}}}"#));
            let l = layer(&a, 0);
            let mut ys: Vec<f64> = l.iter().map(|e| at(e).1).collect();
            ys.sort_by(|a, b| a.total_cmp(b));
            ys.dedup_by(|a, b| (*a - *b).abs() < 0.5);
            ys
        };
        let one = rows_y(r#""line_gap":1"#);
        let two = rows_y(r#""line_gap":1.4"#);
        assert!(one.len() >= 2 && two.len() >= 2, "{one:?} {two:?}");
        let (p1, p2) = (one[1] - one[0], two[1] - two[0]);
        assert!((p2 / p1 - 1.4).abs() < 0.02, "{p1} {p2}");
        // Clamped.
        assert_eq!(
            build_look("karaoke", r#"{"line_gap":9}"#),
            build_look("karaoke", r#"{"line_gap":1.6}"#)
        );
        assert_eq!(
            build_look("karaoke", r#"{"line_gap":0}"#),
            build_look("karaoke", r#"{"line_gap":0.8}"#)
        );
    }

    #[test]
    fn lines_1_never_wraps_and_lines_2_stays_in_two() {
        let one = build_look(
            "karaoke",
            r#"{"lines":1,"max_chars":40,"max_words":8,"size":1.6,"anim":"none"}"#,
        );
        // Every block is one row: all its words share a y.
        let mut by_start: std::collections::BTreeMap<String, Vec<f64>> = Default::default();
        for e in layer(&one, 0) {
            by_start
                .entry(start(e).to_string())
                .or_default()
                .push(at(e).1);
        }
        assert!(by_start.len() > 2);
        for (s, ys) in &by_start {
            assert!(ys.iter().all(|y| (y - ys[0]).abs() < 0.5), "{s} {ys:?}");
        }
        // The same text with room for two rows needs fewer blocks.
        let two = build_look(
            "karaoke",
            r#"{"lines":2,"max_chars":40,"max_words":8,"size":1.6,"anim":"none"}"#,
        );
        let blocks = |a: &str| {
            layer(a, 0)
                .iter()
                .map(|e| start(e).to_string())
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        };
        assert!(
            blocks(&two) < blocks(&one),
            "{} {}",
            blocks(&two),
            blocks(&one)
        );
        // At most two distinct rows per block.
        let mut rows: std::collections::BTreeMap<String, Vec<i64>> = Default::default();
        for e in layer(&two, 0) {
            rows.entry(start(e).to_string())
                .or_default()
                .push(at(e).1.round() as i64);
        }
        for (s, ys) in &mut rows {
            ys.sort();
            ys.dedup();
            assert!(ys.len() <= 2, "{s} {ys:?}");
        }
        // Out of range clamps.
        assert_eq!(
            build_look("karaoke", r#"{"lines":9}"#),
            build_look("karaoke", r#"{"lines":2}"#)
        );
        assert_eq!(
            build_look("karaoke", r#"{"lines":0}"#),
            build_look("karaoke", r#"{"lines":1}"#)
        );
    }

    #[test]
    fn max_chars_is_a_hard_limit_per_block() {
        let ws: Vec<Word> =
            "so I made 3 million dollars last year. Never stop building the best thing ever"
                .split(' ')
                .enumerate()
                .map(|(i, w)| Word {
                    w: w.into(),
                    s: i as f64 * 0.4,
                    e: i as f64 * 0.4 + 0.35,
                    conf: Some(0.9),
                })
                .collect();
        for n in [6usize, 12, 20, 40] {
            for anim in ["pop", "none"] {
                let o = with(&format!(r#"{{"max_chars":{n},"anim":"{anim}"}}"#));
                let a = build_for(&ws, "karaoke", 0.0, &o);
                let mut blocks: std::collections::BTreeMap<String, Vec<String>> =
                    Default::default();
                for e in layer(&a, 0) {
                    blocks
                        .entry(start(e).to_string())
                        .or_default()
                        .push(body(e).to_string());
                }
                assert!(!blocks.is_empty());
                for (s, words) in &blocks {
                    let len = words.join(" ").chars().count();
                    // A single word longer than the budget is its own block.
                    assert!(len <= n || words.len() == 1, "{n} {anim} {s} {words:?}");
                }
            }
        }
        // Fewer characters, more blocks; and no style budget gets in the way.
        let count = |n: usize| {
            let a = build_for(
                &ws,
                "karaoke",
                0.0,
                &with(&format!(r#"{{"max_chars":{n}}}"#)),
            );
            layer(&a, 0)
                .iter()
                .map(|e| start(e).to_string())
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        };
        assert!(count(8) > count(40));
        // Out of range clamps.
        assert_eq!(
            build_look("karaoke", r#"{"max_chars":1}"#),
            build_look("karaoke", r#"{"max_chars":6}"#)
        );
    }

    #[test]
    fn max_words_still_means_at_most_n_words_per_block() {
        for n in 1..=8usize {
            let a = build_look(
                "minimal",
                &format!(r#"{{"max_words":{n},"enter":{{"kind":"fade"}}}}"#),
            );
            let mut blocks: std::collections::BTreeMap<String, usize> = Default::default();
            for e in layer(&a, 0) {
                *blocks.entry(start(e).to_string()).or_default() += 1;
            }
            assert!(blocks.values().all(|c| *c <= n), "{n} {blocks:?}");
        }
    }

    #[test]
    fn align_places_the_rows_of_a_block_against_one_edge() {
        let look = |a: &str| {
            let ass = build_look(
                "karaoke",
                &format!(
                    r#"{{"max_words":6,"max_chars":40,"size":1.25,"anim":"none","align":"{a}"}}"#
                ),
            );
            // Rows of the first block: first and last word of each.
            let evs = layer(&ass, 0);
            let first_start = start(evs[0]).to_string();
            let blk: Vec<&&str> = evs.iter().filter(|e| start(e) == first_start).collect();
            let mut rows: std::collections::BTreeMap<i64, Vec<(f64, String)>> = Default::default();
            for e in &blk {
                let (x, y) = at(e);
                rows.entry(y.round() as i64)
                    .or_default()
                    .push((x, body(e).to_string()));
            }
            rows.into_values()
                .map(|mut r| {
                    r.sort_by(|a, b| a.0.total_cmp(&b.0));
                    r
                })
                .collect::<Vec<_>>()
        };
        let sz = 1.25;
        let edge = |a: &str| -> Vec<(f64, f64)> {
            look(a)
                .into_iter()
                .map(|r| {
                    let f = |t: &str| face().width(t, karaoke_px() * sz);
                    let left = r[0].0 - f(&r[0].1) / 2.0;
                    let last = r.last().unwrap();
                    (left, last.0 + f(&last.1) / 2.0)
                })
                .collect()
        };
        let (l, c, r) = (edge("left"), edge("center"), edge("right"));
        assert!(l.len() >= 2, "{l:?}");
        // Left: the rows start together; right: they end together; centre: neither.
        assert!((l[0].0 - l[1].0).abs() < 1.5, "{l:?}");
        assert!((r[0].1 - r[1].1).abs() < 1.5, "{r:?}");
        assert!(
            (c[0].0 - c[1].0).abs() > 5.0 && (c[0].1 - c[1].1).abs() > 5.0,
            "{c:?}"
        );
        // The block keeps its place: the widest row spans the same extent.
        let span = |v: &[(f64, f64)]| {
            (
                v.iter().map(|x| x.0).fold(f64::MAX, f64::min),
                v.iter().map(|x| x.1).fold(f64::MIN, f64::max),
            )
        };
        let (sl, sr) = (span(&l), span(&r));
        assert!(
            (sl.0 - sr.0).abs() < 1.5 && (sl.1 - sr.1).abs() < 1.5,
            "{sl:?} {sr:?}"
        );
        // Nothing says "centre" and "center" differently; junk is absent.
        assert_eq!(
            build_look("karaoke", r#"{"align":"centre"}"#),
            build_look("karaoke", r#"{"align":"center"}"#)
        );
    }

    #[test]
    fn rotate_tilts_the_whole_block_about_its_middle() {
        let flat = build_look("karaoke", r#"{"rotate":0,"anim":"none"}"#);
        let tilted = build_look("karaoke", r#"{"rotate":6,"anim":"none"}"#);
        let (f, t) = (layer(&flat, 0), layer(&tilted, 0));
        assert_eq!(f.len(), t.len());
        // Clockwise 6 degrees is -6 in ASS, on every word.
        assert!(
            t.iter().all(|e| tag_nums(e, "\\frz") == vec![-6.0]),
            "{}",
            t[0]
        );
        assert!(f.iter().all(|e| !e.contains("\\frz")));
        // The first line: words further right sit lower.
        let ys: Vec<(f64, f64)> = t.iter().take(2).map(|e| at(e)).collect();
        assert!(ys[1].0 > ys[0].0 && ys[1].1 > ys[0].1, "{ys:?}");
        // About the block's middle: every word keeps its distance from it.
        let pivot = (540.0, 960.0);
        let dist = |e: &str| {
            let p = at(e);
            ((p.0 - pivot.0).powi(2) + (p.1 - pivot.1).powi(2)).sqrt()
        };
        for (a, b) in f.iter().zip(&t) {
            assert!((dist(a) - dist(b)).abs() < 0.3, "{a} {b}");
        }
        // Clamped.
        assert_eq!(
            build_look("karaoke", r#"{"rotate":90}"#),
            build_look("karaoke", r#"{"rotate":15}"#)
        );
    }

    #[test]
    fn stroke_object_wins_over_the_v1_outline_and_a_word_can_have_its_own() {
        // v1 on its own: the style row says it, nothing on the events.
        let v1 = build_look(
            "karaoke",
            r##"{"outline":"#FF0000","outline_w":5,"anim":"none"}"##,
        );
        assert_eq!(style_line(&v1, "Karaoke")[16], "5");
        // The object: its colour and width, on the style row and every event.
        let both = build_look(
            "karaoke",
            r##"{"outline":"#FF0000","outline_w":5,"stroke":{"color":"#00FF00","width":9},"anim":"none"}"##,
        );
        let s = style_line(&both, "Karaoke");
        assert_eq!((s[5], s[16]), ("&H0000FF00", "9"));
        assert!(
            layer(&both, 0)
                .iter()
                .all(|e| e.contains("\\bord9") && e.contains("\\3c&H00FF00&")),
            "{}",
            layer(&both, 0)[0]
        );
        // Half given: the other half comes from v1, then the style.
        let w = build_look(
            "karaoke",
            r##"{"outline":"#FF0000","stroke":{"width":7},"anim":"none"}"##,
        );
        assert!(layer(&w, 0)[0].contains("\\bord7") && layer(&w, 0)[0].contains("\\3c&H0000FF&"));
        let c = build_look(
            "karaoke",
            r##"{"stroke":{"color":"#336699"},"anim":"none"}"##,
        );
        assert!(
            layer(&c, 0)[0].contains("\\bord3\\3c&H996633&"),
            "{}",
            layer(&c, 0)[0]
        );
        // Clamped, and scaled with the canvas.
        assert!(build_look("karaoke", r#"{"stroke":{"width":99}}"#).contains("\\bord12"));
        let sq = AssOpts {
            w: 1080,
            h: 1080,
            ..with(r#"{"stroke":{"width":10},"anim":"none"}"#)
        };
        assert!(build_for(&sample(), "karaoke", 0.0, &sq).contains("\\bord9"));
        // A word's own stroke: only the spoken word, and back afterwards.
        let a = build_look(
            "karaoke",
            r##"{"anim":"none","words":{"active":{"stroke":{"color":"#FFFFFF","width":8}}}}"##,
        );
        let l = layer(&a, 0);
        let (first, second) = (l[0], l[1]);
        assert!(first.contains("\\bord8"), "{first}");
        // The first word's text event steps back to 3 when it is over.
        assert!(
            l.iter().any(|e| body(e) == "SO" && e.contains("\\bord3")),
            "{a}"
        );
        assert!(second.contains("\\bord3\\3c&H000000&"), "{second}");
    }

    #[test]
    fn shadow_object_is_an_offset_blurred_copy_under_the_text() {
        let a = build_look(
            "karaoke",
            r##"{"anim":"none","shadow":{"color":"#102030","x":6,"y":8,"blur":8,"opacity":0.6}}"##,
        );
        let (sh, tx) = (layer(&a, 1), layer(&a, 3));
        assert!(!sh.is_empty() && !tx.is_empty());
        // The text moved up to its layer; nothing is left on layer 0.
        assert!(layer(&a, 0).is_empty());
        // One shadow per row where the words keep their shape: one event per line here.
        assert!(sh.len() < tx.len(), "{} {}", sh.len(), tx.len());
        let e = sh[0];
        assert!(e.contains("\\shad0"), "{e}");
        assert!(
            e.contains("\\1c&H302010&") && e.contains("\\3c&H302010&"),
            "{e}"
        );
        assert_eq!(tag_nums(e, "\\blur"), vec![8.0], "{e}");
        // 60% opacity is alpha 0x66.
        assert!(e.contains("\\alpha&H66&"), "{e}");
        // Offset right and down of the row it follows.
        let (xs, ys) = at(e);
        let row: Vec<(f64, f64)> = tx.iter().take(2).map(|e| at(e)).collect();
        let mid = ((row[0].0 + row[1].0) / 2.0, row[0].1);
        assert!((ys - mid.1 - 8.0).abs() < 0.2, "{ys} {mid:?}");
        assert!(xs - mid.0 > 4.0, "{xs} {mid:?}");
        // The libass shadow of the style is gone (minimal has shadow 1).
        let m = build_look("minimal", r##"{"shadow":{"blur":2}}"##);
        assert_eq!(style_line(&m, "Minimal")[17], "0");
        // v1 number: still the style's \shad, and the line writer.
        let v1 = build_look("minimal", r#"{"shadow":4}"#);
        assert_eq!(style_line(&v1, "Minimal")[17], "4");
        // Ranges.
        let big = build_look(
            "karaoke",
            r#"{"shadow":{"x":99,"y":-99,"blur":99,"opacity":9}}"#,
        );
        let e = layer(&big, 1)[0];
        assert_eq!(tag_nums(e, "\\blur"), vec![20.0], "{e}");
        assert!(e.contains("\\alpha&H00&") || !e.contains("\\alpha"), "{e}");
    }

    #[test]
    fn glow_is_a_blurred_bordered_copy_with_the_strength_as_its_opacity() {
        let a = build_look(
            "karaoke",
            r##"{"anim":"none","glow":{"color":"#FFD400","size":20,"strength":0.5}}"##,
        );
        let gl = layer(&a, 2);
        assert!(!gl.is_empty());
        let e = gl[0];
        // size 20: a 11 px border, blurred by 12.
        assert!(e.contains("\\bord14"), "{e}"); // 3 (the style's stroke) + 11
        assert_eq!(tag_nums(e, "\\blur"), vec![12.0], "{e}");
        assert!(
            e.contains("\\1c&H00D4FF&") && e.contains("\\3c&H00D4FF&"),
            "{e}"
        );
        // Strength 0.5 is alpha 0x80.
        assert!(e.contains("\\alpha&H80&"), "{e}");
        // Strength 0 or size 0: no glow events at all.
        for j in [
            r#"{"glow":{"size":20,"strength":0}}"#,
            r#"{"glow":{"size":0}}"#,
        ] {
            assert!(layer(&build_look("karaoke", j), 2).is_empty(), "{j}");
        }
        // Defaults for what an object leaves out: the text's own colour.
        let d = build_look("neon", r#"{"glow":{"size":10}}"#);
        let e = layer(&d, 2)[0];
        assert!(e.contains("\\1c&H"), "{e}");
        // Ranges.
        let big = build_look(
            "karaoke",
            r#"{"anim":"none","glow":{"size":99,"strength":9}}"#,
        );
        let e = layer(&big, 2)[0];
        assert_eq!(tag_nums(e, "\\blur"), vec![24.0], "{e}");
    }

    #[test]
    fn a_glow_on_the_spoken_word_only_lives_while_it_is_spoken() {
        let a = build_look(
            "karaoke",
            r##"{"anim":"none","words":{"active":{"glow":{"color":"#00E5FF","size":20,"strength":1}}}}"##,
        );
        let gl = layer(&a, 2);
        let tx = layer(&a, 3);
        // A glow event per word, each only for that word's time.
        assert_eq!(gl.len(), tx.len());
        for e in &gl {
            assert!(secs_of(end(e)) - secs_of(start(e)) < 0.5, "{e}");
        }
        // The first word "so" is spoken 0.00 to 0.35.
        assert_eq!(start(gl[0]), "0:00:00.00");
        assert_eq!(end(gl[0]), "0:00:00.35");
        // A keyword's glow stacks on the spoken word's and stays once spoken.
        let k = build_look(
            "karaoke",
            r##"{"anim":"none","words":{"keyword":{"glow":{"color":"#FF00FF","size":30,"strength":1}}}}"##,
        );
        let gl = layer(&k, 2);
        assert!(!gl.is_empty());
        // "3" (a digit: a keyword) glows to the end of its line, in the keyword's colour.
        let three = gl.iter().find(|e| e.contains("\\1c&HFF00FF&")).unwrap();
        assert!(tag_nums(three, "\\blur")[0] > 17.0, "{three}");
    }

    /// First move of a drawing event, as the box (w, h) in px.
    fn box_size(e: &str) -> (f64, f64) {
        let d = body(e);
        let nums: Vec<f64> = d
            .split_whitespace()
            .filter_map(|t| t.parse().ok())
            .collect();
        let xs: Vec<f64> = nums.iter().step_by(2).copied().collect();
        let ys: Vec<f64> = nums.iter().skip(1).step_by(2).copied().collect();
        let mx = xs.iter().cloned().fold(f64::MIN, f64::max);
        let my = ys.iter().cloned().fold(f64::MIN, f64::max);
        (mx / 8.0, my / 8.0)
    }

    #[test]
    fn a_box_object_is_a_drawn_shape_sized_from_the_text() {
        let a = build_look(
            "karaoke",
            r##"{"anim":"none","stroke":{"width":0},"box":{"color":"#102030","opacity":0.5,"pad_x":20,"pad_y":10,"radius":0}}"##,
        );
        let bx = layer(&a, 0);
        let tx = layer(&a, 3);
        // One box per line, drawn with \p4, under every other layer.
        assert_eq!(bx.len(), 5, "{a}");
        let e = bx[0];
        assert!(e.contains("\\p4") && e.contains("\\bord0\\shad0"), "{e}");
        assert!(
            e.contains("\\1c&H302010&") && e.contains("\\alpha&H80&"),
            "{e}"
        );
        // Square: no curve.
        assert!(!body(e).contains(" b "), "{e}");
        // Width = ink of "MOST PEOPLE" + 2 pad_x; height = ink band + 2 pad_y.
        let f = face();
        let first_words: Vec<&str> = tx
            .iter()
            .filter(|t| start(t) == start(bx[0]))
            .map(|t| body(t))
            .collect();
        let text = first_words.join(" ");
        let text = text.as_str();
        let (l, _) = f.ink_x(first_words[0], karaoke_px());
        let (_, r) = f.ink_x(first_words[first_words.len() - 1], karaoke_px());
        let want_w = f.width(text, karaoke_px()) - l - r + 40.0;
        let (top, bottom) = f.ink_y(text, karaoke_px()).unwrap();
        let want_h = top - bottom + 20.0;
        let (w, h) = box_size(e);
        assert!((w - want_w).abs() < 0.3, "{w} {want_w}");
        assert!((h - want_h).abs() < 0.3, "{h} {want_h}");
        // The shape is centred on the ink: on the line's x, offset in y by
        // the ink's position in the line box.
        assert!(tx.len() > bx.len());
        // Radius: 0.5 and 1 draw curves; 1 is a pill (the corner radius is half the height).
        let r1 = build_look("karaoke", r##"{"anim":"none","box":{"radius":1}}"##);
        let d = body(layer(&r1, 0)[0]).to_string();
        assert!(d.contains(" b "), "{d}");
        let (w1, h1) = box_size(layer(&r1, 0)[0]);
        let first: f64 = d.split_whitespace().nth(1).unwrap().parse().unwrap();
        assert!((first / 8.0 - h1 / 2.0).abs() < 0.2, "{first} {h1} {w1}");
        let r05 = build_look("karaoke", r##"{"anim":"none","box":{"radius":0.5}}"##);
        let first: f64 = body(layer(&r05, 0)[0])
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        assert!((first / 8.0 - h1 / 4.0).abs() < 0.3, "{first} {h1}");
        // Per word: one box per word, each as wide as its word plus padding.
        let pw = build_look(
            "karaoke",
            r##"{"anim":"none","box":{"per":"word","pad_x":10,"pad_y":6}}"##,
        );
        let boxes = layer(&pw, 0);
        assert_eq!(boxes.len(), layer(&pw, 3).len());
        let (w, _) = box_size(boxes[0]);
        let w0 = body(layer(&pw, 3)[0]);
        let (l, r) = f.ink_x(w0, karaoke_px());
        let want = f.width(w0, karaoke_px()) - l - r + 20.0 + 2.0 * 3.0;
        assert!((w - want).abs() < 0.3, "{w} {want}");
        // Ranges and bad values.
        let big = build_look(
            "karaoke",
            r##"{"anim":"none","box":{"pad_x":900,"opacity":9,"radius":9}}"##,
        );
        let (w, _) = box_size(layer(&big, 0)[0]);
        assert!(w > 120.0 && w < 1000.0);
        assert!(
            build_look("karaoke", r#"{"box":{"per":"paragraph","radius":0.2}}"#).contains("\\p4")
        );
        // A box object that says nothing usable is no box object.
        assert!(!build_look("karaoke", r#"{"box":{"per":"paragraph"}}"#).contains("\\p4"));
    }

    #[test]
    fn a_box_object_replaces_the_styles_own_box_and_v1_forms_still_work() {
        // The style's libass box steps aside.
        let a = build_look("hormozi", r##"{"box":{"radius":0.3}}"##);
        assert_eq!(style_line(&a, "Hormozi")[15], "1");
        assert!(!a.contains("HormoziBox"));
        // The shape takes the style's box colour when none is given.
        assert!(
            layer(&a, 0)[0].contains("\\1c&H000000&"),
            "{}",
            layer(&a, 0)[0]
        );
        let h = build_look("highlight", r##"{"box":{"radius":0.3}}"##);
        assert!(
            layer(&h, 0)[0].contains("\\1c&H35E6A3&"),
            "{}",
            layer(&h, 0)[0]
        );
        // No stroke is invented on a style whose outline was its box padding.
        assert_eq!(style_line(&a, "Hormozi")[16], "0");
        // The object wins over a v1 colour.
        let both = build_look(
            "karaoke",
            r##"{"box":{"color":"#00FF00"},"box_opacity":0.5}"##,
        );
        assert!(layer(&both, 0)[0].contains("\\1c&H00FF00&"));
        assert!(
            layer(&both, 0)[0].contains("\\alpha&H80&"),
            "{}",
            layer(&both, 0)[0]
        );
        // v1: a colour string is still a libass box, "none" still removes it.
        let v1 = build_look("karaoke", r##"{"box":"#101010","anim":"none"}"##);
        assert_eq!(style_line(&v1, "Karaoke")[15], "3");
        assert!(!v1.contains("\\p4"));
        let off = build_look("hormozi", r#"{"box":"none"}"#);
        assert_eq!(style_line(&off, "Hormozi")[15], "1");
    }

    #[test]
    fn the_spoken_words_own_box_follows_it_only_while_it_is_spoken() {
        let a = build_look(
            "karaoke",
            r##"{"anim":"none","words":{"active":{"box":{"color":"#FFD400","radius":1}}}}"##,
        );
        let bx = layer(&a, 0);
        // One shape per word, each only while its word is spoken.
        assert_eq!(bx.len(), layer(&a, 3).len());
        assert!(
            bx[0].contains("\\1c&H00D4FF&") && bx[0].contains("\\p4"),
            "{}",
            bx[0]
        );
        assert_eq!(start(bx[0]), "0:00:00.00");
        assert_eq!(end(bx[0]), "0:00:00.35");
        // Its opacity.
        let h = build_look(
            "karaoke",
            r##"{"anim":"none","words":{"active":{"box":{"opacity":0.25}}}}"##,
        );
        assert!(
            layer(&h, 0)[0].contains("\\alpha&HBF&"),
            "{}",
            layer(&h, 0)[0]
        );
        // It follows the word's scale and lift: its tags are the word's.
        let s = build_look(
            "karaoke",
            r##"{"anim":"none","words":{"active":{"scale":1.2,"lift":0.1,"box":{"color":"#FFD400"}}}}"##,
        );
        let (b, t) = (layer(&s, 0), layer(&s, 3));
        let so = t.iter().find(|e| body(e) == "SO").unwrap();
        assert!(
            b[0].contains("\\fscx120") && so.contains("\\fscx120"),
            "{} {so}",
            b[0]
        );
        // Lifted by a tenth of the type size, and the box with it.
        assert!(at(so).1 < 960.0 - 7.0, "{so}");
        assert!((at(b[0]).1 - at(so).1).abs() < 8.0, "{} {so}", b[0]);
    }

    #[test]
    fn the_layers_are_stacked_box_shadow_glow_text() {
        let a = build_look(
            "karaoke",
            r##"{"anim":"none","box":{"color":"#101010"},"shadow":{"x":4},"glow":{"size":12},"stroke":{"width":2}}"##,
        );
        // Within the first caption line the layers come in rising order.
        let first: Vec<u8> = a
            .lines()
            .filter(|l| l.starts_with("Dialogue: ") && start(l) == "0:00:00.00")
            .map(|l| l[10..11].parse::<u8>().unwrap())
            .collect();
        let mut sorted = first.clone();
        sorted.sort();
        assert_eq!(first, sorted, "{first:?}");
        for n in 0..=3u8 {
            assert!(first.contains(&n), "layer {n} missing: {first:?}");
        }
        // Without any lower layer the text stays on layer 0.
        let plain = build_look("karaoke", r#"{"spacing":0.05,"anim":"none"}"#);
        assert!(!layer(&plain, 0).is_empty() && layer(&plain, 3).is_empty());
    }

    #[test]
    fn the_copies_share_the_words_timing_through_every_motion() {
        let look = r##"{"words":{"mode":"build","active":{"scale":1.15,"lift":0.1,"glow":{"color":"#00E5FF","size":16},
                "box":{"color":"#FFD400","radius":1}},"release_ms":300,"attack_ms":120,"spoken":{"opacity":0.6}},
            "shadow":{"x":3,"y":4,"blur":4},"box":{"per":"word"},
            "enter":{"kind":"slide_up","ms":260},"exit":{"kind":"fade","ms":140}}"##;
        let a = build_look("karaoke", look);
        let (bx, sh, gl, tx) = (layer(&a, 0), layer(&a, 1), layer(&a, 2), layer(&a, 3));
        assert!(!sh.is_empty() && !gl.is_empty() && !bx.is_empty());
        // Per-word shadow events (the words move), cut exactly like the text.
        let tt: std::collections::BTreeSet<_> = times(&tx).into_iter().collect();
        for (name, evs) in [("shadow", &sh), ("glow", &gl)] {
            for t in times(evs) {
                assert!(tt.contains(&t), "{name} {t:?} is not a text slice");
            }
        }
        assert_eq!(times(&sh), times(&tx), "shadow slices equal text slices");
        // They move with the word: the lifted text and its shadow, glow and box
        // are at the same height (the shadow lower by its offset).
        let t0 = tx.iter().find(|e| body(e) == "I").unwrap();
        let s0 = sh.iter().find(|e| body(e) == "I").unwrap();
        let g0 = gl.iter().find(|e| body(e) == "I").unwrap();
        assert!((at(s0).1 - at(t0).1 - 4.0).abs() < 0.3, "{s0} {t0}");
        assert!((at(g0).1 - at(t0).1).abs() < 0.3, "{g0} {t0}");
        assert!((at(s0).0 - at(t0).0 - 3.0).abs() < 0.3);
        // Their scale follows the word's.
        assert_eq!(tag_nums(s0, "\\fscx"), tag_nums(t0, "\\fscx"));
        assert_eq!(tag_nums(g0, "\\fscx"), tag_nums(t0, "\\fscx"));
        // Every override block closes; no NaN.
        for l in a.lines().filter(|l| l.starts_with("Dialogue: ")) {
            assert_eq!(l.matches('{').count(), l.matches('}').count(), "{l}");
            assert!(!l.contains("NaN") && !l.contains("inf"), "{l}");
        }
    }

    #[test]
    fn a_row_of_steady_words_shares_one_shadow_event() {
        let a = build_look("karaoke", r##"{"anim":"none","shadow":{"x":3}}"##);
        let (sh, tx) = (layer(&a, 1), layer(&a, 3));
        // One per line (5 lines), against one per word.
        assert_eq!(sh.len(), 5, "{a}");
        assert!(tx.len() > 10);
        // Words that scale need one each.
        let b = build_look(
            "karaoke",
            r##"{"anim":"none","shadow":{"x":3},"words":{"active":{"scale":1.2}}}"##,
        );
        assert_eq!(layer(&b, 1).len(), layer(&b, 3).len());
    }

    #[test]
    fn dressing_works_on_every_style_and_canvas() {
        let look = r##"{"spacing":0.04,"line_gap":1.2,"align":"left","rotate":-3,
            "stroke":{"color":"#202020","width":4},"shadow":{"x":4,"y":5,"blur":5},
            "glow":{"size":14,"strength":0.7},"box":{"radius":0.6,"per":"line"},
            "words":{"active":{"scale":1.1,"stroke":{"width":6},"glow":{"size":20},"box":{"radius":1}},
                     "keyword":{"glow":{"color":"#FF00FF"}}}}"##;
        for style in [
            "karaoke",
            "hormozi",
            "minimal",
            "beast",
            "neon",
            "highlight",
            "ghost",
            "tiktok",
        ] {
            for (w, h) in [(1080, 1920), (1080, 1350), (1080, 1080), (1920, 1080)] {
                let o = AssOpts { w, h, ..with(look) };
                let a = build_for(&sample(), style, 0.0, &o);
                for n in 0..=3u8 {
                    assert!(!layer(&a, n).is_empty(), "{style} {w}x{h} layer {n}");
                }
                for l in a.lines().filter(|l| l.starts_with("Dialogue: ")) {
                    assert_eq!(l.matches('{').count(), l.matches('}').count(), "{l}");
                    assert!(secs_of(end(l)) > secs_of(start(l)), "{l}");
                    assert!(!l.contains("NaN") && !l.contains("inf"), "{l}");
                }
            }
        }
    }

    #[test]
    fn event_count_of_twelve_words_with_everything_on() {
        let ws: Vec<Word> = (0..12)
            .map(|i| Word {
                w: [
                    "Most", "people", "never", "learn", "how", "to", "speak", "on", "camera.",
                    "Try", "it", "today.",
                ][i]
                    .into(),
                s: 0.3 + i as f64 * 0.4,
                e: 0.3 + i as f64 * 0.4 + 0.35,
                conf: None,
            })
            .collect();
        let look = r##"{"spacing":0.05,"stroke":{"width":4},"shadow":{"x":4,"y":6,"blur":6},
            "glow":{"size":16},"box":{"radius":1,"per":"word"},
            "words":{"active":{"scale":1.15,"lift":0.08,"glow":{"size":24},"box":{"radius":1}},
                     "spoken":{"opacity":0.6},"release_ms":400},
            "enter":{"kind":"slide_up"},"exit":{"kind":"fade"}}"##;
        let a = build_for(&ws, "karaoke", 0.0, &with(look));
        let n: Vec<usize> = (0..=3u8).map(|l| layer(&a, l).len()).collect();
        eprintln!("events per layer (box, shadow, glow, text) for 12 words, everything on: {n:?}, total {}", n.iter().sum::<usize>());
        assert!(n.iter().sum::<usize>() < 12 * 70, "{n:?}");
        // The same look with the words standing still needs far fewer.
        let calm = r##"{"stroke":{"width":4},"shadow":{"x":4,"y":6,"blur":6},"glow":{"size":16,"color":"#FFD400"},
            "box":{"radius":1},"anim":"none"}"##;
        let c = build_for(&ws, "karaoke", 0.0, &with(calm));
        let m: Vec<usize> = (0..=3u8).map(|l| layer(&c, l).len()).collect();
        eprintln!("steady: {m:?}");
        assert!(m[0] <= 6 && m[1] <= 6 && m[2] <= 6, "{m:?}");
    }
}
