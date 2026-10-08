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
//! Times in this file are milliseconds on the clip clock.

use std::ops::Range;

use super::ass::{ends_sentence, is_keyword, Anim};
use super::metrics;
use crate::look::{CaptionsLook, Ease, EnterKind, ExitKind, Fill, Rgb, WordMode, WordState};
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

/// One look of a word, every field concrete.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WordLook {
    pub color: Col,
    pub opacity: f64,
    pub scale: f64,
    pub blur: f64,
    pub lift: f64,
    pub rotate: f64,
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
    let enter = match &cap.enter {
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
    let exit = match &cap.exit {
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
        };
        let up = state(&w.upcoming, sec, 1.0, 1.0);
        let act = state(&w.active, prim, 1.0, 1.0);
        // A spoken word keeps the sung colour, as it does today.
        let spk = state(&w.spoken, act.color, 1.0, 1.0);
        Cfg {
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

    fn knots(&self, out: &mut Vec<f64>) {
        let mut v = Vec::new();
        for s in &self.col.segs {
            v.extend([s.t0, s.t1]);
        }
        for t in [&self.op, &self.sc, &self.blur, &self.lift, &self.rot] {
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

struct Item {
    style: String,
    text: String,
    /// Extra letter spacing (px) so an invisible box text spans the slots.
    spacing: f64,
    w0: f64,
    w1: f64,
    base: (f64, f64),
    centre: (f64, f64),
    line: LineWin,
    word: Option<WordFx>,
    boxed: Option<BoxAlpha>,
    font_px: f64,
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
    col: Col,
}

impl Item {
    fn frame(&self, t: f64) -> Frame {
        let lf = self.line.fx(t);
        let (col, op, sc, blur, lift, rot) = match &self.word {
            Some(w) => (
                w.col.eval(t),
                w.op.eval(t)[0],
                w.sc.eval(t)[0] * w.pulse(t),
                w.blur.eval(t)[0],
                w.lift.eval(t)[0],
                w.rot.eval(t)[0],
            ),
            None => ([0.0; 3], 1.0, 1.0, 0.0, 0.0, 0.0),
        };
        let (cx, cy) = self.centre;
        let (bx, by) = self.base;
        Frame {
            x: cx + (bx - cx) * lf.sc + lf.dx,
            y: cy + (by - lift * self.font_px - cy) * lf.sc + lf.dy,
            fsc: sc * lf.sc * 100.0,
            // Positive rotates clockwise; ASS turns counter-clockwise.
            rot: -rot,
            blur: blur + lf.blur,
            alpha: op * lf.alpha,
            col,
        }
    }

    /// The tags that depend on the frame: scale, rotation, blur, opacity, colour.
    fn props(&self, f: &Frame, colour: Option<Col>) -> [String; 5] {
        let fsc = n1(f.fsc);
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
            None => format!("\\alpha&H{:02X}&", alpha_byte(f.alpha)),
        };
        [
            format!("\\fscx{fsc}\\fscy{fsc}"),
            format!("\\frz{}", n1(f.rot)),
            format!("\\blur{}", n1(f.blur.max(0.0))),
            alpha,
            colour.map_or_else(String::new, |c| col_tag("1c", c)),
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
        for t in [&w.op, &w.sc, &w.blur, &w.lift, &w.rot] {
            t.segs.iter().for_each(&mut hit);
        }
        for s in &w.col.segs {
            if near(s.t0, a, 5.0) && near(s.t1, b, 5.0) {
                found = found.or(Some(s.ease));
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
                && (0..3).all(|i| near(f.col[i], lerp(f0.col[i], f1.col[i]), 1.5))
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
            "Dialogue: 0,{},{},{},,0,0,0,,{text}\n",
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
            let tags: String = (0..5)
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
        let flat_tags: String = [3usize, 4]
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
            ]
        };
        let frames: Vec<Frame> = times.iter().map(|&t| at(t)).collect();
        // Join pieces that continue on the same straight line.
        let mut keep = vec![0usize];
        for j in 1..times.len() - 1 {
            let (p, q, r) = (*keep.last().unwrap_or(&0), j, j + 1);
            let (np, nq, nr) = (nums(&frames[p]), nums(&frames[q]), nums(&frames[r]));
            let straight = (0..7).all(|i| {
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
    let width = |r: Range<usize>| -> f64 {
        r.clone().map(|i| w[i]).sum::<f64>() + sp * (r.len().saturating_sub(1)) as f64
    };
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
    let rows = rows.min(n);
    // Try every set of breaks with that many rows; narrowest widest row wins,
    // then the wider top row.
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
    width: f64,
}

/// The ASS events (box and word, all lines) for the word-level model.
#[allow(clippy::too_many_arguments)]
pub fn events(
    cfg: &Cfg,
    geo: &Geo,
    draw: &Draw,
    lines: &[Vec<Word>],
    spans: &[(f64, f64)],
    offset: f64,
) -> String {
    let Some(face) = metrics::face(geo.font) else {
        return String::new();
    };
    let size = geo.font_px as f64;
    let sp = face.width(" ", size);
    let lh = size;
    let reach = Reach {
        v: SLIDE_V * geo.ph as f64,
        h: SLIDE_H * geo.pw as f64,
        drop: DROP_V * geo.ph as f64,
        blur: FX_BLUR * geo.k,
    };
    let avail = (geo.pw as f64 - geo.cap_l as f64 - geo.cap_r as f64).max(40.0);
    let bumps = cfg.bumps();
    let mut out = String::new();
    let mut fresh = true;
    for (line, &(t0, t1)) in lines.iter().zip(spans) {
        let (l0, l1) = ((t0 - offset) * 1000.0, (t1 - offset) * 1000.0);
        let mut ws: Vec<W> = Vec::new();
        let mut kw_flags = Vec::new();
        for w in line {
            kw_flags.push(is_keyword(&w.w, fresh));
            fresh = ends_sentence(&w.w);
        }
        for (w, kw) in line.iter().zip(&kw_flags) {
            let text = if draw.caps {
                w.w.to_uppercase()
            } else {
                w.w.clone()
            };
            let text = text.replace(['{', '}', '\n', '\r'], "");
            ws.push(W {
                width: face.width(&text, size),
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
        let rows = if single {
            std::iter::once(0..ws.len()).collect()
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
        let block_h = nrows as f64 * lh;
        let top = match geo.place {
            Some((_, py)) => py as f64 - block_h / 2.0,
            None => match geo.alignment {
                1..=3 => geo.ph as f64 - geo.margin_v as f64 - block_h,
                7..=9 => geo.margin_v as f64,
                _ => (geo.ph as f64 - block_h) / 2.0,
            },
        };
        let block_w = rows
            .iter()
            .map(|r| {
                r.clone().map(|i| slot[i]).sum::<f64>() + sp * r.len().saturating_sub(1) as f64
            })
            .fold(0.0, f64::max);
        let centre = (
            match col {
                0 => left + block_w / 2.0,
                2 => right - block_w / 2.0,
                _ => cx,
            },
            top + block_h / 2.0,
        );
        // Positions: (x, y) of each word's centre; and each row's extent.
        let mut pos = vec![(0.0, 0.0); ws.len()];
        let mut row_info: Vec<(f64, f64, Range<usize>, f64)> = Vec::new(); // (cx, cy, words, natural width)
        if single {
            let cy = top + lh / 2.0;
            for (i, p) in pos.iter_mut().enumerate() {
                let x = match col {
                    0 => left + ws[i].width / 2.0,
                    2 => right - ws[i].width / 2.0,
                    _ => cx,
                };
                *p = (x, cy);
            }
        } else {
            for (ri, r) in rows.iter().enumerate() {
                let rw =
                    r.clone().map(|i| slot[i]).sum::<f64>() + sp * r.len().saturating_sub(1) as f64;
                let nat = r.clone().map(|i| ws[i].width).sum::<f64>()
                    + sp * r.len().saturating_sub(1) as f64;
                let x0 = match col {
                    0 => left,
                    2 => right - rw,
                    _ => cx - rw / 2.0,
                };
                let cy = top + (ri as f64 + 0.5) * lh;
                let mut x = x0;
                for i in r.clone() {
                    pos[i] = (x + slot[i] / 2.0, cy);
                    x += slot[i] + sp;
                }
                row_info.push((x0 + rw / 2.0, cy, r.clone(), nat));
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
        // Boxes first, so they sit under the words.
        if let Some((bname, outline, shadow)) = draw.box_style {
            let boxed = Some(BoxAlpha { outline, shadow });
            if single {
                for (i, w) in ws.iter().enumerate() {
                    let (w0, w1) = win[i];
                    let lw = line_win(w0, w1);
                    out.push_str(
                        &Item {
                            style: bname.to_string(),
                            text: w.text.clone(),
                            spacing: 0.0,
                            w0,
                            w1,
                            base: pos[i],
                            centre: pos[i],
                            line: lw,
                            word: None,
                            boxed,
                            font_px: size,
                        }
                        .render(),
                    );
                }
            } else {
                for (cxr, cyr, r, nat) in &row_info {
                    let text = r
                        .clone()
                        .map(|i| ws[i].text.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let slots = r.clone().map(|i| slot[i]).sum::<f64>()
                        + sp * r.len().saturating_sub(1) as f64;
                    let chars = text.chars().count().max(1) as f64;
                    out.push_str(
                        &Item {
                            style: bname.to_string(),
                            text,
                            spacing: (slots - nat) / chars,
                            w0: l0,
                            w1: l1,
                            base: (*cxr, *cyr),
                            centre,
                            line: line_win(l0, l1),
                            word: None,
                            boxed,
                            font_px: size,
                        }
                        .render(),
                    );
                }
            }
        }
        for (i, w) in ws.iter().enumerate() {
            let (w0, w1) = win[i];
            if w1 - w0 < 10.0 {
                continue;
            }
            let [u, a, s] = looks[i];
            let (r0, r1) = tails[i];
            let bump = bumps && !single && w.kw && w.s - l0 >= cfg.enter.ms;
            let wf = word_fx(cfg, w, [u, a, s], r0, r1, bump);
            let lw = if single {
                line_win(w0, w1)
            } else {
                line_win(l0, l1)
            };
            out.push_str(
                &Item {
                    style: draw.text_style.to_string(),
                    text: w.text.clone(),
                    spacing: 0.0,
                    w0,
                    w1,
                    base: pos[i],
                    centre: if single { pos[i] } else { centre },
                    line: lw,
                    word: Some(wf),
                    boxed: None,
                    font_px: size,
                }
                .render(),
            );
        }
    }
    out
}

/// A word's three looks, with the keyword rule applied.
///
/// A keyword is lit in the keyword colour: that replaces the active colour
/// while it is spoken and, unless `spoken.color` is set, stays as its spoken
/// colour. `keyword.scale` multiplies the scale of its active and spoken
/// looks. The upcoming look is the same for every word.
fn word_looks(cfg: &Cfg, kw: bool) -> [WordLook; 3] {
    let up = cfg.up;
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
    WordFx {
        s: w.s,
        e: w.e,
        col,
        op: track(up_op, a.opacity, s.opacity, false),
        sc: track(u.scale, a.scale, s.scale, true),
        blur: track(u.blur, a.blur, s.blur, true),
        lift: track(0.0, a.lift, 0.0, true),
        rot: track(0.0, a.rotate, 0.0, true),
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
}
