//! The word-level caption model: every word is a small timeline of three
//! looks (upcoming, active, spoken) and the line has an entrance and an exit.
//! Used only when a Look sets `captions.words`, `captions.enter` or
//! `captions.exit`; a look without them goes through the original
//! one-event-per-line writer in `ass.rs`, byte for byte as before.
//!
//! How it is drawn. Scale, lift and tilt of one word inside a libass line
//! move its neighbours, so here every word is its own positioned event
//! (`\an5\pos`) and the line is laid out once, from the font's advance
//! widths (`metrics`), with room kept for the biggest look each word takes.
//! Nothing in the line moves when a word changes look. Where something moves
//! (lift, slide, the line scaling about its centre) the event is cut into
//! 40 ms slices, each a straight `\move`; everywhere else the looks are
//! `\t` transforms inside one event per word.
//!
//! The dressing of the text (stroke, shadow, glow, boxes) is drawn as extra
//! events under each word, on their own layers (box 0, shadow 1, glow 2, text
//! 3), and every one of them is an `Item` driven by the same tracks as the
//! word: the copies cannot drift off it. A shadow or glow whose words never
//! change shape, size, opacity or colour is written once per row instead of
//! once per word.
//!
//! Times in this file are milliseconds on the clip clock.

use std::ops::Range;

use super::ass::{ends_sentence, is_keyword, Anim};
use super::metrics;
use crate::look::{
    Align, CaptionsLook, Ease, EnterKind, ExitKind, Fill, GlowLook, Rgb, WordMode, WordState,
};
use crate::whisper::Word;

/// The shortest piece a moving stretch is cut into (ms): anything longer is
/// cut again until each piece is close to a straight line.
const GRID_MS: f64 = 40.0;
/// Where an ease-back is at its highest, as a share of its duration, and by
/// how much it overshoots (share of the travel).
const BACK_AT: f64 = 0.58;
const BACK_PEAK: f64 = 0.10;
const BACK_C1: f64 = 1.70158;
/// Keyword bump: peak scale, and when it peaks and ends after the word starts.
const BUMP: f64 = 1.14;
const BUMP_UP_MS: f64 = 90.0;
const BUMP_END_MS: f64 = 240.0;
/// Entrances and exits: how far a slide travels (share of the frame), and how
/// far a drop falls; blur at its strongest (px at 1080 wide).
const SLIDE_V: f64 = 0.022;
const SLIDE_H: f64 = 0.04;
const DROP_V: f64 = 0.06;
const FX_BLUR: f64 = 8.0;
/// A glow's `size` is how far the light reaches: it is a border of this
/// share of it, blurred by this share of it (px at 1080 wide).
pub const GLOW_BORD: f64 = 0.55;
pub const GLOW_BLUR: f64 = 0.6;
/// What a glow, shadow or box object leaves out (px at 1080 wide).
pub const GLOW_SIZE: f64 = 12.0;
pub const GLOW_STRENGTH: f64 = 0.8;
pub const SHADOW_Y: f64 = 4.0;
pub const SHADOW_BLUR: f64 = 4.0;
pub const SHADOW_OPACITY: f64 = 0.6;
const BOX_PAD_X: f64 = 16.0;
const BOX_PAD_Y: f64 = 8.0;
/// Drawing units per pixel (p4: one unit is an eighth of a pixel).
const DRAW_UNIT: f64 = 8.0;
/// The layers the dressing is drawn on, bottom to top.
pub const LAYER_BOX: u8 = 0;
pub const LAYER_SHADOW: u8 = 1;
pub const LAYER_GLOW: u8 = 2;
pub const LAYER_TEXT: u8 = 3;

// ---- ease -----------------------------------------------------------------

/// Progress `u` (0..1) through an ease.
pub fn ease_fn(e: Ease, u: f64) -> f64 {
    let u = u.clamp(0.0, 1.0);
    match e {
        Ease::Linear => u,
        Ease::Out => u.sqrt(),
        Ease::In => u * u,
        Ease::Back => {
            let c3 = BACK_C1 + 1.0;
            1.0 + c3 * (u - 1.0).powi(3) + BACK_C1 * (u - 1.0).powi(2)
        }
    }
}

/// The `\t` acceleration of an ease that is a plain power curve.
fn accel(e: Ease) -> f64 {
    match e {
        Ease::Out => 0.5,
        Ease::In => 2.0,
        _ => 1.0,
    }
}

/// A value that cannot go past its ends (colour, opacity) has no overshoot.
fn flat(e: Ease) -> Ease {
    if e == Ease::Back {
        Ease::Out
    } else {
        e
    }
}

// ---- numbers and tags ------------------------------------------------------

/// A number to one decimal, bare when whole.
fn n1(v: f64) -> String {
    let r = (v * 10.0).round() / 10.0;
    if r.fract() == 0.0 {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

/// A `\t` acceleration: plain digits.
fn nacc(v: f64) -> String {
    let r = (v * 100.0).round() / 100.0;
    if r.fract() == 0.0 {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

type Col = [f64; 3];

const NO_GLOW: Glow = Glow {
    col: [255.0; 3],
    size: 0.0,
    strength: 0.0,
};

fn col_of(c: Rgb) -> Col {
    [c.0 as f64, c.1 as f64, c.2 as f64]
}

/// `&HAABBGGRR` as a colour (alpha dropped).
pub fn col_of_ass(s: &str) -> Col {
    let h = s.trim_start_matches("&H").trim_end_matches('&');
    let h = if h.len() > 6 { &h[h.len() - 6..] } else { h };
    let v = u32::from_str_radix(h, 16).unwrap_or(0xFFFFFF);
    [
        (v & 0xFF) as f64,
        ((v >> 8) & 0xFF) as f64,
        ((v >> 16) & 0xFF) as f64,
    ]
}

fn col_tag(tag: &str, c: Col) -> String {
    let b = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    format!("\\{tag}&H{:02X}{:02X}{:02X}&", b(c[2]), b(c[1]), b(c[0]))
}

fn alpha_byte(opacity: f64) -> u8 {
    ((1.0 - opacity.clamp(0.0, 1.0)) * 255.0).round() as u8
}

// ---- resolved settings ------------------------------------------------------

/// A stroke, concrete (width in px).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stroke {
    pub col: Col,
    pub w: f64,
}

/// A glow, concrete (size in px); a strength of 0 is no glow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glow {
    pub col: Col,
    pub size: f64,
    pub strength: f64,
}

/// A drop shadow, concrete (px).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    pub col: Col,
    pub x: f64,
    pub y: f64,
    pub blur: f64,
    pub opacity: f64,
}

/// A box behind the line or each word, concrete (px).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoxSpec {
    pub col: Col,
    pub opacity: f64,
    pub pad_x: f64,
    pub pad_y: f64,
    pub radius: f64,
    pub per_word: bool,
    /// One box around the whole block of rows (a headline's card) rather than
    /// one per row.
    pub block: bool,
}

/// The box behind the spoken word (its opacity is a word look).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ABox {
    pub col: Col,
    pub radius: f64,
    pub pad_x: f64,
    pub pad_y: f64,
}

/// One look of a word, every field concrete.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WordLook {
    pub color: Col,
    pub opacity: f64,
    pub scale: f64,
    pub blur: f64,
    pub lift: f64,
    pub rotate: f64,
    pub stroke: Stroke,
    pub glow: Glow,
    /// Opacity of the box behind the word (only the spoken word has one).
    pub abox: f64,
}

/// What the style and the v1 fields hand the dressing: the stroke the text has
/// before the Look, the colour of the style's own box and the type size.
#[derive(Debug, Clone, Copy)]
pub struct Base {
    pub stroke: Stroke,
    /// Colour of the box the style (or a v1 `box`) has, if any.
    pub box_col: Option<Col>,
    /// v1 `box_opacity`, else 1.
    pub box_opacity: f64,
    /// The type size (px), for spacing.
    pub font_px: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Enter {
    pub kind: EnterKind,
    pub ms: f64,
    /// `None` = the kind's own shape (pop and bounce keyframes, or its default ease).
    pub ease: Option<Ease>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Exit {
    pub kind: ExitKind,
    pub ms: f64,
}

/// Everything the word model needs, after the v1 fields, the v2 fields and the
/// style's own values have been folded together.
#[derive(Debug, Clone, PartialEq)]
pub struct Cfg {
    pub mode: WordMode,
    pub fill: Fill,
    pub up: WordLook,
    pub act: WordLook,
    pub spk: WordLook,
    /// Set when `spoken.color` was given: it beats the keyword colour.
    pub spk_color_set: bool,
    pub kw_color: Col,
    pub kw_scale: Option<f64>,
    pub attack: f64,
    pub attack_ease: Ease,
    pub hold: f64,
    pub release: f64,
    pub release_ease: Ease,
    pub enter: Enter,
    pub exit: Exit,
    /// Letter spacing (px), row pitch (multiple of the type size), how the rows
    /// sit in the block, the block's tilt (degrees, clockwise).
    pub spacing: f64,
    pub line_gap: f64,
    pub align: Option<Align>,
    pub rotate: f64,
    pub max_lines: Option<usize>,
    /// Break the line into exactly this many rows, as evenly as it allows
    /// (a headline has decided its rows already).
    pub force_rows: Option<usize>,
    /// Does any word carry its own stroke (so every text event says its stroke)?
    pub stroke_tags: bool,
    /// The widest stroke any look has (px).
    pub stroke_max: f64,
    pub shadow: Option<Shadow>,
    pub boxfx: Option<BoxSpec>,
    pub abox: Option<ABox>,
    /// The glow layers as the Look wrote them: the caption's, the spoken word's
    /// and the keywords'.
    pub glow: Option<GlowLook>,
    pub glow_act: Option<GlowLook>,
    pub glow_kw: Option<GlowLook>,
    /// Canvas scale of the 1080-wide design.
    pub k: f64,
    /// Does anything draw under the text (so the text moves up to its layer)?
    pub decor: bool,
}

/// What a v1 `anim` stands for: reveal mode, entrance and exit.
fn from_anim(a: Anim) -> (WordMode, Enter, Exit) {
    let enter = |kind, ms, ease| Enter { kind, ms, ease };
    let exit = |kind, ms| Exit { kind, ms };
    match a {
        Anim::Pop => (
            WordMode::All,
            enter(EnterKind::Pop, 200.0, None),
            exit(ExitKind::Fade, 60.0),
        ),
        Anim::Words => (
            WordMode::Build,
            enter(EnterKind::Pop, 200.0, None),
            exit(ExitKind::Fade, 60.0),
        ),
        Anim::Static => (
            WordMode::All,
            enter(EnterKind::None, 0.0, None),
            exit(ExitKind::None, 0.0),
        ),
        Anim::Fade => (
            WordMode::All,
            enter(EnterKind::Fade, 200.0, Some(Ease::Linear)),
            exit(ExitKind::Fade, 140.0),
        ),
        Anim::Slide => (
            WordMode::All,
            enter(EnterKind::SlideUp, 260.0, Some(Ease::Linear)),
            exit(ExitKind::Fade, 60.0),
        ),
        Anim::Bounce => (
            WordMode::All,
            enter(EnterKind::Bounce, 340.0, None),
            exit(ExitKind::Fade, 60.0),
        ),
    }
}

fn enter_ms(k: EnterKind) -> f64 {
    match k {
        EnterKind::None => 0.0,
        EnterKind::Pop | EnterKind::Fade => 200.0,
        EnterKind::SlideUp
        | EnterKind::SlideDown
        | EnterKind::SlideLeft
        | EnterKind::SlideRight => 260.0,
        EnterKind::Zoom | EnterKind::Blur => 240.0,
        EnterKind::Bounce => 340.0,
        EnterKind::Drop => 320.0,
    }
}

fn exit_ms(k: ExitKind) -> f64 {
    match k {
        ExitKind::None => 0.0,
        ExitKind::Fade => 140.0,
        _ => 200.0,
    }
}

/// The entrance and exit a Look ends up with: its own, else its `anim`'s. An
/// entrance or exit with no time is none.
pub fn effective_motion(cap: &CaptionsLook, anim: Anim) -> (Enter, Exit) {
    let (_, en, ex) = from_anim(anim);
    merge_motion(cap.enter.as_ref(), cap.exit.as_ref(), en, ex)
}

/// An entrance and an exit given by the Look over the ones the element has by
/// default (`en`, `ex`): a field left out keeps the default's, a different kind
/// takes that kind's own time. An entrance or exit with no time is none.
pub fn merge_motion(
    look_enter: Option<&crate::look::EnterLook>,
    look_exit: Option<&crate::look::ExitLook>,
    en: Enter,
    ex: Exit,
) -> (Enter, Exit) {
    let enter = match look_enter {
        None => en,
        Some(e) => {
            let kind = e.kind.unwrap_or(en.kind);
            let same = kind == en.kind;
            Enter {
                kind,
                ms: e.ms.unwrap_or(if same { en.ms } else { enter_ms(kind) }),
                ease: e.ease.or(if same { en.ease } else { None }),
            }
        }
    };
    let exit = match look_exit {
        None => ex,
        Some(e) => {
            let kind = e.kind.unwrap_or(ex.kind);
            let same = kind == ex.kind;
            Exit {
                kind,
                ms: e.ms.unwrap_or(if same { ex.ms } else { exit_ms(kind) }),
            }
        }
    };
    (
        Enter {
            kind: if enter.ms <= 0.0 {
                EnterKind::None
            } else {
                enter.kind
            },
            ..enter
        },
        Exit {
            kind: if exit.ms <= 0.0 {
                ExitKind::None
            } else {
                exit.kind
            },
            ..exit
        },
    )
}

/// A shadow object with its defaults (black, x 0, y 4, blur 4, opacity 0.6)
/// filled in; lengths scaled from the 1080-wide design by `k`. The headline and
/// the logo read their shadow through this too.
pub fn shadow_from(s: &crate::look::ShadowLook, k: f64) -> Shadow {
    Shadow {
        col: s.color.map_or([0.0; 3], col_of),
        x: s.x.unwrap_or(0.0) * k,
        y: s.y.unwrap_or(SHADOW_Y) * k,
        blur: s.blur.unwrap_or(SHADOW_BLUR) * k,
        opacity: s.opacity.unwrap_or(SHADOW_OPACITY),
    }
}

impl Cfg {
    /// Fold the Look over the style's own colours. `prim`, `sec` and `accent`
    /// are the style's (or the v1 fields') sung, unsung and keyword colours;
    /// `k` scales blur from the 1080-wide design to the canvas.
    pub fn resolve(
        cap: &CaptionsLook,
        anim: Anim,
        prim: Col,
        sec: Col,
        accent: Col,
        k: f64,
        base: &Base,
    ) -> Cfg {
        let w = cap.words.clone().unwrap_or_default();
        let (legacy_mode, ..) = from_anim(anim);
        let (enter, exit) = effective_motion(cap, anim);
        let mode = w.mode.unwrap_or(legacy_mode);
        let fill = w.fill.unwrap_or(Fill::Snap);
        let state = |s: &WordState, color: Col, opacity: f64, scale: f64| WordLook {
            color: s.color.map_or(color, col_of),
            opacity: s.opacity.unwrap_or(opacity),
            scale: s.scale.unwrap_or(scale),
            blur: s.blur.unwrap_or(0.0) * k,
            lift: s.lift.unwrap_or(0.0),
            rotate: s.rotate.unwrap_or(0.0),
            stroke: base.stroke,
            glow: NO_GLOW,
            abox: 0.0,
        };
        let up = state(&w.upcoming, sec, 1.0, 1.0);
        let mut act = state(&w.active, prim, 1.0, 1.0);
        // A spoken word keeps the sung colour, as it does today.
        let spk = state(&w.spoken, act.color, 1.0, 1.0);
        // The spoken word's own stroke is the caption's with its fields on top.
        if let Some(st) = &w.active.stroke {
            act.stroke = Stroke {
                col: st.color.map_or(base.stroke.col, col_of),
                w: st.width.map_or(base.stroke.w, |v| v * k),
            };
        }
        // Boxes: the caption's (one per line or per word), and the spoken word's.
        let boxfx = cap.box_fx.as_ref().map(|b| BoxSpec {
            col: b.color.map(col_of).or(base.box_col).unwrap_or([0.0; 3]),
            opacity: b.opacity.unwrap_or(base.box_opacity),
            pad_x: b.pad_x.unwrap_or(BOX_PAD_X) * k,
            pad_y: b.pad_y.unwrap_or(BOX_PAD_Y) * k,
            radius: b.radius.unwrap_or(0.0),
            per_word: b.per == Some(crate::look::BoxPer::Word),
            block: false,
        });
        let abox = w.active.box_.as_ref().map(|b| ABox {
            col: b.color.map(col_of).or(base.box_col).unwrap_or(accent),
            radius: b
                .radius
                .unwrap_or_else(|| boxfx.as_ref().map_or(0.0, |x| x.radius)),
            pad_x: boxfx.as_ref().map_or(BOX_PAD_X * k, |x| x.pad_x),
            pad_y: boxfx.as_ref().map_or(BOX_PAD_Y * k, |x| x.pad_y),
        });
        if let Some(b) = &w.active.box_ {
            act.abox = b.opacity.unwrap_or(1.0);
        }
        let shadow = cap.shadow_fx.as_ref().map(|s| shadow_from(s, k));
        let stroke_max = [up, act, spk]
            .iter()
            .map(|l| l.stroke.w)
            .fold(base.stroke.w, f64::max);
        let decor = boxfx.is_some()
            || abox.is_some()
            || shadow.is_some()
            || cap.glow.is_some()
            || w.active.glow.is_some()
            || w.keyword.glow.is_some();
        Cfg {
            spacing: cap.spacing.unwrap_or(0.0) * base.font_px,
            line_gap: cap.line_gap.unwrap_or(1.0),
            align: cap.align,
            rotate: cap.rotate.unwrap_or(0.0),
            max_lines: cap.lines,
            force_rows: None,
            stroke_tags: cap.stroke.is_some() || w.active.stroke.is_some(),
            stroke_max,
            shadow,
            boxfx,
            abox,
            glow: cap.glow.clone(),
            glow_act: w.active.glow.clone(),
            glow_kw: w.keyword.glow.clone(),
            k,
            decor,
            mode,
            fill,
            up,
            act,
            spk,
            spk_color_set: w.spoken.color.is_some(),
            kw_color: w.keyword.color.map_or(accent, col_of),
            kw_scale: w.keyword.scale,
            attack: w
                .attack_ms
                .unwrap_or(if mode == WordMode::Build { 80.0 } else { 0.0 }),
            attack_ease: w.attack_ease.unwrap_or(Ease::Out),
            hold: w.hold_ms.unwrap_or(0.0),
            release: w.release_ms.unwrap_or(0.0),
            release_ease: w.release_ease.unwrap_or(Ease::Out),
            enter,
            exit,
        }
    }

    /// Does the entrance bump keywords (the pop family, as today)?
    fn bumps(&self) -> bool {
        self.kw_scale.is_none() && matches!(self.enter.kind, EnterKind::Pop | EnterKind::Bounce)
    }
}

// ---- tracks ----------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Seg<const N: usize> {
    t0: f64,
    t1: f64,
    from: [f64; N],
    to: [f64; N],
    ease: Ease,
}

/// A value over time: a start and ramps in order (a ramp with `t0 == t1` is a step).
#[derive(Debug, Clone)]
struct Track<const N: usize> {
    base: [f64; N],
    segs: Vec<Seg<N>>,
}

impl<const N: usize> Track<N> {
    fn new(base: [f64; N]) -> Self {
        Track {
            base,
            segs: Vec::new(),
        }
    }

    fn ramp(&mut self, t0: f64, t1: f64, to: [f64; N], ease: Ease) {
        let from = self.segs.last().map_or(self.base, |s| s.to);
        if from.iter().zip(&to).all(|(a, b)| (a - b).abs() < 1e-9) {
            return;
        }
        self.segs.push(Seg {
            t0,
            t1: t1.max(t0),
            from,
            to,
            ease,
        });
    }

    fn eval(&self, t: f64) -> [f64; N] {
        // Times come from sums of floats: a moment that is a segment's end
        // up to rounding counts as past it, so a step at a knot has happened.
        const EPS: f64 = 1e-6;
        let mut v = self.base;
        for s in &self.segs {
            if t >= s.t1 - EPS {
                v = s.to;
            } else if t > s.t0 {
                let p = ease_fn(s.ease, (t - s.t0) / (s.t1 - s.t0));
                for (i, o) in v.iter_mut().enumerate() {
                    *o = s.from[i] + (s.to[i] - s.from[i]) * p;
                }
                return v;
            } else {
                return v;
            }
        }
        v
    }
}

// ---- the line: entrance and exit -----------------------------------------------

/// What the line as a whole is doing at one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Fx {
    sc: f64,
    dx: f64,
    dy: f64,
    alpha: f64,
    blur: f64,
}

const NO_FX: Fx = Fx {
    sc: 1.0,
    dx: 0.0,
    dy: 0.0,
    alpha: 1.0,
    blur: 0.0,
};

/// Pixel sizes the line effects travel (frame size, canvas scale).
#[derive(Debug, Clone, Copy)]
struct Reach {
    v: f64,
    h: f64,
    drop: f64,
    blur: f64,
}

#[derive(Debug, Clone, Copy)]
struct LineWin {
    /// The moment the entrance starts and the exit ends.
    e0: f64,
    x1: f64,
    enter: Enter,
    exit: Exit,
    reach: Reach,
}

const POP_KEYS: &[(f64, f64)] = &[(0.0, 0.84), (0.55, 1.05), (1.0, 1.0)];
const BOUNCE_KEYS: &[(f64, f64)] = &[
    (0.0, 0.70),
    (0.353, 1.22),
    (0.618, 0.94),
    (0.824, 1.04),
    (1.0, 1.0),
];

fn keyed(keys: &[(f64, f64)], u: f64) -> f64 {
    for w in keys.windows(2) {
        let ((a, va), (b, vb)) = (w[0], w[1]);
        if u <= b {
            return va + (vb - va) * ((u - a) / (b - a)).clamp(0.0, 1.0);
        }
    }
    keys.last().map_or(1.0, |k| k.1)
}

/// How long an entrance's own fade takes (ms).
fn enter_fade_ms(e: &Enter) -> f64 {
    match e.kind {
        EnterKind::None => 0.0,
        EnterKind::Pop | EnterKind::Bounce => e.ms.min(80.0),
        EnterKind::SlideUp
        | EnterKind::SlideDown
        | EnterKind::SlideLeft
        | EnterKind::SlideRight => e.ms.min(160.0),
        EnterKind::Zoom => e.ms.min(150.0),
        EnterKind::Drop => e.ms.min(100.0),
        EnterKind::Fade | EnterKind::Blur => e.ms,
    }
}

impl LineWin {
    fn enter_fx(&self, t: f64) -> Fx {
        let e = &self.enter;
        if e.kind == EnterKind::None || e.ms <= 0.0 || t >= self.e0 + e.ms {
            return NO_FX;
        }
        let el = (t - self.e0).max(0.0);
        let u = el / e.ms;
        let fade = enter_fade_ms(e);
        let a = if fade > 0.0 {
            (el / fade).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let p = |def: Ease| ease_fn(e.ease.unwrap_or(def), u);
        let mut fx = Fx { alpha: a, ..NO_FX };
        match e.kind {
            EnterKind::None => {}
            EnterKind::Pop => {
                fx.sc = match e.ease {
                    None => keyed(POP_KEYS, u),
                    Some(ez) => 0.84 + 0.16 * ease_fn(ez, u),
                }
            }
            EnterKind::Bounce => fx.sc = keyed(BOUNCE_KEYS, u),
            EnterKind::Fade => fx.alpha = p(Ease::Linear).clamp(0.0, 1.0),
            EnterKind::SlideUp => fx.dy = self.reach.v * (1.0 - p(Ease::Out)),
            EnterKind::SlideDown => fx.dy = -self.reach.v * (1.0 - p(Ease::Out)),
            EnterKind::SlideLeft => fx.dx = self.reach.h * (1.0 - p(Ease::Out)),
            EnterKind::SlideRight => fx.dx = -self.reach.h * (1.0 - p(Ease::Out)),
            EnterKind::Zoom => fx.sc = 0.6 + 0.4 * p(Ease::Out),
            EnterKind::Blur => {
                let q = p(Ease::Out);
                fx.blur = self.reach.blur * (1.0 - q).max(0.0);
                fx.alpha = q.clamp(0.0, 1.0);
            }
            EnterKind::Drop => fx.dy = -self.reach.drop * (1.0 - p(Ease::Back)),
        }
        fx
    }

    fn exit_fx(&self, t: f64) -> Fx {
        let x = &self.exit;
        if x.kind == ExitKind::None || x.ms <= 0.0 {
            return NO_FX;
        }
        let v = ((t - (self.x1 - x.ms)) / x.ms).clamp(0.0, 1.0);
        if v <= 0.0 {
            return NO_FX;
        }
        let q = v * v;
        let mut fx = Fx {
            alpha: 1.0 - v,
            ..NO_FX
        };
        match x.kind {
            ExitKind::None | ExitKind::Fade => {}
            ExitKind::SlideUp => fx.dy = -self.reach.v * q,
            ExitKind::SlideDown => fx.dy = self.reach.v * q,
            ExitKind::Zoom => fx.sc = 1.0 - 0.4 * q,
            ExitKind::Blur => fx.blur = self.reach.blur * v,
        }
        fx
    }

    fn fx(&self, t: f64) -> Fx {
        let (a, b) = (self.enter_fx(t), self.exit_fx(t));
        Fx {
            sc: a.sc * b.sc,
            dx: a.dx + b.dx,
            dy: a.dy + b.dy,
            alpha: a.alpha * b.alpha,
            blur: a.blur + b.blur,
        }
    }

    /// Moments where the line's own motion changes shape.
    fn knots(&self, out: &mut Vec<f64>) {
        let e = &self.enter;
        if e.kind != EnterKind::None && e.ms > 0.0 {
            out.push(self.e0);
            out.push(self.e0 + e.ms);
            out.push(self.e0 + enter_fade_ms(e));
            let keys = match (e.kind, e.ease) {
                (EnterKind::Pop, None) => POP_KEYS,
                (EnterKind::Bounce, _) => BOUNCE_KEYS,
                _ => &[],
            };
            out.extend(keys.iter().map(|k| self.e0 + k.0 * e.ms));
        }
        let x = &self.exit;
        if x.kind != ExitKind::None && x.ms > 0.0 {
            out.push(self.x1 - x.ms);
            out.push(self.x1);
        }
    }
}

// ---- one thing to draw ------------------------------------------------------------

/// A word's looks over time.
#[derive(Debug, Clone)]
struct WordFx {
    s: f64,
    e: f64,
    col: Track<3>,
    op: Track<1>,
    sc: Track<1>,
    blur: Track<1>,
    lift: Track<1>,
    rot: Track<1>,
    /// Stroke width and colour, glow size / strength / colour, opacity of the
    /// box behind the word.
    sw: Track<1>,
    scol: Track<3>,
    gs: Track<1>,
    gk: Track<1>,
    gcol: Track<3>,
    bo: Track<1>,
    bump: bool,
    /// The unswept colour, when the word is filled by a sweep.
    sweep: Option<Col>,
}

impl WordFx {
    fn pulse(&self, t: f64) -> f64 {
        if !self.bump {
            return 1.0;
        }
        let d = t - self.s;
        if d <= 0.0 || d >= BUMP_END_MS {
            1.0
        } else if d < BUMP_UP_MS {
            1.0 + (BUMP - 1.0) * d / BUMP_UP_MS
        } else {
            BUMP - (BUMP - 1.0) * (d - BUMP_UP_MS) / (BUMP_END_MS - BUMP_UP_MS)
        }
    }

    /// The one-value tracks, the flat ones first (an overshoot wins a tie).
    fn flats(&self) -> [&Track<1>; 9] {
        [
            &self.op, &self.sw, &self.gs, &self.gk, &self.bo, &self.sc, &self.blur, &self.lift,
            &self.rot,
        ]
    }

    fn colours(&self) -> [&Track<3>; 3] {
        [&self.col, &self.scol, &self.gcol]
    }

    fn knots(&self, out: &mut Vec<f64>) {
        let mut v = Vec::new();
        for t in self.colours() {
            for s in &t.segs {
                v.extend([s.t0, s.t1]);
            }
        }
        for t in self.flats() {
            for s in &t.segs {
                v.extend([s.t0, s.t1]);
            }
        }
        if self.bump {
            v.extend([self.s, self.s + BUMP_UP_MS, self.s + BUMP_END_MS]);
        }
        if self.sweep.is_some() {
            v.extend([self.s, self.e]);
        }
        out.extend_from_slice(&v);
    }
}

/// Alpha of the box behind a line (outline slot) and of its shadow.
#[derive(Debug, Clone, Copy)]
struct BoxAlpha {
    outline: u8,
    shadow: Option<u8>,
}

/// What an item draws.
#[derive(Debug, Clone, Copy)]
enum Role {
    /// The word itself (also the libass box text: see `Item::boxed`).
    Text,
    /// The word's shadow: a blurred, offset copy in the shadow colour.
    Shadow(Shadow),
    /// The word's glow: a bordered, blurred copy in the glow colour.
    Glow,
    /// A drawn box (px), whose opacity comes from `src`.
    Box { col: Col, opacity: f64, src: BoxSrc },
}

/// Where a drawn box takes its opacity from.
#[derive(Debug, Clone, Copy, PartialEq)]
enum BoxSrc {
    /// Its own opacity, with the line.
    Line,
    /// Its own opacity, with the word's.
    Word,
    /// The word's active-box track.
    Active,
}

struct Item {
    style: String,
    text: String,
    /// Extra letter spacing (px): the caption's, or what an invisible box text
    /// needs to span the slots.
    spacing: f64,
    w0: f64,
    w1: f64,
    base: (f64, f64),
    centre: (f64, f64),
    line: LineWin,
    word: Option<WordFx>,
    boxed: Option<BoxAlpha>,
    font_px: f64,
    role: Role,
    layer: u8,
    /// Offset of what is drawn from `base` (px at the item's own scale, turned
    /// with it): where the ink is, rather than where the line box is.
    anchor: (f64, f64),
    /// Screen offset (px at the line's scale): a shadow's.
    shift: (f64, f64),
    /// The block's tilt (degrees, clockwise).
    tilt: f64,
    /// Say the stroke on every event (the Look sets one).
    stroke_tags: bool,
}

impl Item {
    #[allow(clippy::too_many_arguments)]
    fn plain(
        style: &str,
        text: String,
        (w0, w1): (f64, f64),
        base: (f64, f64),
        centre: (f64, f64),
        line: LineWin,
        word: Option<WordFx>,
        font_px: f64,
    ) -> Item {
        Item {
            style: style.to_string(),
            text,
            spacing: 0.0,
            w0,
            w1,
            base,
            centre,
            line,
            word,
            boxed: None,
            font_px,
            role: Role::Text,
            layer: 0,
            anchor: (0.0, 0.0),
            shift: (0.0, 0.0),
            tilt: 0.0,
            stroke_tags: false,
        }
    }
}

/// Everything about a drawn item at one moment.
#[derive(Debug, Clone, Copy)]
struct Frame {
    x: f64,
    y: f64,
    fsc: f64,
    rot: f64,
    blur: f64,
    alpha: f64,
    /// The line's own opacity (entrance and exit), apart from the word's.
    lalpha: f64,
    col: Col,
    /// Stroke width and colour, glow size / strength / colour, active-box opacity.
    sw: f64,
    scol: Col,
    gs: f64,
    gk: f64,
    gcol: Col,
    bo: f64,
}

impl Item {
    fn frame(&self, t: f64) -> Frame {
        let lf = self.line.fx(t);
        let (col, op, sc, blur, lift, rot, sw, scol, gs, gk, gcol, bo) = match &self.word {
            Some(w) => (
                w.col.eval(t),
                w.op.eval(t)[0],
                w.sc.eval(t)[0] * w.pulse(t),
                w.blur.eval(t)[0],
                w.lift.eval(t)[0],
                w.rot.eval(t)[0],
                w.sw.eval(t)[0],
                w.scol.eval(t),
                w.gs.eval(t)[0],
                w.gk.eval(t)[0],
                w.gcol.eval(t),
                w.bo.eval(t)[0],
            ),
            None => (
                [0.0; 3], 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, [0.0; 3], 0.0, 0.0, [0.0; 3], 0.0,
            ),
        };
        let (cx, cy) = self.centre;
        let (bx, by) = self.base;
        let mut x = cx + (bx - cx) * lf.sc + lf.dx;
        let mut y = cy + (by - lift * self.font_px - cy) * lf.sc + lf.dy;
        let turn = rot + self.tilt;
        let scale = sc * lf.sc;
        if self.anchor != (0.0, 0.0) {
            let (ax, ay) = (self.anchor.0 * scale, self.anchor.1 * scale);
            let th = turn.to_radians();
            x += ax * th.cos() - ay * th.sin();
            y += ax * th.sin() + ay * th.cos();
        }
        x += self.shift.0 * lf.sc;
        y += self.shift.1 * lf.sc;
        Frame {
            x,
            y,
            fsc: scale * 100.0,
            // Positive rotates clockwise; ASS turns counter-clockwise.
            rot: -turn,
            blur: blur + lf.blur,
            alpha: op * lf.alpha,
            lalpha: lf.alpha,
            col,
            sw,
            scol,
            gs,
            gk,
            gcol,
            bo,
        }
    }

    /// The tags that depend on the frame: scale, rotation, blur, opacity,
    /// fill colour, border width and border colour (these last two are empty
    /// where the style's own stay).
    fn props(&self, f: &Frame, colour: Option<Col>) -> [String; 7] {
        let fsc = n1(f.fsc);
        let alpha_of = |op: f64| format!("\\alpha&H{:02X}&", alpha_byte(op));
        let (blur, alpha, fill, bord, edge) = match &self.role {
            Role::Text => {
                let alpha = match &self.boxed {
                    Some(b) => {
                        let a0 = 255.0 - b.outline as f64;
                        let a = 255.0 - (a0 * f.alpha).round();
                        let mut s = format!("\\3a&H{:02X}&", a.clamp(0.0, 255.0) as u8);
                        if let Some(sh) = b.shadow {
                            let a = 255.0 - ((255.0 - sh as f64) * f.alpha).round();
                            s.push_str(&format!("\\4a&H{:02X}&", a.clamp(0.0, 255.0) as u8));
                        }
                        s
                    }
                    None => alpha_of(f.alpha),
                };
                let (bord, edge) = if self.stroke_tags {
                    (format!("\\bord{}", n1(f.sw)), col_tag("3c", f.scol))
                } else {
                    (String::new(), String::new())
                };
                (
                    f.blur,
                    alpha,
                    colour.map_or_else(String::new, |c| col_tag("1c", c)),
                    bord,
                    edge,
                )
            }
            Role::Shadow(sh) => (
                sh.blur + f.blur,
                alpha_of(sh.opacity * f.alpha),
                col_tag("1c", sh.col),
                format!("\\bord{}\\shad0", n1(f.sw)),
                col_tag("3c", sh.col),
            ),
            Role::Glow => (
                f.gs * GLOW_BLUR + f.blur,
                alpha_of(f.gk * f.alpha),
                col_tag("1c", f.gcol),
                format!("\\bord{}\\shad0", n1(f.sw + f.gs * GLOW_BORD)),
                col_tag("3c", f.gcol),
            ),
            Role::Box { col, opacity, src } => {
                let op = match src {
                    BoxSrc::Line => opacity * f.lalpha,
                    BoxSrc::Word => opacity * f.alpha,
                    BoxSrc::Active => f.bo * f.lalpha,
                };
                (
                    f.blur,
                    alpha_of(op),
                    col_tag("1c", *col),
                    "\\bord0\\shad0".to_string(),
                    String::new(),
                )
            }
        };
        [
            format!("\\fscx{fsc}\\fscy{fsc}"),
            format!("\\frz{}", n1(f.rot)),
            format!("\\blur{}", n1(blur.max(0.0))),
            alpha,
            fill,
            bord,
            edge,
        ]
    }
}

/// Is a prop string the tag's resting value (so it can be left out)?
fn is_rest(i: usize, p: &str, boxed: bool) -> bool {
    match i {
        0 => p == "\\fscx100\\fscy100",
        1 => p == "\\frz0",
        2 => p == "\\blur0",
        3 => !boxed && p == "\\alpha&H00&",
        _ => false,
    }
}

fn near(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

fn cs(ms: f64) -> i64 {
    (ms / 10.0).round() as i64
}

fn stamp_cs(c: i64) -> String {
    let c = c.max(0);
    format!(
        "{}:{:02}:{:02}.{:02}",
        c / 360000,
        (c % 360000) / 6000,
        (c % 6000) / 100,
        c % 100
    )
}

/// What a `\t` carries: the time window and the tags it sets.
struct Tween {
    t0: i64,
    t1: i64,
    accel: f64,
    tags: String,
}

impl Tween {
    fn text(&self) -> String {
        if (self.accel - 1.0).abs() < 1e-9 {
            format!("\\t({},{},{})", self.t0, self.t1, self.tags)
        } else {
            format!(
                "\\t({},{},{},{})",
                self.t0,
                self.t1,
                nacc(self.accel),
                self.tags
            )
        }
    }
}

enum Span {
    /// One event, tags animate inside it.
    Run { a: f64, b: f64, cuts: Vec<f64> },
    /// One 40 ms slice that moves on a straight line.
    Slice { a: f64, b: f64 },
}

impl Item {
    fn knots(&self) -> Vec<f64> {
        let mut k = vec![self.w0, self.w1];
        self.line.knots(&mut k);
        if let Some(w) = &self.word {
            w.knots(&mut k);
        }
        let (a, b) = (cs(self.w0) as f64 * 10.0, cs(self.w1) as f64 * 10.0);
        let mut k: Vec<f64> = k
            .into_iter()
            .map(|t| cs(t) as f64 * 10.0)
            .filter(|t| *t >= a && *t <= b)
            .collect();
        k.sort_by(|x, y| x.total_cmp(y));
        k.dedup();
        k
    }

    fn moves(&self, a: f64, b: f64) -> bool {
        let pts = [a + 0.001, (a + b) / 2.0, b - 0.001].map(|t| self.frame(t));
        pts.iter()
            .any(|p| !near(p.x, pts[0].x, 0.05) || !near(p.y, pts[0].y, 0.05))
    }

    fn line_steady(&self, a: f64, b: f64) -> bool {
        let pts = [a + 0.001, (a + b) / 2.0, b - 0.001].map(|t| self.line.fx(t));
        pts.iter().all(|p| {
            near(p.sc, pts[0].sc, 1e-6)
                && near(p.dx, pts[0].dx, 1e-6)
                && near(p.dy, pts[0].dy, 1e-6)
                && near(p.alpha, pts[0].alpha, 1e-6)
                && near(p.blur, pts[0].blur, 1e-6)
        })
    }

    /// The single word ramp spanning exactly `a..b`, if that is all there is.
    fn exact_ease(&self, a: f64, b: f64) -> Option<Ease> {
        let w = self.word.as_ref()?;
        if w.bump && a < w.s + BUMP_END_MS && b > w.s {
            return None;
        }
        if !self.line_steady(a, b) {
            return None;
        }
        let mut found = None;
        let mut hit = |s: &Seg<1>| {
            if near(s.t0, a, 5.0) && near(s.t1, b, 5.0) {
                found = Some(s.ease);
            }
        };
        for t in w.flats() {
            t.segs.iter().for_each(&mut hit);
        }
        for t in w.colours() {
            for s in &t.segs {
                if near(s.t0, a, 5.0) && near(s.t1, b, 5.0) {
                    found = found.or(Some(s.ease));
                }
            }
        }
        found
    }

    /// Is everything about the item close to a straight line over `a..b`?
    fn straight(&self, a: f64, b: f64) -> bool {
        let (f0, f1) = (self.frame(a + 0.001), self.frame(b - 0.001));
        [0.25, 0.5, 0.75].iter().all(|&u| {
            let f = self.frame(a + (b - a) * u);
            let lerp = |x0: f64, x1: f64| x0 + (x1 - x0) * u;
            near(f.x, lerp(f0.x, f1.x), 0.2)
                && near(f.y, lerp(f0.y, f1.y), 0.2)
                && near(f.fsc, lerp(f0.fsc, f1.fsc), 0.4)
                && near(f.rot, lerp(f0.rot, f1.rot), 0.2)
                && near(
                    f.blur.max(0.0),
                    lerp(f0.blur.max(0.0), f1.blur.max(0.0)),
                    0.15,
                )
                && near(f.alpha, lerp(f0.alpha, f1.alpha), 0.01)
                && near(f.sw, lerp(f0.sw, f1.sw), 0.1)
                && near(f.gs, lerp(f0.gs, f1.gs), 0.3)
                && near(f.gk, lerp(f0.gk, f1.gk), 0.01)
                && near(f.bo, lerp(f0.bo, f1.bo), 0.01)
                && (0..3).all(|i| near(f.col[i], lerp(f0.col[i], f1.col[i]), 1.5))
                && (0..3).all(|i| near(f.scol[i], lerp(f0.scol[i], f1.scol[i]), 1.5))
                && (0..3).all(|i| near(f.gcol[i], lerp(f0.gcol[i], f1.gcol[i]), 1.5))
        })
    }

    /// Cut points after `a` up to `b` (on the centisecond), so each piece is
    /// near a straight line or no longer than `GRID_MS`.
    fn cuts(&self, a: f64, b: f64, out: &mut Vec<f64>) {
        let m = ((a + b) / 20.0).round() * 10.0;
        if b - a <= GRID_MS || m <= a || m >= b || self.straight(a, b) {
            out.push(b);
        } else {
            self.cuts(a, m, out);
            self.cuts(m, b, out);
        }
    }

    /// Cut the window into events: straight runs, and slices where it moves.
    fn spans(&self) -> Vec<Span> {
        let knots = self.knots();
        let mut spans: Vec<Span> = Vec::new();
        let mut run: Option<(f64, f64, Vec<f64>, Frame)> = None;
        let flush = |run: &mut Option<(f64, f64, Vec<f64>, Frame)>, spans: &mut Vec<Span>| {
            if let Some((a, b, cuts, _)) = run.take() {
                spans.push(Span::Run { a, b, cuts });
            }
        };
        for w in knots.windows(2) {
            let (a, b) = (w[0], w[1]);
            if b - a < 10.0 {
                continue;
            }
            if self.moves(a, b) {
                flush(&mut run, &mut spans);
                let mut at = vec![a];
                self.cuts(a, b, &mut at);
                for w in at.windows(2) {
                    spans.push(Span::Slice { a: w[0], b: w[1] });
                }
                continue;
            }
            let here = self.frame((a + b) / 2.0);
            let joins = matches!(&run, Some((_, _, _, last))
                if near(last.x, here.x, 0.05) && near(last.y, here.y, 0.05));
            if let (true, Some((_, rb, cuts, last))) = (joins, run.as_mut()) {
                cuts.push(a);
                *rb = b;
                *last = here;
            } else {
                flush(&mut run, &mut spans);
                run = Some((a, b, Vec::new(), here));
            }
        }
        flush(&mut run, &mut spans);
        spans
    }

    /// How the word's colour is shown over `a..b`.
    fn colour_mode(&self, a: f64, b: f64) -> ColourMode {
        if !matches!(self.role, Role::Text) {
            return ColourMode::Fixed(None);
        }
        match self.word.as_ref().and_then(|w| w.sweep.map(|u| (w, u))) {
            Some((w, u)) if b > w.s && a < w.e => ColourMode::Sweep(u, w.s, w.e),
            Some((w, u)) if b <= w.s => ColourMode::Fixed(Some(u)),
            _ => ColourMode::Fixed(None),
        }
    }

    fn colour_at(&self, mode: &ColourMode, f: &Frame) -> Option<Col> {
        self.word.as_ref()?;
        match mode {
            ColourMode::Fixed(Some(u)) => Some(*u),
            _ => Some(f.col),
        }
    }

    fn head(&self, a: f64, f: &Frame, mode: &ColourMode, pos: String) -> String {
        let mut t = format!("\\an5\\q2{pos}");
        if self.spacing.abs() > 0.05 {
            t.push_str(&format!("\\fsp{}", n1(self.spacing)));
        }
        let boxed = self.boxed.is_some();
        let colour = self.colour_at(mode, f);
        for (i, p) in self.props(f, colour).iter().enumerate() {
            if !p.is_empty() && !is_rest(i, p, boxed) {
                t.push_str(p);
            }
        }
        if matches!(self.role, Role::Box { .. }) {
            t.push_str("\\p4");
        }
        if let ColourMode::Sweep(u, s, e) = mode {
            t.push_str(&col_tag("2c", *u));
            let skip = cs(s - a);
            let dur = cs(e - s).max(1);
            if skip != 0 {
                t.push_str(&format!("\\k{skip}"));
            }
            t.push_str(&format!("\\kf{dur}"));
        }
        t
    }

    fn event(&self, a: f64, b: f64, text: &str) -> String {
        format!(
            "Dialogue: {},{},{},{},,0,0,0,,{text}\n",
            self.layer,
            stamp_cs(cs(a)),
            stamp_cs(cs(b)),
            self.style
        )
    }

    /// Tags that differ between two frames, as one `\t`.
    fn diff(&self, f0: &Frame, f1: &Frame, m0: &ColourMode, m1: &ColourMode) -> String {
        let p0 = self.props(f0, self.colour_at(m0, f0));
        let p1 = self.props(f1, self.colour_at(m1, f1));
        p0.iter()
            .zip(&p1)
            .filter(|(a, b)| a != b)
            .map(|(_, b)| b.as_str())
            .collect()
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for span in self.spans() {
            match span {
                Span::Slice { a, b } => {
                    let (fa, fb) = (self.frame(a), self.frame(b - 0.001));
                    let mode = self.colour_mode(a, b);
                    let len = (b - a).round() as i64;
                    let pos = format!(
                        "\\move({},{},{},{},0,{len})",
                        n1(fa.x),
                        n1(fa.y),
                        n1(fb.x),
                        n1(fb.y)
                    );
                    let mut head = self.head(a, &fa, &mode, pos);
                    let tags = self.diff(&fa, &fb, &mode, &mode);
                    if !tags.is_empty() {
                        head.push_str(
                            &Tween {
                                t0: 0,
                                t1: len,
                                accel: 1.0,
                                tags,
                            }
                            .text(),
                        );
                    }
                    out.push_str(&self.event(a, b, &format!("{{{head}}}{}", self.text)));
                }
                Span::Run { a, b, cuts } => {
                    let fa = self.frame(a);
                    let mode = self.colour_mode(a, b);
                    let pos = format!("\\pos({},{})", n1(fa.x), n1(fa.y));
                    let mut head = self.head(a, &fa, &mode, pos);
                    let mut bounds = vec![a];
                    bounds.extend(cuts.iter().copied());
                    bounds.push(b);
                    let mut tweens: Vec<Tween> = Vec::new();
                    for (i, w) in bounds.windows(2).enumerate() {
                        let (ia, ib) = (w[0], w[1]);
                        let rel = |t: f64| (t - a).round() as i64;
                        if i > 0 {
                            // A step at the cut.
                            let (before, after) = (self.frame(ia - 0.001), self.frame(ia));
                            let tags = self.diff(&before, &after, &mode, &mode);
                            if !tags.is_empty() {
                                tweens.push(Tween {
                                    t0: rel(ia),
                                    t1: rel(ia),
                                    accel: 1.0,
                                    tags,
                                });
                            }
                        }
                        let (f0, f1) = (self.frame(ia + 0.001), self.frame(ib - 0.001));
                        if self.diff(&f0, &f1, &mode, &mode).is_empty()
                            && self
                                .diff(&f0, &self.frame((ia + ib) / 2.0), &mode, &mode)
                                .is_empty()
                        {
                            continue;
                        }
                        match self.exact_ease(ia, ib) {
                            Some(ez) => self.exact(ia, ib, ez, &mode, &rel, &mut tweens),
                            None => self.sampled(ia, ib, &mode, &rel, &mut tweens),
                        }
                    }
                    tweens.sort_by_key(|t| t.t0);
                    for t in &tweens {
                        head.push_str(&t.text());
                    }
                    out.push_str(&self.event(a, b, &format!("{{{head}}}{}", self.text)));
                }
            }
        }
        out
    }

    /// One word ramp, written with its ease (back is overshoot then settle).
    fn exact(
        &self,
        a: f64,
        b: f64,
        ez: Ease,
        mode: &ColourMode,
        rel: &dyn Fn(f64) -> i64,
        out: &mut Vec<Tween>,
    ) {
        let (f0, f1) = (self.frame(a + 0.001), self.frame(b - 0.001));
        let prop = |f: &Frame| self.props(f, self.colour_at(mode, f));
        let (p0, p1) = (prop(&f0), prop(&f1));
        // Which prop strings change: 0 scale, 1 rot, 2 blur, 3 alpha, 4 colour.
        let changed = |i: usize| p0[i] != p1[i];
        if ez != Ease::Back {
            let tags: String = (0..7)
                .filter(|&i| changed(i))
                .map(|i| p1[i].as_str())
                .collect();
            if !tags.is_empty() {
                out.push(Tween {
                    t0: rel(a),
                    t1: rel(b),
                    accel: accel(ez),
                    tags,
                });
            }
            return;
        }
        // Back: the colour and opacity just ease out; scale, tilt and blur
        // run past the target and settle.
        let flat_tags: String = [3usize, 4, 5, 6]
            .iter()
            .filter(|&&i| changed(i))
            .map(|&i| p1[i].as_str())
            .collect();
        if !flat_tags.is_empty() {
            out.push(Tween {
                t0: rel(a),
                t1: rel(b),
                accel: accel(Ease::Out),
                tags: flat_tags,
            });
        }
        if [0usize, 1, 2].iter().any(|&i| changed(i)) {
            let peak = |x0: f64, x1: f64| x0 + (x1 - x0) * (1.0 + BACK_PEAK);
            let fp = Frame {
                fsc: peak(f0.fsc, f1.fsc),
                rot: peak(f0.rot, f1.rot),
                blur: peak(f0.blur, f1.blur).max(0.0),
                ..f1
            };
            let pp = prop(&fp);
            let mid = a + (b - a) * BACK_AT;
            let up: String = [0usize, 1, 2]
                .iter()
                .filter(|&&i| changed(i))
                .map(|&i| pp[i].as_str())
                .collect();
            let down: String = [0usize, 1, 2]
                .iter()
                .filter(|&&i| changed(i))
                .map(|&i| p1[i].as_str())
                .collect();
            out.push(Tween {
                t0: rel(a),
                t1: rel(mid),
                accel: 0.4,
                tags: up,
            });
            out.push(Tween {
                t0: rel(mid),
                t1: rel(b),
                accel: 1.0,
                tags: down,
            });
        }
    }

    /// A stretch where several things move at once: straight pieces on a grid,
    /// pieces that continue each other joined into one.
    fn sampled(
        &self,
        a: f64,
        b: f64,
        mode: &ColourMode,
        rel: &dyn Fn(f64) -> i64,
        out: &mut Vec<Tween>,
    ) {
        let mut times = vec![a];
        self.cuts(a, b, &mut times);
        let at = |t: f64| self.frame(if t >= b { b - 0.001 } else { t + 0.001 });
        let nums = |f: &Frame| {
            [
                f.fsc,
                f.rot,
                f.blur.max(0.0),
                f.alpha,
                f.col[0],
                f.col[1],
                f.col[2],
                f.sw,
                f.gs,
                f.gk,
                f.bo,
                f.scol[0],
                f.scol[1],
                f.scol[2],
                f.gcol[0],
                f.gcol[1],
                f.gcol[2],
                f.lalpha,
            ]
        };
        let frames: Vec<Frame> = times.iter().map(|&t| at(t)).collect();
        // Join pieces that continue on the same straight line.
        let mut keep = vec![0usize];
        for j in 1..times.len() - 1 {
            let (p, q, r) = (*keep.last().unwrap_or(&0), j, j + 1);
            let (np, nq, nr) = (nums(&frames[p]), nums(&frames[q]), nums(&frames[r]));
            let straight = (0..np.len()).all(|i| {
                let s1 = (nq[i] - np[i]) / (times[q] - times[p]);
                let s2 = (nr[i] - nq[i]) / (times[r] - times[q]);
                (s1 - s2).abs() <= 1e-4 * (1.0 + s1.abs().max(s2.abs()))
            });
            if !straight {
                keep.push(j);
            }
        }
        keep.push(times.len() - 1);
        for w in keep.windows(2) {
            let (i0, i1) = (w[0], w[1]);
            let tags = self.diff(&frames[i0], &frames[i1], mode, mode);
            if !tags.is_empty() {
                out.push(Tween {
                    t0: rel(times[i0]),
                    t1: rel(times[i1]),
                    accel: 1.0,
                    tags,
                });
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ColourMode {
    /// The word's own colour (or, before a sweep, the unswept colour).
    Fixed(Option<Col>),
    /// Unswept colour, sweep start, sweep end.
    Sweep(Col, f64, f64),
}

// ---- layout -------------------------------------------------------------------

/// Where the caption block sits and how it is typeset.
pub struct Geo {
    pub pw: u32,
    pub ph: u32,
    /// Canvas scale of the 1080-wide design.
    pub k: f64,
    pub alignment: u32,
    pub margin_v: u32,
    pub cap_l: u32,
    pub cap_r: u32,
    /// An explicit centre (output pixels), which wins over the alignment.
    pub place: Option<(i64, i64)>,
    pub font: &'static str,
    pub font_px: u32,
}

/// Names and box alphas of the styles the events use.
pub struct Draw<'a> {
    pub text_style: &'a str,
    /// Set when the style has a box: its style name, the box's alpha byte in
    /// the outline slot and the shadow's alpha when it casts one.
    pub box_style: Option<(&'a str, u8, Option<u8>)>,
    pub caps: bool,
}

/// Break a line's words into rows no wider than `avail`, as evenly as the
/// least number of rows allows.
fn break_rows(w: &[f64], sp: f64, avail: f64) -> Vec<Range<usize>> {
    let n = w.len();
    // Greedy for the row count.
    let mut rows = 1;
    let mut cur = 0.0;
    for (i, x) in w.iter().enumerate() {
        let add = if i == 0 || cur == 0.0 { *x } else { sp + x };
        if cur > 0.0 && cur + add > avail {
            rows += 1;
            cur = *x;
        } else {
            cur += add;
        }
    }
    if rows == 1 || n < 2 {
        return std::iter::once(0..n).collect();
    }
    balanced_rows(w, sp, rows.min(n), avail)
}

/// Break a line's words into exactly `rows` rows: the narrowest widest row
/// wins (rows wider than `avail` only when nothing fits), then the wider top row.
fn balanced_rows(w: &[f64], sp: f64, rows: usize, avail: f64) -> Vec<Range<usize>> {
    let n = w.len();
    let width = |r: Range<usize>| -> f64 {
        r.clone().map(|i| w[i]).sum::<f64>() + sp * (r.len().saturating_sub(1)) as f64
    };
    let rows = rows.clamp(1, n.max(1));
    if rows == 1 || n < 2 {
        return std::iter::once(0..n).collect();
    }
    if n > 22 {
        // Far more words than any caption or headline has: an even split.
        let cuts: Vec<usize> = (0..=rows).map(|i| i * n / rows).collect();
        return cuts.windows(2).map(|c| c[0]..c[1]).collect();
    }
    let mut best: Option<(f64, Vec<usize>)> = None;
    for mask in 0u32..(1 << (n - 1)) {
        if mask.count_ones() as usize != rows - 1 {
            continue;
        }
        let mut cuts = vec![0usize];
        cuts.extend((1..n).filter(|i| mask & (1 << (i - 1)) != 0));
        cuts.push(n);
        let ws: Vec<f64> = cuts.windows(2).map(|c| width(c[0]..c[1])).collect();
        let widest = ws.iter().cloned().fold(0.0, f64::max);
        let fits = ws.iter().all(|x| *x <= avail + 0.5);
        let key = widest + if fits { 0.0 } else { 1e6 };
        let better = match &best {
            None => true,
            Some((k, _)) => key < *k - 0.5,
        };
        if better {
            best = Some((key, cuts));
        }
    }
    let cuts = best.map_or_else(|| vec![0, n], |b| b.1);
    cuts.windows(2).map(|c| c[0]..c[1]).collect()
}

/// A word to lay out and time.
struct W {
    text: String,
    s: f64,
    e: f64,
    kw: bool,
    /// Advance width, with the letter spacing after every character.
    width: f64,
}

/// A drawing (`p4` units) of a box `w` by `h` px with corners of radius `r`
/// px, its top-left corner at the origin.
fn rounded_box(w: f64, h: f64, r: f64) -> String {
    const KAPPA: f64 = 0.552_284_749_8;
    let u = |v: f64| (v * DRAW_UNIT).round() as i64;
    let (w, h) = (w.max(1.0), h.max(1.0));
    let r = r.clamp(0.0, w.min(h) / 2.0);
    if r < 0.05 {
        return format!("m 0 0 l {} 0 {} {} 0 {}", u(w), u(w), u(h), u(h));
    }
    let c = r * KAPPA;
    let pt = |x: f64, y: f64| format!("{} {}", u(x), u(y));
    [
        format!("m {}", pt(r, 0.0)),
        format!("l {}", pt(w - r, 0.0)),
        format!("b {} {} {}", pt(w - r + c, 0.0), pt(w, r - c), pt(w, r)),
        format!("l {}", pt(w, h - r)),
        format!(
            "b {} {} {}",
            pt(w, h - r + c),
            pt(w - r + c, h),
            pt(w - r, h)
        ),
        format!("l {}", pt(r, h)),
        format!(
            "b {} {} {}",
            pt(r - c, h),
            pt(0.0, h - r + c),
            pt(0.0, h - r)
        ),
        format!("l {}", pt(0.0, r)),
        format!("b {} {} {}", pt(0.0, r - c), pt(r - c, 0.0), pt(r, 0.0)),
    ]
    .join(" ")
}

/// The colour a glow takes when none is given: the letters' own, or white when
/// they are too dark to glow.
fn glow_default(fill: Col) -> Col {
    let luma = 0.2126 * fill[0] + 0.7152 * fill[1] + 0.0722 * fill[2];
    if luma < 70.0 {
        [255.0; 3]
    } else {
        fill
    }
}

/// One state's glow: the layers of the Look over each other (a layer's unset
/// fields keep the one below), the defaults under all of them. No layer, no
/// glow.
pub fn resolve_glow(layers: &[Option<&GlowLook>], fill: Col, k: f64) -> Glow {
    if layers.iter().all(|l| l.is_none()) {
        return Glow {
            col: fill,
            ..NO_GLOW
        };
    }
    let pick =
        |f: &dyn Fn(&GlowLook) -> Option<f64>| layers.iter().flatten().rev().find_map(|g| f(g));
    Glow {
        col: layers
            .iter()
            .flatten()
            .rev()
            .find_map(|g| g.color)
            .map_or_else(|| glow_default(fill), col_of),
        size: pick(&|g| g.size).unwrap_or(GLOW_SIZE) * k,
        strength: pick(&|g| g.strength).unwrap_or(GLOW_STRENGTH),
    }
}

/// Does a word keep its shape, size, opacity and stroke through all three of
/// its looks (so its copies need not follow it word by word)?
fn same_shape(l: &[WordLook; 3]) -> bool {
    l.iter().all(|x| {
        x.scale == 1.0
            && x.lift == 0.0
            && x.rotate == 0.0
            && x.blur == l[0].blur
            && x.opacity == l[0].opacity
            && x.stroke == l[0].stroke
    })
}

/// The part of a word's window in which something that is on in some of its
/// looks can be seen: all of it when it is on before or after the word is
/// spoken, else from the word's start to the end of its release.
fn live_window(win: (f64, f64), on: [bool; 3], start: f64, release_end: f64) -> Option<(f64, f64)> {
    if !on.iter().any(|x| *x) {
        return None;
    }
    let a = if on[0] { win.0 } else { win.0.max(start) };
    let b = if on[2] { win.1 } else { win.1.min(release_end) };
    (b - a >= 10.0).then_some((a, b))
}

/// A word that never changes: the same tracks as a word's, with nothing to
/// ramp, for the copies written once per row.
fn steady_fx(l: &WordLook, s: f64, e: f64) -> WordFx {
    WordFx {
        s,
        e,
        col: Track::new(l.color),
        op: Track::new([l.opacity]),
        sc: Track::new([1.0]),
        blur: Track::new([l.blur]),
        lift: Track::new([0.0]),
        rot: Track::new([0.0]),
        sw: Track::new([l.stroke.w]),
        scol: Track::new(l.stroke.col),
        gs: Track::new([l.glow.size]),
        gk: Track::new([l.glow.strength]),
        gcol: Track::new(l.glow.col),
        bo: Track::new([0.0]),
        bump: false,
        sweep: None,
    }
}

/// Split caption blocks so that none needs more than `max_rows` rows in the
/// room there is (a word wider than the room still gets a block of its own).
/// Sized for the biggest look a word takes, so a block that fits stays fitting.
pub fn fit_rows(
    cfg: &Cfg,
    geo: &Geo,
    caps: bool,
    lines: Vec<Vec<Word>>,
    max_rows: usize,
) -> Vec<Vec<Word>> {
    let Some(face) = metrics::face(geo.font) else {
        return lines;
    };
    if cfg.mode == WordMode::Single {
        return lines;
    }
    let size = geo.font_px as f64;
    let sp = face.width(" ", size) + cfg.spacing;
    let avail = (geo.pw as f64 - geo.cap_l as f64 - geo.cap_r as f64).max(40.0);
    let grow = [cfg.up.scale, cfg.act.scale, cfg.spk.scale]
        .iter()
        .fold(1.0f64, |m, s| m.max(*s))
        * cfg.kw_scale.unwrap_or(1.0).max(1.0);
    let width = |w: &Word| {
        let t = if caps {
            w.w.to_uppercase()
        } else {
            w.w.clone()
        };
        let t = t.replace(['{', '}', '\n', '\r'], "");
        (face.width(&t, size) + t.chars().count() as f64 * cfg.spacing) * grow
    };
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let mut cur: Vec<Word> = Vec::new();
        for w in line {
            cur.push(w);
            if cur.len() > 1 {
                let ws: Vec<f64> = cur.iter().map(width).collect();
                if break_rows(&ws, sp, avail).len() > max_rows {
                    if let Some(last) = cur.pop() {
                        out.push(std::mem::take(&mut cur));
                        cur.push(last);
                    }
                }
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

/// A row of a laid-out block.
struct Row {
    words: Range<usize>,
    /// Where it starts (px) and how wide it is, with room kept for the biggest
    /// look of each word; the middle of its line box.
    x0: f64,
    width: f64,
    cy: f64,
}

/// The ASS events (box, shadow, glow and word, all lines) for the word-level model.
pub fn events(
    cfg: &Cfg,
    geo: &Geo,
    draw: &Draw,
    lines: &[Vec<Word>],
    spans: &[(f64, f64)],
    offset: f64,
) -> String {
    events_with(cfg, geo, draw, lines, spans, offset, None)
}

/// [`events`], with the keyword words of the first line given (the headline
/// picks its accent word itself) instead of found by the caption rules.
#[allow(clippy::too_many_arguments)]
fn events_with(
    cfg: &Cfg,
    geo: &Geo,
    draw: &Draw,
    lines: &[Vec<Word>],
    spans: &[(f64, f64)],
    offset: f64,
    first_line_keywords: Option<&[bool]>,
) -> String {
    let Some(face) = metrics::face(geo.font) else {
        return String::new();
    };
    let size = geo.font_px as f64;
    let spc = cfg.spacing;
    let sp = face.width(" ", size) + spc;
    let pitch = size * cfg.line_gap;
    let (asc, desc) = face.line_box(size);
    let reach = Reach {
        v: SLIDE_V * geo.ph as f64,
        h: SLIDE_H * geo.pw as f64,
        drop: DROP_V * geo.ph as f64,
        blur: FX_BLUR * geo.k,
    };
    let avail = (geo.pw as f64 - geo.cap_l as f64 - geo.cap_r as f64).max(40.0);
    let bumps = cfg.bumps();
    let text_layer = if cfg.decor { LAYER_TEXT } else { 0 };
    let tilt = cfg.rotate;
    let mut out = String::new();
    let mut fresh = true;
    for (li, (line, &(t0, t1))) in lines.iter().zip(spans).enumerate() {
        let (l0, l1) = ((t0 - offset) * 1000.0, (t1 - offset) * 1000.0);
        let mut ws: Vec<W> = Vec::new();
        let mut kw_flags = Vec::new();
        for w in line {
            kw_flags.push(is_keyword(&w.w, fresh));
            fresh = ends_sentence(&w.w);
        }
        if let (0, Some(kw)) = (li, first_line_keywords) {
            if kw.len() == kw_flags.len() {
                kw_flags = kw.to_vec();
            }
        }
        for (w, kw) in line.iter().zip(&kw_flags) {
            let text = if draw.caps {
                w.w.to_uppercase()
            } else {
                w.w.clone()
            };
            let text = text.replace(['{', '}', '\n', '\r'], "");
            ws.push(W {
                width: face.width(&text, size) + text.chars().count() as f64 * spc,
                text,
                s: (w.s - offset) * 1000.0,
                e: (w.e - offset) * 1000.0,
                kw: *kw,
            });
        }
        // The looks of each word, and the room its biggest look needs.
        let looks: Vec<[WordLook; 3]> = ws.iter().map(|w| word_looks(cfg, w.kw)).collect();
        let slot: Vec<f64> = ws
            .iter()
            .zip(&looks)
            .map(|(w, l)| w.width * l.iter().map(|x| x.scale).fold(1.0, f64::max))
            .collect();
        let single = cfg.mode == WordMode::Single;
        let one_row = single || cfg.max_lines == Some(1);
        let rows: Vec<Range<usize>> = if one_row {
            std::iter::once(0..ws.len()).collect()
        } else if let Some(n) = cfg.force_rows {
            balanced_rows(&slot, sp, n, avail)
        } else {
            break_rows(&slot, sp, avail)
        };
        let nrows = if single { 1 } else { rows.len() };
        // The block: left/centre/right of the margins, or on the requested point.
        let col = if geo.place.is_some() {
            1
        } else {
            (geo.alignment.saturating_sub(1)) % 3
        };
        let area_cx = geo.cap_l as f64 + avail / 2.0;
        let (cx, left, right) = match geo.place {
            Some((px, _)) => (px as f64, px as f64, px as f64),
            None => (area_cx, geo.cap_l as f64, geo.pw as f64 - geo.cap_r as f64),
        };
        let block_h = (nrows - 1) as f64 * pitch + size;
        let top = match geo.place {
            Some((_, py)) => py as f64 - block_h / 2.0,
            None => match geo.alignment {
                1..=3 => geo.ph as f64 - geo.margin_v as f64 - block_h,
                7..=9 => geo.margin_v as f64,
                _ => (geo.ph as f64 - block_h) / 2.0,
            },
        };
        let row_w = |r: &Range<usize>| {
            r.clone().map(|i| slot[i]).sum::<f64>() + sp * r.len().saturating_sub(1) as f64
        };
        let block_w = rows.iter().map(row_w).fold(0.0, f64::max);
        let centre = (
            match col {
                0 => left + block_w / 2.0,
                2 => right - block_w / 2.0,
                _ => cx,
            },
            top + block_h / 2.0,
        );
        // The block turns about its middle.
        let rotp = |p: (f64, f64)| -> (f64, f64) {
            if tilt == 0.0 {
                return p;
            }
            let th = tilt.to_radians();
            let (dx, dy) = (p.0 - centre.0, p.1 - centre.1);
            (
                centre.0 + dx * th.cos() - dy * th.sin(),
                centre.1 + dx * th.sin() + dy * th.cos(),
            )
        };
        // Positions: (x, y) of each word's centre, before the tilt; and the rows.
        let mut pos = vec![(0.0, 0.0); ws.len()];
        let mut row_info: Vec<Row> = Vec::new();
        if single {
            let cy = top + size / 2.0;
            for (i, p) in pos.iter_mut().enumerate() {
                let x = match col {
                    0 => left + ws[i].width / 2.0,
                    2 => right - ws[i].width / 2.0,
                    _ => cx,
                };
                *p = (x, cy);
            }
        } else {
            let block_l = match col {
                0 => left,
                2 => right - block_w,
                _ => cx - block_w / 2.0,
            };
            for (ri, r) in rows.iter().enumerate() {
                let rw = row_w(r);
                let x0 = match cfg.align {
                    None => match col {
                        0 => left,
                        2 => right - rw,
                        _ => cx - rw / 2.0,
                    },
                    Some(Align::Left) => block_l,
                    Some(Align::Right) => block_l + block_w - rw,
                    Some(Align::Center) => block_l + (block_w - rw) / 2.0,
                };
                let cy = top + size / 2.0 + ri as f64 * pitch;
                let mut x = x0;
                for i in r.clone() {
                    pos[i] = (x + slot[i] / 2.0, cy);
                    x += slot[i] + sp;
                }
                row_info.push(Row {
                    words: r.clone(),
                    x0,
                    width: rw,
                    cy,
                });
            }
        }
        // When a word is shown: the whole line, from its own start, or until
        // the next word comes (a release is cut there).
        let tails: Vec<(f64, f64)> = ws.iter().map(|w| timeline(cfg, w)).collect();
        let win: Vec<(f64, f64)> = ws
            .iter()
            .enumerate()
            .map(|(i, w)| match cfg.mode {
                WordMode::All => (l0, l1),
                WordMode::Build => (w.s.max(l0), l1),
                WordMode::Single => {
                    // Up until the next word, in its spoken look; or until its
                    // release is over when that look is invisible.
                    let w0 = w.s.max(l0);
                    let next = ws.get(i + 1).map_or(l1, |n| n.s).min(l1);
                    let end = if looks[i][2].opacity <= 0.0 {
                        next.min(tails[i].1)
                    } else {
                        next
                    };
                    (w0, end.max(w0 + 10.0))
                }
            })
            .collect();
        let line_win = |w0: f64, w1: f64| LineWin {
            e0: w0,
            x1: w1,
            enter: cfg.enter,
            exit: cfg.exit,
            reach,
        };
        // The window the word's own line motion runs on.
        let lwin = |i: usize| {
            if single {
                line_win(win[i].0, win[i].1)
            } else {
                line_win(l0, l1)
            }
        };
        let bump: Vec<bool> = ws
            .iter()
            .map(|w| bumps && !single && w.kw && w.s - l0 >= cfg.enter.ms)
            .collect();
        let wfs: Vec<WordFx> = ws
            .iter()
            .enumerate()
            .map(|(i, w)| word_fx(cfg, w, looks[i], tails[i].0, tails[i].1, bump[i]))
            .collect();
        // Where the letters of a word and of a row really are inside their
        // line boxes: the side bearings, and the ink's height above and below
        // the baseline. A box hugs the ink, not the line box.
        let drawn_box = |w: f64, h: f64, radius: f64| rounded_box(w, h, radius * w.min(h) / 2.0);
        let band =
            |text: &str| -> (f64, f64) { face.ink_y(text, size).unwrap_or((0.72 * size, 0.0)) };
        let ink_oy = |(t, b): (f64, f64)| (asc - desc) / 2.0 - (t + b) / 2.0;
        let word_box = |i: usize, bnd: (f64, f64), pad: (f64, f64), radius: f64| {
            let (lsb, rsb) = face.ink_x(&ws[i].text, size);
            let w = ws[i].width - spc - lsb - rsb + 2.0 * (pad.0 + cfg.stroke_max);
            let h = (bnd.0 - bnd.1) + 2.0 * (pad.1 + cfg.stroke_max);
            (w, h, drawn_box(w, h, radius), (lsb - rsb) / 2.0)
        };
        // The ink band of the row a word is in (a word shown alone: its own).
        let row_band = |i: usize| -> (f64, f64) {
            match row_info.iter().find(|r| r.words.contains(&i)) {
                Some(r) if !single => band(
                    &r.words
                        .clone()
                        .map(|j| ws[j].text.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                _ => band(&ws[i].text),
            }
        };
        // Rows of words that keep their shape, so their shadow and glow can be
        // one event per row.
        let steady = |r: &Range<usize>, glow: bool| -> bool {
            single_free(cfg, single)
                && !r.is_empty()
                && r.clone().all(|i| {
                    let l = &looks[i];
                    let f = &looks[r.start][0];
                    same_shape(l)
                        && !bump[i]
                        && l[0].opacity == f.opacity
                        && l[0].blur == f.blur
                        && l[0].stroke == f.stroke
                        && (!glow || (l.iter().all(|x| x.glow == f.glow) && l[0].glow == f.glow))
                })
        };
        // Boxes first, so they sit under everything.
        if let Some((bname, outline, shadow)) = draw.box_style {
            let boxed = Some(BoxAlpha { outline, shadow });
            if single {
                for (i, w) in ws.iter().enumerate() {
                    let (w0, w1) = win[i];
                    let base = rotp(pos[i]);
                    out.push_str(
                        &Item {
                            boxed,
                            tilt,
                            ..Item::plain(
                                bname,
                                w.text.clone(),
                                (w0, w1),
                                base,
                                base,
                                line_win(w0, w1),
                                None,
                                size,
                            )
                        }
                        .render(),
                    );
                }
            } else {
                for row in &row_info {
                    let r = &row.words;
                    let text = r
                        .clone()
                        .map(|i| ws[i].text.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let chars = text.chars().count().max(1) as f64;
                    let adv = face.width(&text, size);
                    out.push_str(
                        &Item {
                            spacing: (row.width - adv) / chars,
                            boxed,
                            tilt,
                            ..Item::plain(
                                bname,
                                text,
                                (l0, l1),
                                rotp((row.x0 + row.width / 2.0, row.cy)),
                                centre,
                                line_win(l0, l1),
                                None,
                                size,
                            )
                        }
                        .render(),
                    );
                }
            }
        }
        // The box shape: one per row, or one per word following the word.
        if let Some(bx) = &cfg.boxfx {
            if bx.per_word || single {
                for i in 0..ws.len() {
                    let bnd = row_band(i);
                    let (_, _, text, ax) = word_box(i, bnd, (bx.pad_x, bx.pad_y), bx.radius);
                    out.push_str(
                        &Item {
                            role: Role::Box {
                                col: bx.col,
                                opacity: bx.opacity,
                                src: BoxSrc::Word,
                            },
                            layer: LAYER_BOX,
                            anchor: (ax, ink_oy(bnd)),
                            tilt,
                            ..Item::plain(
                                draw.text_style,
                                text,
                                win[i],
                                rotp(pos[i]),
                                if single { rotp(pos[i]) } else { centre },
                                lwin(i),
                                Some(wfs[i].clone()),
                                size,
                            )
                        }
                        .render(),
                    );
                }
            } else if bx.block {
                // One card around every row: from the left-most ink to the
                // right-most, from the top of the first row's ink to the bottom
                // of the last row's.
                let joined = |r: &Range<usize>| {
                    r.clone()
                        .map(|i| ws[i].text.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                let (mut l, mut rr) = (f64::INFINITY, f64::NEG_INFINITY);
                for row in &row_info {
                    let (first, last) = (row.words.start, row.words.end - 1);
                    let (lsb, _) = face.ink_x(&ws[first].text, size);
                    let (_, rsb) = face.ink_x(&ws[last].text, size);
                    l = l.min(pos[first].0 - (ws[first].width - spc) / 2.0 + lsb);
                    rr = rr.max(pos[last].0 + (ws[last].width - spc) / 2.0 - rsb);
                }
                if let (Some(first), Some(last)) = (row_info.first(), row_info.last()) {
                    let top_ink = first.cy + (asc - desc) / 2.0 - band(&joined(&first.words)).0;
                    let bottom_ink = last.cy + (asc - desc) / 2.0 - band(&joined(&last.words)).1;
                    let w = (rr - l) + 2.0 * (bx.pad_x + cfg.stroke_max);
                    let h = (bottom_ink - top_ink) + 2.0 * (bx.pad_y + cfg.stroke_max);
                    out.push_str(
                        &Item {
                            role: Role::Box {
                                col: bx.col,
                                opacity: bx.opacity,
                                src: BoxSrc::Line,
                            },
                            layer: LAYER_BOX,
                            tilt,
                            ..Item::plain(
                                draw.text_style,
                                drawn_box(w, h, bx.radius),
                                (l0, l1),
                                rotp(((l + rr) / 2.0, (top_ink + bottom_ink) / 2.0)),
                                centre,
                                line_win(l0, l1),
                                None,
                                size,
                            )
                        }
                        .render(),
                    );
                }
            } else {
                for row in &row_info {
                    let r = &row.words;
                    let joined = r
                        .clone()
                        .map(|i| ws[i].text.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let bnd = band(&joined);
                    let (first, last) = (r.start, r.end - 1);
                    let (lsb, _) = face.ink_x(&ws[first].text, size);
                    let (_, rsb) = face.ink_x(&ws[last].text, size);
                    let l = pos[first].0 - (ws[first].width - spc) / 2.0 + lsb;
                    let rr = pos[last].0 + (ws[last].width - spc) / 2.0 - rsb;
                    let w = (rr - l) + 2.0 * (bx.pad_x + cfg.stroke_max);
                    let h = (bnd.0 - bnd.1) + 2.0 * (bx.pad_y + cfg.stroke_max);
                    out.push_str(
                        &Item {
                            role: Role::Box {
                                col: bx.col,
                                opacity: bx.opacity,
                                src: BoxSrc::Line,
                            },
                            layer: LAYER_BOX,
                            anchor: (0.0, ink_oy(bnd)),
                            tilt,
                            ..Item::plain(
                                draw.text_style,
                                drawn_box(w, h, bx.radius),
                                (l0, l1),
                                rotp(((l + rr) / 2.0, row.cy)),
                                centre,
                                line_win(l0, l1),
                                None,
                                size,
                            )
                        }
                        .render(),
                    );
                }
            }
        }
        // The box behind the spoken word.
        if let Some(ab) = &cfg.abox {
            for (i, w) in ws.iter().enumerate() {
                let l = &looks[i];
                let Some((a, b)) = live_window(
                    win[i],
                    [l[0].abox > 0.0, l[1].abox > 0.0, l[2].abox > 0.0],
                    w.s,
                    tails[i].1,
                ) else {
                    continue;
                };
                let bnd = row_band(i);
                let (_, _, text, ax) = word_box(i, bnd, (ab.pad_x, ab.pad_y), ab.radius);
                out.push_str(
                    &Item {
                        role: Role::Box {
                            col: ab.col,
                            opacity: 1.0,
                            src: BoxSrc::Active,
                        },
                        layer: LAYER_BOX,
                        anchor: (ax, ink_oy(bnd)),
                        tilt,
                        ..Item::plain(
                            draw.text_style,
                            text,
                            (a, b),
                            rotp(pos[i]),
                            if single { rotp(pos[i]) } else { centre },
                            lwin(i),
                            Some(wfs[i].clone()),
                            size,
                        )
                    }
                    .render(),
                );
            }
        }
        // Shadow and glow copies: per word, or once per row where the words
        // keep their shape.
        let copies = |role_of: &dyn Fn(&WordLook) -> Option<Role>,
                      layer: u8,
                      glow: bool,
                      shift: (f64, f64),
                      out: &mut String| {
            let mut done = vec![false; ws.len()];
            if !single {
                for row in &row_info {
                    let r = &row.words;
                    if !steady(r, glow) {
                        continue;
                    }
                    let l0k = &looks[r.start][0];
                    let Some(role) = role_of(l0k) else {
                        r.clone().for_each(|i| done[i] = true);
                        continue;
                    };
                    let text = r
                        .clone()
                        .map(|i| ws[i].text.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    out.push_str(
                        &Item {
                            role,
                            layer,
                            spacing: spc,
                            anchor: (spc / 2.0, 0.0),
                            shift,
                            tilt,
                            ..Item::plain(
                                draw.text_style,
                                text,
                                (l0, l1),
                                rotp((row.x0 + row.width / 2.0, row.cy)),
                                centre,
                                line_win(l0, l1),
                                Some(steady_fx(l0k, l0, l1)),
                                size,
                            )
                        }
                        .render(),
                    );
                    r.clone().for_each(|i| done[i] = true);
                }
            }
            for (i, w) in ws.iter().enumerate() {
                if done[i] || win[i].1 - win[i].0 < 10.0 {
                    continue;
                }
                let l = &looks[i];
                let on = [
                    role_of(&l[0]).is_some(),
                    role_of(&l[1]).is_some(),
                    role_of(&l[2]).is_some(),
                ];
                let window = if glow {
                    live_window(win[i], on, w.s, tails[i].1)
                } else {
                    Some(win[i])
                };
                let Some((a, b)) = window else { continue };
                let Some(role) = role_of(&l[1])
                    .or_else(|| role_of(&l[0]))
                    .or_else(|| role_of(&l[2]))
                else {
                    continue;
                };
                out.push_str(
                    &Item {
                        role,
                        layer,
                        spacing: spc,
                        anchor: (spc / 2.0, 0.0),
                        shift,
                        tilt,
                        ..Item::plain(
                            draw.text_style,
                            w.text.clone(),
                            (a, b),
                            rotp(pos[i]),
                            if single { rotp(pos[i]) } else { centre },
                            lwin(i),
                            Some(wfs[i].clone()),
                            size,
                        )
                    }
                    .render(),
                );
            }
        };
        if let Some(sh) = cfg.shadow {
            copies(
                &|_| (sh.opacity > 0.0).then_some(Role::Shadow(sh)),
                LAYER_SHADOW,
                false,
                (sh.x, sh.y),
                &mut out,
            );
        }
        if cfg.glow.is_some() || cfg.glow_act.is_some() || cfg.glow_kw.is_some() {
            copies(
                &|l| (l.glow.strength > 0.0 && l.glow.size > 0.0).then_some(Role::Glow),
                LAYER_GLOW,
                true,
                (0.0, 0.0),
                &mut out,
            );
        }
        for (i, w) in ws.iter().enumerate() {
            let (w0, w1) = win[i];
            if w1 - w0 < 10.0 {
                continue;
            }
            out.push_str(
                &Item {
                    spacing: spc,
                    layer: text_layer,
                    anchor: (spc / 2.0, 0.0),
                    tilt,
                    stroke_tags: cfg.stroke_tags,
                    ..Item::plain(
                        draw.text_style,
                        w.text.clone(),
                        (w0, w1),
                        rotp(pos[i]),
                        if single { rotp(pos[i]) } else { centre },
                        lwin(i),
                        Some(wfs[i].clone()),
                        size,
                    )
                }
                .render(),
            );
        }
    }
    out
}

// ---- the headline ----------------------------------------------------------------

/// The headline's layers sit above the captions': box, shadow, glow, text.
pub const HEADLINE_LAYER: u8 = 10;

/// How many rows a headline of these words takes, and how wide the block may
/// be: `avail` px first, then up to `safe` px when that is what it takes to stay
/// in `max_rows`. A text that needs more rows even at `safe` is `None`. Text
/// that is `min_rows` long or short is spread over at least that many rows
/// (never more words than rows). Widths are the font's advances (with
/// `spacing` px after every character).
pub fn headline_rows(
    font: &str,
    size: f64,
    spacing: f64,
    words: &[String],
    (avail, safe): (f64, f64),
    (min_rows, max_rows): (usize, usize),
) -> Option<(usize, f64)> {
    let face = metrics::face(font)?;
    let sp = face.width(" ", size) + spacing;
    let w: Vec<f64> = words
        .iter()
        .map(|t| face.width(t, size) + t.chars().count() as f64 * spacing)
        .collect();
    for room in [avail, safe.max(avail)] {
        let r = break_rows(&w, sp, room).len();
        if r <= max_rows {
            let rows = r.max(min_rows).min(max_rows).min(words.len().max(1));
            return Some((rows, room));
        }
    }
    None
}

/// Where the headline's block goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HeadlineY {
    /// The card's top edge, in px.
    Top(f64),
    /// The middle of the card, in px.
    Centre(f64),
}

/// A headline to draw with the word-level machinery.
pub struct HeadlineDraw<'a> {
    /// Entrance, exit, stroke, shadow, glow, card, spacing, alignment: as
    /// resolved from the Look (`Cfg::resolve`).
    pub cfg: Cfg,
    pub pw: u32,
    pub ph: u32,
    pub font: &'static str,
    pub font_px: u32,
    /// The ASS style the events use.
    pub text_style: &'a str,
    /// The words as drawn (the case is already applied).
    pub words: Vec<String>,
    /// The word in the accent colour.
    pub accent: Option<usize>,
    /// Rows of text (from [`headline_rows`]) and the room for the block (px).
    pub rows: usize,
    pub avail: f64,
    /// Where the block is centred horizontally (px), and vertically.
    pub cx: f64,
    pub y: HeadlineY,
    /// On screen from `start` to `end`, seconds on the clip clock.
    pub start: f64,
    pub end: f64,
    /// The card keeps this far from the frame's edges (px).
    pub margin: f64,
}

/// The ASS events of a headline: card, shadow, glow and text, on the layers
/// [`HEADLINE_LAYER`] and up. The headline is one line of words that are all
/// on screen from `start` to `end`, laid out in `rows` rows, with the line's
/// entrance and exit; the accent word is the line's one keyword.
pub fn headline_events(h: &HeadlineDraw) -> String {
    let Some(face) = metrics::face(h.font) else {
        return String::new();
    };
    if h.words.is_empty() || h.end - h.start < 0.02 {
        return String::new();
    }
    let size = h.font_px as f64;
    let mut cfg = h.cfg.clone();
    let spc = cfg.spacing;
    let sp = face.width(" ", size) + spc;
    let (asc, desc) = face.line_box(size);
    let pitch = size * cfg.line_gap;
    let widths: Vec<f64> = h
        .words
        .iter()
        .map(|t| face.width(t, size) + t.chars().count() as f64 * spc)
        .collect();
    // The room the block gets, as the events will see it.
    let margin_x = ((h.pw as f64 - h.avail) / 2.0).round().max(0.0);
    let avail = (h.pw as f64 - 2.0 * margin_x).max(40.0);
    let rows = if h.rows <= 1 {
        std::iter::once(0..widths.len()).collect::<Vec<_>>()
    } else {
        balanced_rows(&widths, sp, h.rows, avail)
    };
    cfg.max_lines = (rows.len() == 1).then_some(1);
    cfg.force_rows = (rows.len() > 1).then_some(rows.len());
    if let Some(b) = cfg.boxfx.as_mut() {
        b.block = true;
    }
    // The block and its card.
    let text_of = |r: &Range<usize>| {
        r.clone()
            .map(|i| h.words[i].as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let ink = |t: &str| face.ink_y(t, size).unwrap_or((0.72 * size, 0.0));
    let block_w = rows
        .iter()
        .map(|r| r.clone().map(|i| widths[i]).sum::<f64>() + sp * r.len().saturating_sub(1) as f64)
        .fold(0.0, f64::max);
    let nrows = rows.len();
    let top_ink =
        size / 2.0 + (asc - desc) / 2.0 - rows.first().map_or(0.0, |r| ink(&text_of(r)).0);
    let bottom_ink = size / 2.0 + (nrows - 1) as f64 * pitch + (asc - desc) / 2.0
        - rows.last().map_or(0.0, |r| ink(&text_of(r)).1);
    let (pad_x, pad_y) = cfg
        .boxfx
        .as_ref()
        .map_or((0.0, 0.0), |b| (b.pad_x, b.pad_y));
    let (ex, ey) = (pad_x + cfg.stroke_max, pad_y + cfg.stroke_max);
    let card_h = (bottom_ink - top_ink) + 2.0 * ey;
    let block_h = (nrows - 1) as f64 * pitch + size;
    // `top` is the top of the block's first line box; the card's top edge is
    // `top_ink - ey` below it.
    let mut top = match h.y {
        HeadlineY::Top(t) => t - (top_ink - ey),
        HeadlineY::Centre(c) => c - (top_ink + bottom_ink) / 2.0,
    };
    // The whole card inside the frame (the top wins when it cannot be).
    let over = top + top_ink - ey + card_h - (h.ph as f64 - h.margin);
    if over > 0.0 {
        top -= over;
    }
    let under = h.margin - (top + top_ink - ey);
    if under > 0.0 {
        top += under;
    }
    let half = (block_w / 2.0 + ex)
        .min(h.pw as f64 / 2.0 - h.margin)
        .max(0.0);
    let px = h.cx.clamp(half + h.margin, h.pw as f64 - half - h.margin);
    let geo = Geo {
        pw: h.pw,
        ph: h.ph,
        k: cfg.k,
        alignment: 5,
        margin_v: 0,
        cap_l: margin_x as u32,
        cap_r: margin_x as u32,
        place: Some((px.round() as i64, (top + block_h / 2.0).round() as i64)),
        font: h.font,
        font_px: h.font_px,
    };
    let draw = Draw {
        text_style: h.text_style,
        box_style: None,
        caps: false,
    };
    let line: Vec<Word> = h
        .words
        .iter()
        .map(|t| Word {
            w: t.clone(),
            s: h.start,
            e: h.end,
            conf: None,
        })
        .collect();
    let kw: Vec<bool> = (0..line.len()).map(|i| Some(i) == h.accent).collect();
    let text = events_with(
        &cfg,
        &geo,
        &draw,
        &[line],
        &[(h.start, h.end)],
        0.0,
        Some(&kw),
    );
    // Above the captions: every event's layer moves up.
    text.lines()
        .map(|l| match l.strip_prefix("Dialogue: ") {
            Some(rest) => {
                let (n, tail) = rest.split_once(',').unwrap_or((rest, ""));
                let n: u8 = n.parse().unwrap_or(0);
                format!("Dialogue: {},{tail}\n", n + HEADLINE_LAYER)
            }
            None => format!("{l}\n"),
        })
        .collect()
}

/// Can a row's shadow and glow be written once for the whole row? Only when
/// the words are all on show together.
fn single_free(cfg: &Cfg, single: bool) -> bool {
    !single && cfg.mode == WordMode::All
}

/// A word's three looks, with the keyword rule applied.
///
/// A keyword is lit in the keyword colour: that replaces the active colour
/// while it is spoken and, unless `spoken.color` is set, stays as its spoken
/// colour. `keyword.scale` multiplies the scale of its active and spoken
/// looks. The upcoming look is the same for every word. The glow layers stack:
/// the caption's, then the spoken word's, then (for a keyword) the keywords';
/// a keyword keeps its glow once spoken, as it keeps its colour.
fn word_looks(cfg: &Cfg, kw: bool) -> [WordLook; 3] {
    let mut up = cfg.up;
    let mut act = cfg.act;
    let mut spk = cfg.spk;
    if kw {
        act.color = cfg.kw_color;
        if !cfg.spk_color_set {
            spk.color = cfg.kw_color;
        }
        let m = cfg.kw_scale.unwrap_or(1.0);
        act.scale *= m;
        spk.scale *= m;
    }
    let cap = cfg.glow.as_ref();
    let (ga, gk) = (cfg.glow_act.as_ref(), cfg.glow_kw.as_ref());
    up.glow = resolve_glow(&[cap], up.color, cfg.k);
    act.glow = resolve_glow(&[cap, ga, kw.then_some(gk).flatten()], act.color, cfg.k);
    spk.glow = resolve_glow(&[cap, kw.then_some(gk).flatten()], spk.color, cfg.k);
    [up, act, spk]
}

/// When a word's release starts and ends (ms).
///
/// The attack runs from the word's start for `attack`; the word then stays
/// active until its end plus `hold` (never before the attack is done), and
/// the release runs from there for `release`.
fn timeline(cfg: &Cfg, w: &W) -> (f64, f64) {
    let a1 = w.s + cfg.attack;
    let r0 = (w.e + cfg.hold).max(a1);
    (r0, r0 + cfg.release)
}

fn word_fx(cfg: &Cfg, w: &W, [u, a, s]: [WordLook; 3], r0: f64, r1: f64, bump: bool) -> WordFx {
    let a1 = w.s + cfg.attack;
    let build = cfg.mode == WordMode::Build;
    let sweep = cfg.fill == Fill::Sweep && u.color != a.color;
    let up_op = if build { 0.0 } else { u.opacity };
    let ea = cfg.attack_ease;
    let er = cfg.release_ease;
    // Under a sweep the colour starts out as the active colour: the unswept
    // part is drawn in the secondary slot.
    let mut col = Track::new(if sweep { a.color } else { u.color });
    if !sweep {
        col.ramp(w.s, a1, a.color, flat(ea));
    }
    col.ramp(r0, r1, s.color, flat(er));
    let track = |base: f64, va: f64, vs: f64, overshoot: bool| {
        let mut t = Track::new([base]);
        t.ramp(w.s, a1, [va], if overshoot { ea } else { flat(ea) });
        t.ramp(r0, r1, [vs], if overshoot { er } else { flat(er) });
        t
    };
    let colour = |bu: Col, ba: Col, bs: Col| {
        let mut t = Track::new(bu);
        t.ramp(w.s, a1, ba, flat(ea));
        t.ramp(r0, r1, bs, flat(er));
        t
    };
    WordFx {
        s: w.s,
        e: w.e,
        col,
        op: track(up_op, a.opacity, s.opacity, false),
        sc: track(u.scale, a.scale, s.scale, true),
        blur: track(u.blur, a.blur, s.blur, true),
        lift: track(0.0, a.lift, 0.0, true),
        rot: track(0.0, a.rotate, 0.0, true),
        sw: track(u.stroke.w, a.stroke.w, s.stroke.w, false),
        scol: colour(u.stroke.col, a.stroke.col, s.stroke.col),
        gs: track(u.glow.size, a.glow.size, s.glow.size, false),
        gk: track(u.glow.strength, a.glow.strength, s.glow.strength, false),
        gcol: colour(u.glow.col, a.glow.col, s.glow.col),
        bo: track(u.abox, a.abox, s.abox, false),
        bump,
        sweep: sweep.then_some(u.color),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eases_start_and_end_where_they_should() {
        for e in [Ease::Linear, Ease::Out, Ease::In, Ease::Back] {
            assert!((ease_fn(e, 0.0)).abs() < 1e-9, "{e:?}");
            assert!((ease_fn(e, 1.0) - 1.0).abs() < 1e-9, "{e:?}");
        }
        assert!(ease_fn(Ease::Out, 0.25) > 0.25);
        assert!(ease_fn(Ease::In, 0.25) < 0.25);
        let peak = ease_fn(Ease::Back, BACK_AT);
        assert!((peak - (1.0 + BACK_PEAK)).abs() < 0.01, "{peak}");
    }

    #[test]
    fn a_wide_line_breaks_into_balanced_rows() {
        let r = break_rows(&[300.0, 300.0, 300.0, 300.0], 20.0, 700.0);
        assert_eq!(r, vec![0..2, 2..4]);
        assert_eq!(break_rows(&[100.0, 100.0], 20.0, 700.0), vec![0..2]);
        // A word wider than the room still gets a row of its own.
        assert_eq!(break_rows(&[900.0, 100.0], 20.0, 700.0), vec![0..1, 1..2]);
    }

    // ---- the headline's rows --------------------------------------------------

    #[test]
    fn balanced_rows_cover_the_words_and_keep_the_widest_row_narrow() {
        let w = [100.0, 80.0, 120.0, 60.0, 90.0, 70.0];
        // One row is everything.
        assert_eq!(balanced_rows(&w, 10.0, 1, 1000.0), vec![0..6]);
        for rows in 2..=4 {
            let r = balanced_rows(&w, 10.0, rows, 1000.0);
            assert_eq!(r.len(), rows);
            assert_eq!(r[0].start, 0);
            assert_eq!(r.last().unwrap().end, 6);
            assert!(r.windows(2).all(|p| p[0].end == p[1].start));
            // No row is empty.
            assert!(r.iter().all(|x| !x.is_empty()));
        }
        // Two rows: the split that makes the widest row narrowest.
        let width =
            |r: &Range<usize>| r.clone().map(|i| w[i]).sum::<f64>() + 10.0 * (r.len() - 1) as f64;
        let two = balanced_rows(&w, 10.0, 2, 1000.0);
        let widest = two.iter().map(width).fold(0.0, f64::max);
        for cut in 1..6 {
            let alt = [0..cut, cut..6];
            assert!(widest <= alt.iter().map(width).fold(0.0, f64::max) + 0.5);
        }
        // More rows than words: one word per row.
        let many = balanced_rows(&w[..3], 10.0, 5, 1000.0);
        assert_eq!(many, vec![0..1, 1..2, 2..3]);
        // A long line is split evenly instead of searched.
        let long = vec![10.0; 30];
        let r = balanced_rows(&long, 5.0, 3, 1000.0);
        assert_eq!(r, vec![0..10, 10..20, 20..30]);
        // No words: nothing to break.
        assert_eq!(balanced_rows(&[], 5.0, 2, 100.0), vec![0..0]);
    }

    #[test]
    fn headline_rows_widen_the_block_before_giving_up() {
        let words = |t: &str| -> Vec<String> { t.split(' ').map(String::from).collect() };
        let long = words("Why most founders quit too early and what to do");
        let arch = "Archivo Black";
        // Plenty of room: as many rows as the width needs, at least the minimum.
        assert_eq!(
            headline_rows(arch, 64.0, 0.0, &long, (1008.0, 1008.0), (1, 3)),
            Some((2, 1008.0))
        );
        assert_eq!(
            headline_rows(arch, 64.0, 0.0, &long, (540.0, 1008.0), (1, 3)),
            Some((3, 540.0))
        );
        // Too narrow for three rows: the block widens to what is safe.
        assert_eq!(
            headline_rows(arch, 64.0, 0.0, &long, (300.0, 1008.0), (1, 3)),
            Some((2, 1008.0))
        );
        // Not even then: it does not fit.
        assert_eq!(
            headline_rows(arch, 64.0, 0.0, &long, (300.0, 1008.0), (1, 1)),
            None
        );
        assert_eq!(
            headline_rows(arch, 64.0, 0.0, &long, (1008.0, 1008.0), (1, 1)),
            None
        );
        // A short text can be spread over two rows, but never over more rows than words.
        let short = words("Big news");
        assert_eq!(
            headline_rows(arch, 64.0, 0.0, &short, (1008.0, 1008.0), (1, 3)),
            Some((1, 1008.0))
        );
        assert_eq!(
            headline_rows(arch, 64.0, 0.0, &short, (1008.0, 1008.0), (2, 3)),
            Some((2, 1008.0))
        );
        assert_eq!(
            headline_rows(arch, 64.0, 0.0, &words("Big"), (1008.0, 1008.0), (2, 3)),
            Some((1, 1008.0))
        );
        // Letter spacing makes words wider.
        let tight = headline_rows(arch, 64.0, 0.0, &long, (1008.0, 1008.0), (1, 3))
            .unwrap()
            .0;
        let loose = headline_rows(arch, 64.0, 12.0, &long, (1008.0, 1008.0), (1, 3))
            .unwrap()
            .0;
        assert!(loose >= tight);
        // A font with no metrics cannot be laid out.
        assert_eq!(
            headline_rows("Comic Sans", 64.0, 0.0, &long, (1008.0, 1008.0), (1, 3)),
            None
        );
    }
}
