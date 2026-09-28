//! Frame compositor: one decoded source frame + one camera rect → one
//! 1080x1920 output frame, all in planar YUV 4:2:0 (no RGB round trip).
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
//! Framings that are not 9:16 (the wide/no-face shot, dollies between it
//! and a close-up) fit inside the frame over a blurred, dimmed fill made
//! from the same moment of video.

use fast_image_resize::images::{CroppedImageMut, Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

pub const OUT_W: u32 = 1080;
pub const OUT_H: u32 = 1920;

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

/// Largest 9:16 rect that fits a `sw` x `sh` source, centered.
pub fn base_rect(sw: f64, sh: f64) -> Rect {
    let (w, h) = if sw / sh > 9.0 / 16.0 {
        (sh * 9.0 / 16.0, sh)
    } else {
        (sw, sw * 16.0 / 9.0)
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
    pub fn covers(&self) -> bool {
        self.dx == 0 && self.dy == 0 && self.dw == OUT_W && self.dh == OUT_H
    }
}

/// Relative aspect slack treated as "is 9:16": a sliver bar under ~1.5%
/// of the frame reads as a glitch, so near-9:16 rects fill the frame
/// (trimming a hair off the long side) instead of letterboxing.
const COVER_SLACK: f64 = 0.015;

/// Pure (unit-tested): fit `r` (already clamped to the source) into the
/// 1080x1920 frame.
pub fn layout(r: &Rect) -> Layout {
    let target = OUT_W as f64 / OUT_H as f64;
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
            dw: OUT_W,
            dh: OUT_H,
        };
    }
    let even = |v: f64| ((v / 2.0).round() * 2.0).max(2.0) as u32;
    if a > target {
        // Wider than 9:16: full width, bars top and bottom.
        let dh = even(OUT_W as f64 / a).min(OUT_H);
        let src_h = r.w * dh as f64 / OUT_W as f64;
        Layout {
            src: Rect::from_center(r.cx(), r.cy(), r.w, src_h),
            dx: 0,
            dy: ((OUT_H - dh) / 2) & !1,
            dw: OUT_W,
            dh,
        }
    } else {
        // Taller than 9:16: full height, bars left and right.
        let dw = even(OUT_H as f64 * a).min(OUT_W);
        let src_w = r.h * dw as f64 / OUT_H as f64;
        Layout {
            src: Rect::from_center(r.cx(), r.cy(), src_w, r.h),
            dx: ((OUT_W - dw) / 2) & !1,
            dy: 0,
            dw,
            dh: OUT_H,
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

/// Blur-fill tiny size (9:16): upscaled bilinearly, this is a wide soft
/// blur at almost no cost.
const BG_W: u32 = 36;
const BG_H: u32 = 64;
/// Fill brightness (luma gain toward black): keeps the subject the brightest
/// thing on screen.
const BG_DIM: f32 = 0.72;

/// Reusable compositor for one source geometry.
pub struct Compositor {
    src: Geom,
    out: Vec<u8>,
    ry: Resizer,
    ruv: Resizer,
    bg_tiny: Vec<u8>,
    bg_scratch: Vec<u8>,
}

impl Compositor {
    pub fn new(src_w: u32, src_h: u32) -> Self {
        let out_g = Geom { w: OUT_W, h: OUT_H };
        let mut out = vec![0u8; out_g.frame_len()];
        // Neutral chroma so an unwritten frame is black, not green.
        out[out_g.luma_len()..].fill(128);
        Self {
            src: Geom { w: src_w, h: src_h },
            out,
            ry: Resizer::new(),
            ruv: Resizer::new(),
            bg_tiny: vec![0u8; Geom { w: BG_W, h: BG_H }.frame_len()],
            bg_scratch: vec![0u8; Geom { w: BG_W, h: BG_H }.frame_len()],
        }
    }

    pub fn src_geom(&self) -> Geom {
        self.src
    }

    /// Compose one frame: `frame` is a decoded yuv420p source frame, `rect`
    /// the camera rect in decoded pixels. `flash` 0..1 blends toward white
    /// (merge-join dips). Returns the finished 1080x1920 yuv420p frame.
    pub fn compose(&mut self, frame: &[u8], rect: Rect, flash: f32) -> anyhow::Result<&[u8]> {
        let src = self.src;
        if frame.len() < src.frame_len() {
            anyhow::bail!("short source frame ({} < {})", frame.len(), src.frame_len());
        }
        let r = rect.clamped(src.w as f64, src.h as f64);
        let lay = layout(&r);
        if !lay.covers() {
            self.blur_fill(frame, &r)?;
        }
        let out_g = Geom { w: OUT_W, h: OUT_H };
        let (sy, su, sv) = src.split(frame);
        let (oy, ou, ov) = out_g.split_mut(&mut self.out);
        let fg = ResizeAlg::Convolution(FilterType::CatmullRom);
        let (ry, ruv) = (&mut self.ry, &mut self.ruv);
        // Luma on this thread, both chroma planes on a helper: the three
        // resizes are independent and luma is 2/3 of the work.
        std::thread::scope(|s| -> anyhow::Result<()> {
            let chroma = s.spawn(move || -> anyhow::Result<()> {
                let c = lay.src.scaled(0.5);
                let (cw, ch) = (src.cw(), src.ch());
                let d = (lay.dx / 2, lay.dy / 2, lay.dw / 2, lay.dh / 2);
                resize_into(ruv, su, cw, ch, &c, ou, OUT_W / 2, OUT_H / 2, d, fg)?;
                resize_into(ruv, sv, cw, ch, &c, ov, OUT_W / 2, OUT_H / 2, d, fg)
            });
            let d = (lay.dx, lay.dy, lay.dw, lay.dh);
            resize_into(ry, sy, src.w, src.h, &lay.src, oy, OUT_W, OUT_H, d, fg)?;
            chroma
                .join()
                .map_err(|_| anyhow::anyhow!("chroma worker panicked"))?
        })?;
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
        Ok(&self.out)
    }

    /// Blurred, dimmed fill behind a letterboxed framing: the 9:16 slice of
    /// the source around the rect's center, shrunk to 36x64, box-blurred,
    /// then stretched back up bilinearly (a wide, smooth blur for ~2 ms).
    fn blur_fill(&mut self, frame: &[u8], r: &Rect) -> anyhow::Result<()> {
        let src = self.src;
        let (sw, sh) = (src.w as f64, src.h as f64);
        let b = base_rect(sw, sh);
        let region = Rect::from_center(r.cx(), r.cy(), b.w, b.h).clamped(sw, sh);
        let tiny_g = Geom { w: BG_W, h: BG_H };
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
                BG_W,
                BG_H,
                full(BG_W, BG_H),
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
                box_blur(ty, sy2, BG_W as usize, BG_H as usize, 2);
                box_blur(tu, su2, tiny_g.cw() as usize, tiny_g.ch() as usize, 1);
                box_blur(tv, sv2, tiny_g.cw() as usize, tiny_g.ch() as usize, 1);
            }
            for p in ty.iter_mut() {
                *p = (16.0 + (*p as f32 - 16.0).max(0.0) * BG_DIM).round() as u8;
            }
        }
        let out_g = Geom { w: OUT_W, h: OUT_H };
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
            BG_W,
            BG_H,
            &whole(BG_W, BG_H),
            oy,
            OUT_W,
            OUT_H,
            (0, 0, OUT_W, OUT_H),
            up,
        )?;
        let (cw, ch) = (tiny_g.cw(), tiny_g.ch());
        let dc = (0, 0, OUT_W / 2, OUT_H / 2);
        resize_into(
            &mut self.ruv,
            tu,
            cw,
            ch,
            &whole(cw, ch),
            ou,
            OUT_W / 2,
            OUT_H / 2,
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
            OUT_W / 2,
            OUT_H / 2,
            dc,
            up,
        )?;
        Ok(())
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

    #[test]
    fn nine_sixteen_rects_fill_the_frame_even_when_rounded() {
        // The old path's 156x276 crop left 5px bars; near-9:16 now fills.
        let l = layout(&Rect {
            x: 380.0,
            y: 0.0,
            w: 156.0,
            h: 276.0,
        });
        assert!(l.covers(), "{l:?}");
        assert!((l.src.w / l.src.h - 9.0 / 16.0).abs() < 1e-9);
        assert!((l.src.cx() - 458.0).abs() < 1e-9, "trim stays centered");
    }

    #[test]
    fn wide_rects_letterbox_centered_with_matching_aspect() {
        let l = layout(&Rect {
            x: 0.0,
            y: 0.0,
            w: 640.0,
            h: 360.0,
        });
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
        let l = layout(&Rect {
            x: 0.0,
            y: 0.0,
            w: 720.0,
            h: 1600.0,
        });
        assert_eq!((l.dy, l.dh), (0, 1920));
        assert_eq!((l.dx, l.dw), (108, 864));
        let (sa, da) = (l.src.w / l.src.h, l.dw as f64 / l.dh as f64);
        assert!((sa - da).abs() < 1e-9, "{sa} vs {da}");
    }

    #[test]
    fn base_rect_is_the_largest_nine_sixteen() {
        let b = base_rect(640.0, 360.0);
        assert!((b.w - 202.5).abs() < 1e-9 && (b.h - 360.0).abs() < 1e-9);
        assert!((b.x - 218.75).abs() < 1e-9);
        // Very tall portrait: width-bound.
        let t = base_rect(1080.0, 2400.0);
        assert!((t.w - 1080.0).abs() < 1e-9 && (t.h - 1920.0).abs() < 1e-9);
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
        let mut c = Compositor::new(w, h);
        let base = base_rect(w as f64, h as f64);
        let mean_row = |c: &mut Compositor, dx: f64| -> f64 {
            let r = Rect {
                x: base.x + dx,
                ..base
            };
            let out = c.compose(&src, r, 0.0).unwrap();
            let row = &out[960 * OUT_W as usize..961 * OUT_W as usize];
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
        let mut c = Compositor::new(w, h);
        let out = c
            .compose(
                &src,
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 640.0,
                    h: 360.0,
                },
                0.0,
            )
            .unwrap()
            .to_vec();
        // Top bar row is filled (not black), and dimmer than the picture.
        let top = &out[10 * 1080..11 * 1080];
        let mid = &out[960 * 1080..961 * 1080];
        let mean = |r: &[u8]| r.iter().map(|&v| v as f64).sum::<f64>() / r.len() as f64;
        assert!(mean(top) > 30.0, "fill present: {}", mean(top));
        assert!(mean(top) < mean(mid), "fill dimmer than picture");
        let white = c
            .compose(&src, base_rect(640.0, 360.0), 1.0)
            .unwrap()
            .to_vec();
        assert!(white[..1080 * 1920].iter().all(|&v| v == 235));
        assert!(white[1080 * 1920..].iter().all(|&v| v == 128));
    }
}
