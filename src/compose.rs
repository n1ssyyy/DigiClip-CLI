//! Frame compositor: one decoded source frame + one camera rect → one
//! output frame on the job's [`Canvas`] (1080x1920 by default), all in
//! planar YUV 4:2:0 (no RGB round trip).
//!
//! Why this lives in Rust instead of an ffmpeg filtergraph: ffmpeg's
//! `crop` only takes whole pixels (and even ones on 4:2:0), and `scale`
//! rebuilds its filter whenever the crop size changes, so a slow pan on a
//! 640x360 source stepped ~7 output pixels at a time and the 9:16 aspect
//! rounding left 1–10 px bars that flickered during zooms. Here the crop
//! box is `f64` end to end: `fast_image_resize` convolves straight from
//! the fractional source rect (SIMD, per plane), so motion is sub-pixel
//! smooth and a 9:16 rect always fills the frame exactly.
//!
//! Framings that don't match the canvas aspect (the wide/no-face shot)
//! fit inside the frame over a blurred, dimmed fill made from the same
//! moment of video. An optional progress bar is drawn last.

use fast_image_resize::images::{CroppedImageMut, Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

/// Output canvas (even dimensions). Every render targets one; the camera,
/// tracker and captions derive their window shapes from its aspect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Canvas {
    pub w: u32,
    pub h: u32,
}

impl Default for Canvas {
    fn default() -> Self {
        Canvas::TALL
    }
}

impl Canvas {
    /// 9:16 — TikTok, Reels, Shorts.
    pub const TALL: Canvas = Canvas { w: 1080, h: 1920 };
    /// 4:5 — Instagram / Facebook feed.
    pub const PORTRAIT: Canvas = Canvas { w: 1080, h: 1350 };
    /// 1:1 — feeds, LinkedIn.
    pub const SQUARE: Canvas = Canvas { w: 1080, h: 1080 };
    /// 16:9 — YouTube, X.
    pub const WIDE: Canvas = Canvas { w: 1920, h: 1080 };

    /// Parse `9:16`, `4:5`, `1:1`, `16:9` (or `9x16`, ...).
    pub fn parse(s: &str) -> Option<Canvas> {
        match s.trim().replace('x', ":").as_str() {
            "9:16" => Some(Canvas::TALL),
            "4:5" => Some(Canvas::PORTRAIT),
            "1:1" => Some(Canvas::SQUARE),
            "16:9" => Some(Canvas::WIDE),
            _ => None,
        }
    }
    /// Width / height.
    pub fn aspect(&self) -> f64 {
        self.w as f64 / self.h as f64
    }
    /// File-name tag: `9x16`, `4x5`, `1x1`, `16x9`.
    pub fn tag(&self) -> &'static str {
        match (self.w, self.h) {
            (1080, 1350) => "4x5",
            (1080, 1080) => "1x1",
            (1920, 1080) => "16x9",
            _ => "9x16",
        }
    }
    /// Largest canvas-aspect rect that fits a `sw` x `sh` source, centered.
    pub fn base_rect(&self, sw: f64, sh: f64) -> Rect {
        base_rect(sw, sh, self.aspect())
    }
    /// Fewest source rows a crop may shrink to: bounds upscaling at ~3.6x
    /// of the output height so low-res sources stay sharp instead of mush.
    pub fn min_crop_h(&self) -> f64 {
        self.h as f64 * (540.0 / 1920.0)
    }
}

/// BT.709 limited-range YUV of an sRGB color.
pub fn yuv709(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let y = 16.0 + 219.0 * (0.2126 * r + 0.7152 * g + 0.0722 * b);
    let u = 128.0 + 224.0 * (-0.1146 * r - 0.3854 * g + 0.5 * b);
    let v = 128.0 + 224.0 * (0.5 * r - 0.4542 * g - 0.0458 * b);
    let c = |x: f64| x.round().clamp(0.0, 255.0) as u8;
    (c(y), c(u), c(v))
}

/// `#RRGGBB` / `RRGGBB` → (r, g, b).
pub fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some((p(0)?, p(2)?, p(4)?))
}

/// A source rectangle in source pixels (top-left + size, fractional).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn cx(&self) -> f64 {
        self.x + self.w / 2.0
    }
    pub fn cy(&self) -> f64 {
        self.y + self.h / 2.0
    }
    pub fn from_center(cx: f64, cy: f64, w: f64, h: f64) -> Self {
        Rect {
            x: cx - w / 2.0,
            y: cy - h / 2.0,
            w,
            h,
        }
    }
    /// Clamp inside a `sw` x `sh` frame (size first, then position).
    pub fn clamped(&self, sw: f64, sh: f64) -> Self {
        let w = self.w.clamp(2.0, sw);
        let h = self.h.clamp(2.0, sh);
        let x = (self.cx() - w / 2.0).clamp(0.0, (sw - w).max(0.0));
        let y = (self.cy() - h / 2.0).clamp(0.0, (sh - h).max(0.0));
        Rect { x, y, w, h }
    }
    /// Scale all coordinates (source px → decoded px).
    pub fn scaled(&self, s: f64) -> Self {
        Rect {
            x: self.x * s,
            y: self.y * s,
            w: self.w * s,
            h: self.h * s,
        }
    }
}

/// Largest `aspect` (w/h) rect that fits a `sw` x `sh` source, centered.
pub fn base_rect(sw: f64, sh: f64, aspect: f64) -> Rect {
    let (w, h) = if sw / sh > aspect {
        (sh * aspect, sh)
    } else {
        (sw, sw / aspect)
    };
    Rect::from_center(sw / 2.0, sh / 2.0, w, h)
}

/// Where a source rect lands in the output frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    /// Source rect actually sampled (aspect matched to `dst` exactly).
    pub src: Rect,
    /// Destination rect in output pixels (even-aligned for 4:2:0).
    pub dx: u32,
    pub dy: u32,
    pub dw: u32,
    pub dh: u32,
}

impl Layout {
    pub fn covers(&self, c: Canvas) -> bool {
        self.dx == 0 && self.dy == 0 && self.dw == c.w && self.dh == c.h
    }
}

/// Relative aspect slack treated as "is the canvas aspect": a sliver bar
/// under ~1.5% of the frame reads as a glitch, so near-matching rects fill
/// the frame (trimming a hair off the long side) instead of letterboxing.
const COVER_SLACK: f64 = 0.015;

/// Pure (unit-tested): fit `r` (already clamped to the source) into the
/// canvas.
pub fn layout(r: &Rect, c: Canvas) -> Layout {
    let (out_w, out_h) = (c.w, c.h);
    let target = c.aspect();
    let a = r.w / r.h;
    if (a / target - 1.0).abs() <= COVER_SLACK {
        // Fill: trim the long side around the center to exact 9:16.
        let (w, h) = if a > target {
            (r.h * target, r.h)
        } else {
            (r.w, r.w / target)
        };
        return Layout {
            src: Rect::from_center(r.cx(), r.cy(), w, h),
            dx: 0,
            dy: 0,
            dw: out_w,
            dh: out_h,
        };
    }
    let even = |v: f64| ((v / 2.0).round() * 2.0).max(2.0) as u32;
    if a > target {
        // Wider than 9:16: full width, bars top and bottom.
        let dh = even(out_w as f64 / a).min(out_h);
        let src_h = r.w * dh as f64 / out_w as f64;
        Layout {
            src: Rect::from_center(r.cx(), r.cy(), r.w, src_h),
            dx: 0,
            dy: ((out_h - dh) / 2) & !1,
            dw: out_w,
            dh,
        }
    } else {
        // Taller than 9:16: full height, bars left and right.
        let dw = even(out_h as f64 * a).min(out_w);
        let src_w = r.h * dw as f64 / out_h as f64;
        Layout {
            src: Rect::from_center(r.cx(), r.cy(), src_w, r.h),
            dx: ((out_w - dw) / 2) & !1,
            dy: 0,
            dw,
            dh: out_h,
        }
    }
}

/// Planar YUV 4:2:0 frame geometry (chroma dims round up, like ffmpeg).
#[derive(Debug, Clone, Copy)]
pub struct Geom {
    pub w: u32,
    pub h: u32,
}

impl Geom {
    pub fn cw(&self) -> u32 {
        self.w.div_ceil(2)
    }
    pub fn ch(&self) -> u32 {
        self.h.div_ceil(2)
    }
    pub fn luma_len(&self) -> usize {
        self.w as usize * self.h as usize
    }
    pub fn chroma_len(&self) -> usize {
        self.cw() as usize * self.ch() as usize
    }
    /// Bytes in one frame.
    pub fn frame_len(&self) -> usize {
        self.luma_len() + 2 * self.chroma_len()
    }
    fn split<'a>(&self, buf: &'a [u8]) -> (&'a [u8], &'a [u8], &'a [u8]) {
        let (y, rest) = buf.split_at(self.luma_len());
        let (u, v) = rest.split_at(self.chroma_len());
        (y, u, &v[..self.chroma_len()])
    }
    fn split_mut<'a>(&self, buf: &'a mut [u8]) -> (&'a mut [u8], &'a mut [u8], &'a mut [u8]) {
        let (y, rest) = buf.split_at_mut(self.luma_len());
        let (u, v) = rest.split_at_mut(self.chroma_len());
        (y, u, &mut v[..self.chroma_len()])
    }
}

/// Blur-fill tiny height (width follows the canvas aspect): upscaled
/// bilinearly, this is a wide soft blur at almost no cost.
const BG_H: u32 = 64;
/// Fill brightness (luma gain toward black): keeps the subject the brightest
/// thing on screen.
const BG_DIM: f32 = 0.72;
/// The Look's `fill_dim` that gives today's [`BG_DIM`]. Below it the fill
/// brightens linearly to untouched (`0`), above it darkens linearly to
/// [`BG_DIM_MAX`] (`1`).
pub const FILL_DIM_TODAY: f64 = 0.4;
/// Fill luma gain at `fill_dim` 1: nearly black.
const BG_DIM_MAX: f32 = 0.04;

/// Luma gain of the blurred fill for the Look's `fill_dim` (0..1). Passes
/// through today's gain at [`FILL_DIM_TODAY`]. Pure (unit-tested).
pub fn fill_gain(dim: f64) -> f32 {
    let d = dim.clamp(0.0, 1.0);
    let lerp = |a: f32, b: f32, u: f64| a * (1.0 - u as f32) + b * u as f32;
    if d <= FILL_DIM_TODAY {
        lerp(1.0, BG_DIM, d / FILL_DIM_TODAY)
    } else {
        lerp(
            BG_DIM,
            BG_DIM_MAX,
            (d - FILL_DIM_TODAY) / (1.0 - FILL_DIM_TODAY),
        )
    }
}

/// Rows of the top panel in a split-screen frame: an even half today, the
/// Look's `layout.split` share of the height otherwise (even, so the 4:2:0
/// chroma rows line up). Pure (unit-tested).
pub fn split_rows(out_h: u32, split: Option<f64>) -> u32 {
    match split {
        Some(s) if (s - 0.5).abs() > 1e-9 => {
            let rows = ((out_h as f64 * s / 2.0).round() as u32) * 2;
            rows.clamp(2, (out_h.saturating_sub(2)) & !1)
        }
        _ => (out_h / 2) & !1,
    }
}

/// Strength of the vignette at `vignette` 1: the corners keep this share
/// less of their brightness.
const VIGNETTE_MAX: f64 = 0.7;

/// Vignette gain (0..1) at a normalised radius `r` (0 centre, 1 corner),
/// for strength `v` (0..1). Smoothstep from 20% of the way out, so the
/// middle of the frame is untouched and the falloff has no visible edge.
/// Pure (unit-tested).
pub fn vignette_gain(r: f64, v: f64) -> f64 {
    let t = ((r - 0.2) / 0.8).clamp(0.0, 1.0);
    1.0 - VIGNETTE_MAX * v.clamp(0.0, 1.0) * t * t * (3.0 - 2.0 * t)
}

/// Per-clip tables for a colour grade, indexed by (limited-range) luma:
/// the luma curve, a chroma gain and a chroma offset. Built once; the
/// per-frame work is table lookups.
struct GradeTab {
    luma: [u8; 256],
    sat: [f32; 256],
    du: [f32; 256],
    dv: [f32; 256],
}

fn smooth01(e0: f64, e1: f64, x: f64) -> f64 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl GradeTab {
    /// `None` for [`crate::look::Grade::None`].
    fn new(g: crate::look::Grade) -> Option<Self> {
        use crate::look::Grade;
        // Chroma offsets (8-bit units) for the casts: -3.6 U / +2.8 V is
        // about +5 on red and -7.6 on blue (of 255) at constant luma. The
        // cast fades out into the deepest shadows so black stays black.
        let (du, dv, punchy, mono) = match g {
            Grade::None => return None,
            Grade::Warm => (-3.6, 2.8, false, false),
            Grade::Cool => (3.6, -2.8, false, false),
            Grade::Mono => (0.0, 0.0, false, true),
            Grade::Punchy => (0.0, 0.0, true, false),
        };
        let mut t = GradeTab {
            luma: [0; 256],
            sat: [1.0; 256],
            du: [0.0; 256],
            dv: [0.0; 256],
        };
        for i in 0..256usize {
            let y = i as f64;
            t.luma[i] = i as u8;
            if punchy {
                // A soft S-curve in the limited luma range (the ends stay
                // put, so nothing clips) and a touch more colour in the mids.
                let n = ((y - 16.0) / 219.0).clamp(0.0, 1.0);
                let s = n * n * (3.0 - 2.0 * n);
                let n2 = n + 0.30 * (s - n);
                t.luma[i] = if (16..=235).contains(&i) {
                    (16.0 + 219.0 * n2).round() as u8
                } else {
                    i as u8
                };
                t.sat[i] = (1.0
                    + 0.18 * smooth01(16.0, 60.0, y) * (1.0 - smooth01(190.0, 235.0, y)))
                    as f32;
            }
            if mono {
                t.sat[i] = 0.0;
            }
            let fade = smooth01(16.0, 72.0, y);
            t.du[i] = (du * fade) as f32;
            t.dv[i] = (dv * fade) as f32;
        }
        Some(t)
    }
}

/// Picture effects of one compositor: a vignette gain map (Q12 per pixel)
/// and/or a grade. Applied to the finished picture, before the flash dip
/// and the progress bar.
struct Fx {
    vig: Option<Vec<u16>>,
    grade: Option<GradeTab>,
}

impl Fx {
    fn new(look: &crate::look::EffectsLook, c: Canvas) -> Option<Self> {
        let v = look.vignette.unwrap_or(0.0);
        let vig = (v > 0.0).then(|| {
            let (w, h) = (c.w as usize, c.h as usize);
            let (hw, hh) = (w as f64 / 2.0, h as f64 / 2.0);
            let mut m = Vec::with_capacity(w * h);
            // The gain only depends on the distance from the centre, with
            // each axis normalised to its half-size (an ellipse; the
            // corners are r = 1).
            let col: Vec<f64> = (0..w)
                .map(|x| ((x as f64 + 0.5 - hw) / hw).powi(2))
                .collect();
            for y in 0..h {
                let dy = ((y as f64 + 0.5 - hh) / hh).powi(2);
                for &dx2 in &col {
                    let r = ((dx2 + dy) / 2.0).sqrt();
                    m.push((vignette_gain(r, v) * 4096.0).round() as u16);
                }
            }
            m
        });
        let grade = look.grade.and_then(GradeTab::new);
        (vig.is_some() || grade.is_some()).then_some(Fx { vig, grade })
    }

    /// Grade, then vignette, over a finished yuv420p picture.
    fn apply(&self, out: &mut [u8], c: Canvas) {
        let g = Geom { w: c.w, h: c.h };
        let (w, cw) = (c.w as usize, g.cw() as usize);
        let (oy, uv) = out.split_at_mut(g.luma_len());
        let (ou, ov) = uv.split_at_mut(g.chroma_len());
        if let Some(t) = &self.grade {
            // Chroma first: it reads the ungraded luma around each sample.
            if t.sat.iter().any(|&s| s != 1.0) || t.du.iter().any(|&d| d != 0.0) {
                for cy in 0..g.ch() as usize {
                    let r0 = (cy * 2).min(c.h as usize - 1) * w;
                    let r1 = (cy * 2 + 1).min(c.h as usize - 1) * w;
                    for cx in 0..cw {
                        let x0 = cx * 2;
                        let x1 = (x0 + 1).min(w - 1);
                        let l = (oy[r0 + x0] as u32
                            + oy[r0 + x1] as u32
                            + oy[r1 + x0] as u32
                            + oy[r1 + x1] as u32
                            + 2)
                            / 4;
                        let i = l as usize;
                        let k = cy * cw + cx;
                        let f = |p: u8, d: f32| {
                            (128.0 + (p as f32 - 128.0) * t.sat[i] + d)
                                .round()
                                .clamp(16.0, 240.0) as u8
                        };
                        ou[k] = f(ou[k], t.du[i]);
                        ov[k] = f(ov[k], t.dv[i]);
                    }
                }
            }
            if t.luma.iter().enumerate().any(|(i, &v)| v as usize != i) {
                for p in oy.iter_mut() {
                    *p = t.luma[*p as usize];
                }
            }
        }
        if let Some(m) = &self.vig {
            // Ordered dither on the rounding: a smooth falloff over 8-bit
            // luma would otherwise band.
            const BAYER: [i32; 16] = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];
            let dither = |x: usize, y: usize| (BAYER[(y & 3) * 4 + (x & 3)] * 2 + 1) * 128;
            for (y, (row, gains)) in oy.chunks_mut(w).zip(m.chunks(w)).enumerate() {
                for (x, (p, &k)) in row.iter_mut().zip(gains).enumerate() {
                    let v = (*p as i32 - 16) * k as i32 + dither(x, y);
                    *p = (16 + (v >> 12)).clamp(0, 255) as u8;
                }
            }
            for cy in 0..g.ch() as usize {
                let gy = (cy * 2).min(c.h as usize - 1) * w;
                for cx in 0..cw {
                    let k = m[gy + (cx * 2).min(w - 1)] as i32;
                    let i = cy * cw + cx;
                    let d = dither(cx, cy);
                    for pl in [&mut *ou, &mut *ov] {
                        let v = (pl[i] as i32 - 128) * k + d;
                        pl[i] = (128 + (v >> 12)).clamp(0, 255) as u8;
                    }
                }
            }
        }
    }
}

/// Reusable compositor for one source geometry and canvas.
pub struct Compositor {
    src: Geom,
    canvas: Canvas,
    out: Vec<u8>,
    ry: Resizer,
    ruv: Resizer,
    bg_w: u32,
    bg_tiny: Vec<u8>,
    bg_scratch: Vec<u8>,
    /// Progress bar color (YUV), if drawn.
    bar: Option<(u8, u8, u8)>,
    /// The Look's bar: along the top edge, and the thickness multiplier.
    bar_top: bool,
    bar_height: f64,
    /// The bar's colour (sRGB) and the Look's section, kept to plan the shaped
    /// bar (inset, radius, track, glow) once both are known.
    bar_rgb: Option<crate::look::Rgba>,
    bar_look: Option<crate::look::BarLook>,
    bar_fx: Option<crate::bar::BarFx>,
    /// Opacity of the plain bar's fill and of its dimmed track (1, 1 = opaque).
    bar_alpha: (f32, f32),
    /// Vignette and grade over the picture (never over the bar).
    fx: Option<Fx>,
    /// Luma gain of the blurred fill, and how much of its colour stays.
    fill_gain: f32,
    fill_chroma: f32,
    /// The Look's split seam (top panel's share of the height).
    split: Option<f64>,
}

impl Compositor {
    pub fn new(src_w: u32, src_h: u32, canvas: Canvas) -> Self {
        let out_g = Geom {
            w: canvas.w,
            h: canvas.h,
        };
        let mut out = vec![0u8; out_g.frame_len()];
        // Neutral chroma so an unwritten frame is black, not green.
        out[out_g.luma_len()..].fill(128);
        let bg_w = (((BG_H as f64 * canvas.aspect()) / 2.0).round() as u32 * 2).max(8);
        let bg = Geom { w: bg_w, h: BG_H };
        Self {
            src: Geom { w: src_w, h: src_h },
            canvas,
            out,
            ry: Resizer::new(),
            ruv: Resizer::new(),
            bg_w,
            bg_tiny: vec![0u8; bg.frame_len()],
            bg_scratch: vec![0u8; bg.frame_len()],
            bar: None,
            bar_top: false,
            bar_height: 1.0,
            bar_rgb: None,
            bar_look: None,
            bar_fx: None,
            bar_alpha: (1.0, 1.0),
            fx: None,
            fill_gain: BG_DIM,
            fill_chroma: 1.0,
            split: None,
        }
    }

    /// The Look's effects: vignette and grade over the picture, and how
    /// dark the blurred fill is. Absent or neutral fields change nothing.
    pub fn with_effects(mut self, look: Option<&crate::look::EffectsLook>) -> Self {
        if let Some(l) = look {
            self.fx = Fx::new(l, self.canvas);
            if let Some(d) = l.fill_dim {
                self.fill_gain = fill_gain(d);
                // Past today's darkness the colour fades with the light.
                self.fill_chroma = (self.fill_gain / BG_DIM).min(1.0);
            }
        }
        self
    }

    /// The Look's layout: where the split-screen seam sits.
    pub fn with_split(mut self, look: Option<&crate::look::LayoutLook>) -> Self {
        self.split = look.and_then(|l| l.split);
        self
    }

    /// Draw a progress bar (sRGB color) along the bottom edge.
    pub fn with_bar(self, rgb: Option<(u8, u8, u8)>) -> Self {
        self.with_bar_rgba(rgb.map(|(r, g, b)| crate::look::Rgba::rgb(r, g, b)))
    }

    /// [`with_bar`](Self::with_bar), the colour with its own alpha (`#RRGGBBAA`).
    pub fn with_bar_rgba(mut self, rgba: Option<crate::look::Rgba>) -> Self {
        self.bar = rgba.map(|c| yuv709(c.0, c.1, c.2));
        self.bar_rgb = rgba;
        self.plan_bar()
    }

    /// Where the Look puts the bar and how thick it is.
    pub fn with_bar_look(mut self, look: Option<&crate::look::BarLook>) -> Self {
        if let Some(l) = look {
            self.bar_top = l.pos == Some(crate::look::BarPos::Top);
            self.bar_height = l.height.unwrap_or(1.0);
            self.bar_look = Some(l.clone());
        }
        self.plan_bar()
    }

    /// The shaped bar (inset, rounded ends, track, glow), planned once for the
    /// clip when the Look asks for one. A bar without those fields is the plain
    /// drawing.
    fn plan_bar(mut self) -> Self {
        self.bar_fx = match (self.bar_rgb, &self.bar_look) {
            (Some(rgb), Some(l)) if l.shaped() => Some(crate::bar::BarFx::new(self.canvas, l, rgb)),
            _ => None,
        };
        // The plain bar's alpha: the fill colour's own times the Look's `opacity`
        // for the fill, the Look's `opacity` for the dimmed track.
        let elem = self
            .bar_look
            .as_ref()
            .and_then(|l| l.opacity)
            .unwrap_or(1.0)
            .clamp(0.0, 1.0) as f32;
        self.bar_alpha = (
            self.bar_rgb.map_or(1.0, |c| c.3 as f32 / 255.0) * elem,
            elem,
        );
        self
    }

    pub fn src_geom(&self) -> Geom {
        self.src
    }

    /// Compose one frame: `frame` is a decoded yuv420p source frame, `rect`
    /// the camera rect in decoded pixels. `flash` 0..1 blends toward white
    /// (merge-join dips); `progress` 0..1 fills the progress bar (if on).
    /// Returns the finished yuv420p canvas frame.
    pub fn compose(
        &mut self,
        frame: &[u8],
        rect: Rect,
        flash: f32,
        progress: f32,
    ) -> anyhow::Result<&[u8]> {
        let src = self.src;
        if frame.len() < src.frame_len() {
            anyhow::bail!("short source frame ({} < {})", frame.len(), src.frame_len());
        }
        let canvas = self.canvas;
        let r = rect.clamped(src.w as f64, src.h as f64);
        let lay = layout(&r, canvas);
        if !lay.covers(canvas) {
            self.blur_fill(frame, &r)?;
        }
        self.place(frame, &lay.src, (lay.dx, lay.dy, lay.dw, lay.dh))?;
        self.finish(flash, progress);
        Ok(&self.out)
    }

    /// Compose a split-screen frame: `top` fills the upper half, `bottom`
    /// the lower (camera rects in decoded pixels, trimmed to the half's
    /// aspect around their centers).
    pub fn compose_split(
        &mut self,
        frame: &[u8],
        top: Rect,
        bottom: Rect,
        flash: f32,
        progress: f32,
    ) -> anyhow::Result<&[u8]> {
        let src = self.src;
        if frame.len() < src.frame_len() {
            anyhow::bail!("short source frame ({} < {})", frame.len(), src.frame_len());
        }
        let (out_w, out_h) = (self.canvas.w, self.canvas.h);
        // Even halves (4:2:0 chroma rows).
        let half = split_rows(out_h, self.split);
        for (r, dy, dh) in [(top, 0, half), (bottom, half, out_h - half)] {
            let r = r.clamped(src.w as f64, src.h as f64);
            let aspect = out_w as f64 / dh as f64;
            let (w, h) = if r.w / r.h > aspect {
                (r.h * aspect, r.h)
            } else {
                (r.w, r.w / aspect)
            };
            self.place(
                frame,
                &Rect::from_center(r.cx(), r.cy(), w, h),
                (0, dy, out_w, dh),
            )?;
        }
        self.finish(flash, progress);
        Ok(&self.out)
    }

    /// Resample source rect `r` into the output region `d` (all planes).
    fn place(&mut self, frame: &[u8], r: &Rect, d: (u32, u32, u32, u32)) -> anyhow::Result<()> {
        let src = self.src;
        let (out_w, out_h) = (self.canvas.w, self.canvas.h);
        let out_g = Geom { w: out_w, h: out_h };
        let (sy, su, sv) = src.split(frame);
        let (oy, ou, ov) = out_g.split_mut(&mut self.out);
        let fg = ResizeAlg::Convolution(FilterType::CatmullRom);
        let (ry, ruv) = (&mut self.ry, &mut self.ruv);
        // Luma on this thread, both chroma planes on a helper: the three
        // resizes are independent and luma is 2/3 of the work.
        std::thread::scope(|s| -> anyhow::Result<()> {
            let chroma = s.spawn(move || -> anyhow::Result<()> {
                let c = r.scaled(0.5);
                let (cw, ch) = (src.cw(), src.ch());
                let dc = (d.0 / 2, d.1 / 2, d.2 / 2, d.3 / 2);
                resize_into(ruv, su, cw, ch, &c, ou, out_w / 2, out_h / 2, dc, fg)?;
                resize_into(ruv, sv, cw, ch, &c, ov, out_w / 2, out_h / 2, dc, fg)
            });
            resize_into(ry, sy, src.w, src.h, r, oy, out_w, out_h, d, fg)?;
            chroma
                .join()
                .map_err(|_| anyhow::anyhow!("chroma worker panicked"))?
        })
    }

    /// Picture effects, flash dip and progress bar over the composed frame
    /// (in that order: the bar keeps its colours, the dip washes everything).
    fn finish(&mut self, flash: f32, progress: f32) {
        let canvas = self.canvas;
        let out_g = Geom {
            w: canvas.w,
            h: canvas.h,
        };
        if let Some(fx) = &self.fx {
            fx.apply(&mut self.out, canvas);
        }
        if flash > 0.0 {
            let a = flash.clamp(0.0, 1.0);
            let (oy, uv) = self.out.split_at_mut(out_g.luma_len());
            for p in oy.iter_mut() {
                *p = (*p as f32 + (235.0 - *p as f32) * a).round() as u8;
            }
            for p in uv.iter_mut() {
                *p = (128.0 + (*p as f32 - 128.0) * (1.0 - a)).round() as u8;
            }
        }
        if let (Some(_), Some(fx)) = (self.bar, &self.bar_fx) {
            fx.draw(&mut self.out, progress);
        } else if let Some(color) = self.bar {
            if self.bar_alpha == (1.0, 1.0) {
                draw_bar_at(
                    &mut self.out,
                    canvas,
                    color,
                    progress,
                    self.bar_top,
                    self.bar_height,
                );
            } else {
                draw_bar_over(
                    &mut self.out,
                    canvas,
                    color,
                    progress,
                    (self.bar_top, self.bar_height),
                    self.bar_alpha,
                );
            }
        }
    }

    /// Blurred, dimmed fill behind a letterboxed framing: the 9:16 slice of
    /// the source around the rect's center, shrunk to 36x64, box-blurred,
    /// then stretched back up bilinearly (a wide, smooth blur for ~2 ms).
    fn blur_fill(&mut self, frame: &[u8], r: &Rect) -> anyhow::Result<()> {
        let src = self.src;
        let (sw, sh) = (src.w as f64, src.h as f64);
        let b = self.canvas.base_rect(sw, sh);
        let region = Rect::from_center(r.cx(), r.cy(), b.w, b.h).clamped(sw, sh);
        let bg_w = self.bg_w;
        let (out_w, out_h) = (self.canvas.w, self.canvas.h);
        let tiny_g = Geom { w: bg_w, h: BG_H };
        let (sy, su, sv) = src.split(frame);
        {
            let (ty, tu, tv) = tiny_g.split_mut(&mut self.bg_tiny);
            let down = ResizeAlg::Convolution(FilterType::Box);
            let full = |w, h| (0, 0, w, h);
            resize_into(
                &mut self.ry,
                sy,
                src.w,
                src.h,
                &region,
                ty,
                bg_w,
                BG_H,
                full(bg_w, BG_H),
                down,
            )?;
            let c = region.scaled(0.5);
            let (cw, ch) = (tiny_g.cw(), tiny_g.ch());
            resize_into(
                &mut self.ruv,
                su,
                src.cw(),
                src.ch(),
                &c,
                tu,
                cw,
                ch,
                full(cw, ch),
                down,
            )?;
            resize_into(
                &mut self.ruv,
                sv,
                src.cw(),
                src.ch(),
                &c,
                tv,
                cw,
                ch,
                full(cw, ch),
                down,
            )?;
        }
        // Two box passes per plane ≈ a soft Gaussian.
        {
            let (ty, tu, tv) = tiny_g.split_mut(&mut self.bg_tiny);
            let (sy2, su2, sv2) = tiny_g.split_mut(&mut self.bg_scratch);
            for _ in 0..2 {
                box_blur(ty, sy2, bg_w as usize, BG_H as usize, 2);
                box_blur(tu, su2, tiny_g.cw() as usize, tiny_g.ch() as usize, 1);
                box_blur(tv, sv2, tiny_g.cw() as usize, tiny_g.ch() as usize, 1);
            }
            for p in ty.iter_mut() {
                *p = (16.0 + (*p as f32 - 16.0).max(0.0) * self.fill_gain).round() as u8;
            }
            if self.fill_chroma < 1.0 {
                for p in tu.iter_mut().chain(tv.iter_mut()) {
                    *p = (128.0 + (*p as f32 - 128.0) * self.fill_chroma).round() as u8;
                }
            }
        }
        let out_g = Geom { w: out_w, h: out_h };
        let (ty, tu, tv) = tiny_g.split(&self.bg_tiny);
        let (oy, ou, ov) = out_g.split_mut(&mut self.out);
        let up = ResizeAlg::Convolution(FilterType::Bilinear);
        let whole = |w: u32, h: u32| Rect {
            x: 0.0,
            y: 0.0,
            w: w as f64,
            h: h as f64,
        };
        resize_into(
            &mut self.ry,
            ty,
            bg_w,
            BG_H,
            &whole(bg_w, BG_H),
            oy,
            out_w,
            out_h,
            (0, 0, out_w, out_h),
            up,
        )?;
        let (cw, ch) = (tiny_g.cw(), tiny_g.ch());
        let dc = (0, 0, out_w / 2, out_h / 2);
        resize_into(
            &mut self.ruv,
            tu,
            cw,
            ch,
            &whole(cw, ch),
            ou,
            out_w / 2,
            out_h / 2,
            dc,
            up,
        )?;
        resize_into(
            &mut self.ruv,
            tv,
            cw,
            ch,
            &whole(cw, ch),
            ov,
            out_w / 2,
            out_h / 2,
            dc,
            up,
        )?;
        Ok(())
    }
}

/// The colour of the progress bar: the flat `progress_bar` option switches the
/// bar on and sets its colour; the Look's `bar.color` replaces the colour but
/// never switches a bar on.
pub fn bar_color(
    flat: Option<crate::look::Rgba>,
    look: Option<&crate::look::BarLook>,
) -> Option<crate::look::Rgba> {
    flat.map(|f| look.and_then(|l| l.color).unwrap_or(f))
}

/// Progress bar along the bottom edge: the filled part in `color`, the
/// rest a dimmed track so the bar reads on any footage. Pure (unit-tested).
pub fn draw_bar(out: &mut [u8], c: Canvas, color: (u8, u8, u8), progress: f32) {
    draw_bar_at(out, c, color, progress, false, 1.0);
}

/// [`draw_bar_at`] with alpha: the dimmed track is laid over the picture at
/// `track_a`, then the fill over that at `fill_a` (straight alpha, each layer
/// rounded to 8 bits; a `fill_a` and `track_a` of 1 give [`draw_bar_at`]'s pixels).
pub fn draw_bar_over(
    out: &mut [u8],
    c: Canvas,
    color: (u8, u8, u8),
    progress: f32,
    (top, height): (bool, f64),
    (fill_a, track_a): (f32, f32),
) {
    let mix = |from: u8, to: u8, a: f32| -> u8 {
        let (f, t) = (from as f32, to as f32);
        (f + (t - f) * a.clamp(0.0, 1.0)).round().clamp(0.0, 255.0) as u8
    };
    let g = Geom { w: c.w, h: c.h };
    let bh = bar_thickness(c, height);
    let fill = ((progress.clamp(0.0, 1.0) as f64 * c.w as f64 / 2.0).round() as u32 * 2).min(c.w);
    let (w, y0) = (c.w as usize, if top { 0 } else { (c.h - bh) as usize });
    let y1 = y0 + bh as usize;
    let (oy, uv) = out.split_at_mut(g.luma_len());
    for y in y0..y1 {
        let row = &mut oy[y * w..(y + 1) * w];
        for (x, p) in row.iter_mut().enumerate() {
            let dim = 16 + (p.saturating_sub(16) as u32 * 45 / 100) as u8;
            let under = mix(*p, dim, track_a);
            *p = if (x as u32) < fill {
                mix(under, color.0, fill_a)
            } else {
                under
            };
        }
    }
    let (cw, cl) = (g.cw() as usize, g.chroma_len());
    let (ou, ov) = uv.split_at_mut(cl);
    for (plane, val) in [(ou, color.1), (&mut ov[..cl], color.2)] {
        for y in y0 / 2..y1 / 2 {
            let row = &mut plane[y * cw..(y + 1) * cw];
            for (x, p) in row.iter_mut().enumerate() {
                let dim = (128 + (*p as i32 - 128) / 2) as u8;
                let under = mix(*p, dim, track_a);
                *p = if ((x * 2) as u32) < fill {
                    mix(under, val, fill_a)
                } else {
                    under
                };
            }
        }
    }
}

/// Bar thickness in px (even): 0.65% of the height, at least 8, times the
/// Look's `height` multiplier (never under 4).
pub fn bar_thickness(c: Canvas, height: f64) -> u32 {
    let even = |v: f64| (v / 2.0).round() as u32 * 2;
    even(c.h as f64 * 0.0065 * height)
        .max(even(8.0 * height))
        .max(4)
        .min(c.h & !1)
}

/// [`draw_bar`] along the top or bottom edge, `height` times as thick.
pub fn draw_bar_at(
    out: &mut [u8],
    c: Canvas,
    color: (u8, u8, u8),
    progress: f32,
    top: bool,
    height: f64,
) {
    let g = Geom { w: c.w, h: c.h };
    let bh = bar_thickness(c, height);
    let fill = ((progress.clamp(0.0, 1.0) as f64 * c.w as f64 / 2.0).round() as u32 * 2).min(c.w);
    let (w, y0) = (c.w as usize, if top { 0 } else { (c.h - bh) as usize });
    let y1 = y0 + bh as usize;
    let (oy, uv) = out.split_at_mut(g.luma_len());
    for y in y0..y1 {
        let row = &mut oy[y * w..(y + 1) * w];
        for (x, p) in row.iter_mut().enumerate() {
            *p = if (x as u32) < fill {
                color.0
            } else {
                16 + (p.saturating_sub(16) as u32 * 45 / 100) as u8
            };
        }
    }
    let (cw, cl) = (g.cw() as usize, g.chroma_len());
    let (ou, ov) = uv.split_at_mut(cl);
    for (plane, val) in [(ou, color.1), (&mut ov[..cl], color.2)] {
        for y in y0 / 2..y1 / 2 {
            let row = &mut plane[y * cw..(y + 1) * cw];
            for (x, p) in row.iter_mut().enumerate() {
                *p = if ((x * 2) as u32) < fill {
                    val
                } else {
                    (128 + (*p as i32 - 128) / 2) as u8
                };
            }
        }
    }
}

/// Resize the fractional `crop` of one source plane into the `dst` rect of
/// one output plane.
#[allow(clippy::too_many_arguments)]
fn resize_into(
    resizer: &mut Resizer,
    src: &[u8],
    sw: u32,
    sh: u32,
    crop: &Rect,
    dst: &mut [u8],
    dw_full: u32,
    dh_full: u32,
    d: (u32, u32, u32, u32),
    alg: ResizeAlg,
) -> anyhow::Result<()> {
    let (dx, dy, dw, dh) = d;
    if dw == 0 || dh == 0 {
        return Ok(());
    }
    let src_img = ImageRef::new(sw, sh, src, PixelType::U8)?;
    let mut dst_img = Image::from_slice_u8(dw_full, dh_full, dst, PixelType::U8)?;
    let mut view = CroppedImageMut::new(&mut dst_img, dx, dy, dw, dh)?;
    let c = crop.clamped(sw as f64, sh as f64);
    let opts = ResizeOptions::new()
        .resize_alg(alg)
        .crop(c.x, c.y, c.w.max(1.0), c.h.max(1.0));
    resizer.resize(&src_img, &mut view, &opts)?;
    Ok(())
}

/// Separable box blur (radius `r`, edge-clamped), in place via `tmp`.
fn box_blur(p: &mut [u8], tmp: &mut [u8], w: usize, h: usize, r: usize) {
    let n = (2 * r + 1) as u32;
    for y in 0..h {
        for x in 0..w {
            let mut s = 0u32;
            for k in 0..n as usize {
                let xx = (x + k).saturating_sub(r).min(w - 1);
                s += p[y * w + xx] as u32;
            }
            tmp[y * w + x] = ((s + n / 2) / n) as u8;
        }
    }
    for y in 0..h {
        for x in 0..w {
            let mut s = 0u32;
            for k in 0..n as usize {
                let yy = (y + k).saturating_sub(r).min(h - 1);
                s += tmp[yy * w + x] as u32;
            }
            p[y * w + x] = ((s + n / 2) / n) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TALL: Canvas = Canvas::TALL;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn nine_sixteen_rects_fill_the_frame_even_when_rounded() {
        // The old path's 156x276 crop left 5px bars; near-9:16 now fills.
        let l = layout(&rect(380.0, 0.0, 156.0, 276.0), TALL);
        assert!(l.covers(TALL), "{l:?}");
        assert!((l.src.w / l.src.h - 9.0 / 16.0).abs() < 1e-9);
        assert!((l.src.cx() - 458.0).abs() < 1e-9, "trim stays centered");
    }

    #[test]
    fn wide_rects_letterbox_centered_with_matching_aspect() {
        let l = layout(&rect(0.0, 0.0, 640.0, 360.0), TALL);
        assert_eq!((l.dx, l.dw), (0, 1080));
        assert_eq!(l.dh, 608);
        assert_eq!(l.dy, 656);
        assert_eq!(l.dy % 2, 0);
        // No distortion: sampled aspect equals destination aspect.
        let sa = l.src.w / l.src.h;
        let da = l.dw as f64 / l.dh as f64;
        assert!((sa - da).abs() < 1e-9, "{sa} vs {da}");
    }

    #[test]
    fn tall_rects_pillarbox() {
        // A framing narrower than 9:16 (0.45) shown whole.
        let l = layout(&rect(0.0, 0.0, 720.0, 1600.0), TALL);
        assert_eq!((l.dy, l.dh), (0, 1920));
        assert_eq!((l.dx, l.dw), (108, 864));
        let (sa, da) = (l.src.w / l.src.h, l.dw as f64 / l.dh as f64);
        assert!((sa - da).abs() < 1e-9, "{sa} vs {da}");
    }

    #[test]
    fn base_rect_is_the_largest_canvas_rect() {
        let b = TALL.base_rect(640.0, 360.0);
        assert!((b.w - 202.5).abs() < 1e-9 && (b.h - 360.0).abs() < 1e-9);
        assert!((b.x - 218.75).abs() < 1e-9);
        // Very tall portrait: width-bound.
        let t = TALL.base_rect(1080.0, 2400.0);
        assert!((t.w - 1080.0).abs() < 1e-9 && (t.h - 1920.0).abs() < 1e-9);
        // Square from 16:9: full height; 16:9 from 16:9: the whole frame.
        let q = Canvas::SQUARE.base_rect(1920.0, 1080.0);
        assert!((q.w - 1080.0).abs() < 1e-9 && (q.x - 420.0).abs() < 1e-9);
        let w = Canvas::WIDE.base_rect(1920.0, 1080.0);
        assert!((w.w - 1920.0).abs() < 1e-9 && (w.h - 1080.0).abs() < 1e-9);
        // 16:9 from a vertical phone video: width-bound, centered.
        let v = Canvas::WIDE.base_rect(1080.0, 1920.0);
        assert!((v.w - 1080.0).abs() < 1e-9 && (v.h - 607.5).abs() < 1e-9);
    }

    #[test]
    fn canvas_parse_and_tags() {
        assert_eq!(Canvas::parse("9:16"), Some(Canvas::TALL));
        assert_eq!(Canvas::parse("4x5"), Some(Canvas::PORTRAIT));
        assert_eq!(Canvas::parse(" 1:1 "), Some(Canvas::SQUARE));
        assert_eq!(Canvas::parse("16:9"), Some(Canvas::WIDE));
        assert_eq!(Canvas::parse("3:2"), None);
        assert_eq!(Canvas::WIDE.tag(), "16x9");
        assert_eq!(Canvas::default(), Canvas::TALL);
        assert_eq!(parse_hex("#FFD400"), Some((255, 212, 0)));
        assert_eq!(parse_hex("zz0000"), None);
        // Reference BT.709 limited values.
        assert_eq!(yuv709(255, 255, 255), (235, 128, 128));
        assert_eq!(yuv709(0, 0, 0), (16, 128, 128));
    }

    /// Horizontal luma ramp source, `w` x `h`.
    fn ramp(w: u32, h: u32) -> Vec<u8> {
        let g = Geom { w, h };
        let mut f = vec![128u8; g.frame_len()];
        for y in 0..h as usize {
            for x in 0..w as usize {
                f[y * w as usize + x] = (16 + (x * 200) / w as usize) as u8;
            }
        }
        f
    }

    #[test]
    fn subpixel_pans_move_the_picture_subpixel() {
        let (w, h) = (640u32, 360u32);
        let src = ramp(w, h);
        let mut c = Compositor::new(w, h, TALL);
        let base = TALL.base_rect(w as f64, h as f64);
        let mean_row = |c: &mut Compositor, dx: f64| -> f64 {
            let r = Rect {
                x: base.x + dx,
                ..base
            };
            let out = c.compose(&src, r, 0.0, 0.0).unwrap();
            let row = &out[960 * 1080..961 * 1080];
            row.iter().map(|&v| v as f64).sum::<f64>() / row.len() as f64
        };
        let m0 = mean_row(&mut c, 0.0);
        let m1 = mean_row(&mut c, 0.25);
        let m2 = mean_row(&mut c, 0.5);
        // A quarter-pixel pan must move the image (integer crop could not),
        // and monotonically.
        assert!(m1 > m0 && m2 > m1, "{m0} {m1} {m2}");
        // ~200/640 luma per source px → 0.25px ≈ 0.078.
        assert!((m1 - m0 - 0.078).abs() < 0.03, "step {}", m1 - m0);
    }

    #[test]
    fn letterbox_gets_a_dim_blur_fill_and_flash_whitens() {
        let (w, h) = (640u32, 360u32);
        let src = ramp(w, h);
        let mut c = Compositor::new(w, h, TALL);
        let out = c
            .compose(&src, rect(0.0, 0.0, 640.0, 360.0), 0.0, 0.0)
            .unwrap()
            .to_vec();
        // Top bar row is filled (not black), and dimmer than the picture.
        let top = &out[10 * 1080..11 * 1080];
        let mid = &out[960 * 1080..961 * 1080];
        let mean = |r: &[u8]| r.iter().map(|&v| v as f64).sum::<f64>() / r.len() as f64;
        assert!(mean(top) > 30.0, "fill present: {}", mean(top));
        assert!(mean(top) < mean(mid), "fill dimmer than picture");
        let white = c
            .compose(&src, TALL.base_rect(640.0, 360.0), 1.0, 0.0)
            .unwrap()
            .to_vec();
        assert!(white[..1080 * 1920].iter().all(|&v| v == 235));
        assert!(white[1080 * 1920..].iter().all(|&v| v == 128));
    }

    #[test]
    fn other_canvases_fill_exactly() {
        let (w, h) = (1280u32, 720u32);
        let src = ramp(w, h);
        for canvas in [Canvas::PORTRAIT, Canvas::SQUARE, Canvas::WIDE] {
            let base = canvas.base_rect(w as f64, h as f64);
            assert!(layout(&base, canvas).covers(canvas), "{canvas:?}");
            let mut c = Compositor::new(w, h, canvas);
            let out = c.compose(&src, base, 0.0, 0.0).unwrap();
            let g = Geom {
                w: canvas.w,
                h: canvas.h,
            };
            assert_eq!(out.len(), g.frame_len());
            // Picture everywhere: the ramp's left edge is dark, right bright.
            let row = &out[(canvas.h as usize / 2) * canvas.w as usize..][..canvas.w as usize];
            assert!(row[4] < row[canvas.w as usize - 4], "{canvas:?}");
        }
        // A 16:9 wide shot on a square canvas letterboxes over the fill.
        let l = layout(&rect(0.0, 0.0, 1280.0, 720.0), Canvas::SQUARE);
        assert_eq!((l.dw, l.dh), (1080, 608));
    }

    /// The bar exactly as it was drawn before the Look could move it.
    fn old_bar(out: &mut [u8], c: Canvas, color: (u8, u8, u8), progress: f32) {
        let g = Geom { w: c.w, h: c.h };
        let bh = (((c.h as f64 * 0.0065) / 2.0).round() as u32 * 2).max(8);
        let fill =
            ((progress.clamp(0.0, 1.0) as f64 * c.w as f64 / 2.0).round() as u32 * 2).min(c.w);
        let (w, y0) = (c.w as usize, (c.h - bh) as usize);
        let (oy, uv) = out.split_at_mut(g.luma_len());
        for y in y0..c.h as usize {
            for (x, p) in oy[y * w..(y + 1) * w].iter_mut().enumerate() {
                *p = if (x as u32) < fill {
                    color.0
                } else {
                    16 + (p.saturating_sub(16) as u32 * 45 / 100) as u8
                };
            }
        }
        let (cw, cl) = (g.cw() as usize, g.chroma_len());
        let (ou, ov) = uv.split_at_mut(cl);
        for (plane, val) in [(ou, color.1), (&mut ov[..cl], color.2)] {
            for y in y0 / 2..g.ch() as usize {
                for (x, p) in plane[y * cw..(y + 1) * cw].iter_mut().enumerate() {
                    *p = if ((x * 2) as u32) < fill {
                        val
                    } else {
                        (128 + (*p as i32 - 128) / 2) as u8
                    };
                }
            }
        }
    }

    fn gradient_frame(c: Canvas) -> Vec<u8> {
        let g = Geom { w: c.w, h: c.h };
        let mut f: Vec<u8> = (0..g.frame_len())
            .map(|i| (i * 7 % 200 + 20) as u8)
            .collect();
        f[g.luma_len()..].fill(110);
        f
    }

    #[test]
    fn default_and_empty_bar_looks_draw_the_same_pixels_as_before() {
        let yellow = yuv709(255, 212, 0);
        for c in [Canvas::TALL, Canvas::SQUARE, Canvas::WIDE, Canvas::PORTRAIT] {
            for p in [0.0f32, 0.37, 1.0] {
                let mut want = gradient_frame(c);
                old_bar(&mut want, c, yellow, p);
                let mut got = gradient_frame(c);
                draw_bar(&mut got, c, yellow, p);
                assert!(got == want, "{c:?} {p}");
                let mut got = gradient_frame(c);
                draw_bar_at(&mut got, c, yellow, p, false, 1.0);
                assert!(got == want, "{c:?} {p}");
            }
            // Through the compositor: no look, an empty one, a bottom one.
            let src = ramp(640, 360);
            let base = c.base_rect(640.0, 360.0);
            let mut frames = Vec::new();
            for look in [
                None,
                Some(crate::look::BarLook::default()),
                Some(crate::look::BarLook {
                    pos: Some(crate::look::BarPos::Bottom),
                    height: Some(1.0),
                    ..Default::default()
                }),
            ] {
                let mut comp = Compositor::new(640, 360, c)
                    .with_bar(Some((255, 212, 0)))
                    .with_bar_look(look.as_ref());
                frames.push(comp.compose(&src, base, 0.0, 0.5).unwrap().to_vec());
            }
            assert!(frames[0] == frames[1] && frames[0] == frames[2], "{c:?}");
        }
    }

    #[test]
    fn bar_thickness_follows_the_height_multiplier() {
        // Today: 0.65% of the height, even, at least 8.
        assert_eq!(bar_thickness(Canvas::TALL, 1.0), 12);
        assert_eq!(bar_thickness(Canvas::SQUARE, 1.0), 8);
        assert_eq!(bar_thickness(Canvas::WIDE, 1.0), 8);
        assert_eq!(bar_thickness(Canvas::TALL, 2.0), 24);
        assert_eq!(bar_thickness(Canvas::TALL, 0.5), 6);
        assert_eq!(bar_thickness(Canvas::SQUARE, 0.5), 4);
        assert_eq!(bar_thickness(Canvas::SQUARE, 3.0), 24);
        for c in [Canvas::TALL, Canvas::SQUARE, Canvas::WIDE, Canvas::PORTRAIT] {
            for h in [0.5, 0.75, 1.0, 1.5, 2.0, 3.0] {
                let t = bar_thickness(c, h);
                assert!(t.is_multiple_of(2) && t >= 4, "{c:?} {h} -> {t}");
            }
            assert!(bar_thickness(c, 3.0) > bar_thickness(c, 1.0));
            assert!(bar_thickness(c, 0.5) < bar_thickness(c, 1.0));
        }
    }

    #[test]
    fn bar_rows_land_at_the_top_or_bottom_with_the_right_thickness() {
        let yellow = yuv709(255, 212, 0);
        for c in [Canvas::TALL, Canvas::SQUARE, Canvas::WIDE] {
            let g = Geom { w: c.w, h: c.h };
            let w = c.w as usize;
            for height in [0.5, 1.0, 2.0, 3.0] {
                let bh = bar_thickness(c, height) as usize;
                for top in [true, false] {
                    let mut out = vec![100u8; g.frame_len()];
                    out[g.luma_len()..].fill(128);
                    draw_bar_at(&mut out, c, yellow, 1.0, top, height);
                    let rows: Vec<usize> = (0..c.h as usize)
                        .filter(|&y| out[y * w..(y + 1) * w].iter().all(|&v| v == yellow.0))
                        .collect();
                    let want: Vec<usize> = if top {
                        (0..bh).collect()
                    } else {
                        (c.h as usize - bh..c.h as usize).collect()
                    };
                    assert_eq!(rows, want, "{c:?} height {height} top {top}");
                    // Every other luma row is untouched picture.
                    for y in (0..c.h as usize).filter(|y| !want.contains(y)) {
                        assert!(out[y * w..(y + 1) * w].iter().all(|&v| v == 100));
                    }
                    // Chroma: the bar's rows carry the colour, the rest do not.
                    let cw = g.cw() as usize;
                    let u = &out[g.luma_len()..];
                    for y in 0..g.ch() as usize {
                        let on = want.contains(&(y * 2));
                        let row = &u[y * cw..(y + 1) * cw];
                        assert_eq!(
                            row.iter().all(|&v| v == yellow.1),
                            on,
                            "{c:?} chroma row {y}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_top_bar_fills_left_to_right_over_a_dim_track() {
        let c = Canvas::SQUARE;
        let g = Geom { w: c.w, h: c.h };
        let mut out = vec![200u8; g.frame_len()];
        out[g.luma_len()..].fill(128);
        let yellow = yuv709(255, 212, 0);
        draw_bar_at(&mut out, c, yellow, 0.5, true, 1.0);
        let first = &out[..c.w as usize];
        assert_eq!((first[0], first[539]), (yellow.0, yellow.0));
        assert!(first[541] < 110, "track dimmed: {}", first[541]);
        // Bottom edge untouched.
        let last = &out[(c.h as usize - 1) * c.w as usize..][..c.w as usize];
        assert!(last.iter().all(|&v| v == 200));
    }

    #[test]
    fn the_compositor_puts_the_looked_bar_where_the_look_says() {
        let src = ramp(640, 360);
        let c = Canvas::PORTRAIT;
        let base = c.base_rect(640.0, 360.0);
        let look = crate::look::BarLook {
            pos: Some(crate::look::BarPos::Top),
            height: Some(2.0),
            ..Default::default()
        };
        let mut comp = Compositor::new(640, 360, c)
            .with_bar(Some((255, 212, 0)))
            .with_bar_look(Some(&look));
        let out = comp.compose(&src, base, 0.0, 1.0).unwrap().to_vec();
        let y = yuv709(255, 212, 0).0;
        let bh = bar_thickness(c, 2.0) as usize;
        let w = c.w as usize;
        assert!(out[..bh * w].iter().all(|&v| v == y));
        assert!(!out[bh * w..(bh + 1) * w].iter().all(|&v| v == y));
        assert!(!out[(c.h as usize - 1) * w..c.h as usize * w]
            .iter()
            .all(|&v| v == y));
        // Bar off: the look alone draws nothing.
        let mut comp = Compositor::new(640, 360, c).with_bar_look(Some(&look));
        let plain = comp.compose(&src, base, 0.0, 1.0).unwrap().to_vec();
        let mut none = Compositor::new(640, 360, c);
        assert!(plain == none.compose(&src, base, 0.0, 1.0).unwrap());
    }

    #[test]
    fn progress_bar_fills_left_to_right_over_a_dim_track() {
        let c = Canvas::TALL;
        let g = Geom { w: c.w, h: c.h };
        let mut out = vec![0u8; g.frame_len()];
        out[..g.luma_len()].fill(200);
        out[g.luma_len()..].fill(128);
        let yellow = yuv709(255, 212, 0);
        draw_bar(&mut out, c, yellow, 0.5);
        let last = &out[(c.h as usize - 1) * c.w as usize..][..c.w as usize];
        assert_eq!(last[0], yellow.0);
        assert_eq!(last[539], yellow.0);
        assert!(last[541] < 110, "track dimmed: {}", last[541]);
        // Above the bar: untouched picture.
        let above = &out[(c.h as usize - 20) * c.w as usize..][..c.w as usize];
        assert!(above.iter().all(|&v| v == 200));
        // Chroma of the filled part carries the color.
        let u = &out[g.luma_len()..][(g.ch() as usize - 1) * g.cw() as usize..];
        assert_eq!(u[0], yellow.1);
        // Full.
        let mut o2 = vec![100u8; g.frame_len()];
        draw_bar(&mut o2, c, yellow, 1.0);
        let last = &o2[(c.h as usize - 1) * c.w as usize..][..c.w as usize];
        assert!(last.iter().all(|&v| v == yellow.0));
    }

    // ---- effects, fill dimming and the split seam ---------------------------

    use crate::look::{EffectsLook, Grade, LayoutLook};

    fn fx(vignette: Option<f64>, grade: Option<Grade>, fill_dim: Option<f64>) -> EffectsLook {
        EffectsLook {
            vignette,
            grade,
            fill_dim,
        }
    }

    /// A flat yuv420p frame.
    fn flat(w: u32, h: u32, y: u8, u: u8, v: u8) -> Vec<u8> {
        let g = Geom { w, h };
        let mut f = vec![y; g.frame_len()];
        f[g.luma_len()..g.luma_len() + g.chroma_len()].fill(u);
        f[g.luma_len() + g.chroma_len()..].fill(v);
        f
    }

    /// A picture with real range: a luma ramp across, a chroma sweep down.
    fn colourful(w: u32, h: u32) -> Vec<u8> {
        let g = Geom { w, h };
        let mut f = vec![0u8; g.frame_len()];
        for y in 0..h as usize {
            for x in 0..w as usize {
                f[y * w as usize + x] = (16 + (x * 219) / w as usize) as u8;
            }
        }
        let (cw, ch) = (g.cw() as usize, g.ch() as usize);
        for y in 0..ch {
            for x in 0..cw {
                f[g.luma_len() + y * cw + x] = (64 + (y * 128) / ch) as u8;
                f[g.luma_len() + g.chroma_len() + y * cw + x] = (64 + (x * 128) / cw) as u8;
            }
        }
        f
    }

    /// sRGB (0..255, unclamped) of a limited-range BT.709 sample.
    fn rgb(p: (u8, u8, u8)) -> (f64, f64, f64) {
        let yy = (p.0 as f64 - 16.0) / 219.0;
        let (cb, cr) = ((p.1 as f64 - 128.0) / 224.0, (p.2 as f64 - 128.0) / 224.0);
        (
            (yy + 1.5748 * cr) * 255.0,
            (yy - 0.1873 * cb - 0.4681 * cr) * 255.0,
            (yy + 1.8556 * cb) * 255.0,
        )
    }

    /// The (y, u, v) of output pixel (x, y).
    fn px(out: &[u8], c: Canvas, x: usize, y: usize) -> (u8, u8, u8) {
        let g = Geom { w: c.w, h: c.h };
        (
            out[y * c.w as usize + x],
            out[g.luma_len() + (y / 2) * g.cw() as usize + x / 2],
            out[g.luma_len() + g.chroma_len() + (y / 2) * g.cw() as usize + x / 2],
        )
    }

    fn compose_with(
        c: Canvas,
        src: &[u8],
        r: Rect,
        look: Option<&EffectsLook>,
        bar: bool,
        progress: f32,
    ) -> Vec<u8> {
        let mut comp = Compositor::new(640, 360, c).with_effects(look);
        if bar {
            comp = comp.with_bar(Some((255, 212, 0)));
        }
        comp.compose(src, r, 0.0, progress).unwrap().to_vec()
    }

    #[test]
    fn absent_and_neutral_effects_render_todays_pixels() {
        let c = Canvas::SQUARE;
        let src = colourful(640, 360);
        // The second rect letterboxes, so a blur fill sits behind it.
        for r in [c.base_rect(640.0, 360.0), rect(0.0, 0.0, 640.0, 360.0)] {
            let want = compose_with(c, &src, r, None, true, 0.4);
            for look in [
                EffectsLook::default(),
                fx(Some(0.0), None, None),
                fx(None, Some(Grade::None), None),
                fx(Some(0.0), Some(Grade::None), None),
                fx(None, None, Some(FILL_DIM_TODAY)),
            ] {
                let got = compose_with(c, &src, r, Some(&look), true, 0.4);
                assert!(got == want, "{look:?}");
            }
        }
    }

    #[test]
    fn fill_dim_passes_through_todays_darkness() {
        assert_eq!(fill_gain(FILL_DIM_TODAY), BG_DIM);
        assert_eq!(fill_gain(0.0), 1.0);
        assert!(fill_gain(1.0) < 0.06 && fill_gain(1.0) > 0.0);
        let mut last = f32::INFINITY;
        for i in 0..=20 {
            let g = fill_gain(i as f64 / 20.0);
            assert!(g < last, "darker as it goes up: {i}");
            last = g;
        }
        assert_eq!(fill_gain(-3.0), 1.0);
        assert_eq!(fill_gain(9.0), fill_gain(1.0));
    }

    #[test]
    fn fill_dim_darkens_the_fill_and_leaves_the_video_alone() {
        let c = Canvas::TALL;
        let src = colourful(640, 360);
        let wide = rect(0.0, 0.0, 640.0, 360.0);
        let lay = layout(&wide, c);
        let (dy, dh) = (lay.dy as usize, lay.dh as usize);
        let w = c.w as usize;
        let at = |d: Option<f64>| {
            let look = fx(None, None, d);
            compose_with(c, &src, wide, Some(&look), false, 0.0)
        };
        let mean = |o: &[u8], rows: std::ops::Range<usize>| {
            let s: u64 = o[rows.start * w..rows.end * w]
                .iter()
                .map(|&v| v as u64)
                .sum();
            s as f64 / ((rows.end - rows.start) * w) as f64
        };
        let (bright, today, dark, black) = (at(Some(0.0)), at(None), at(Some(0.8)), at(Some(1.0)));
        assert!(today == at(Some(FILL_DIM_TODAY)));
        let fill = 0..dy - 4;
        let (m0, m1, m2, m3) = (
            mean(&bright, fill.clone()),
            mean(&today, fill.clone()),
            mean(&dark, fill.clone()),
            mean(&black, fill),
        );
        assert!(m0 > m1 && m1 > m2 && m2 > m3, "{m0} {m1} {m2} {m3}");
        assert!(m3 < 22.0, "nearly black: {m3}");
        assert!(m0 > m1 * 1.25, "not darkened at 0: {m0} vs {m1}");
        // The sharp video area is the same pixels whatever the fill does.
        let rows = (dy + 2) * w..(dy + dh - 2) * w;
        for o in [&bright, &dark, &black] {
            assert!(o[rows.clone()] == today[rows.clone()]);
        }
        // Its chroma too.
        let g = Geom { w: c.w, h: c.h };
        let cr = (dy / 2 + 2) * g.cw() as usize..(dy / 2 + dh / 2 - 2) * g.cw() as usize;
        assert!(black[g.luma_len()..][cr.clone()] == today[g.luma_len()..][cr]);
    }

    #[test]
    fn vignette_darkens_corners_not_the_centre() {
        let c = Canvas::TALL;
        let src = flat(640, 360, 150, 128, 128);
        let base = c.base_rect(640.0, 360.0);
        let plain = compose_with(c, &src, base, None, false, 0.0);
        let (w, h) = (c.w as usize, c.h as usize);
        let mut corners = Vec::new();
        for v in [0.25, 0.6, 1.0] {
            let o = compose_with(c, &src, base, Some(&fx(Some(v), None, None)), false, 0.0);
            let centre = px(&o, c, w / 2, h / 2).0 as i32 - px(&plain, c, w / 2, h / 2).0 as i32;
            assert!(centre.abs() <= 1, "centre moved by {centre} at {v}");
            let corner = px(&o, c, 4, 4).0;
            let edge = px(&o, c, w / 2, 4).0;
            let mid = px(&o, c, w / 2, h / 4).0;
            assert!(corner < edge && edge < px(&plain, c, w / 2, 4).0, "{v}");
            assert!(mid >= edge, "falls off toward the edge: {mid} {edge}");
            corners.push(corner);
            // Smooth: along the diagonal luma never rises, and the falloff
            // has no jump between samples 8px apart.
            let mut prev = px(&o, c, w / 2, h / 2).0 as i32;
            for k in (0..w / 2).step_by(8) {
                let (x, y) = (w / 2 - k, h / 2 - k * h / w);
                let l = px(&o, c, x, y).0 as i32;
                assert!(l <= prev + 1, "monotone: {l} after {prev}");
                assert!(prev - l <= 5, "no jump: {prev} -> {l} at {k}");
                prev = l;
            }
        }
        assert!(corners[0] > corners[1] && corners[1] > corners[2]);
        // Strong but usable: full strength still leaves some light.
        assert!(corners[2] > 16 + 20, "{}", corners[2]);
        // Black stays black.
        let blk = flat(640, 360, 16, 128, 128);
        let o = compose_with(c, &blk, base, Some(&fx(Some(1.0), None, None)), false, 0.0);
        assert!(o[..w * h].iter().all(|&v| v == 16));
    }

    #[test]
    fn vignette_does_not_band_a_smooth_gradient() {
        // The dithered 8-bit falloff keeps local averages on the smooth
        // curve: the step between block means changes by well under one
        // level from block to block (banding would show as jumps).
        let c = Canvas::SQUARE;
        let src = flat(640, 360, 120, 128, 128);
        let o = compose_with(
            c,
            &src,
            c.base_rect(640.0, 360.0),
            Some(&fx(Some(0.8), None, None)),
            false,
            0.0,
        );
        let w = c.w as usize;
        let row = c.h as usize / 2;
        let blocks: Vec<f64> = (0..w / 16)
            .map(|b| {
                let mut s = 0u32;
                for y in row..row + 16 {
                    for x in b * 16..b * 16 + 16 {
                        s += o[y * w + x] as u32;
                    }
                }
                s as f64 / 256.0
            })
            .collect();
        let steps: Vec<f64> = blocks.windows(2).map(|p| p[1] - p[0]).collect();
        let worst = steps
            .windows(2)
            .map(|p| (p[1] - p[0]).abs())
            .fold(0.0, f64::max);
        assert!(worst < 0.8, "block means kink by {worst}");
    }

    #[test]
    fn grades_move_the_picture_the_right_way() {
        let c = Canvas::SQUARE;
        let base = c.base_rect(640.0, 360.0);
        // Skin-ish, neutral and darker mid tones.
        for (y, u, v) in [(150u8, 112u8, 150u8), (128, 128, 128), (90, 120, 140)] {
            let src = flat(640, 360, y, u, v);
            let at = |g: Option<Grade>| {
                let look = g.map(|g| fx(None, Some(g), None));
                let o = compose_with(c, &src, base, look.as_ref(), false, 0.0);
                px(&o, c, 540, 540)
            };
            let plain = rgb(at(None));
            let warm = rgb(at(Some(Grade::Warm)));
            let cool = rgb(at(Some(Grade::Cool)));
            assert!(warm.0 > plain.0 + 2.0 && warm.2 < plain.2 - 2.0, "{warm:?}");
            assert!(cool.0 < plain.0 - 2.0 && cool.2 > plain.2 + 2.0, "{cool:?}");
            // Gentle: nothing moves by more than ~14 of 255.
            for g in [warm, cool] {
                assert!((g.0 - plain.0).abs() < 12.0 && (g.2 - plain.2).abs() < 14.0);
            }
            // Same light: the casts leave luma alone.
            assert_eq!(at(Some(Grade::Warm)).0, y);
            // Mono: grey, at the picture's own luma.
            let mono = at(Some(Grade::Mono));
            assert_eq!((mono.1, mono.2), (128, 128));
            assert_eq!(mono.0, y);
            let m = rgb(mono);
            assert!((m.0 - m.1).abs() < 0.5 && (m.1 - m.2).abs() < 0.5, "{m:?}");
        }
    }

    #[test]
    fn punchy_adds_contrast_and_colour_without_clipping() {
        let c = Canvas::SQUARE;
        let base = c.base_rect(640.0, 360.0);
        let src = colourful(640, 360);
        let look = fx(None, Some(Grade::Punchy), None);
        let plain = compose_with(c, &src, base, None, false, 0.0);
        let punchy = compose_with(c, &src, base, Some(&look), false, 0.0);
        let g = Geom { w: c.w, h: c.h };
        let row = c.h as usize / 2 * c.w as usize;
        // Mid-tones spread apart.
        let (a, b) = (c.w as usize * 2 / 5, c.w as usize * 3 / 5);
        let spread = |o: &[u8]| o[row + b] as i32 - o[row + a] as i32;
        assert!(spread(&punchy) > spread(&plain) + 3, "contrast");
        assert!(punchy[..g.luma_len()]
            .iter()
            .all(|&v| (16..=235).contains(&v)));
        // More colour: chroma further from neutral overall.
        let off = |o: &[u8]| {
            o[g.luma_len()..]
                .iter()
                .map(|&v| (v as f64 - 128.0).abs())
                .sum::<f64>()
        };
        assert!(off(&punchy) > off(&plain) * 1.08);
        assert!(punchy[g.luma_len()..]
            .iter()
            .all(|&v| (16..=240).contains(&v)));
        // Skin stays skin.
        let skin = flat(640, 360, 150, 112, 150);
        let o = compose_with(c, &skin, base, Some(&look), false, 0.0);
        let p = compose_with(c, &skin, base, None, false, 0.0);
        let (r1, g1, b1) = rgb(px(&o, c, 540, 540));
        let (r0, g0, b0) = rgb(px(&p, c, 540, 540));
        assert!(r1 <= 255.0 && b1 >= 0.0, "{r1} {b1}");
        assert!((r1 - r0).abs() < 25.0 && (g1 - g0).abs() < 25.0 && (b1 - b0).abs() < 25.0);
    }

    #[test]
    fn effects_leave_the_progress_bar_alone() {
        let c = Canvas::TALL;
        let g = Geom { w: c.w, h: c.h };
        let src = colourful(640, 360);
        let base = c.base_rect(640.0, 360.0);
        let bh = bar_thickness(c, 1.0) as usize;
        let w = c.w as usize;
        let yellow = yuv709(255, 212, 0);
        for look in [
            fx(Some(1.0), None, None),
            fx(None, Some(Grade::Warm), None),
            fx(None, Some(Grade::Mono), None),
            fx(None, Some(Grade::Punchy), None),
            fx(Some(0.7), Some(Grade::Cool), Some(0.9)),
        ] {
            let o = compose_with(c, &src, base, Some(&look), true, 1.0);
            let plain = compose_with(c, &src, base, None, true, 1.0);
            // The whole (full) bar: luma rows and chroma rows.
            let y0 = (c.h as usize - bh) * w;
            let end = c.h as usize * w;
            assert!(o[y0..end] == plain[y0..end], "{look:?}");
            assert!(o[y0..end].iter().all(|&v| v == yellow.0));
            for (pl, want) in [yellow.1, yellow.2].into_iter().enumerate() {
                let off = g.luma_len() + pl * g.chroma_len();
                let r0 = off + (c.h as usize - bh) / 2 * g.cw() as usize;
                let r1 = off + g.chroma_len();
                assert!(o[r0..r1].iter().all(|&v| v == want), "{look:?}");
            }
            // ... while the picture did change.
            assert!(o != plain, "{look:?}");
            // Half-filled bar: the filled part is exact too.
            let half = compose_with(c, &src, base, Some(&look), true, 0.5);
            assert!(half[y0..y0 + 500].iter().all(|&v| v == yellow.0));
        }
    }

    #[test]
    fn split_rows_follow_the_look() {
        assert_eq!(split_rows(1920, None), 960);
        assert_eq!(split_rows(1920, Some(0.5)), 960);
        assert_eq!(split_rows(1350, None), 674);
        assert_eq!(split_rows(1350, Some(0.5)), 674);
        assert_eq!(split_rows(1920, Some(0.6)), 1152);
        assert_eq!(split_rows(1920, Some(0.4)), 768);
        assert_eq!(split_rows(1920, Some(0.7)), 1344);
        assert_eq!(split_rows(1920, Some(0.3)), 576);
        for s in [0.3, 0.45, 0.55, 0.7] {
            assert_eq!(split_rows(1350, Some(s)) % 2, 0);
        }
    }

    #[test]
    fn the_split_seam_moves_with_the_look() {
        let c = Canvas::TALL;
        let (w, h) = (640u32, 360u32);
        // Bright left half, dark right half: the top panel shows the left
        // half, the bottom the right, so the seam is where luma drops.
        let mut src = flat(w, h, 200, 128, 128);
        for y in 0..h as usize {
            src[y * w as usize + w as usize / 2..(y + 1) * w as usize].fill(60);
        }
        let top = rect(0.0, 0.0, 300.0, 266.0);
        let bottom = rect(340.0, 0.0, 300.0, 266.0);
        let frame = |l: Option<LayoutLook>| {
            Compositor::new(w, h, c)
                .with_split(l.as_ref())
                .compose_split(&src, top, bottom, 0.0, 0.0)
                .unwrap()
                .to_vec()
        };
        let seam = |l: Option<LayoutLook>| {
            let o = frame(l);
            (0..c.h as usize)
                .find(|&y| o[y * c.w as usize + 540] < 100)
                .unwrap()
        };
        let look = |s: f64| Some(LayoutLook { split: Some(s) });
        let today = seam(None);
        assert_eq!(today, 960);
        assert_eq!(seam(look(0.5)), today);
        assert_eq!(seam(Some(LayoutLook::default())), today);
        assert_eq!(seam(look(0.6)), today + 192);
        assert_eq!(seam(look(0.4)), today - 192);
        // Same pixels as today when absent or 0.5.
        assert!(frame(None) == frame(look(0.5)));
        assert!(frame(None) == frame(Some(LayoutLook::default())));
    }

    // ---- Look: the dressed bar through the compositor -----------------------

    fn bar_of(json: &str) -> crate::look::BarLook {
        crate::look::Look::parse(&format!(r#"{{"bar":{json}}}"#))
            .bar
            .unwrap()
    }

    fn with_bar_look(
        c: Canvas,
        look: Option<&crate::look::BarLook>,
        effects: Option<&EffectsLook>,
        p: f32,
    ) -> Vec<u8> {
        let src = colourful(640, 360);
        let base = c.base_rect(640.0, 360.0);
        Compositor::new(640, 360, c)
            .with_effects(effects)
            .with_bar(Some((255, 212, 0)))
            .with_bar_look(look)
            .compose(&src, base, 0.0, p)
            .unwrap()
            .to_vec()
    }

    #[test]
    fn the_bar_colour_is_the_looks_when_the_bar_is_on() {
        use crate::look::BarLook;
        let flat = Some(crate::look::Rgba::rgb(255, 212, 0));
        let red = BarLook {
            color: Some(crate::look::Rgba::rgb(255, 59, 48)),
            ..Default::default()
        };
        assert_eq!(bar_color(flat, None), flat);
        assert_eq!(bar_color(flat, Some(&BarLook::default())), flat);
        assert_eq!(
            bar_color(flat, Some(&red)),
            Some(crate::look::Rgba::rgb(255, 59, 48))
        );
        // It never switches a bar on.
        assert_eq!(bar_color(None, Some(&red)), None);
        assert_eq!(bar_color(None, None), None);
    }

    #[test]
    fn a_bar_look_without_shape_fields_is_the_plain_bar_to_the_byte() {
        let c = Canvas::TALL;
        let want = with_bar_look(c, None, None, 0.4);
        for j in [
            "{}",
            r#"{"pos":"bottom","height":1}"#,
            r##"{"color":"#FFD400"}"##,
        ] {
            let l = bar_of(j);
            assert!(!l.shaped(), "{j}");
            assert!(with_bar_look(c, Some(&l), None, 0.4) == want, "{j}");
        }
        // A bar look with the bar off draws nothing.
        let src = colourful(640, 360);
        let base = c.base_rect(640.0, 360.0);
        let l = bar_of(r#"{"inset":0.04,"radius":1,"glow":{"size":20}}"#);
        let mut off = Compositor::new(640, 360, c).with_bar_look(Some(&l));
        let mut none = Compositor::new(640, 360, c);
        assert!(
            off.compose(&src, base, 0.0, 0.5).unwrap()
                == none.compose(&src, base, 0.0, 0.5).unwrap()
        );
    }

    #[test]
    fn square_unglowed_untracked_bar_through_the_shaped_path_matches_the_plain_one() {
        // Shaped by `radius: 0` alone: same pixels at the whole-pixel fills.
        let c = Canvas::TALL;
        let l = bar_of(r#"{"radius":0}"#);
        assert!(l.shaped());
        for p in [0.0f32, 0.5, 1.0] {
            let want = with_bar_look(c, None, None, p);
            assert!(with_bar_look(c, Some(&l), None, p) == want, "{p}");
        }
    }

    #[test]
    fn a_shaped_bar_is_inset_rounded_and_drawn_over_the_effects() {
        let c = Canvas::TALL;
        let g = Geom { w: c.w, h: c.h };
        let w = c.w as usize;
        let cw = g.cw() as usize;
        let bh = bar_thickness(c, 1.0) as usize;
        let l = bar_of(r##"{"inset":0.04,"radius":1,"track":"#FFFFFF","track_opacity":0.3}"##);
        let yellow = yuv709(255, 212, 0);
        let src = colourful(640, 360);
        let base_rect = c.base_rect(640.0, 360.0);
        let bare = Compositor::new(640, 360, c)
            .compose(&src, base_rect, 0.0, 0.0)
            .unwrap()
            .to_vec();
        let out = with_bar_look(c, Some(&l), None, 0.5);
        // Bottom bar, 44 px in from the left and right and from the bottom.
        let y_mid = c.h as usize - 44 - bh / 2;
        assert_eq!(out[y_mid * w + 44 + 8], yellow.0);
        assert_ne!(
            out[y_mid * w + 1080 - 44 - 8],
            yellow.0,
            "unfilled half is track"
        );
        // Outside the inset, and under the bar, the picture is untouched.
        assert_eq!(out[y_mid * w + 10], bare[y_mid * w + 10]);
        assert_eq!(
            out[(c.h as usize - 20) * w + 540],
            bare[(c.h as usize - 20) * w + 540]
        );
        // Chroma carries the fill colour on the filled part.
        let u = |o: &[u8], x: usize, y: usize| o[g.luma_len() + (y / 2) * cw + x / 2];
        assert_eq!(u(&out, 100, y_mid & !1), yellow.1);
        // The effects run before the bar, so it keeps its colours under a
        // vignette and a mono grade, while the track shows the graded picture.
        let fxl = EffectsLook {
            vignette: Some(1.0),
            grade: Some(Grade::Mono),
            fill_dim: None,
        };
        let dressed = with_bar_look(c, Some(&l), Some(&fxl), 0.5);
        assert_eq!(dressed[y_mid * w + 44 + 8], yellow.0);
        assert_eq!(u(&dressed, 100, y_mid & !1), yellow.1);
        assert_ne!(
            dressed[y_mid * w + 1080 - 44 - 8],
            out[y_mid * w + 1080 - 44 - 8]
        );
    }

    // ---- alpha: the bar over the frame -----------------------------------------

    fn mixf(from: u8, to: u8, a: f32) -> u8 {
        let (f, t) = (from as f32, to as f32);
        (f + (t - f) * a.clamp(0.0, 1.0)).round().clamp(0.0, 255.0) as u8
    }

    #[test]
    fn a_half_opaque_bar_is_laid_over_the_picture_in_straight_alpha() {
        let c = Canvas::SQUARE;
        let (w, h) = (c.w as usize, c.h as usize);
        let src = colourful(640, 360);
        let base = c.base_rect(640.0, 360.0);
        let build = |bar: Option<crate::look::Rgba>, look: Option<&crate::look::BarLook>| {
            Compositor::new(640, 360, c)
                .with_bar_rgba(bar)
                .with_bar_look(look)
                .compose(&src, base, 0.0, 0.5)
                .unwrap()
                .to_vec()
        };
        let pic = build(None, None);
        let yellow = crate::look::Rgba::rgb(255, 212, 0);
        let (yy, yu, yv) = yuv709(255, 212, 0);
        let bh = bar_thickness(c, 1.0) as usize;
        let dim = |p: u8| 16 + (p.saturating_sub(16) as u32 * 45 / 100) as u8;
        let dim_c = |p: u8| (128 + (p as i32 - 128) / 2) as u8;
        let fill_to = w / 2; // progress 0.5
        let g = Geom { w: c.w, h: c.h };
        let cw = g.cw() as usize;

        // opacity 0.5: the dimmed track at 0.5 over the picture, then the fill at 0.5.
        let half = build(Some(yellow), Some(&bar_of(r#"{"opacity":0.5}"#)));
        for y in h - bh..h {
            for x in [0, 17, fill_to - 1, fill_to, fill_to + 5, w - 1] {
                let p = pic[y * w + x];
                let under = mixf(p, dim(p), 0.5);
                let want = if x < fill_to {
                    mixf(under, yy, 0.5)
                } else {
                    under
                };
                assert_eq!(half[y * w + x], want, "luma {x},{y}");
            }
        }
        for (plane, val) in [(1usize, yu), (2, yv)] {
            let base = g.luma_len() + (plane - 1) * g.chroma_len();
            let (cy, x) = ((h - bh) / 2 + 1, 5usize);
            let p = pic[base + cy * cw + x];
            let want = mixf(mixf(p, dim_c(p), 0.5), val, 0.5);
            assert_eq!(half[base + cy * cw + x], want, "chroma {plane}");
        }
        // Above the bar: untouched.
        assert!(half[..(h - bh) * w] == pic[..(h - bh) * w]);

        // A flat colour with an alpha: the fill at that alpha over the opaque dim track.
        let flat = build(Some(crate::look::Rgba(255, 212, 0, 128)), None);
        let (x, y) = (17, h - 2);
        let p = pic[y * w + x];
        assert_eq!(flat[y * w + x], mixf(dim(p), yy, 128.0 / 255.0));
        let p = pic[y * w + w - 3];
        assert_eq!(flat[y * w + w - 3], dim(p));

        // Opacity 0 draws nothing; opacity 1 and an opaque colour are the bar as it was.
        assert!(build(Some(yellow), Some(&bar_of(r#"{"opacity":0}"#))) == pic);
        let old = build(Some(yellow), None);
        assert!(build(Some(yellow), Some(&bar_of(r#"{"opacity":1}"#))) == old);
        assert!(build(Some(crate::look::Rgba(255, 212, 0, 255)), None) == old);
        let mut by_hand = pic.clone();
        draw_bar_at(&mut by_hand, c, (yy, yu, yv), 0.5, false, 1.0);
        assert!(old == by_hand);
    }

    #[test]
    fn drawing_the_bar_over_with_full_alpha_is_drawing_it() {
        let c = Canvas::PORTRAIT;
        let g = Geom { w: c.w, h: c.h };
        let yellow = yuv709(255, 212, 0);
        for top in [true, false] {
            let mut a = vec![90u8; g.frame_len()];
            a[g.luma_len()..].fill(110);
            let mut b = a.clone();
            draw_bar_at(&mut a, c, yellow, 0.37, top, 2.0);
            draw_bar_over(&mut b, c, yellow, 0.37, (top, 2.0), (1.0, 1.0));
            assert!(a == b, "top {top}");
        }
    }
}
