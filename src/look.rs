//! The Look: one optional JSON object that says how a clip is dressed.
//!
//! It rides along with the job options (`--look <json>` or `--look
//! @file.json`). Every section and every field is optional and absent
//! means "what the engine does today". Parsing is lenient on purpose and
//! never fails a job: unknown fields are ignored, numbers are clamped into
//! their range, a bad enum value or a malformed colour counts as absent,
//! and JSON that does not parse at all is no look (one warning line).
//!
//! v1 contract (every section is applied):
//!
//! ```text
//! look: { v: 1,
//!   captions: { show, x, y, size, font, case, color, active, accent,
//!               outline, outline_w, shadow, box, box_opacity, max_words, anim },
//!   headline: { x, y, size, ink, card, accent, anim, seconds },
//!   bar:      { pos, height },
//!   logo:     { x, y, size, opacity },
//!   camera:   { feel, zoom, punch },
//!   effects:  { vignette, grade, fill_dim },
//!   layout:   { split } }
//! ```
//!
//! `captions` also takes the word-level section (all optional, absent = what
//! the engine renders today):
//!
//! ```text
//! captions: { ...,
//!   words: { mode: all|build|single, fill: snap|sweep,
//!            upcoming: { color, opacity, scale, blur },
//!            active:   { color, opacity, scale, lift, rotate },
//!            spoken:   { color, opacity, scale, blur },
//!            keyword:  { color, scale },
//!            attack_ms, attack_ease, hold_ms, release_ms, release_ease },
//!   enter: { kind, ms, ease },
//!   exit:  { kind, ms } }
//! ```
//!
//! and the text-dressing fields (`look.captions.fx` and `look.captions.type`):
//!
//! ```text
//! captions: { ...,
//!   spacing, line_gap, lines, max_chars, align, rotate,
//!   stroke: { color, width },
//!   shadow: <number> | { color, x, y, blur, opacity },
//!   glow:   { color, size, strength },
//!   box:    "#RRGGBB" | "none" | { color, opacity, pad_x, pad_y, radius, per },
//!   words: { active:  { stroke: { color, width }, glow: { color, size, strength },
//!                       box: { color, opacity, radius } },
//!            keyword: { glow: { color, size, strength } } } }
//! ```
//!
//! and the headline, progress bar and logo take the same depth:
//!
//! ```text
//! headline: { ...,
//!   font, case: upper|asis, spacing, align, max_lines, width,
//!   stroke: { color, width },
//!   shadow: { color, x, y, blur, opacity },
//!   glow:   { color, size, strength },
//!   card:   "#RRGGBB" | "none" | { color, opacity, pad, radius },
//!   accent_word: auto|none|first|last,
//!   enter: { kind, ms, ease }, exit: { kind, ms }, delay_s }
//! bar:  { ..., color, track, track_opacity, inset, radius,
//!         glow: { color, size, strength } }
//! logo: { ..., rotate, shadow: { color, x, y, blur, opacity },
//!         glow: { color, size, strength } }
//! ```
//!
//! `x`/`y` are the centre of an element as a fraction of the output frame
//! (0..1, right and down). Sizes multiply today's size. Colours are
//! `#RRGGBB` strings.

use std::path::Path;

use serde_json::Value;

use crate::captions::ass::Anim;

/// What this engine can do with a look (and the commands that show one),
/// announced to the app in the `hello` snapshot (`caps`). Later chunks
/// append to it.
pub const CAPS: &[&str] = &[
    "look",
    "look.captions",
    "look.captions.words",
    "look.captions.motion",
    "look.captions.fx",
    "look.captions.type",
    "look.headline",
    "look.headline.v2",
    "look.bar",
    "look.bar.v2",
    "look.logo",
    "look.logo.v2",
    "look.camera",
    "look.effects",
    "look.layout",
    "preview_frame",
];

/// The fonts libass can reach (the provisioned ones).
pub const FONTS: [&str; 4] = ["Anton", "Archivo Black", "Inter Medium", "JetBrains Mono"];

/// An sRGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// `#RRGGBB` (the `#` is optional); anything else is `None`.
    pub fn parse(s: &str) -> Option<Rgb> {
        let h = s.trim().trim_start_matches('#');
        if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
        Some(Rgb(p(0)?, p(2)?, p(4)?))
    }

    /// ASS colour `&HAABBGGRR` (alpha 00 = opaque, FF = clear).
    pub fn ass(self, alpha: u8) -> String {
        format!("&H{alpha:02X}{:02X}{:02X}{:02X}", self.2, self.1, self.0)
    }
}

/// Caption letter case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    /// UPPERCASE.
    Upper,
    /// The words as transcribed.
    AsIs,
}

/// What sits behind a caption line, or a headline's card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxLook {
    /// No box (removes the one a style has).
    None,
    /// An opaque box in this colour.
    Color(Rgb),
}

/// How a value travels between two looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ease {
    Linear,
    /// Fast start, slow arrival.
    Out,
    /// Slow start, fast arrival.
    In,
    /// Overshoots a little, then settles.
    Back,
}

impl Ease {
    pub fn parse(s: &str) -> Option<Ease> {
        match s.trim().to_ascii_lowercase().as_str() {
            "linear" => Some(Ease::Linear),
            "out" => Some(Ease::Out),
            "in" => Some(Ease::In),
            "back" => Some(Ease::Back),
            _ => None,
        }
    }
}

/// Which words of a caption line are on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordMode {
    /// The whole line from its start.
    All,
    /// Words appear as they are spoken.
    Build,
    /// One word at a time.
    Single,
}

/// How a word takes its active colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    /// At once, at the word's start.
    Snap,
    /// A left-to-right sweep over the word's own duration.
    Sweep,
}

/// How the lines of a caption block sit inside the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Where a box is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxPer {
    /// One box per caption line.
    Line,
    /// One box per word, following the word.
    Word,
}

/// A stroke around the letters. Unset fields keep the style's (or the v1
/// `outline` / `outline_w`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StrokeLook {
    pub color: Option<Rgb>,
    /// 0..12 px at a 1080-wide frame.
    pub width: Option<f64>,
}

/// A soft drop shadow.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShadowLook {
    pub color: Option<Rgb>,
    /// Offset, -30..30 px at a 1080-wide frame (right / down positive).
    pub x: Option<f64>,
    pub y: Option<f64>,
    /// 0..20 px.
    pub blur: Option<f64>,
    /// 0..1.
    pub opacity: Option<f64>,
}

/// A soft light around the letters.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GlowLook {
    pub color: Option<Rgb>,
    /// How far the light reaches, 0..40 px at a 1080-wide frame.
    pub size: Option<f64>,
    /// 0..1.
    pub strength: Option<f64>,
}

/// A box drawn as a shape behind the line (or each word).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoxFxLook {
    pub color: Option<Rgb>,
    /// 0..1.
    pub opacity: Option<f64>,
    /// Room between the letters and the box, 0..60 px at a 1080-wide frame.
    pub pad_x: Option<f64>,
    pub pad_y: Option<f64>,
    /// 0 = square corners, 1 = round ends.
    pub radius: Option<f64>,
    pub per: Option<BoxPer>,
}

/// The box behind just the spoken word.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActiveBoxLook {
    pub color: Option<Rgb>,
    /// 0..1.
    pub opacity: Option<f64>,
    /// 0..1.
    pub radius: Option<f64>,
}

/// One look of a word (before, during or after it is spoken). Each state
/// only reads the fields it has in the contract; the others stay `None`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WordState {
    pub color: Option<Rgb>,
    /// 0..1.
    pub opacity: Option<f64>,
    /// 0.5..1.5.
    pub scale: Option<f64>,
    /// 0..10 (px at a 1080-wide frame).
    pub blur: Option<f64>,
    /// -0.3..0.3 em, positive = up (active only).
    pub lift: Option<f64>,
    /// -10..10 degrees, positive = clockwise (active only).
    pub rotate: Option<f64>,
    /// The spoken word's own stroke (active only).
    pub stroke: Option<StrokeLook>,
    /// The spoken word's own glow (active only).
    pub glow: Option<GlowLook>,
    /// A box behind the spoken word (active only).
    pub box_: Option<ActiveBoxLook>,
}

/// The emphasis words (the ones the engine already accents).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeywordLook {
    pub color: Option<Rgb>,
    /// 0.5..1.5.
    pub scale: Option<f64>,
    /// The emphasis words' own glow.
    pub glow: Option<GlowLook>,
}

/// Word-level section of the captions: states, transitions, reveal mode.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WordsLook {
    pub mode: Option<WordMode>,
    pub upcoming: WordState,
    pub active: WordState,
    pub spoken: WordState,
    pub keyword: KeywordLook,
    pub fill: Option<Fill>,
    /// 0..400.
    pub attack_ms: Option<f64>,
    pub attack_ease: Option<Ease>,
    /// 0..600.
    pub hold_ms: Option<f64>,
    /// 0..2000.
    pub release_ms: Option<f64>,
    pub release_ease: Option<Ease>,
}

/// How a caption line comes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnterKind {
    None,
    Pop,
    Fade,
    SlideUp,
    SlideDown,
    SlideLeft,
    SlideRight,
    Zoom,
    Bounce,
    Blur,
    Drop,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnterLook {
    pub kind: Option<EnterKind>,
    /// 0..800.
    pub ms: Option<f64>,
    pub ease: Option<Ease>,
}

/// How a caption line leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    None,
    Fade,
    SlideUp,
    SlideDown,
    Zoom,
    Blur,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExitLook {
    pub kind: Option<ExitKind>,
    /// 0..600.
    pub ms: Option<f64>,
}

/// Caption section. `None` fields keep the style's own value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CaptionsLook {
    /// `false` = no caption lines at all (the headline is unaffected).
    pub show: Option<bool>,
    /// Centre of the caption block (fraction of the frame). When either is
    /// set the block is anchored there, middle-centre; the other is 0.5.
    pub x: Option<f64>,
    pub y: Option<f64>,
    /// Type size multiplier, 0.5..2.
    pub size: Option<f64>,
    /// One of [`FONTS`].
    pub font: Option<&'static str>,
    pub case: Option<Case>,
    /// Text colour (not-yet-spoken; also the spoken colour unless `active`
    /// is set).
    pub color: Option<Rgb>,
    /// Colour of the word being spoken / already spoken.
    pub active: Option<Rgb>,
    /// Keyword colour.
    pub accent: Option<Rgb>,
    /// Text outline colour (ignored while a box is drawn: the box takes
    /// the outline slot).
    pub outline: Option<Rgb>,
    /// Outline width in px at a 1080-wide frame, 0..8 (box padding when a
    /// box is drawn).
    pub outline_w: Option<f64>,
    /// Shadow depth in px at a 1080-wide frame, 0..6.
    pub shadow: Option<f64>,
    /// Box behind the line: a colour, or `None` to remove the style's box.
    pub box_: Option<BoxLook>,
    /// Box opacity 0..1 (1 when a box colour is set without it).
    pub box_opacity: Option<f64>,
    /// Words per line, 1..8.
    pub max_words: Option<usize>,
    pub anim: Option<Anim>,
    /// Word looks, transitions and reveal mode (`None` = today's captions).
    pub words: Option<WordsLook>,
    /// How a line comes in / leaves (`None` = what `anim` says).
    pub enter: Option<EnterLook>,
    pub exit: Option<ExitLook>,
    /// Letter spacing, -0.05..0.3 em.
    pub spacing: Option<f64>,
    /// Row pitch of a wrapped block as a multiple of the line height, 0.8..1.6.
    pub line_gap: Option<f64>,
    /// Most lines in one caption block, 1 or 2.
    pub lines: Option<usize>,
    /// Most characters in one caption block, 6..40.
    pub max_chars: Option<usize>,
    /// How the lines sit inside the block.
    pub align: Option<Align>,
    /// Tilt of the whole block, -15..15 degrees, positive = clockwise.
    pub rotate: Option<f64>,
    /// Stroke around the letters (wins over `outline` / `outline_w`).
    pub stroke: Option<StrokeLook>,
    /// Shadow as an object (a plain number is `shadow`).
    pub shadow_fx: Option<ShadowLook>,
    pub glow: Option<GlowLook>,
    /// Box as an object (a colour or `none` is `box_`).
    pub box_fx: Option<BoxFxLook>,
}

impl CaptionsLook {
    /// Does this caption need the positioned writer (every word its own
    /// event)? It does when it has word looks, an entrance or exit, or any
    /// of the text-dressing fields. A caption without any of them goes through
    /// the one-event-per-line writer, byte for byte as it always did.
    pub fn positioned(&self) -> bool {
        self.words.is_some()
            || self.enter.is_some()
            || self.exit.is_some()
            || self.spacing.is_some()
            || self.line_gap.is_some()
            || self.lines.is_some()
            || self.max_chars.is_some()
            || self.align.is_some()
            || self.rotate.is_some()
            || self.stroke.is_some()
            || self.shadow_fx.is_some()
            || self.glow.is_some()
            || self.box_fx.is_some()
    }
}

/// Headline motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlineAnim {
    Pop,
    Fade,
    None,
}

/// Which word of the headline takes the accent colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccentWord {
    /// The engine's own choice (a keyword, else the longest content word).
    Auto,
    /// No word: the whole headline is in the ink colour.
    None,
    First,
    Last,
}

/// The headline's card as an object.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CardLook {
    /// White unless set.
    pub color: Option<Rgb>,
    /// 0..1 (1 unless set).
    pub opacity: Option<f64>,
    /// Room between the letters and the card, 0..80 px at a 1080-wide frame
    /// (today's 24 px times `size` unless set).
    pub pad: Option<f64>,
    /// 0 = square corners, 1 = round ends: a fraction of half the card's
    /// shorter side.
    pub radius: Option<f64>,
}

/// Headline section. Only matters while a headline is on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeadlineLook {
    /// Centre of the card (fraction of the frame). When either is set the
    /// card is anchored there; the other is 0.5 (x) or where the card sits
    /// today (y).
    pub x: Option<f64>,
    pub y: Option<f64>,
    /// Type and card padding multiplier, 0.5..2.
    pub size: Option<f64>,
    pub ink: Option<Rgb>,
    /// Card colour, or `None` for no card (the text gets an outline).
    pub card: Option<BoxLook>,
    pub accent: Option<Rgb>,
    pub anim: Option<HeadlineAnim>,
    /// Seconds on screen, >= 0 (0 = the whole clip).
    pub seconds: Option<f64>,
    /// One of [`FONTS`] (Archivo Black unless set).
    pub font: Option<&'static str>,
    pub case: Option<Case>,
    /// Letter spacing, -0.05..0.3 em.
    pub spacing: Option<f64>,
    /// How the rows sit inside the card.
    pub align: Option<Align>,
    /// Most rows of text, 1..3.
    pub max_lines: Option<usize>,
    /// Widest the text block may be, as a fraction of the frame's width,
    /// 0.4..1.
    pub width: Option<f64>,
    pub stroke: Option<StrokeLook>,
    pub shadow: Option<ShadowLook>,
    pub glow: Option<GlowLook>,
    /// The card as an object (a colour or `none` is `card`).
    pub card_fx: Option<CardLook>,
    pub accent_word: Option<AccentWord>,
    pub enter: Option<EnterLook>,
    pub exit: Option<ExitLook>,
    /// Seconds after the clip starts before the headline enters, 0..5.
    pub delay_s: Option<f64>,
}

impl HeadlineLook {
    /// Does this headline need the positioned writer (every word its own
    /// event, laid out from the font's metrics, with a vector card)? It does as
    /// soon as it has any field beyond the v1 ones (`x`, `y`, `size`, `ink`,
    /// `card` as a colour or `none`, `accent`, `anim`, `seconds`). A headline
    /// without any of them goes through the one-event writer, byte for byte as
    /// it always did.
    pub fn positioned(&self) -> bool {
        self.font.is_some()
            || self.case.is_some()
            || self.spacing.is_some()
            || self.align.is_some()
            || self.max_lines.is_some()
            || self.width.is_some()
            || self.stroke.is_some()
            || self.shadow.is_some()
            || self.glow.is_some()
            || self.card_fx.is_some()
            || self.accent_word.is_some()
            || self.enter.is_some()
            || self.exit.is_some()
            || self.delay_s.is_some()
    }
}

/// Where the progress bar runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarPos {
    Top,
    Bottom,
}

/// Progress bar section. Only matters while the progress bar is on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BarLook {
    pub pos: Option<BarPos>,
    /// Thickness multiplier, 0.5..3.
    pub height: Option<f64>,
    /// Colour of the filled part; wins over the flat `progress_bar` colour
    /// (which still switches the bar on).
    pub color: Option<Rgb>,
    /// Colour of the unfilled part (a dark translucent track unless set).
    pub track: Option<Rgb>,
    /// Opacity of the track, 0..1.
    pub track_opacity: Option<f64>,
    /// Margin from the frame's left and right edges and from the edge the bar
    /// sits on, as a fraction of the frame's width, 0..0.1.
    pub inset: Option<f64>,
    /// 0 = square ends, 1 = round ends: a fraction of half the bar's thickness.
    pub radius: Option<f64>,
    /// A soft halo of the fill colour around the filled part.
    pub glow: Option<GlowLook>,
}

impl BarLook {
    /// Does this bar need the shaped drawing (`bar::BarFx`)? It does when it has
    /// a track colour or opacity, an inset, a radius or a glow. Position,
    /// height and colour alone keep the plain drawing, pixel for pixel.
    pub fn shaped(&self) -> bool {
        self.track.is_some()
            || self.track_opacity.is_some()
            || self.inset.is_some()
            || self.radius.is_some()
            || self.glow.is_some()
    }
}

/// Logo section. Only matters while a logo file is set.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogoLook {
    /// Centre of the logo box (fraction of the frame). When either is set
    /// it wins over the corner; the other is 0.5.
    pub x: Option<f64>,
    pub y: Option<f64>,
    /// 0.4..2.5.
    pub size: Option<f64>,
    /// 0..1.
    pub opacity: Option<f64>,
    /// Turn about the logo's centre, -30..30 degrees, positive = clockwise.
    pub rotate: Option<f64>,
    /// A blurred copy of the logo's shape in one colour, under it.
    pub shadow: Option<ShadowLook>,
    /// A soft halo of the logo's shape, under it.
    pub glow: Option<GlowLook>,
}

/// How the virtual camera moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraFeel {
    /// One framing per shot, held; only cuts between shots reframe.
    Locked,
    /// Wider dead bands, slower response: moves less, more gently.
    Steady,
    /// Today's camera.
    Smooth,
    /// Narrower dead bands, quicker response.
    Lively,
}

/// Camera section.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CameraLook {
    pub feel: Option<CameraFeel>,
    /// How tight the face framing is, 0.8..1.4 (1 = today).
    pub zoom: Option<f64>,
    /// Peak scale of the emphasis punch-ins, 1.0..1.4 (absent = today's;
    /// only matters while punch-ins are on).
    pub punch: Option<f64>,
}

/// Colour grade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grade {
    None,
    Warm,
    Cool,
    Mono,
    Punchy,
}

/// Effects section. Vignette and grade touch the picture only (never the
/// captions, headline, logo or progress bar).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EffectsLook {
    /// Corner darkening, 0..1 (0 = none).
    pub vignette: Option<f64>,
    pub grade: Option<Grade>,
    /// Darkening of the blurred fill behind letterboxed video, 0..1.
    pub fill_dim: Option<f64>,
}

/// Layout section.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayoutLook {
    /// Where the split-screen seam sits (fraction of the height), 0.3..0.7.
    pub split: Option<f64>,
}

/// A whole look. A section is `Some` when the key was an object.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Look {
    /// Contract version the sender wrote (informational).
    pub v: Option<u32>,
    pub captions: Option<CaptionsLook>,
    pub headline: Option<HeadlineLook>,
    pub bar: Option<BarLook>,
    pub logo: Option<LogoLook>,
    pub camera: Option<CameraLook>,
    pub effects: Option<EffectsLook>,
    pub layout: Option<LayoutLook>,
}

impl Look {
    /// No effective field anywhere: the job renders as it always did.
    pub fn is_empty(&self) -> bool {
        fn blank<T: Default + PartialEq>(s: &Option<T>) -> bool {
            s.as_ref().is_none_or(|s| *s == T::default())
        }
        blank(&self.captions)
            && blank(&self.headline)
            && blank(&self.bar)
            && blank(&self.logo)
            && blank(&self.camera)
            && blank(&self.effects)
            && blank(&self.layout)
    }

    /// Read a look out of a JSON value, leniently (never fails).
    pub fn from_value(v: &Value) -> Look {
        let Some(o) = v.as_object() else {
            tracing::warn!("look: not a JSON object, ignored");
            return Look::default();
        };
        let sect = |k: &str| o.get(k).and_then(Value::as_object);
        Look {
            v: o.get("v")
                .and_then(Value::as_f64)
                .map(|n| n.clamp(0.0, 1000.0) as u32),
            captions: sect("captions").map(captions),
            headline: sect("headline").map(headline),
            bar: sect("bar").map(bar),
            logo: sect("logo").map(logo),
            camera: sect("camera").map(camera),
            effects: sect("effects").map(effects),
            layout: sect("layout").map(layout),
        }
    }

    /// Parse JSON text. Malformed JSON is no look, with one warning line.
    pub fn parse(text: &str) -> Look {
        match serde_json::from_str::<Value>(text) {
            Ok(v) => Look::from_value(&v),
            Err(e) => {
                tracing::warn!("look: malformed JSON, ignored ({e})");
                Look::default()
            }
        }
    }

    /// A `--look` argument: JSON text, or `@path` to a JSON file. An
    /// unreadable file is no look, with one warning line.
    pub fn from_arg(arg: &str) -> Look {
        let a = arg.trim();
        match a.strip_prefix('@') {
            Some(p) => match std::fs::read_to_string(Path::new(p.trim())) {
                // Editors love a BOM on Windows.
                Ok(t) => Look::parse(t.trim_start_matches('\u{feff}')),
                Err(e) => {
                    tracing::warn!("look: cannot read {}, ignored ({e})", p.trim());
                    Look::default()
                }
            },
            None => Look::parse(a),
        }
    }
}

type Obj = serde_json::Map<String, Value>;

/// A number clamped into `lo..=hi`.
fn num(o: &Obj, k: &str, lo: f64, hi: f64) -> Option<f64> {
    o.get(k)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .map(|n| n.clamp(lo, hi))
}

fn color(o: &Obj, k: &str) -> Option<Rgb> {
    o.get(k).and_then(Value::as_str).and_then(Rgb::parse)
}

fn word(o: &Obj, k: &str) -> Option<String> {
    o.get(k)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
}

fn captions(o: &Obj) -> CaptionsLook {
    CaptionsLook {
        show: o.get("show").and_then(Value::as_bool),
        x: num(o, "x", 0.0, 1.0),
        y: num(o, "y", 0.0, 1.0),
        size: num(o, "size", 0.5, 2.0),
        font: o.get("font").and_then(Value::as_str).and_then(|f| {
            FONTS
                .iter()
                .find(|n| n.eq_ignore_ascii_case(f.trim()))
                .copied()
        }),
        case: match word(o, "case").as_deref() {
            Some("upper") => Some(Case::Upper),
            Some("asis") => Some(Case::AsIs),
            _ => None,
        },
        color: color(o, "color"),
        active: color(o, "active"),
        accent: color(o, "accent"),
        outline: color(o, "outline"),
        outline_w: num(o, "outline_w", 0.0, 8.0),
        shadow: num(o, "shadow", 0.0, 6.0),
        box_: match o.get("box") {
            Some(Value::Null) => Some(BoxLook::None),
            Some(Value::String(s)) if s.trim().eq_ignore_ascii_case("none") => Some(BoxLook::None),
            Some(Value::String(s)) => Rgb::parse(s).map(BoxLook::Color),
            _ => None,
        },
        shadow_fx: sect(o, "shadow")
            .map(shadow_fx)
            .filter(|s| *s != ShadowLook::default()),
        box_fx: sect(o, "box")
            .map(box_fx)
            .filter(|b| *b != BoxFxLook::default()),
        spacing: num(o, "spacing", -0.05, 0.3),
        line_gap: num(o, "line_gap", 0.8, 1.6),
        lines: num(o, "lines", 1.0, 2.0).map(|n| n.round() as usize),
        max_chars: num(o, "max_chars", 6.0, 40.0).map(|n| n.round() as usize),
        align: match word(o, "align").as_deref() {
            Some("left") => Some(Align::Left),
            Some("center" | "centre") => Some(Align::Center),
            Some("right") => Some(Align::Right),
            _ => None,
        },
        rotate: num(o, "rotate", -15.0, 15.0),
        stroke: sect(o, "stroke")
            .map(stroke)
            .filter(|s| *s != StrokeLook::default()),
        glow: sect(o, "glow")
            .map(glow)
            .filter(|g| *g != GlowLook::default()),
        box_opacity: num(o, "box_opacity", 0.0, 1.0),
        max_words: num(o, "max_words", 1.0, 8.0).map(|n| n.round() as usize),
        anim: word(o, "anim").and_then(|a| Anim::from_name(&a)),
        words: sect(o, "words")
            .map(words)
            .filter(|w| *w != WordsLook::default()),
        enter: sect(o, "enter")
            .map(enter)
            .filter(|e| *e != EnterLook::default()),
        exit: sect(o, "exit")
            .map(exit)
            .filter(|e| *e != ExitLook::default()),
    }
}

fn sect<'a>(o: &'a Obj, k: &str) -> Option<&'a Obj> {
    o.get(k).and_then(Value::as_object)
}

fn stroke(o: &Obj) -> StrokeLook {
    StrokeLook {
        color: color(o, "color"),
        width: num(o, "width", 0.0, 12.0),
    }
}

fn shadow_fx(o: &Obj) -> ShadowLook {
    ShadowLook {
        color: color(o, "color"),
        x: num(o, "x", -30.0, 30.0),
        y: num(o, "y", -30.0, 30.0),
        blur: num(o, "blur", 0.0, 20.0),
        opacity: num(o, "opacity", 0.0, 1.0),
    }
}

fn glow(o: &Obj) -> GlowLook {
    GlowLook {
        color: color(o, "color"),
        size: num(o, "size", 0.0, 40.0),
        strength: num(o, "strength", 0.0, 1.0),
    }
}

fn box_fx(o: &Obj) -> BoxFxLook {
    BoxFxLook {
        color: color(o, "color"),
        opacity: num(o, "opacity", 0.0, 1.0),
        pad_x: num(o, "pad_x", 0.0, 60.0),
        pad_y: num(o, "pad_y", 0.0, 60.0),
        radius: num(o, "radius", 0.0, 1.0),
        per: match word(o, "per").as_deref() {
            Some("line") => Some(BoxPer::Line),
            Some("word") => Some(BoxPer::Word),
            _ => None,
        },
    }
}

fn active_box(o: &Obj) -> ActiveBoxLook {
    ActiveBoxLook {
        color: color(o, "color"),
        opacity: num(o, "opacity", 0.0, 1.0),
        radius: num(o, "radius", 0.0, 1.0),
    }
}

fn ease(o: &Obj, k: &str) -> Option<Ease> {
    word(o, k).and_then(|e| Ease::parse(&e))
}

/// One word state; only the fields in `keep` are read.
fn word_state(o: &Obj, k: &str, keep: &[&str]) -> WordState {
    let Some(s) = sect(o, k) else {
        return WordState::default();
    };
    let has = |f: &str| keep.contains(&f);
    WordState {
        color: color(s, "color").filter(|_| has("color")),
        opacity: num(s, "opacity", 0.0, 1.0).filter(|_| has("opacity")),
        scale: num(s, "scale", 0.5, 1.5).filter(|_| has("scale")),
        blur: num(s, "blur", 0.0, 10.0).filter(|_| has("blur")),
        lift: num(s, "lift", -0.3, 0.3).filter(|_| has("lift")),
        rotate: num(s, "rotate", -10.0, 10.0).filter(|_| has("rotate")),
        stroke: sect(s, "stroke")
            .filter(|_| has("stroke"))
            .map(stroke)
            .filter(|v| *v != StrokeLook::default()),
        glow: sect(s, "glow")
            .filter(|_| has("glow"))
            .map(glow)
            .filter(|v| *v != GlowLook::default()),
        box_: sect(s, "box")
            .filter(|_| has("box"))
            .map(active_box)
            .filter(|v| *v != ActiveBoxLook::default()),
    }
}

fn words(o: &Obj) -> WordsLook {
    WordsLook {
        mode: match word(o, "mode").as_deref() {
            Some("all") => Some(WordMode::All),
            Some("build") => Some(WordMode::Build),
            Some("single") => Some(WordMode::Single),
            _ => None,
        },
        upcoming: word_state(o, "upcoming", &["color", "opacity", "scale", "blur"]),
        active: word_state(
            o,
            "active",
            &[
                "color", "opacity", "scale", "lift", "rotate", "stroke", "glow", "box",
            ],
        ),
        spoken: word_state(o, "spoken", &["color", "opacity", "scale", "blur"]),
        keyword: sect(o, "keyword").map_or_else(KeywordLook::default, |k| KeywordLook {
            color: color(k, "color"),
            scale: num(k, "scale", 0.5, 1.5),
            glow: sect(k, "glow")
                .map(glow)
                .filter(|v| *v != GlowLook::default()),
        }),
        fill: match word(o, "fill").as_deref() {
            Some("snap") => Some(Fill::Snap),
            Some("sweep") => Some(Fill::Sweep),
            _ => None,
        },
        attack_ms: num(o, "attack_ms", 0.0, 400.0),
        attack_ease: ease(o, "attack_ease"),
        hold_ms: num(o, "hold_ms", 0.0, 600.0),
        release_ms: num(o, "release_ms", 0.0, 2000.0),
        release_ease: ease(o, "release_ease"),
    }
}

fn enter(o: &Obj) -> EnterLook {
    EnterLook {
        kind: match word(o, "kind").as_deref() {
            Some("none") => Some(EnterKind::None),
            Some("pop") => Some(EnterKind::Pop),
            Some("fade") => Some(EnterKind::Fade),
            Some("slide_up") => Some(EnterKind::SlideUp),
            Some("slide_down") => Some(EnterKind::SlideDown),
            Some("slide_left") => Some(EnterKind::SlideLeft),
            Some("slide_right") => Some(EnterKind::SlideRight),
            Some("zoom") => Some(EnterKind::Zoom),
            Some("bounce") => Some(EnterKind::Bounce),
            Some("blur") => Some(EnterKind::Blur),
            Some("drop") => Some(EnterKind::Drop),
            _ => None,
        },
        ms: num(o, "ms", 0.0, 800.0),
        ease: ease(o, "ease"),
    }
}

fn exit(o: &Obj) -> ExitLook {
    ExitLook {
        kind: match word(o, "kind").as_deref() {
            Some("none") => Some(ExitKind::None),
            Some("fade") => Some(ExitKind::Fade),
            Some("slide_up") => Some(ExitKind::SlideUp),
            Some("slide_down") => Some(ExitKind::SlideDown),
            Some("zoom") => Some(ExitKind::Zoom),
            Some("blur") => Some(ExitKind::Blur),
            _ => None,
        },
        ms: num(o, "ms", 0.0, 600.0),
    }
}

fn font(o: &Obj) -> Option<&'static str> {
    o.get("font").and_then(Value::as_str).and_then(|f| {
        FONTS
            .iter()
            .find(|n| n.eq_ignore_ascii_case(f.trim()))
            .copied()
    })
}

fn case(o: &Obj) -> Option<Case> {
    match word(o, "case").as_deref() {
        Some("upper") => Some(Case::Upper),
        Some("asis") => Some(Case::AsIs),
        _ => None,
    }
}

fn align(o: &Obj) -> Option<Align> {
    match word(o, "align").as_deref() {
        Some("left") => Some(Align::Left),
        Some("center" | "centre") => Some(Align::Center),
        Some("right") => Some(Align::Right),
        _ => None,
    }
}

fn card_fx(o: &Obj) -> CardLook {
    CardLook {
        color: color(o, "color"),
        opacity: num(o, "opacity", 0.0, 1.0),
        pad: num(o, "pad", 0.0, 80.0),
        radius: num(o, "radius", 0.0, 1.0),
    }
}

/// A section that ended up with nothing in it is no section.
fn nonempty<T: Default + PartialEq>(v: T) -> Option<T> {
    (v != T::default()).then_some(v)
}

fn headline(o: &Obj) -> HeadlineLook {
    HeadlineLook {
        x: num(o, "x", 0.0, 1.0),
        y: num(o, "y", 0.0, 1.0),
        size: num(o, "size", 0.5, 2.0),
        ink: color(o, "ink"),
        card: match o.get("card") {
            Some(Value::Null) => Some(BoxLook::None),
            Some(Value::String(s)) if s.trim().eq_ignore_ascii_case("none") => Some(BoxLook::None),
            Some(Value::String(s)) => Rgb::parse(s).map(BoxLook::Color),
            _ => None,
        },
        accent: color(o, "accent"),
        anim: match word(o, "anim").as_deref() {
            Some("pop") => Some(HeadlineAnim::Pop),
            Some("fade") => Some(HeadlineAnim::Fade),
            Some("none") => Some(HeadlineAnim::None),
            _ => None,
        },
        seconds: num(o, "seconds", 0.0, 3600.0),
        font: font(o),
        case: case(o),
        spacing: num(o, "spacing", -0.05, 0.3),
        align: align(o),
        max_lines: num(o, "max_lines", 1.0, 3.0).map(|n| n.round() as usize),
        width: num(o, "width", 0.4, 1.0),
        stroke: sect(o, "stroke").map(stroke).and_then(nonempty),
        shadow: sect(o, "shadow").map(shadow_fx).and_then(nonempty),
        glow: sect(o, "glow").map(glow).and_then(nonempty),
        card_fx: sect(o, "card").map(card_fx).and_then(nonempty),
        accent_word: match word(o, "accent_word").as_deref() {
            Some("auto") => Some(AccentWord::Auto),
            Some("none") => Some(AccentWord::None),
            Some("first") => Some(AccentWord::First),
            Some("last") => Some(AccentWord::Last),
            _ => None,
        },
        enter: sect(o, "enter").map(enter).and_then(nonempty),
        exit: sect(o, "exit").map(exit).and_then(nonempty),
        delay_s: num(o, "delay_s", 0.0, 5.0),
    }
}

fn bar(o: &Obj) -> BarLook {
    BarLook {
        pos: match word(o, "pos").as_deref() {
            Some("top") => Some(BarPos::Top),
            Some("bottom") => Some(BarPos::Bottom),
            _ => None,
        },
        height: num(o, "height", 0.5, 3.0),
        color: color(o, "color"),
        track: color(o, "track"),
        track_opacity: num(o, "track_opacity", 0.0, 1.0),
        inset: num(o, "inset", 0.0, 0.1),
        radius: num(o, "radius", 0.0, 1.0),
        glow: sect(o, "glow").map(glow).and_then(nonempty),
    }
}

fn logo(o: &Obj) -> LogoLook {
    LogoLook {
        x: num(o, "x", 0.0, 1.0),
        y: num(o, "y", 0.0, 1.0),
        size: num(o, "size", 0.4, 2.5),
        opacity: num(o, "opacity", 0.0, 1.0),
        rotate: num(o, "rotate", -30.0, 30.0),
        shadow: sect(o, "shadow").map(shadow_fx).and_then(nonempty),
        glow: sect(o, "glow").map(glow).and_then(nonempty),
    }
}

fn camera(o: &Obj) -> CameraLook {
    CameraLook {
        feel: match word(o, "feel").as_deref() {
            Some("locked") => Some(CameraFeel::Locked),
            Some("steady") => Some(CameraFeel::Steady),
            Some("smooth") => Some(CameraFeel::Smooth),
            Some("lively") => Some(CameraFeel::Lively),
            _ => None,
        },
        zoom: num(o, "zoom", 0.8, 1.4),
        punch: num(o, "punch", 1.0, 1.4),
    }
}

fn effects(o: &Obj) -> EffectsLook {
    EffectsLook {
        vignette: num(o, "vignette", 0.0, 1.0),
        grade: match word(o, "grade").as_deref() {
            Some("none") => Some(Grade::None),
            Some("warm") => Some(Grade::Warm),
            Some("cool") => Some(Grade::Cool),
            Some("mono") => Some(Grade::Mono),
            Some("punchy") => Some(Grade::Punchy),
            _ => None,
        },
        fill_dim: num(o, "fill_dim", 0.0, 1.0),
    }
}

fn layout(o: &Obj) -> LayoutLook {
    LayoutLook {
        split: num(o, "split", 0.3, 0.7),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_convert_to_ass_order() {
        assert_eq!(Rgb::parse("#FF3B30"), Some(Rgb(255, 59, 48)));
        assert_eq!(Rgb(255, 59, 48).ass(0), "&H00303BFF");
        assert_eq!(Rgb(0x11, 0x22, 0x33).ass(0x80), "&H80332211");
        assert_eq!(Rgb::parse("00e5ff"), Some(Rgb(0, 229, 255)));
        assert_eq!(Rgb::parse("#FFF"), None);
        assert_eq!(Rgb::parse("#GG0000"), None);
        assert_eq!(Rgb::parse(""), None);
    }

    #[test]
    fn empty_and_junk_looks_are_empty() {
        assert!(Look::parse("{}").is_empty());
        assert!(Look::parse(r#"{"v":1}"#).is_empty());
        assert!(Look::parse(r#"{"captions":{}}"#).is_empty());
        assert!(Look::parse(r#"{"captions":{"font":"Comic Sans","size":"big"}}"#).is_empty());
        assert!(Look::parse("{nope").is_empty());
        assert!(Look::parse("[1,2]").is_empty());
        assert!(Look::parse("").is_empty());
        assert!(!Look::parse(r#"{"captions":{"x":0.2}}"#).is_empty());
        assert!(!Look::parse(r#"{"bar":{"pos":"top"}}"#).is_empty());
    }

    #[test]
    fn captions_fields_are_clamped_and_validated() {
        let l = Look::parse(
            r##"{"v":1,"unknown":7,"captions":{"show":false,"x":-3,"y":9,"size":9,
                "font":"anton","case":"Upper","color":"#ffffff","active":"red",
                "accent":"#00FF00","outline":"#101010","outline_w":99,"shadow":-1,
                "box":"#000000","box_opacity":2,"max_words":20,"anim":"Bounce","extra":1}}"##,
        );
        let c = l.captions.unwrap();
        assert_eq!(l.v, Some(1));
        assert_eq!(c.show, Some(false));
        assert_eq!((c.x, c.y, c.size), (Some(0.0), Some(1.0), Some(2.0)));
        assert_eq!(c.font, Some("Anton"));
        assert_eq!(c.case, Some(Case::Upper));
        assert_eq!(c.color, Some(Rgb(255, 255, 255)));
        assert_eq!(c.active, None); // malformed colour = absent
        assert_eq!(c.accent, Some(Rgb(0, 255, 0)));
        assert_eq!((c.outline_w, c.shadow), (Some(8.0), Some(0.0)));
        assert_eq!(c.box_, Some(BoxLook::Color(Rgb(0, 0, 0))));
        assert_eq!((c.box_opacity, c.max_words), (Some(1.0), Some(8)));
        assert_eq!(c.anim, Some(Anim::Bounce));
    }

    #[test]
    fn word_section_parses_clamps_and_drops_bad_values() {
        let c = Look::parse(
            r##"{"captions":{"words":{"mode":"Build","fill":"sweep",
                "upcoming":{"color":"#102030","opacity":-1,"scale":9,"blur":99,"lift":1,"rotate":3},
                "active":{"color":"nope","opacity":0.5,"scale":0.1,"lift":9,"rotate":-99,"blur":4},
                "spoken":{"color":"#FFFFFF","opacity":2,"scale":1.2,"blur":2},
                "keyword":{"color":"#FF00FF","scale":2},
                "attack_ms":999,"attack_ease":"BACK","hold_ms":-5,"release_ms":5000,"release_ease":"wobble"},
              "enter":{"kind":"Slide_Up","ms":9999,"ease":"out"},
              "exit":{"kind":"zoom","ms":-3}}}"##,
        )
        .captions
        .unwrap();
        let w = c.words.unwrap();
        assert_eq!((w.mode, w.fill), (Some(WordMode::Build), Some(Fill::Sweep)));
        // Each state keeps only the fields it has in the contract.
        assert_eq!(
            w.upcoming,
            WordState {
                color: Some(Rgb(0x10, 0x20, 0x30)),
                opacity: Some(0.0),
                scale: Some(1.5),
                blur: Some(10.0),
                lift: None,
                rotate: None,
                ..Default::default()
            }
        );
        assert_eq!(
            w.active,
            WordState {
                color: None,
                opacity: Some(0.5),
                scale: Some(0.5),
                blur: None,
                lift: Some(0.3),
                rotate: Some(-10.0),
                ..Default::default()
            }
        );
        assert_eq!(
            (w.spoken.opacity, w.spoken.scale, w.spoken.blur),
            (Some(1.0), Some(1.2), Some(2.0))
        );
        assert_eq!(
            (w.keyword.color, w.keyword.scale),
            (Some(Rgb(255, 0, 255)), Some(1.5))
        );
        assert_eq!(
            (
                w.attack_ms,
                w.attack_ease,
                w.hold_ms,
                w.release_ms,
                w.release_ease
            ),
            (Some(400.0), Some(Ease::Back), Some(0.0), Some(2000.0), None)
        );
        let e = c.enter.unwrap();
        assert_eq!(
            (e.kind, e.ms, e.ease),
            (Some(EnterKind::SlideUp), Some(800.0), Some(Ease::Out))
        );
        let x = c.exit.unwrap();
        assert_eq!((x.kind, x.ms), (Some(ExitKind::Zoom), Some(0.0)));
    }

    #[test]
    fn empty_word_sections_are_no_sections() {
        for j in [
            r#"{"captions":{"words":{}}}"#,
            r#"{"captions":{"words":{"mode":"sideways","fill":3,"upcoming":"x","keyword":[]}}}"#,
            r#"{"captions":{"enter":{}}}"#,
            r#"{"captions":{"enter":{"kind":"spin","ease":"wobble"}}}"#,
            r#"{"captions":{"exit":{"kind":"slide_left"}}}"#,
            r#"{"captions":{"words":5,"enter":"pop","exit":null}}"#,
        ] {
            let c = Look::parse(j).captions.unwrap();
            assert_eq!((c.words, c.enter, c.exit), (None, None, None), "{j}");
            assert!(Look::parse(j).is_empty(), "{j}");
        }
        assert!(!Look::parse(r#"{"captions":{"words":{"mode":"single"}}}"#).is_empty());
        assert!(!Look::parse(r#"{"captions":{"enter":{"kind":"none"}}}"#).is_empty());
    }

    #[test]
    fn bad_enums_fall_back_to_absent() {
        let c = Look::parse(
            r#"{"captions":{"case":"title","anim":"spin","font":"Arial","box":"teal"}}"#,
        )
        .captions
        .unwrap();
        assert_eq!(c, CaptionsLook::default());
        // box: null and "none" remove the box.
        for j in [
            r#"{"captions":{"box":null}}"#,
            r#"{"captions":{"box":"None"}}"#,
        ] {
            assert_eq!(Look::parse(j).captions.unwrap().box_, Some(BoxLook::None));
        }
    }

    #[test]
    fn other_sections_parse_with_their_ranges() {
        let l = Look::parse(
            r##"{"headline":{"x":0.5,"size":0.1,"ink":"#111111","anim":"fade","seconds":-4},
                "bar":{"pos":"top","height":9},"logo":{"size":0.1,"opacity":3},
                "camera":{"feel":"lively","zoom":5,"punch":0.2},
                "effects":{"vignette":2,"grade":"mono","fill_dim":0.4},"layout":{"split":0.9}}"##,
        );
        let h = l.headline.unwrap();
        assert_eq!(
            (h.size, h.seconds, h.anim),
            (Some(0.5), Some(0.0), Some(HeadlineAnim::Fade))
        );
        assert_eq!(h.card, None);
        let b = l.bar.unwrap();
        assert_eq!((b.pos, b.height), (Some(BarPos::Top), Some(3.0)));
        let g = l.logo.unwrap();
        assert_eq!((g.size, g.opacity), (Some(0.4), Some(1.0)));
        let c = l.camera.unwrap();
        assert_eq!(
            (c.feel, c.zoom, c.punch),
            (Some(CameraFeel::Lively), Some(1.4), Some(1.0))
        );
        let e = l.effects.unwrap();
        assert_eq!(
            (e.vignette, e.grade, e.fill_dim),
            (Some(1.0), Some(Grade::Mono), Some(0.4))
        );
        assert_eq!(l.layout.unwrap().split, Some(0.7));
    }

    #[test]
    fn headline_card_is_a_colour_or_none() {
        let card = |j: &str| Look::parse(j).headline.unwrap().card;
        assert_eq!(
            card(r##"{"headline":{"card":"#111111"}}"##),
            Some(BoxLook::Color(Rgb(0x11, 0x11, 0x11)))
        );
        assert_eq!(card(r#"{"headline":{"card":null}}"#), Some(BoxLook::None));
        assert_eq!(card(r#"{"headline":{"card":"None"}}"#), Some(BoxLook::None));
        assert_eq!(card(r#"{"headline":{"card":"teal"}}"#), None);
        assert_eq!(card(r#"{"headline":{"card":7}}"#), None);
        assert!(!Look::parse(r#"{"headline":{"card":null}}"#).is_empty());
    }

    #[test]
    fn caps_announce_the_look_and_its_sections() {
        assert!(CAPS.contains(&"look"));
        for s in [
            "captions", "headline", "bar", "logo", "camera", "effects", "layout",
        ] {
            assert!(CAPS.contains(&format!("look.{s}").as_str()), "{s}");
        }
        assert!(CAPS.contains(&"look.captions.words"));
        assert!(CAPS.contains(&"look.captions.motion"));
    }

    #[test]
    fn look_arg_reads_json_or_a_file_the_same() {
        let json = r##"{"captions":{"x":0.3,"y":0.8,"color":"#FFFFFF","anim":"slide"}}"##;
        let path = std::env::temp_dir().join(format!("digiclip-look-{}.json", std::process::id()));
        std::fs::write(&path, format!("\u{feff}{json}")).unwrap();
        let from_file = Look::from_arg(&format!("@{}", path.display()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(from_file, Look::from_arg(json));
        assert!(!from_file.is_empty());
        // A missing file is no look, not an error.
        assert!(Look::from_arg("@/no/such/look.json").is_empty());
    }

    #[test]
    fn dressing_fields_parse_clamp_and_fall_back() {
        let c = Look::parse(
            r##"{"captions":{"spacing":9,"line_gap":0,"lines":7,"max_chars":1,"align":"Right","rotate":-99,
                "stroke":{"color":"#102030","width":99},
                "shadow":{"color":"#000000","x":-99,"y":99,"blur":99,"opacity":9},
                "glow":{"color":"#FFD400","size":99,"strength":-1},
                "box":{"color":"#101010","opacity":2,"pad_x":999,"pad_y":-1,"radius":3,"per":"Word"}}}"##,
        )
        .captions
        .unwrap();
        assert_eq!((c.spacing, c.line_gap), (Some(0.3), Some(0.8)));
        assert_eq!((c.lines, c.max_chars), (Some(2), Some(6)));
        assert_eq!((c.align, c.rotate), (Some(Align::Right), Some(-15.0)));
        assert_eq!(
            c.stroke,
            Some(StrokeLook {
                color: Some(Rgb(0x10, 0x20, 0x30)),
                width: Some(12.0)
            })
        );
        let sh = c.shadow_fx.unwrap();
        assert_eq!(
            (sh.x, sh.y, sh.blur, sh.opacity),
            (Some(-30.0), Some(30.0), Some(20.0), Some(1.0))
        );
        assert_eq!(c.shadow, None);
        let g = c.glow.unwrap();
        assert_eq!((g.size, g.strength), (Some(40.0), Some(0.0)));
        let b = c.box_fx.unwrap();
        assert_eq!(
            (b.opacity, b.pad_x, b.pad_y, b.radius, b.per),
            (
                Some(1.0),
                Some(60.0),
                Some(0.0),
                Some(1.0),
                Some(BoxPer::Word)
            )
        );
        assert_eq!(c.box_, None);
        // Lower ends and the other spellings.
        let c = Look::parse(
            r#"{"captions":{"spacing":-9,"line_gap":9,"lines":0,"max_chars":99,"align":"centre"}}"#,
        )
        .captions
        .unwrap();
        assert_eq!((c.spacing, c.line_gap), (Some(-0.05), Some(1.6)));
        assert_eq!((c.lines, c.max_chars), (Some(1), Some(40)));
        assert_eq!(c.align, Some(Align::Center));
    }

    #[test]
    fn bad_dressing_values_are_absent_and_empty_objects_are_nothing() {
        for j in [
            r#"{"captions":{"spacing":"wide","align":"middle","rotate":null,"lines":"two"}}"#,
            r#"{"captions":{"stroke":{},"glow":{},"shadow":{},"box":{}}}"#,
            r#"{"captions":{"stroke":5,"glow":"big","shadow":[1],"box":false}}"#,
            r##"{"captions":{"glow":{"color":"gold","size":"big"},"stroke":{"color":"red"}}}"##,
            r#"{"captions":{"box":{"per":"paragraph","radius":"round"}}}"#,
            r#"{"captions":{"words":{"active":{"glow":{},"stroke":5,"box":[]},"keyword":{"glow":"x"}}}}"#,
        ] {
            let l = Look::parse(j);
            assert!(l.is_empty(), "{j}: {l:?}");
        }
        // A box of `false` is not `none`.
        assert_eq!(
            Look::parse(r#"{"captions":{"box":false}}"#)
                .captions
                .unwrap()
                .box_,
            None
        );
    }

    #[test]
    fn v1_forms_are_still_read_and_the_objects_sit_beside_them() {
        let c = Look::parse(
            r##"{"captions":{"box":"#101010","box_opacity":0.5,"shadow":4,"outline":"#FF0000","outline_w":5}}"##,
        )
        .captions
        .unwrap();
        assert_eq!(c.box_, Some(BoxLook::Color(Rgb(0x10, 0x10, 0x10))));
        assert_eq!((c.box_opacity, c.shadow), (Some(0.5), Some(4.0)));
        assert_eq!((c.outline, c.outline_w), (Some(Rgb(255, 0, 0)), Some(5.0)));
        assert!(c.box_fx.is_none() && c.shadow_fx.is_none() && c.stroke.is_none());
        assert!(!c.positioned());
        // Objects land in their own fields and make the caption positioned.
        let c = Look::parse(
            r##"{"captions":{"box":{"radius":1},"shadow":{"x":2},"stroke":{"width":3}}}"##,
        )
        .captions
        .unwrap();
        assert!(c.box_.is_none() && c.shadow.is_none());
        assert!(c.box_fx.is_some() && c.shadow_fx.is_some() && c.stroke.is_some());
        assert!(c.positioned());
        // "none" is still v1.
        let c = Look::parse(r#"{"captions":{"box":"none"}}"#)
            .captions
            .unwrap();
        assert_eq!(c.box_, Some(BoxLook::None));
        assert!(!c.positioned());
    }

    #[test]
    fn word_dressing_is_read_for_the_states_that_have_it() {
        let w = Look::parse(
            r##"{"captions":{"words":{
                "active":{"stroke":{"color":"#FFFFFF","width":99},"glow":{"size":99,"strength":0.5},
                          "box":{"color":"#FFD400","opacity":2,"radius":-1}},
                "spoken":{"stroke":{"width":3},"glow":{"size":3},"box":{"radius":1}},
                "upcoming":{"glow":{"size":3}},
                "keyword":{"glow":{"color":"#FF00FF","size":99,"strength":9}}}}}"##,
        )
        .captions
        .unwrap()
        .words
        .unwrap();
        let a = w.active;
        assert_eq!(a.stroke.unwrap().width, Some(12.0));
        assert_eq!(a.glow.unwrap().size, Some(40.0));
        let b = a.box_.unwrap();
        assert_eq!(
            (b.color, b.opacity, b.radius),
            (Some(Rgb(255, 212, 0)), Some(1.0), Some(0.0))
        );
        // Only the spoken word has these.
        for s in [&w.spoken, &w.upcoming] {
            assert!(s.stroke.is_none() && s.glow.is_none() && s.box_.is_none());
        }
        let k = w.keyword.glow.unwrap();
        assert_eq!(
            (k.color, k.size, k.strength),
            (Some(Rgb(255, 0, 255)), Some(40.0), Some(1.0))
        );
    }

    #[test]
    fn caps_announce_the_dressing() {
        assert!(CAPS.contains(&"look.captions.fx"));
        assert!(CAPS.contains(&"look.captions.type"));
    }

    fn headline_of(json: &str) -> HeadlineLook {
        Look::parse(&format!(r##"{{"headline":{json}}}"##))
            .headline
            .unwrap()
    }

    #[test]
    fn headline_dressing_fields_parse_clamp_and_fall_back() {
        let h = headline_of(
            r##"{"font":"jetbrains mono","case":"Upper","spacing":9,"align":"Right","max_lines":9,
                "width":0.1,"delay_s":99,
                "stroke":{"color":"#102030","width":99},
                "shadow":{"color":"#000000","x":-99,"y":99,"blur":99,"opacity":9},
                "glow":{"color":"#FFD400","size":99,"strength":-1},
                "accent_word":"LAST",
                "enter":{"kind":"slide_down","ms":9999,"ease":"back"},
                "exit":{"kind":"blur","ms":-5}}"##,
        );
        assert_eq!(h.font, Some("JetBrains Mono"));
        assert_eq!(
            (h.case, h.spacing, h.align),
            (Some(Case::Upper), Some(0.3), Some(Align::Right))
        );
        assert_eq!(
            (h.max_lines, h.width, h.delay_s),
            (Some(3), Some(0.4), Some(5.0))
        );
        assert_eq!(
            h.stroke,
            Some(StrokeLook {
                color: Some(Rgb(0x10, 0x20, 0x30)),
                width: Some(12.0)
            })
        );
        let sh = h.shadow.unwrap();
        assert_eq!(
            (sh.x, sh.y, sh.blur, sh.opacity),
            (Some(-30.0), Some(30.0), Some(20.0), Some(1.0))
        );
        let g = h.glow.unwrap();
        assert_eq!(
            (g.color, g.size, g.strength),
            (Some(Rgb(255, 212, 0)), Some(40.0), Some(0.0))
        );
        assert_eq!(h.accent_word, Some(AccentWord::Last));
        let e = h.enter.unwrap();
        assert_eq!(
            (e.kind, e.ms, e.ease),
            (Some(EnterKind::SlideDown), Some(800.0), Some(Ease::Back))
        );
        let x = h.exit.unwrap();
        assert_eq!((x.kind, x.ms), (Some(ExitKind::Blur), Some(0.0)));
        // The lower ends.
        let h = headline_of(
            r#"{"spacing":-9,"max_lines":0,"width":9,"delay_s":-3,"font":"anton","align":"centre"}"#,
        );
        assert_eq!(
            (h.spacing, h.max_lines, h.width, h.delay_s),
            (Some(-0.05), Some(1), Some(1.0), Some(0.0))
        );
        assert_eq!((h.font, h.align), (Some("Anton"), Some(Align::Center)));
        for (j, want) in [
            ("first", AccentWord::First),
            ("Auto", AccentWord::Auto),
            ("none", AccentWord::None),
        ] {
            let h = headline_of(&format!(r#"{{"accent_word":"{j}"}}"#));
            assert_eq!(h.accent_word, Some(want));
        }
    }

    #[test]
    fn bad_headline_values_are_absent_and_empty_objects_are_nothing() {
        for j in [
            r#"{"headline":{"font":"Comic Sans","case":"title","align":"middle","accent_word":"second"}}"#,
            r#"{"headline":{"spacing":"wide","max_lines":"two","width":null,"delay_s":"soon"}}"#,
            r#"{"headline":{"stroke":{},"shadow":{},"glow":{},"card":{},"enter":{},"exit":{}}}"#,
            r#"{"headline":{"stroke":5,"shadow":[1],"glow":"big","enter":"pop","exit":3}}"#,
            r#"{"headline":{"enter":{"kind":"spin","ease":"wobble"},"exit":{"kind":"slide_left"}}}"#,
            r##"{"headline":{"glow":{"color":"gold"},"card":{"color":"teal","pad":"big"}}}"##,
        ] {
            let l = Look::parse(j);
            assert!(l.is_empty(), "{j}: {l:?}");
            assert!(!l.headline.unwrap().positioned(), "{j}");
        }
        assert!(!Look::parse(r#"{"headline":{"width":0.7}}"#).is_empty());
        assert!(!Look::parse(r#"{"headline":{"delay_s":0}}"#).is_empty());
    }

    #[test]
    fn the_headline_card_is_a_colour_none_or_an_object() {
        // v1 forms stay v1: they do not make the headline positioned.
        for j in [
            r##"{"card":"#111111"}"##,
            r#"{"card":"none"}"#,
            r#"{"card":null}"#,
            r##"{"x":0.5,"y":0.2,"size":1.2,"ink":"#FFFFFF","accent":"#FF0000","anim":"fade","seconds":2}"##,
        ] {
            let h = headline_of(j);
            assert!(!h.positioned(), "{j}");
            assert!(h.card_fx.is_none(), "{j}");
        }
        // The object lands in its own field and leaves the v1 slot empty.
        let h = headline_of(r##"{"card":{"color":"#102030","opacity":2,"pad":999,"radius":-1}}"##);
        assert!(h.card.is_none() && h.positioned());
        assert_eq!(
            h.card_fx,
            Some(CardLook {
                color: Some(Rgb(0x10, 0x20, 0x30)),
                opacity: Some(1.0),
                pad: Some(80.0),
                radius: Some(0.0)
            })
        );
        // An object with a single field is enough.
        assert!(headline_of(r#"{"card":{"radius":1}}"#).positioned());
        // Each other field on its own makes it positioned.
        for j in [
            r#"{"font":"Anton"}"#,
            r#"{"case":"asis"}"#,
            r#"{"spacing":0}"#,
            r#"{"align":"left"}"#,
            r#"{"max_lines":2}"#,
            r#"{"width":0.8}"#,
            r#"{"stroke":{"width":1}}"#,
            r#"{"shadow":{"y":1}}"#,
            r#"{"glow":{"size":1}}"#,
            r#"{"accent_word":"none"}"#,
            r#"{"enter":{"kind":"none"}}"#,
            r#"{"exit":{"kind":"fade"}}"#,
            r#"{"delay_s":1}"#,
        ] {
            assert!(headline_of(j).positioned(), "{j}");
        }
    }

    #[test]
    fn bar_fields_parse_clamp_and_fall_back() {
        let b = Look::parse(
            r##"{"bar":{"pos":"top","height":2,"color":"#FF3B30","track":"#FFFFFF","track_opacity":9,
                "inset":0.5,"radius":-2,"glow":{"color":"#00E5FF","size":99,"strength":0.5}}}"##,
        )
        .bar
        .unwrap();
        assert_eq!((b.pos, b.height), (Some(BarPos::Top), Some(2.0)));
        assert_eq!(
            (b.color, b.track),
            (Some(Rgb(255, 59, 48)), Some(Rgb(255, 255, 255)))
        );
        assert_eq!(
            (b.track_opacity, b.inset, b.radius),
            (Some(1.0), Some(0.1), Some(0.0))
        );
        let g = b.glow.unwrap();
        assert_eq!(
            (g.color, g.size, g.strength),
            (Some(Rgb(0, 229, 255)), Some(40.0), Some(0.5))
        );
        let b = Look::parse(r#"{"bar":{"inset":-1,"track_opacity":-1,"radius":9}}"#)
            .bar
            .unwrap();
        assert_eq!(
            (b.inset, b.track_opacity, b.radius),
            (Some(0.0), Some(0.0), Some(1.0))
        );
        // Bad values are absent; an empty glow is no glow.
        let l = Look::parse(
            r##"{"bar":{"color":"red","track":"#12","track_opacity":"half","inset":"wide","radius":null,"glow":{}}}"##,
        );
        assert!(l.is_empty(), "{l:?}");
    }

    #[test]
    fn a_bar_is_shaped_only_by_the_fields_that_change_its_shape() {
        let bar = |j: &str| Look::parse(&format!(r#"{{"bar":{j}}}"#)).bar.unwrap();
        // v1 fields and the colour keep the plain drawing.
        for j in [
            r#"{}"#,
            r#"{"pos":"top","height":2}"#,
            r##"{"color":"#FF0000"}"##,
        ] {
            assert!(!bar(j).shaped(), "{j}");
        }
        for j in [
            r##"{"track":"#000000"}"##,
            r#"{"track_opacity":0.5}"#,
            r#"{"inset":0}"#,
            r#"{"radius":0}"#,
            r#"{"glow":{"size":1}}"#,
        ] {
            assert!(bar(j).shaped(), "{j}");
        }
    }

    #[test]
    fn logo_fields_parse_clamp_and_fall_back() {
        let g = Look::parse(
            r##"{"logo":{"rotate":-99,"shadow":{"color":"#000000","x":99,"y":-99,"blur":99,"opacity":-1},
                "glow":{"color":"#FFD400","size":99,"strength":9}}}"##,
        )
        .logo
        .unwrap();
        assert_eq!(g.rotate, Some(-30.0));
        let sh = g.shadow.unwrap();
        assert_eq!(
            (sh.x, sh.y, sh.blur, sh.opacity),
            (Some(30.0), Some(-30.0), Some(20.0), Some(0.0))
        );
        let gl = g.glow.unwrap();
        assert_eq!((gl.size, gl.strength), (Some(40.0), Some(1.0)));
        let g = Look::parse(r#"{"logo":{"rotate":99}}"#).logo.unwrap();
        assert_eq!(g.rotate, Some(30.0));
        for j in [
            r#"{"logo":{"rotate":"left","shadow":{},"glow":5}}"#,
            r#"{"logo":{"shadow":[1],"glow":{"color":"red"}}}"#,
        ] {
            assert!(Look::parse(j).is_empty(), "{j}");
        }
        // v1 fields are untouched by the new ones.
        let g = Look::parse(r#"{"logo":{"x":0.2,"size":1.5,"opacity":0.5}}"#)
            .logo
            .unwrap();
        assert_eq!((g.x, g.size, g.opacity), (Some(0.2), Some(1.5), Some(0.5)));
        assert!(g.rotate.is_none() && g.shadow.is_none() && g.glow.is_none());
    }

    #[test]
    fn caps_announce_the_headline_bar_and_logo_depth() {
        for c in ["look.headline.v2", "look.bar.v2", "look.logo.v2"] {
            assert!(CAPS.contains(&c), "{c}");
        }
    }
}
