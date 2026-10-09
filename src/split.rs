//! Split-screen layout for two-person talks (`--layout`): on a tall canvas
//! the left person fills the top half and the right person the bottom
//! half, each in a steady head-and-shoulders crop (set per source shot, so
//! a multi-cam cut reframes on the cut), captions on the seam.

use crate::compose::{Canvas, Rect};
use crate::track::{Duo, Face};

/// `auto` splits when at least this share of the tracked samples shows two
/// people side by side.
pub const AUTO_MIN_FRAC: f64 = 0.6;
/// Fewest pair sightings a shot needs for its own crops.
const SHOT_MIN: usize = 3;
/// Head-and-shoulders: crop height in face heights.
const HEADROOM: f64 = 3.2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// Split when two people share the frame for most of the clip.
    Auto,
    /// One camera, always.
    Single,
    /// Split whenever two people are seen at all.
    Split,
}

/// Split canvases stack two halves: only tall ones have room.
pub fn fits(canvas: Canvas) -> bool {
    canvas.h > canvas.w
}

/// Should this clip split? `duo` pair sightings out of `samples` tracked.
pub fn wanted(mode: Mode, canvas: Canvas, duo: usize, samples: usize) -> bool {
    if !fits(canvas) {
        return false;
    }
    match mode {
        Mode::Single => false,
        Mode::Split => duo >= SHOT_MIN,
        Mode::Auto => samples > 0 && duo >= 10 && duo as f64 / samples as f64 >= AUTO_MIN_FRAC,
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// The typical face of a run of sightings (median box).
fn median_face(fs: &[&Face]) -> Face {
    Face {
        x: median(fs.iter().map(|f| f.cx()).collect()),
        y: median(fs.iter().map(|f| f.y + f.h / 2.0).collect()),
        w: median(fs.iter().map(|f| f.w).collect()),
        h: median(fs.iter().map(|f| f.h).collect()),
        score: 1.0,
    }
}

/// Half-canvas crops for a pair of typical faces (`x`/`y` are centers).
/// Each crop keeps the face a little above center and stops short of the
/// other person where it can.
fn pair_rects(l: &Face, r: &Face, sw: f64, sh: f64, canvas: Canvas, top_frac: f64) -> (Rect, Rect) {
    let sep = (r.x - l.x).abs();
    // The top panel takes `top_frac` of the height (half today).
    let one = |f: &Face, share: f64| {
        let aspect = canvas.w as f64 / (canvas.h as f64 * share);
        let min_h = canvas.min_crop_h() * share;
        let mut h = (f.h * HEADROOM).max(min_h).min(sh);
        // Stay off the neighbor, but never tighter than the face needs.
        let floor = (f.h * 1.6).max(min_h);
        if h * aspect > sep * 0.95 {
            h = (sep * 0.95 / aspect).max(floor).min(h);
        }
        let mut w = h * aspect;
        if w > sw {
            w = sw;
            h = w / aspect;
        }
        Rect::from_center(f.x, f.y + 0.12 * h, w, h).clamped(sw, sh)
    };
    (one(l, top_frac), one(r, 1.0 - top_frac))
}

/// Crops per source shot over `[a, b)`: `(shot start, top, bottom)`. A
/// shot with too few sightings borrows its neighbor's crops (then the
/// clip's). Empty without any sighting. `top_frac`: the top panel's share of
/// the height (0.5 = an even split).
#[allow(clippy::too_many_arguments)]
pub fn plan(
    duo: &[Duo],
    shots: &[f64],
    a: f64,
    b: f64,
    sw: f64,
    sh: f64,
    canvas: Canvas,
    top_frac: f64,
) -> Vec<(f64, Rect, Rect)> {
    if duo.is_empty() {
        return vec![];
    }
    let rects_of = |ds: &[&Duo]| {
        let l: Vec<&Face> = ds.iter().map(|d| &d.left).collect();
        let r: Vec<&Face> = ds.iter().map(|d| &d.right).collect();
        pair_rects(&median_face(&l), &median_face(&r), sw, sh, canvas, top_frac)
    };
    let all: Vec<&Duo> = duo.iter().collect();
    let whole = rects_of(&all);
    let mut bounds: Vec<f64> = vec![a];
    bounds.extend(shots.iter().copied().filter(|&s| s > a && s < b));
    let per: Vec<Option<(Rect, Rect)>> = bounds
        .iter()
        .enumerate()
        .map(|(i, &t0)| {
            let t1 = bounds.get(i + 1).copied().unwrap_or(f64::INFINITY);
            let ds: Vec<&Duo> = duo.iter().filter(|d| d.t >= t0 && d.t < t1).collect();
            (ds.len() >= SHOT_MIN).then(|| rects_of(&ds))
        })
        .collect();
    (0..bounds.len())
        .map(|i| {
            let pick = per[i]
                .or_else(|| per[..i].iter().rev().find_map(|p| *p))
                .or_else(|| per[i + 1..].iter().find_map(|p| *p))
                .unwrap_or(whole);
            (bounds[i], pick.0, pick.1)
        })
        .collect()
}

/// The crops in effect at source time `t`.
pub fn at(plan: &[(f64, Rect, Rect)], t: f64) -> Option<(Rect, Rect)> {
    plan.iter()
        .rev()
        .find(|p| p.0 <= t)
        .or(plan.first())
        .map(|p| (p.1, p.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(cx: f64, cy: f64, h: f64) -> Face {
        Face {
            x: cx - h * 0.4,
            y: cy - h / 2.0,
            w: h * 0.8,
            h,
            score: 0.9,
        }
    }

    fn pod(t: f64) -> Duo {
        Duo {
            t,
            left: face(560.0, 400.0, 180.0),
            right: face(1400.0, 420.0, 170.0),
        }
    }

    #[test]
    fn auto_needs_a_steady_pair_on_a_tall_canvas() {
        assert!(wanted(Mode::Auto, Canvas::TALL, 70, 100));
        assert!(!wanted(Mode::Auto, Canvas::TALL, 40, 100));
        assert!(!wanted(Mode::Auto, Canvas::SQUARE, 100, 100));
        assert!(!wanted(Mode::Single, Canvas::TALL, 100, 100));
        assert!(wanted(Mode::Split, Canvas::TALL, 5, 100));
        assert!(!wanted(Mode::Split, Canvas::TALL, 1, 100));
        assert!(wanted(Mode::Split, Canvas::PORTRAIT, 5, 100));
    }

    #[test]
    fn crops_frame_each_person_in_a_half() {
        let duo: Vec<Duo> = (0..30).map(|i| pod(i as f64 * 0.1)).collect();
        let p = plan(&duo, &[], 0.0, 3.0, 1920.0, 1080.0, Canvas::TALL, 0.5);
        assert_eq!(p.len(), 1);
        let (top, bot) = at(&p, 1.0).unwrap();
        for (r, cx) in [(top, 560.0), (bot, 1400.0)] {
            // Half-canvas aspect (1080 x 960), inside the source.
            assert!((r.w / r.h - 1.125).abs() < 1e-6);
            assert!(r.x >= 0.0 && r.x + r.w <= 1920.0 + 1e-9);
            assert!(r.y >= 0.0 && r.y + r.h <= 1080.0 + 1e-9);
            assert!(r.x < cx && cx < r.x + r.w);
        }
        // Each crop stops short of the other person.
        assert!(top.x + top.w < 1400.0);
        assert!(bot.x > 560.0);
    }

    #[test]
    fn a_moved_seam_gives_each_panel_its_own_aspect() {
        let duo: Vec<Duo> = (0..30).map(|i| pod(i as f64 * 0.1)).collect();
        let crops = |frac: f64| {
            let p = plan(&duo, &[], 0.0, 3.0, 1920.0, 1080.0, Canvas::TALL, frac);
            at(&p, 1.0).unwrap()
        };
        let (top, bot) = crops(0.6);
        // 1080x1152 on top, 1080x768 below.
        assert!((top.w / top.h - 1080.0 / 1152.0).abs() < 1e-6);
        assert!((bot.w / bot.h - 1080.0 / 768.0).abs() < 1e-6);
        assert!(top.x >= 0.0 && top.x + top.w <= 1920.0 + 1e-9);
        assert!(bot.y >= 0.0 && bot.y + bot.h <= 1080.0 + 1e-9);
        // 0.5 is the even split, as before.
        let (t5, b5) = crops(0.5);
        assert!((t5.w / t5.h - 1.125).abs() < 1e-6 && (b5.w / b5.h - 1.125).abs() < 1e-6);
    }

    #[test]
    fn shots_get_their_own_crops_and_thin_shots_borrow() {
        let mut duo: Vec<Duo> = (0..10).map(|i| pod(i as f64 * 0.2)).collect();
        // After the cut at 5s the pair sits further right.
        for i in 0..10 {
            let mut d = pod(5.0 + i as f64 * 0.2);
            d.left.x += 200.0;
            d.right.x += 200.0;
            duo.push(d);
        }
        let p = plan(
            &duo,
            &[5.0, 9.0],
            0.0,
            12.0,
            1920.0,
            1080.0,
            Canvas::TALL,
            0.5,
        );
        assert_eq!(p.len(), 3);
        let (a, _) = at(&p, 1.0).unwrap();
        let (b, _) = at(&p, 6.0).unwrap();
        let (c, _) = at(&p, 10.0).unwrap();
        assert!(b.cx() > a.cx() + 150.0);
        // No sightings after 9s: the previous shot's crops hold.
        assert_eq!(b, c);
        assert!(plan(&[], &[], 0.0, 1.0, 1920.0, 1080.0, Canvas::TALL, 0.5).is_empty());
    }
}
