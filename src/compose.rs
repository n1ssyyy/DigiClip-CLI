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
        }
    }

    /// Draw a progress bar (sRGB color) along the bottom edge.
    pub fn with_bar(mut self, rgb: Option<(u8, u8, u8)>) -> Self {
        self.bar = rgb.map(|(r, g, b)| yuv709(r, g, b));
        self
    }

    /// Where the Look puts the bar and how thick it is.
    pub fn with_bar_look(mut self, look: Option<&crate::look::BarLook>) -> Self {
        if let Some(l) = look {
            self.bar_top = l.pos == Some(crate::look::BarPos::Top);
            self.bar_height = l.height.unwrap_or(1.0);
        }
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
        let half = (out_h / 2) & !1;
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

    /// Flash dip and progress bar over the composed frame.
    fn finish(&mut self, flash: f32, progress: f32) {
        let canvas = self.canvas;
        let out_g = Geom {
            w: canvas.w,
            h: canvas.h,
        };
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
        if let Some(color) = self.bar {
            draw_bar_at(
                &mut self.out,
                canvas,
                color,
                progress,
                self.bar_top,
                self.bar_height,
            );
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
                *p = (16.0 + (*p as f32 - 16.0).max(0.0) * BG_DIM).round() as u8;
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

/// Progress bar along the bottom edge: the filled part in `color`, the
/// rest a dimmed track so the bar reads on any footage. Pure (unit-tested).
pub fn draw_bar(out: &mut [u8], c: Canvas, color: (u8, u8, u8), progress: f32) {
    draw_bar_at(out, c, color, progress, false, 1.0);
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
}
