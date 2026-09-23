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
            // Head gap: keep the tail (lead-in); body gaps: keep the head.
            let (ra, rb) = if first {
                (prev_e, (w.s - keep).max(prev_e))
            } else {
                (prev_e + keep, w.s)
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
        // gap 0.95->5.0 (4.05s pause): keep 0.25 head, cut the rest.
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
            (p.keeps[0].b - 1.2).abs() < 1e-9,
            "pause head kept: {:?}",
            p.keeps[0]
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
    fn overlapping_ranges_fuse() {
        // fb3's bug: 86-101 + 97-112 played the shared minute twice.
        let r = fuse_overlaps(vec![(97.0, 112.0), (23.8, 38.8), (86.0, 101.0)]);
        assert_eq!(r.len(), 2, "got {r:?}");
        assert!((r[0].0 - 23.8).abs() < 1e-9 && (r[0].1 - 38.8).abs() < 1e-9);
        assert!((r[1].0 - 86.0).abs() < 1e-9 && (r[1].1 - 112.0).abs() < 1e-9);
    }
}
