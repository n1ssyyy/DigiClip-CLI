//! The progress bar when the Look dresses it: inset from the frame's edges,
//! rounded ends, a track of its own and a soft glow around the filled part.
//!
//! A bar without any of those fields is drawn by `compose::draw_bar_at`,
//! pixel for pixel as it always was; this module only runs when the Look
//! sets `inset`, `radius`, `track`, `track_opacity` or `glow`.
//!
//! The work that does not change from frame to frame is done once per clip in
//! [`BarFx::new`]: the track's shape (a rounded rectangle, one coverage byte
//! per pixel) and the glow's vertical profile. A frame then costs the bar's
//! own rows, plus the rows the halo reaches, and nothing of the rest of the
//! frame: the filled part's coverage is found only where the leading end can
//! be round, and the halo is the blur of a rectangle, which is separable (one
//! profile per row, computed once; one per column, computed per frame).
//!
//! Alpha is straight alpha over what is below, each layer rounded to 8 bits:
//! the fill at its colour's alpha times `bar.opacity`, the track at its own
//! opacity times its colour's alpha times `bar.opacity` (the plain dimmed
//! track: the dimmed picture blended at `bar.opacity`), the glow at its
//! strength times its colour's alpha times `bar.opacity`.
//!
//! Order, bottom to top: the picture, the track, the glow, the fill. The glow
//! is what a caption's glow is: the shape grown by 0.55 of `size` and blurred
//! by 0.6 of `size`, at `strength` opacity (see `captions::motion`).

use crate::captions::motion::{resolve_glow, GLOW_BLUR, GLOW_BORD};
use crate::compose::{bar_thickness, Canvas, Geom};
use crate::look::{BarLook, Rgba};

/// How much of the picture a track without a colour of its own keeps: the
/// luma is dimmed to 45 %, the chroma halved (what the bar always did).
const TRACK_LUMA_KEEP: u32 = 45;

/// Opacity of a track colour that comes without `track_opacity` (the dimming
/// of the plain track is a black at 55 %).
pub const TRACK_OPACITY: f64 = 0.55;

/// Rounded to an even number (4:2:0 chroma needs the bar on even rows and columns).
fn even(v: f64) -> usize {
    ((v / 2.0).round() * 2.0).max(0.0) as usize
}

/// `erf` (Abramowitz & Stegun 7.1.26, error under 2e-7).
fn erf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs());
    let y = 1.0
        - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t
            + 0.254_829_592)
            * t
            * (-x * x).exp();
    y.copysign(x)
}

/// The normal distribution's cumulative at `z` standard deviations.
fn phi(z: f64) -> f64 {
    0.5 * (1.0 + erf(z / std::f64::consts::SQRT_2))
}

/// Coverage (0..1) of the pixel centred on `(px, py)` by the rectangle
/// `left..right` x `top..bottom` with corners of radius `r` px: a one pixel
/// ramp over the signed distance to the edge, so the edges are antialiased and
/// a square rectangle on whole pixels is exactly 0 or 1.
fn rect_cov(px: f64, py: f64, (left, top, right, bottom): (f64, f64, f64, f64), r: f64) -> f64 {
    let (hx, hy) = ((right - left) / 2.0, (bottom - top) / 2.0);
    if hx <= 0.0 || hy <= 0.0 {
        return 0.0;
    }
    let r = r.clamp(0.0, hx.min(hy));
    let qx = (px - (left + right) / 2.0).abs() - (hx - r);
    let qy = (py - (top + bottom) / 2.0).abs() - (hy - r);
    let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
    (0.5 - d).clamp(0.0, 1.0)
}

/// The halo around the filled part.
struct Halo {
    /// Colour (YUV) and opacity at the middle of the bar.
    yuv: (u8, u8, u8),
    strength: f32,
    /// The shape is grown by `grow` px and blurred by `sigma` px.
    grow: f64,
    sigma: f64,
    /// Furthest the halo reaches beyond the bar (px).
    reach: usize,
    /// First luma row and one past the last row the halo can touch (even).
    ya: usize,
    yb: usize,
    /// Vertical profile per luma row `ya..yb` and per chroma row `ya/2..yb/2`.
    v_luma: Vec<f32>,
    v_chroma: Vec<f32>,
}

/// A progress bar, planned for one canvas.
pub struct BarFx {
    w: usize,
    h: usize,
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
    /// Radius of the ends (px).
    radius: f64,
    /// Fill colour (YUV).
    fill: (u8, u8, u8),
    /// Opacity of the fill: its colour's own alpha times the Look's `opacity`.
    fill_a: f32,
    /// The track's colour (YUV) and opacity (its own, its colour's alpha and the
    /// Look's `opacity`, multiplied); `None` is the plain dimmed track.
    track: Option<((u8, u8, u8), f32)>,
    /// Opacity of the plain dimmed track: the Look's `opacity`.
    dim_a: f32,
    /// Coverage of the track's shape, `(y1 - y0)` rows of `(x1 - x0)`.
    mask: Vec<u8>,
    halo: Option<Halo>,
}

impl BarFx {
    /// Plan the bar. `fill` is the bar colour (sRGB with its alpha: the flat
    /// `progress_bar` or the Look's `bar.color`).
    pub fn new(c: Canvas, look: &BarLook, fill: Rgba) -> BarFx {
        let elem = look.opacity.unwrap_or(1.0).clamp(0.0, 1.0);
        let fill_a = (fill.opacity() * elem) as f32;
        let fill = (fill.0, fill.1, fill.2);
        let (w, h) = (c.w as usize, c.h as usize);
        let k = crate::captions::ass::design_scale(c.w, c.h);
        let bh = bar_thickness(c, look.height.unwrap_or(1.0)) as usize;
        // The margin from the left, right and the bar's own edge, as a share
        // of the frame's width; never so much that the bar disappears.
        let room = (w / 2).saturating_sub(8).min((h.saturating_sub(bh)) / 2) & !1;
        let inset = even(look.inset.unwrap_or(0.0) * w as f64).min(room);
        let (x0, x1) = (inset, w - inset);
        let top = look.pos == Some(crate::look::BarPos::Top);
        let y0 = if top { inset } else { h - inset - bh };
        let y1 = y0 + bh;
        let radius = look.radius.unwrap_or(0.0) * bh as f64 / 2.0;
        let rect = (x0 as f64, y0 as f64, x1 as f64, y1 as f64);
        let mut mask = Vec::with_capacity((x1 - x0) * bh);
        for y in y0..y1 {
            for x in x0..x1 {
                let a = rect_cov(x as f64 + 0.5, y as f64 + 0.5, rect, radius);
                mask.push((a * 255.0).round() as u8);
            }
        }
        let yuv = |(r, g, b): (u8, u8, u8)| crate::compose::yuv709(r, g, b);
        let track = (look.track.is_some() || look.track_opacity.is_some()).then(|| {
            let col = look.track.map_or((0, 0, 0), |t| (t.0, t.1, t.2));
            let own = look.track.map_or(1.0, |t| t.opacity());
            let o = look.track_opacity.unwrap_or(TRACK_OPACITY) * own * elem;
            (yuv(col), o as f32)
        });
        let halo = look
            .glow
            .as_ref()
            .map(|g| {
                resolve_glow(
                    &[Some(g)],
                    [fill.0 as f64, fill.1 as f64, fill.2 as f64, 255.0],
                    k,
                )
            })
            .filter(|g| g.size > 0.0 && g.strength > 0.0)
            .map(|g| {
                let grow = g.size * GLOW_BORD;
                let sigma = (g.size * GLOW_BLUR).max(0.3);
                let reach = (grow + 3.0 * sigma).ceil() as usize;
                let ya = y0.saturating_sub(reach) & !1;
                let yb = ((y1 + reach).min(h) + 1) & !1;
                let band = |yc: f64| {
                    (phi((yc - (y0 as f64 - grow)) / sigma)
                        - phi((yc - (y1 as f64 + grow)) / sigma)) as f32
                };
                Halo {
                    yuv: yuv((
                        g.col[0].round() as u8,
                        g.col[1].round() as u8,
                        g.col[2].round() as u8,
                    )),
                    strength: (g.strength * elem) as f32,
                    grow,
                    sigma,
                    reach,
                    ya,
                    yb,
                    v_luma: (ya..yb).map(|y| band(y as f64 + 0.5)).collect(),
                    v_chroma: (ya / 2..yb / 2)
                        .map(|y| band(y as f64 * 2.0 + 1.0))
                        .collect(),
                }
            });
        BarFx {
            w,
            h,
            x0,
            x1,
            y0,
            y1,
            radius,
            fill: yuv(fill),
            fill_a,
            track,
            dim_a: elem as f32,
            mask,
            halo,
        }
    }

    /// The bar's rows and columns: `(x0, y0, x1, y1)`, the track's bounds.
    pub fn bounds(&self) -> (usize, usize, usize, usize) {
        (self.x0, self.y0, self.x1, self.y1)
    }

    /// Draw the bar over a finished yuv420p frame; `progress` is 0..1.
    pub fn draw(&self, out: &mut [u8], progress: f32) {
        let g = Geom {
            w: self.w as u32,
            h: self.h as u32,
        };
        let (oy, uv) = out.split_at_mut(g.luma_len());
        let (ou, ov) = uv.split_at_mut(g.chroma_len());
        let (w, cw) = (self.w, self.w / 2);
        let bw = self.x1 - self.x0;
        let fe = self.x0 as f64 + progress.clamp(0.0, 1.0) as f64 * bw as f64;

        // ---- track ----
        for y in self.y0..self.y1 {
            let m = &self.mask[(y - self.y0) * bw..(y - self.y0 + 1) * bw];
            let row = &mut oy[y * w + self.x0..y * w + self.x1];
            for (p, &a) in row.iter_mut().zip(m) {
                if a == 0 {
                    continue;
                }
                let dark = match self.track {
                    None => {
                        let dim = 16 + (p.saturating_sub(16) as u32 * TRACK_LUMA_KEEP / 100) as u8;
                        if self.dim_a < 1.0 {
                            mix(*p, dim, self.dim_a)
                        } else {
                            dim
                        }
                    }
                    Some((c, o)) => mix(*p, c.0, o),
                };
                *p = if a == 255 {
                    dark
                } else {
                    mix(*p, dark, a as f32 / 255.0)
                };
            }
        }
        for (plane, which) in [(&mut *ou, 1usize), (&mut *ov, 2usize)] {
            for cy in self.y0 / 2..self.y1 / 2 {
                for cx in self.x0 / 2..self.x1 / 2 {
                    let a = self.mask_cov(cx * 2 - self.x0, cy * 2 - self.y0, bw);
                    if a <= 0.0 {
                        continue;
                    }
                    let p = &mut plane[cy * cw + cx];
                    let dark = match self.track {
                        None => {
                            let dim = (128 + (*p as i32 - 128) / 2) as u8;
                            if self.dim_a < 1.0 {
                                mix(*p, dim, self.dim_a)
                            } else {
                                dim
                            }
                        }
                        Some((c, o)) => mix(*p, if which == 1 { c.1 } else { c.2 }, o),
                    };
                    *p = if a >= 1.0 { dark } else { mix(*p, dark, a) };
                }
            }
        }

        // ---- glow ----
        if let Some(hl) = &self.halo {
            if fe - self.x0 as f64 >= 0.5 {
                self.draw_halo(hl, fe, oy, ou, ov);
            }
        }

        // ---- fill ----
        let end = (fe.ceil() as usize).min(self.x1);
        if end <= self.x0 {
            return;
        }
        let fw = end - self.x0;
        let r = self.radius.min((fe - self.x0 as f64) / 2.0);
        let rect = (self.x0 as f64, self.y0 as f64, fe, self.y1 as f64);
        let rows = self.y1 - self.y0;
        let mut cov = vec![0u8; fw * rows];
        for (j, row) in cov.chunks_mut(fw).enumerate() {
            let py = (self.y0 + j) as f64 + 0.5;
            for (i, a) in row.iter_mut().enumerate() {
                *a = (rect_cov((self.x0 + i) as f64 + 0.5, py, rect, r) * 255.0).round() as u8;
            }
        }
        for (j, row) in cov.chunks(fw).enumerate() {
            let dst = &mut oy[(self.y0 + j) * w + self.x0..(self.y0 + j) * w + end];
            for (p, &a) in dst.iter_mut().zip(row) {
                if a == 255 && self.fill_a >= 1.0 {
                    *p = self.fill.0;
                } else if a > 0 {
                    *p = mix(*p, self.fill.0, a as f32 / 255.0 * self.fill_a);
                }
            }
        }
        for (plane, val) in [(&mut *ou, self.fill.1), (&mut *ov, self.fill.2)] {
            for cy in self.y0 / 2..self.y1 / 2 {
                for cx in self.x0 / 2..end.div_ceil(2) {
                    let (lx, ly) = (cx * 2 - self.x0, cy * 2 - self.y0);
                    let q = |dx: usize, dy: usize| {
                        if lx + dx < fw {
                            cov[(ly + dy) * fw + lx + dx] as f32
                        } else {
                            0.0
                        }
                    };
                    let a = (q(0, 0) + q(1, 0) + q(0, 1) + q(1, 1)) / (4.0 * 255.0);
                    if a <= 0.0 {
                        continue;
                    }
                    let p = &mut plane[cy * cw + cx];
                    *p = if a >= 1.0 && self.fill_a >= 1.0 {
                        val
                    } else {
                        mix(*p, val, a * self.fill_a)
                    };
                }
            }
        }
    }

    /// Average coverage of the 2x2 luma block at `(lx, ly)` inside the bar.
    fn mask_cov(&self, lx: usize, ly: usize, bw: usize) -> f32 {
        let at = |dx: usize, dy: usize| self.mask[(ly + dy) * bw + lx + dx] as f32;
        (at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1)) / (4.0 * 255.0)
    }

    /// The halo of a fill that ends at `fe`: blur of the grown rectangle, a
    /// product of a row profile (precomputed) and a column profile.
    fn draw_halo(&self, hl: &Halo, fe: f64, oy: &mut [u8], ou: &mut [u8], ov: &mut [u8]) {
        let w = self.w;
        let xa = self.x0.saturating_sub(hl.reach) & !1;
        let xb = (((fe.ceil() as usize) + hl.reach).min(w) + 1) & !1;
        let col = |xc: f64| {
            (phi((xc - (self.x0 as f64 - hl.grow)) / hl.sigma)
                - phi((xc - (fe + hl.grow)) / hl.sigma)) as f32
        };
        let h_luma: Vec<f32> = (xa..xb).map(|x| col(x as f64 + 0.5)).collect();
        let h_chroma: Vec<f32> = (xa / 2..xb / 2)
            .map(|x| col(x as f64 * 2.0 + 1.0))
            .collect();
        for y in hl.ya..hl.yb {
            let v = hl.v_luma[y - hl.ya] * hl.strength;
            if v < 0.002 {
                continue;
            }
            let row = &mut oy[y * w + xa..y * w + xb];
            for (p, &hx) in row.iter_mut().zip(&h_luma) {
                let a = v * hx;
                if a >= 0.002 {
                    *p = mix(*p, hl.yuv.0, a);
                }
            }
        }
        let cw = w / 2;
        for (plane, val) in [(ou, hl.yuv.1), (ov, hl.yuv.2)] {
            for cy in hl.ya / 2..hl.yb / 2 {
                let v = hl.v_chroma[cy - hl.ya / 2] * hl.strength;
                if v < 0.002 {
                    continue;
                }
                let row = &mut plane[cy * cw + xa / 2..cy * cw + xb / 2];
                for (p, &hx) in row.iter_mut().zip(&h_chroma) {
                    let a = v * hx;
                    if a >= 0.002 {
                        *p = mix(*p, val, a);
                    }
                }
            }
        }
    }
}

/// `from` moved `a` (0..1) of the way to `to`.
fn mix(from: u8, to: u8, a: f32) -> u8 {
    let (f, t) = (from as f32, to as f32);
    (f + (t - f) * a.clamp(0.0, 1.0)).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::look::{GlowLook, Look, Rgba};

    const C: Canvas = Canvas::TALL;

    fn look(json: &str) -> BarLook {
        Look::parse(&format!(r#"{{"bar":{json}}}"#)).bar.unwrap()
    }

    /// A frame with a luma ramp (so the dimming is visible) and flat chroma.
    fn frame() -> Vec<u8> {
        let g = Geom { w: C.w, h: C.h };
        let mut f = vec![128u8; g.frame_len()];
        for y in 0..C.h as usize {
            for x in 0..C.w as usize {
                f[y * C.w as usize + x] = 60 + ((x + y) % 120) as u8;
            }
        }
        f
    }

    fn luma(f: &[u8], x: usize, y: usize) -> u8 {
        f[y * C.w as usize + x]
    }

    fn chroma(f: &[u8], plane: usize, x: usize, y: usize) -> u8 {
        let g = Geom { w: C.w, h: C.h };
        f[g.luma_len() + (plane - 1) * g.chroma_len() + (y / 2) * (C.w as usize / 2) + x / 2]
    }

    const YELLOW: (u8, u8, u8) = (255, 212, 0);
    const YELLOW_A: Rgba = Rgba::rgb(255, 212, 0);

    fn yellow_yuv() -> (u8, u8, u8) {
        crate::compose::yuv709(YELLOW.0, YELLOW.1, YELLOW.2)
    }

    #[test]
    fn inset_is_the_margin_from_the_sides_and_the_edge_it_sits_on() {
        let fx = BarFx::new(C, &look(r#"{"inset":0.04}"#), YELLOW_A);
        // 4 % of 1080 is 43.2 px: the nearest even number, 44.
        let bh = bar_thickness(C, 1.0) as usize;
        assert_eq!(fx.bounds(), (44, 1920 - 44 - bh, 1080 - 44, 1920 - 44));
        let top = BarFx::new(C, &look(r#"{"inset":0.04,"pos":"top"}"#), YELLOW_A);
        assert_eq!(top.bounds(), (44, 44, 1080 - 44, 44 + bh));
        // No inset: the bar runs to the frame's edges as before.
        let flat = BarFx::new(C, &look(r#"{"radius":0}"#), YELLOW_A);
        assert_eq!(flat.bounds(), (0, 1920 - bh, 1080, 1920));
        // Pixels: the fill starts at the inset and not before, rows likewise.
        let mut f = frame();
        let before = f.clone();
        fx.draw(&mut f, 0.5);
        let (x0, y0, x1, y1) = fx.bounds();
        let mid = (y0 + y1) / 2;
        assert_eq!(luma(&f, x0 + 4, mid), yellow_yuv().0);
        assert_eq!(luma(&f, x0 - 1, mid), luma(&before, x0 - 1, mid));
        assert_eq!(luma(&f, x0 + 4, y0 - 1), luma(&before, x0 + 4, y0 - 1));
        assert_eq!(luma(&f, x0 + 4, y1), luma(&before, x0 + 4, y1));
        assert_eq!(luma(&f, x1 - 1 + 1, mid), luma(&before, x1, mid));
        // Chroma carries the colour on the bar's rows only.
        assert_eq!(chroma(&f, 1, x0 + 4, mid), yellow_yuv().1);
        assert_eq!(
            chroma(&f, 1, x0 + 4, y1 + 2),
            chroma(&before, 1, x0 + 4, y1 + 2)
        );
    }

    #[test]
    fn the_fill_ends_where_the_progress_says_and_the_track_is_the_plain_dimming() {
        let fx = BarFx::new(C, &look(r#"{"inset":0}"#), YELLOW_A);
        let mut f = frame();
        let before = f.clone();
        fx.draw(&mut f, 0.4);
        let (x0, y0, x1, y1) = fx.bounds();
        let mid = (y0 + y1) / 2;
        let fe = x0 as f64 + 0.4 * (x1 - x0) as f64; // 432
        let fe_px = fe as usize;
        assert_eq!(luma(&f, fe_px - 2, mid), yellow_yuv().0);
        // Past the end: the track, exactly what the plain bar draws.
        for x in [fe_px + 2, fe_px + 100, x1 - 1] {
            let p = luma(&before, x, mid);
            assert_eq!(
                luma(&f, x, mid),
                16 + (p.saturating_sub(16) as u32 * 45 / 100) as u8
            );
            let c = chroma(&before, 1, x, mid) as i32;
            assert_eq!(chroma(&f, 1, x, mid) as i32, 128 + (c - 128) / 2);
        }
        // The whole bar against the old drawing: the track pixels are equal.
        let mut old = before.clone();
        crate::compose::draw_bar(&mut old, C, yellow_yuv(), 0.4);
        for y in y0..y1 {
            for x in fe_px + 2..x1 {
                assert_eq!(luma(&f, x, y), luma(&old, x, y), "{x},{y}");
            }
            for x in x0..fe_px - 2 {
                assert_eq!(luma(&f, x, y), luma(&old, x, y), "{x},{y}");
            }
        }
        // A fraction of a pixel is a blend, not a jump.
        let mut g = before.clone();
        fx.draw(&mut g, (x0 as f32 + 0.5 + 100.25) / 1080.0);
        let a = luma(&g, 100 + x0, mid);
        let t = 16 + (luma(&before, 100 + x0, mid).saturating_sub(16) as u32 * 45 / 100) as u8;
        assert!(
            a > t.min(yellow_yuv().0) && a < t.max(yellow_yuv().0) || a == t,
            "{a} {t}"
        );
    }

    #[test]
    fn the_track_takes_its_own_colour_and_opacity() {
        let mut f = frame();
        let before = f.clone();
        let fx = BarFx::new(
            C,
            &look(r##"{"track":"#FFFFFF","track_opacity":0.3}"##),
            YELLOW_A,
        );
        fx.draw(&mut f, 0.2);
        let (_, y0, x1, y1) = fx.bounds();
        let mid = (y0 + y1) / 2;
        let x = x1 - 20;
        let want = mix(luma(&before, x, mid), 235, 0.3);
        assert_eq!(luma(&f, x, mid), want);
        assert!(luma(&f, x, mid) > luma(&before, x, mid).min(200));
        // A track colour alone has the strength of the plain dimming (55 %).
        let mut g = before.clone();
        BarFx::new(C, &look(r##"{"track":"#FFFFFF"}"##), YELLOW_A).draw(&mut g, 0.2);
        assert_eq!(luma(&g, x, mid), mix(luma(&before, x, mid), 235, 0.55));
        // An opacity alone dims towards black.
        let mut g = before.clone();
        BarFx::new(C, &look(r#"{"track_opacity":0.8}"#), YELLOW_A).draw(&mut g, 0.2);
        assert_eq!(luma(&g, x, mid), mix(luma(&before, x, mid), 16, 0.8));
        // Fully clear: the picture shows through.
        let mut g = before.clone();
        BarFx::new(C, &look(r#"{"track_opacity":0}"#), YELLOW_A).draw(&mut g, 0.2);
        assert_eq!(luma(&g, x, mid), luma(&before, x, mid));
    }

    #[test]
    fn radius_rounds_the_ends_of_the_bar_and_the_leading_end_of_the_fill() {
        let fx = BarFx::new(C, &look(r#"{"inset":0.04,"radius":1}"#), YELLOW_A);
        let (x0, y0, x1, y1) = fx.bounds();
        let bh = y1 - y0;
        let mut f = frame();
        let before = f.clone();
        fx.draw(&mut f, 0.5);
        let mid = (y0 + y1) / 2;
        // The corners of the bar's box stay picture, the middle of the end does not.
        assert_eq!(luma(&f, x0, y0), luma(&before, x0, y0));
        assert_eq!(luma(&f, x0, y1 - 1), luma(&before, x0, y1 - 1));
        assert_eq!(luma(&f, x1 - 1, y0), luma(&before, x1 - 1, y0));
        assert_ne!(luma(&f, x0, mid), luma(&before, x0, mid));
        // A round end: the pill is bh/2 in radius, so a pixel one radius in is full.
        assert_eq!(luma(&f, x0 + bh / 2, mid), yellow_yuv().0);
        // The fill's leading end is round too: its corner is not fill.
        let fe = x0 + (x1 - x0) / 2;
        let dim = |p: u8| 16 + (p.saturating_sub(16) as u32 * 45 / 100) as u8;
        assert_eq!(
            luma(&f, fe - 1, y0),
            dim(luma(&before, fe - 1, y0)),
            "corner is track"
        );
        assert_eq!(luma(&f, fe - 4, mid), yellow_yuv().0);
        // Antialiased: some edge pixel is neither picture, track nor fill.
        let edge = luma(&f, x0 + 1, y0 + 1);
        assert!(edge != yellow_yuv().0 && edge != luma(&before, x0 + 1, y0 + 1));
        // Square ends where radius is 0.
        let sq = BarFx::new(C, &look(r#"{"inset":0.04,"radius":0}"#), YELLOW_A);
        let mut g = before.clone();
        sq.draw(&mut g, 0.5);
        assert_eq!(luma(&g, x0, y0), yellow_yuv().0);
        // A half radius is half as round: the corner pixel is mostly covered.
        let half = BarFx::new(C, &look(r#"{"inset":0.04,"radius":0.5}"#), YELLOW_A);
        let mut g = before.clone();
        half.draw(&mut g, 0.5);
        assert_ne!(luma(&g, x0, y0), yellow_yuv().0);
    }

    #[test]
    fn the_glow_hugs_the_fill_and_stays_near_the_bar() {
        let l = look(r##"{"inset":0.04,"radius":1,"glow":{"size":20,"strength":0.9}}"##);
        let fx = BarFx::new(C, &l, YELLOW_A);
        let (x0, y0, x1, y1) = fx.bounds();
        let mut f = frame();
        let before = f.clone();
        fx.draw(&mut f, 0.4);
        let fe = x0 + (0.4 * (x1 - x0) as f64) as usize;
        // Lit just above the filled part, brighter than the picture there.
        let above = luma(&f, fe - 60, y0 - 4);
        assert_ne!(above, luma(&before, fe - 60, y0 - 4));
        // And over the unfilled track just past the end of the fill.
        assert_ne!(luma(&f, fe + 12, (y0 + y1) / 2), {
            let p = luma(&before, fe + 12, (y0 + y1) / 2);
            16 + (p.saturating_sub(16) as u32 * 45 / 100) as u8
        });
        // Fades with distance, reaches nowhere near the other side of the frame.
        let near = (luma(&f, fe - 60, y0 - 4) as i32 - luma(&before, fe - 60, y0 - 4) as i32).abs();
        let far =
            (luma(&f, fe - 60, y0 - 24) as i32 - luma(&before, fe - 60, y0 - 24) as i32).abs();
        assert!(near > far, "{near} {far}");
        // Everything further than the halo's reach from the bar is untouched.
        let reach = (20.0f64 * 0.55 + 3.0 * 20.0 * 0.6).ceil() as usize + 2;
        for y in 0..C.h as usize {
            if y + reach >= y0 && y < y1 + reach {
                continue;
            }
            for x in (0..C.w as usize).step_by(7) {
                assert_eq!(luma(&f, x, y), luma(&before, x, y), "{x},{y}");
            }
        }
        // And along the bar: nothing left of the fill's start minus the reach,
        // nothing right of its end plus the reach.
        for x in (0..x0.saturating_sub(reach)).chain(fe + reach + 2..C.w as usize) {
            for y in y0.saturating_sub(reach)..(y1 + reach).min(C.h as usize) {
                let want = if x >= x0 && x < x1 {
                    let p = luma(&before, x, y);
                    let a = if y >= y0 && y < y1 {
                        mask_at(&fx, x, y)
                    } else {
                        0
                    };
                    mix(
                        p,
                        16 + (p.saturating_sub(16) as u32 * 45 / 100) as u8,
                        a as f32 / 255.0,
                    )
                } else {
                    luma(&before, x, y)
                };
                assert_eq!(luma(&f, x, y), want, "{x},{y}");
            }
        }
        // Chroma follows: the halo is yellow.
        assert_ne!(
            chroma(&f, 1, fe - 60, y0 - 4),
            chroma(&before, 1, fe - 60, y0 - 4)
        );
    }

    fn mask_at(fx: &BarFx, x: usize, y: usize) -> u8 {
        fx.mask[(y - fx.y0) * (fx.x1 - fx.x0) + (x - fx.x0)]
    }

    #[test]
    fn a_glow_takes_its_own_colour_and_none_means_the_fill_colour() {
        let fill = BarFx::new(C, &look(r#"{"glow":{"size":16}}"#), YELLOW_A);
        let blue = BarFx::new(
            C,
            &look(r##"{"glow":{"size":16,"color":"#0000FF"}}"##),
            YELLOW_A,
        );
        let (mut a, mut b) = (frame(), frame());
        fill.draw(&mut a, 0.5);
        blue.draw(&mut b, 0.5);
        let (_, y0, _, _) = fill.bounds();
        // Same place, different chroma.
        assert_ne!(chroma(&a, 2, 500, y0 - 3), chroma(&b, 2, 500, y0 - 3));
        let y = crate::compose::yuv709(255, 212, 0);
        assert!((chroma(&a, 1, 500, y0 - 3) as i32 - 128).signum() == (y.1 as i32 - 128).signum());
        // No size or no strength: no halo at all.
        for j in [r#"{"glow":{"size":0}}"#, r#"{"glow":{"strength":0}}"#] {
            let mut g = frame();
            let want = {
                let mut w = frame();
                BarFx::new(C, &look(r#"{"inset":0}"#), YELLOW_A).draw(&mut w, 0.5);
                w
            };
            let l = look(j);
            let fx = BarFx::new(C, &l, YELLOW_A);
            fx.draw(&mut g, 0.5);
            assert_eq!(g, want, "{j}");
        }
        let _ = (GlowLook::default(), Rgba::rgb(0, 0, 0));
    }

    #[test]
    fn glow_sizes_scale_with_the_canvas_like_captions_do() {
        // 9:16 at 1080 wide is the design size: 20 px reaches 11 + 36 px.
        let a = BarFx::new(C, &look(r#"{"glow":{"size":20}}"#), YELLOW_A);
        let b = BarFx::new(Canvas::SQUARE, &look(r#"{"glow":{"size":20}}"#), YELLOW_A);
        let ra = a.halo.as_ref().unwrap().reach;
        let rb = b.halo.as_ref().unwrap().reach;
        assert!(rb < ra, "{rb} {ra}");
    }

    #[test]
    fn progress_extremes_are_empty_and_full() {
        let fx = BarFx::new(C, &look(r#"{"inset":0.02,"radius":1}"#), YELLOW_A);
        let (x0, y0, x1, y1) = fx.bounds();
        let mid = (y0 + y1) / 2;
        let mut f = frame();
        fx.draw(&mut f, 0.0);
        assert_ne!(luma(&f, x0 + 20, mid), yellow_yuv().0);
        let mut f = frame();
        fx.draw(&mut f, 1.0);
        assert_eq!(luma(&f, x1 - 20, mid), yellow_yuv().0);
        assert_eq!(luma(&f, x0 + 20, mid), yellow_yuv().0);
        let mut f = frame();
        fx.draw(&mut f, 7.0);
        assert_eq!(luma(&f, x1 - 20, mid), yellow_yuv().0);
    }

    // ---- alpha ---------------------------------------------------------------

    #[test]
    fn bar_opacity_and_colour_alpha_lay_track_glow_and_fill_over_the_picture() {
        let yy = yellow_yuv();
        let dim = |p: u8| 16 + (p.saturating_sub(16) as u32 * 45 / 100) as u8;
        let fx = BarFx::new(
            C,
            &look(r##"{"inset":0,"track":"#FFFFFF","track_opacity":0.8,"opacity":0.5}"##),
            Rgba(255, 212, 0, 128),
        );
        let mut f = frame();
        let before = f.clone();
        fx.draw(&mut f, 0.5);
        let (x0, y0, x1, y1) = fx.bounds();
        let mid = (y0 + y1) / 2;
        let white = crate::compose::yuv709(255, 255, 255).0;
        // Track: its opacity 0.8, times the bar's 0.5 (a six-digit colour is opaque).
        let track_a = (0.8 * 1.0 * 0.5) as f32;
        // Fill: the colour's alpha (128/255) times the bar's 0.5, over the track.
        let fill_a = (128.0 / 255.0 * 0.5) as f32;
        let x_fill = x0 + 10;
        let under = mix(luma(&before, x_fill, mid), white, track_a);
        assert_eq!(luma(&f, x_fill, mid), mix(under, yy.0, fill_a));
        let x_track = x1 - 10;
        assert_eq!(
            luma(&f, x_track, mid),
            mix(luma(&before, x_track, mid), white, track_a)
        );
        // Outside the bar the picture is untouched.
        assert_eq!(luma(&f, x_fill, y0 - 1), luma(&before, x_fill, y0 - 1));
        // The plain dimmed track is blended at the bar's opacity.
        let fx = BarFx::new(
            C,
            &look(r#"{"inset":0,"opacity":0.5}"#),
            Rgba::rgb(255, 212, 0),
        );
        let mut g = before.clone();
        fx.draw(&mut g, 0.5);
        let p = luma(&before, x_track, mid);
        assert_eq!(luma(&g, x_track, mid), mix(p, dim(p), 0.5));
        // Opacity 0 draws nothing; 1 is the bar as it was.
        let mut z = before.clone();
        BarFx::new(C, &look(r#"{"inset":0,"opacity":0}"#), YELLOW_A).draw(&mut z, 0.5);
        assert!(z == before);
        let (mut a, mut b) = (before.clone(), before.clone());
        BarFx::new(
            C,
            &look(r#"{"inset":0.02,"radius":1,"glow":{"size":10}}"#),
            YELLOW_A,
        )
        .draw(&mut a, 0.5);
        BarFx::new(
            C,
            &look(
                r##"{"inset":0.02,"radius":1,"glow":{"size":10,"color":"#FFD400FF"},"opacity":1}"##,
            ),
            Rgba(255, 212, 0, 255),
        )
        .draw(&mut b, 0.5);
        assert!(a == b);
    }

    #[test]
    fn the_glow_takes_the_glow_colours_alpha_and_the_bar_opacity() {
        let strong = |j: &str| {
            let mut f = frame();
            let fx = BarFx::new(C, &look(j), YELLOW_A);
            fx.draw(&mut f, 0.5);
            let (x0, y0, _, _) = fx.bounds();
            // A pixel just above the bar, inside the halo.
            luma(&f, x0 + 200, y0 - 3)
        };
        let full = strong(r##"{"inset":0.04,"glow":{"size":20,"color":"#FFFFFF"}}"##);
        let half_colour = strong(r##"{"inset":0.04,"glow":{"size":20,"color":"#FFFFFF80"}}"##);
        let half_bar =
            strong(r##"{"inset":0.04,"glow":{"size":20,"color":"#FFFFFF"},"opacity":0.5}"##);
        let none = strong(r##"{"inset":0.04,"opacity":0.5}"##);
        // The halo lifts the picture; half the strength lifts it about half as far.
        assert!(full > none && half_colour > none && half_bar > none);
        assert!(half_colour < full && half_bar < full);
        let lift = |v: u8| v as i32 - none as i32;
        assert!(
            (lift(half_colour) * 2 - lift(full)).abs() <= 2,
            "{half_colour} {full} {none}"
        );
        assert!(
            (lift(half_bar) * 2 - lift(full)).abs() <= 3,
            "{half_bar} {full} {none}"
        );
    }
}
