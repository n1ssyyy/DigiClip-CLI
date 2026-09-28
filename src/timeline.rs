//! Clip timelines: kept source intervals, the tight clock, and tightening.
//!
//! Picking yields single ranges; tightening and merging compile them down
//! to TIMELINES (ordered kept intervals). Rendering consumes timelines:
//! one segment render per keep, concatenated. Joins between keeps are
//! always hard cuts (jump cuts snap — the path never glides across
//! deleted time).

use crate::whisper::Word;
use serde::{Deserialize, Serialize};

/// One kept source interval [a, b).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Keep {
    pub a: f64,
    pub b: f64,
}

/// A removed span, with the human reason (auditable in cut_plan.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Removed {
    pub a: f64,
    pub b: f64,
    pub reason: String,
}

/// Tightening result for one picked range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CutPlan {
    pub keeps: Vec<Keep>,
    pub removed: Vec<Removed>,
    pub tight_dur: f64,
}

pub fn plan_dur(keeps: &[Keep]) -> f64 {
    keeps.iter().map(|k| (k.b - k.a).max(0.0)).sum()
}

/// Source clock -> tight clock (None when the instant was removed).
pub fn tight(t: f64, keeps: &[Keep]) -> Option<f64> {
    let mut o = 0.0;
    for k in keeps {
        if t < k.a - 1e-6 {
            return None;
        }
        if t <= k.b + 1e-6 {
            return Some(o + (t - k.a).clamp(0.0, k.b - k.a));
        }
        o += k.b - k.a;
    }
    None
}

/// Words retimed to the tight clock (dropped outside keeps; straddlers
/// clamp to the keep edge; slivers <20ms drop).
pub fn retime(words: &[Word], keeps: &[Keep]) -> Vec<Word> {
    let mut offs: Vec<f64> = Vec::with_capacity(keeps.len());
    let mut o = 0.0;
    for k in keeps {
        offs.push(o);
        o += k.b - k.a;
    }
    words
        .iter()
        .filter_map(|w| {
            for (k, off) in keeps.iter().zip(offs.iter()) {
                if w.s >= k.a - 1e-6 && w.s <= k.b + 1e-6 {
                    let s = off + (w.s - k.a).clamp(0.0, k.b - k.a);
                    let e = if w.e <= k.b + 1e-6 {
                        off + (w.e - k.a).clamp(0.0, k.b - k.a)
                    } else {
                        off + (k.b - k.a)
                    };
                    if e - s < 0.02 {
                        return None;
                    }
                    let mut q = w.clone();
                    q.s = s;
                    q.e = e;
                    return Some(q);
                }
            }
            None
        })
        .collect()
}

fn clean(w: &str) -> String {
    w.trim()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// Default filler set for punchy tightening (single tokens only —
/// whisper words are single tokens).
pub const FILLERS: &[&str] = &["um", "uh", "umm", "uhh", "hmm", "er", "ah", "mm", "mhm"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TightenMode {
    Off,
    Light,
    Punchy,
}

pub struct TightenCfg {
    pub mode: TightenMode,
    pub pause_above: f64,
    pub pause_keep: f64,
    pub fillers: Vec<String>,
}

impl Default for TightenCfg {
    fn default() -> Self {
        Self {
            mode: TightenMode::Light,
            pause_above: 0.8,
            pause_keep: 0.25,
            fillers: FILLERS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// Compile [start, end) + words into kept intervals.
/// - Off: single keep (today's exact path).
/// - Light: pauses longer than pause_above shrink to pause_keep (kept at
///   the gap head, a natural beat). Never deletes words.
/// - Punchy: light + filler words go (padded 80ms), merged with pauses.
///
/// Guards: keeps <0.4s absorb (no strobing slivers); if the plan would
/// remove >60% or leave <4s, it relaxes a level (punchy->light->off).
pub fn tighten(start: f64, end: f64, words: &[Word], cfg: &TightenCfg) -> CutPlan {
    if cfg.mode == TightenMode::Off || end - start < 1.0 {
        return CutPlan {
            keeps: vec![Keep { a: start, b: end }],
            removed: vec![],
            tight_dur: (end - start).max(0.0),
        };
    }
    let in_range: Vec<&Word> = words.iter().filter(|w| w.e > start && w.s < end).collect();
    let mut removed: Vec<(f64, f64, String)> = Vec::new();
    // Word gaps (incl. head/tail of the range).
    let mut prev_e = start;
    let mut first = true;
    for w in &in_range {
        let gap = w.s - prev_e;
        if gap > cfg.pause_above {
            let keep = cfg.pause_keep.min(gap);
            // Head gap: keep the tail (lead-in). Body gaps split the kept
            // beat: a hang after the last word (its release/breath) and a
            // pre-roll before the next (whisper onsets run late; cutting
            // exactly on them clips the first consonant).
            let (ra, rb) = if first {
                (prev_e, (w.s - keep).max(prev_e))
            } else {
                (
                    prev_e + keep * 0.6,
                    (w.s - keep * 0.4).max(prev_e + keep * 0.6),
                )
            };
            if rb - ra > 0.05 {
                removed.push((ra, rb, format!("pause {gap:.1}s->{keep:.2}s")));
            }
        }
        prev_e = prev_e.max(w.e);
        first = false;
    }
    let tail = end - prev_e;
    if tail > cfg.pause_above {
        let ra = prev_e + cfg.pause_keep.min(tail);
        if end - ra > 0.05 {
            removed.push((ra, end, format!("tail pause {tail:.1}s")));
        }
    }
    if cfg.mode == TightenMode::Punchy {
        // Filler pad reaches into silence only — never into the neighbor
        // words (dense speech keeps every neighbor's head for captions).
        for (idx, w) in in_range.iter().enumerate() {
            if cfg.fillers.iter().any(|f| f == &clean(&w.w)) {
                let ra =
                    (w.s - 0.08)
                        .max(start)
                        .max(if idx > 0 { in_range[idx - 1].e } else { start });
                let rb = (w.e + 0.08).min(end).min(if idx + 1 < in_range.len() {
                    in_range[idx + 1].s
                } else {
                    end
                });
                if rb - ra > 0.02 {
                    removed.push((ra, rb, format!("filler '{}'", clean(&w.w))));
                }
            }
        }
    }
    let mut plan = complement(start, end, &mut removed);
    // Relaxation guard: never gut the clip.
    let removed_frac = 1.0 - plan.tight_dur / (end - start).max(1e-9);
    if plan.tight_dur < 4.0 || removed_frac > 0.6 {
        if cfg.mode == TightenMode::Punchy {
            let light = TightenCfg {
                mode: TightenMode::Light,
                ..Default::default()
            };
            let light = TightenCfg {
                pause_above: cfg.pause_above,
                pause_keep: cfg.pause_keep,
                ..light
            };
            plan = tighten(start, end, words, &light);
        } else {
            plan = CutPlan {
                keeps: vec![Keep { a: start, b: end }],
                removed: vec![],
                tight_dur: (end - start).max(0.0),
            };
        }
    }
    plan
}

/// Merge overlapping removals, complement into keeps, absorb <0.4s slivers.
fn complement(start: f64, end: f64, removed: &mut Vec<(f64, f64, String)>) -> CutPlan {
    removed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut merged: Vec<(f64, f64, String)> = Vec::new();
    for (a, b, r) in removed.drain(..) {
        if b <= start || a >= end {
            continue;
        }
        let (a, b) = (a.max(start), b.min(end));
        if b - a <= 0.0 {
            continue;
        }
        if let Some(last) = merged.last_mut() {
            if a <= last.1 + 0.05 {
                last.1 = last.1.max(b);
                if !last.2.contains(&r) {
                    last.2.push_str(&format!(" + {r}"));
                }
                continue;
            }
        }
        merged.push((a, b, r));
    }
    let mut keeps: Vec<Keep> = Vec::new();
    let mut cur = start;
    for (a, b, _) in &merged {
        if *a - cur >= 0.4 {
            keeps.push(Keep { a: cur, b: *a });
        }
        cur = cur.max(*b);
    }
    if end - cur >= 0.4 {
        keeps.push(Keep { a: cur, b: end });
    }
    if keeps.is_empty() {
        keeps.push(Keep { a: start, b: end });
        merged.clear();
    }
    let removed: Vec<Removed> = merged
        .into_iter()
        .map(|(a, b, reason)| Removed { a, b, reason })
        .collect();
    let tight_dur = plan_dur(&keeps);
    CutPlan {
        keeps,
        removed,
        tight_dur,
    }
}

/// Move every cut edge of a plan to the quietest instant nearby, never
/// into a neighboring word. Whisper word stamps drift ±50–150 ms, so edges
/// placed on them clip consonants and breaths; the audio envelope knows
/// where the silence really is. Clip edges also get breathing room: a
/// lead-in before the first word and a hang after the last, as far as the
/// silence (and the neighbors) allow. Removed spans follow their edges.
pub fn refine_edges(plan: &mut CutPlan, env: &crate::audio::Envelope, words: &[Word]) {
    if env.is_empty() || plan.keeps.is_empty() {
        return;
    }
    let n = plan.keeps.len();
    let old = plan.keeps.clone();
    // Last word end at/before t, first word start at/after t.
    let end_before = |t: f64| {
        words
            .iter()
            .filter(|w| w.e <= t + 1e-6)
            .map(|w| w.e)
            .fold(f64::NEG_INFINITY, f64::max)
    };
    let start_after = |t: f64| {
        words
            .iter()
            .filter(|w| w.s >= t - 1e-6)
            .map(|w| w.s)
            .fold(f64::INFINITY, f64::min)
    };
    for i in 0..n {
        let k = plan.keeps[i];
        // Words inside this keep bound both edges.
        let inside: Vec<&Word> = words.iter().filter(|w| w.s < k.b && w.e > k.a).collect();
        let first_s = inside.first().map(|w| w.s).unwrap_or(k.a);
        let last_e = inside.iter().map(|w| w.e).fold(k.a, f64::max);
        // --- start edge ---
        let floor = if i > 0 {
            plan.keeps[i - 1].b + 0.02
        } else {
            f64::NEG_INFINITY
        };
        let prev_e = end_before(k.a.min(first_s));
        let (lo, hi, pref) = if i == 0 {
            // Clip head: up to ~0.2 s of lead-in, preferring ~0.12 s.
            let lo = (first_s - 0.2).max(prev_e + 0.03).max(floor);
            (lo, first_s, (first_s - 0.12).max(lo))
        } else {
            let lo = (k.a - 0.06).max(prev_e + 0.02).max(floor);
            (lo, (k.a + 0.04).min(first_s), k.a)
        };
        if hi - lo > 0.03 {
            plan.keeps[i].a = env.quietest(lo, hi, pref.clamp(lo, hi));
        } else if i == 0 && lo < k.a {
            plan.keeps[i].a = lo.max(0.0).min(k.a);
        }
        // --- end edge ---
        let next_s = start_after(k.b.max(last_e));
        let ceil = if i + 1 < n {
            old[i + 1].a - 0.02
        } else {
            f64::INFINITY
        };
        let (lo, hi, pref) = if i + 1 == n {
            // Clip tail: hang up to ~0.3 s past the last word.
            let hi = (last_e + 0.3).min(next_s - 0.03).min(ceil);
            (last_e, hi, (last_e + 0.2).min(hi))
        } else {
            let lo = (k.b - 0.06).max(last_e);
            (lo, (k.b + 0.06).min(next_s - 0.02).min(ceil), k.b)
        };
        if hi - lo > 0.03 {
            plan.keeps[i].b = env.quietest(lo, hi, pref.clamp(lo, hi));
        }
        let min_len = 0.2;
        if plan.keeps[i].b - plan.keeps[i].a < min_len {
            plan.keeps[i] = k; // never let a refinement gut a keep
        }
    }
    // Removed spans track the moved edges.
    for r in plan.removed.iter_mut() {
        for (o, k) in old.iter().zip(plan.keeps.iter()) {
            if (r.b - o.a).abs() < 1e-6 {
                r.b = k.a;
            }
            if (r.a - o.b).abs() < 1e-6 {
                r.a = k.b;
            }
        }
    }
    plan.removed.retain(|r| r.b - r.a > 1e-3);
    plan.tight_dur = plan_dur(&plan.keeps);
}

/// Snap keeps onto the output frame grid: starts to the nearest frame,
/// lengths to whole frames. Video frames and audio samples then share one
/// exact clock (captions retimed from the snapped keeps land on the frame
/// they belong to). Neighbors closer than a frame fuse (no one-frame
/// jump cuts).
pub fn snap_keeps(keeps: &mut Vec<Keep>, fps: (u32, u32)) {
    let r = fps.0 as f64 / fps.1.max(1) as f64;
    let mut out: Vec<Keep> = Vec::with_capacity(keeps.len());
    let mut prev_b = f64::NEG_INFINITY;
    for k in keeps.iter() {
        let a = (k.a * r).round() / r;
        let b = (k.b * r).round() / r;
        let mut k2 = Keep {
            a,
            b: b.max(a + 1.0 / r),
        };
        if let Some(last) = out.last_mut() {
            // Less than a frame was removed (or rounding made them touch):
            // one continuous keep, not a one-frame jump cut.
            if k.a - prev_b < 1.0 / r || k2.a <= last.b + 0.5 / r {
                last.b = last.b.max(k2.b);
                prev_b = k.b;
                continue;
            }
            k2.a = k2.a.max(last.b);
        }
        prev_b = k.b;
        out.push(k2);
    }
    *keeps = out;
}

/// Fuse overlapping/nearby ranges (gap < 1.0s) into unions, sorted.
/// Merge joins shared content instead of repeating it back-to-back.
pub fn fuse_overlaps(mut ranges: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    ranges.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut out: Vec<(f64, f64)> = Vec::new();
    for (a, b) in ranges {
        if b - a < 0.5 {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if a <= last.1 + 1.0 {
                last.1 = last.1.max(b);
                continue;
            }
        }
        out.push((a, b));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tw(text: &str) -> Vec<Word> {
        text.split_whitespace()
            .enumerate()
            .map(|(i, w)| crate::whisper::Word {
                w: w.into(),
                s: i as f64 * 0.5,
                e: i as f64 * 0.5 + 0.45,
                conf: Some(0.9),
            })
            .collect()
    }

    #[test]
    fn off_is_identity() {
        let words = tw("hello world");
        let p = tighten(
            0.0,
            10.0,
            &words,
            &TightenCfg {
                mode: TightenMode::Off,
                ..Default::default()
            },
        );
        assert_eq!(p.keeps.len(), 1);
        assert!((p.tight_dur - 10.0).abs() < 1e-9);
    }

    #[test]
    fn light_trims_long_pause_keeps_words() {
        // gap 0.95->5.0 (4.05s pause): keep a 0.15 hang + 0.10 pre-roll.
        let mut words = tw("one two");
        for (i, w) in ["three", "four", "five", "six", "seven", "eight"]
            .iter()
            .enumerate()
        {
            words.push(crate::whisper::Word {
                w: (*w).into(),
                s: 5.0 + i as f64 * 0.5,
                e: 5.45 + i as f64 * 0.5,
                conf: Some(0.9),
            });
        }
        let p = tighten(0.0, 10.0, &words, &TightenCfg::default());
        assert_eq!(p.keeps.len(), 2);
        assert!(
            (p.keeps[0].b - 1.1).abs() < 1e-9,
            "pause hang kept: {:?}",
            p.keeps[0]
        );
        assert!(
            (p.keeps[1].a - 4.9).abs() < 1e-9,
            "pre-roll before the next word: {:?}",
            p.keeps[1]
        );
        // All words survive a light tighten.
        let rt = retime(&words, &p.keeps);
        assert_eq!(rt.len(), 8);
        assert!(rt[2].s < 2.0, "third word pulled tight: {}", rt[2].s);
    }

    #[test]
    fn punchy_removes_filler_and_drops_its_caption() {
        let words = tw("this is um the point here today folks listen up now");
        let cfg = TightenCfg {
            mode: TightenMode::Punchy,
            ..Default::default()
        };
        let p = tighten(0.0, 10.0, &words, &cfg);
        assert!(
            p.removed.iter().any(|r| r.reason.contains("um")),
            "plan: {:?}",
            p.removed
        );
        let rt = retime(&words, &p.keeps);
        assert!(
            !rt.iter().any(|w| clean(&w.w) == "um"),
            "filler caption dropped"
        );
        assert!(rt.len() + 1 == words.len());
    }

    #[test]
    fn tight_maps_and_clamps() {
        let keeps = vec![Keep { a: 10.0, b: 12.0 }, Keep { a: 20.0, b: 25.0 }];
        assert!((tight(11.0, &keeps).unwrap() - 1.0).abs() < 1e-9);
        assert!((tight(22.0, &keeps).unwrap() - 4.0).abs() < 1e-9);
        assert!(tight(15.0, &keeps).is_none());
    }

    #[test]
    fn gut_guard_relaxes() {
        // 90% pause: punchy must refuse to nuke the clip.
        let words = tw("hi");
        let cfg = TightenCfg {
            mode: TightenMode::Punchy,
            ..Default::default()
        };
        let p = tighten(0.0, 30.0, &words, &cfg);
        assert!(p.tight_dur >= 4.0 || p.removed.is_empty(), "plan: {p:?}");
    }

    #[test]
    fn snapped_keeps_are_whole_frames_and_never_overlap() {
        let mut k = vec![
            Keep { a: 1.013, b: 2.51 },
            Keep { a: 2.52, b: 4.0 }, // closer than a frame: fuses
            Keep { a: 7.004, b: 9.3 },
        ];
        snap_keeps(&mut k, (30, 1));
        assert_eq!(k.len(), 2, "{k:?}");
        for x in &k {
            let (fa, fl) = (x.a * 30.0, (x.b - x.a) * 30.0);
            assert!(
                (fa - fa.round()).abs() < 1e-6 && (fl - fl.round()).abs() < 1e-6,
                "{x:?}"
            );
        }
        assert!(k[1].a >= k[0].b);
    }

    #[test]
    fn refined_edges_land_in_silence_and_give_breathing_room() {
        // Speech 1.0-2.0 and 3.0-4.0 (1 kHz tone), silence elsewhere; the
        // transcript's stamps are late by ~80 ms on the second onset.
        let n = 5 * 16000;
        let pcm: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / 16000.0;
                if (1.0..2.0).contains(&t) || (3.0..4.0).contains(&t) {
                    0.5 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin() as f32
                } else {
                    0.0
                }
            })
            .collect();
        let env = crate::audio::Envelope::from_samples(&pcm, 16000);
        let w = |s: f64, e: f64| crate::whisper::Word {
            w: "x".into(),
            s,
            e,
            conf: Some(0.9),
        };
        let words = vec![w(1.0, 2.0), w(3.08, 4.0)];
        let mut p = CutPlan {
            keeps: vec![Keep { a: 1.0, b: 2.15 }, Keep { a: 2.98, b: 4.0 }],
            removed: vec![Removed {
                a: 2.15,
                b: 2.98,
                reason: "pause".into(),
            }],
            tight_dur: 0.0,
        };
        refine_edges(&mut p, &env, &words);
        let (k0, k1) = (p.keeps[0], p.keeps[1]);
        assert!(
            k0.a < 1.0 && k0.a > 0.75,
            "lead-in before the first word: {k0:?}"
        );
        assert!(k0.b >= 2.0, "never clips the word: {k0:?}");
        assert!(
            k1.a <= 3.0,
            "the real onset (3.0) survives the late stamp: {k1:?}"
        );
        assert!(k1.b > 4.0 && k1.b <= 4.31, "tail hang: {k1:?}");
        assert!((p.removed[0].a - k0.b).abs() < 1e-9 && (p.removed[0].b - k1.a).abs() < 1e-9);
    }

    #[test]
    fn overlapping_ranges_fuse() {
        // fb3's bug: 86-101 + 97-112 played the shared minute twice.
        let r = fuse_overlaps(vec![(97.0, 112.0), (23.8, 38.8), (86.0, 101.0)]);
        assert_eq!(r.len(), 2, "got {r:?}");
        assert!((r[0].0 - 23.8).abs() < 1e-9 && (r[0].1 - 38.8).abs() < 1e-9);
        assert!((r[1].0 - 86.0).abs() < 1e-9 && (r[1].1 - 112.0).abs() < 1e-9);
    }
}
