//! Clip validation + normalization (port of ClipValidator.php).

use crate::openrouter::{RawClip, Scores};
use crate::scorer::is_hook_filler;
use crate::whisper::Word;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clip {
    pub rank: usize,
    pub start_s: f64,
    pub end_s: f64,
    pub hook_line: String,
    pub why_it_works: String,
    pub score_total: f64,
    /// Per-dimension scorecard (hook/retention/value/share) when the picker
    /// supplied one — the virality breakdown, not just a total.
    pub scores: Option<Scores>,
    pub title: Option<String>,
    pub hashtags: Vec<String>,
    pub caption_style: String,
    pub source: String,
}

pub struct Validator {
    pub min_s: f64,
    pub max_s: f64,
    pub pad_s: f64,
    /// Completeness gate (default on): boundaries snap to finished
    /// sentences — extend to the next sentence end within reach, else trim
    /// back. Best-effort inside min/max; a no-op when the transcript has
    /// no sentence punctuation.
    pub complete_gate: bool,
    /// Hook guard (default on): the opening word is never a filler —
    /// the start shifts past leading ums/uhs (max 4s).
    pub hook_guard: bool,
    /// How far past max_s the gate may extend to reach a sentence end.
    pub gate_grow_s: f64,
}

impl Default for Validator {
    fn default() -> Self {
        Self {
            min_s: 15.0,
            max_s: 90.0,
            pad_s: 0.25,
            complete_gate: true,
            hook_guard: true,
            gate_grow_s: 30.0,
        }
    }
}

fn total_of(c: &RawClip) -> f64 {
    match &c.scores {
        Some(s) => {
            0.4 * s.hook as f64
                + 0.25 * s.retention as f64
                + 0.2 * s.value as f64
                + 0.15 * s.share as f64
        }
        None => 50.0,
    }
}

pub(crate) fn jaccard(a: &str, b: &str) -> f64 {
    let sa: std::collections::HashSet<&str> = a.split_whitespace().collect();
    let sb: std::collections::HashSet<&str> = b.split_whitespace().collect();
    if sa.is_empty() && sb.is_empty() {
        return 1.0;
    }
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count().max(1) as f64;
    inter / union
}

fn ends_sentence(w: &str) -> bool {
    w.ends_with(['.', '!', '?', '…'])
}

/// First sentence end at/after t (word end time), if any.
fn sent_end_after(words: &[Word], t: f64) -> Option<f64> {
    words
        .iter()
        .filter(|w| w.e >= t - 1e-9)
        .find(|w| ends_sentence(&w.w))
        .map(|w| w.e)
}

/// Last sentence end at/before t (word end time), if any.
fn sent_end_before(words: &[Word], t: f64) -> Option<f64> {
    words
        .iter()
        .filter(|w| w.s <= t + 1e-9)
        .filter(|w| ends_sentence(&w.w))
        .last()
        .map(|w| w.e)
}

impl Validator {
    fn snap(&self, t: f64, words: &[Word], to_start: bool) -> f64 {
        if words.is_empty() {
            return t.max(0.0);
        }
        let mut best = if to_start {
            words[0].s
        } else {
            words[words.len() - 1].e
        };
        let mut best_d = f64::INFINITY;
        for w in words {
            let c = if to_start { w.s } else { w.e };
            let d = (c - t).abs();
            if d < best_d {
                best_d = d;
                best = c;
            }
        }
        best.max(0.0)
    }

    fn one(&self, c: &RawClip, words: &[Word], duration: f64, source: &str) -> Option<Clip> {
        if duration <= 0.0 {
            return None;
        }
        let mut s = c.start_s.clamp(0.0, duration);
        let mut e = c.end_s.clamp(0.0, duration);
        if e <= s {
            return None;
        }
        // Pad + snap to word boundaries.
        s = self.snap((s - self.pad_s).max(0.0), words, true);
        e = self.snap((e + self.pad_s).min(duration), words, false);
        if self.complete_gate {
            // End: extend to the next sentence end within reach, else trim
            // back to the last one (never below min_s). Start: nudge back
            // to the sentence start when it's close (<=2s).
            if let Some(se) = sent_end_after(words, e) {
                if se <= e + self.gate_grow_s && se - s <= self.max_s + self.gate_grow_s {
                    e = se;
                } else if let Some(sb) = sent_end_before(words, e) {
                    if sb - s >= self.min_s {
                        e = sb;
                    }
                }
            } else if let Some(sb) = sent_end_before(words, e) {
                if sb - s >= self.min_s {
                    e = sb;
                }
            }
            if let Some(pe) = sent_end_before(words, s - 0.01) {
                if let Some(w) = words.iter().find(|w| w.s >= pe - 1e-9) {
                    if s - w.s <= 2.0 && w.s < e {
                        s = w.s;
                    }
                }
            }
        }
        if self.hook_guard {
            // Never open on a filler: shift past leading ums/uhs (<=4s).
            if let Some(i) = words.iter().position(|w| w.e >= s - 1e-9 && w.s < e) {
                let mut j = i;
                while j < words.len() && words[j].s < e.min(s + 4.0) && is_hook_filler(&words[j].w)
                {
                    j += 1;
                }
                if j > i && j < words.len() && words[j].s < e {
                    s = words[j].s;
                }
            }
        }
        if e - s < self.min_s {
            // Try to grow to min.
            e = (s + self.min_s).min(duration);
            if e - s < self.min_s {
                s = (e - self.min_s).max(0.0);
            }
            if e - s < self.min_s {
                return None;
            }
        }
        if e - s > self.max_s {
            e = s + self.max_s;
        }
        let hashtags: Vec<String> = c
            .hashtags
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|h| {
                let h = h.trim().trim_start_matches('#').to_string();
                format!("#{h}")
            })
            .filter(|h| h.len() > 1)
            .take(8)
            .collect();
        let style = c.caption_style.clone().unwrap_or_else(|| "karaoke".into());
        let style = match style.as_str() {
            "tiktok" | "karaoke" | "hormozi" | "minimal" | "beast" | "neon" | "highlight"
            | "ghost" => style,
            _ => "karaoke".into(),
        };
        Some(Clip {
            rank: 0,
            start_s: (s * 100.0).round() / 100.0,
            end_s: (e * 100.0).round() / 100.0,
            hook_line: c
                .hook_line
                .clone()
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect(),
            why_it_works: c
                .why_it_works
                .clone()
                .unwrap_or_default()
                .chars()
                .take(500)
                .collect(),
            score_total: total_of(c),
            scores: c.scores.clone(),
            title: c.title.clone(),
            hashtags,
            caption_style: style,
            source: source.into(),
        })
    }

    pub fn normalize(
        &self,
        raw: Vec<RawClip>,
        words: &[Word],
        duration: f64,
        count: usize,
        source: &str,
    ) -> Vec<Clip> {
        let mut clips: Vec<Clip> = raw
            .iter()
            .filter_map(|c| self.one(c, words, duration, source))
            .collect();
        // Sort by score desc, drop >50% overlaps + near-dupe hooks.
        clips.sort_by(|a, b| b.score_total.partial_cmp(&a.score_total).unwrap());
        let mut kept: Vec<Clip> = Vec::new();
        for c in clips {
            let mut dup = false;
            for k in &kept {
                let overlap = (c.end_s.min(k.end_s) - c.start_s.max(k.start_s)).max(0.0);
                let min_dur = (c.end_s - c.start_s).min(k.end_s - k.start_s).max(1.0);
                if overlap / min_dur > 0.5 || jaccard(&c.hook_line, &k.hook_line) > 0.8 {
                    dup = true;
                    break;
                }
            }
            if !dup {
                kept.push(c);
            }
            if kept.len() >= count {
                break;
            }
        }
        kept.sort_by(|a, b| b.score_total.partial_cmp(&a.score_total).unwrap());
        for (i, c) in kept.iter_mut().enumerate() {
            c.rank = i + 1;
        }
        kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openrouter::RawClip;

    fn tw(text: &str) -> Vec<Word> {
        text.split_whitespace()
            .enumerate()
            .map(|(i, w)| Word {
                w: w.into(),
                s: i as f64 * 0.5,
                e: i as f64 * 0.5 + 0.45,
                conf: Some(0.9),
            })
            .collect()
    }

    fn raw(s: f64, e: f64) -> RawClip {
        RawClip {
            start_s: s,
            end_s: e,
            hook_line: Some("hook".into()),
            why_it_works: None,
            scores: None,
            title: None,
            hashtags: None,
            caption_style: None,
        }
    }

    #[test]
    fn hook_guard_shifts_past_leading_filler() {
        // 40 words = 20s; opens "Um uh so …".
        let words = tw("Um uh so this is the real opening line of the clip and it continues for a while with more words here to fill the time today okay fine");
        let clips = Validator::default().normalize(vec![raw(0.0, 19.0)], &words, 20.0, 3, "t");
        assert_eq!(clips.len(), 1);
        assert!(
            (clips[0].start_s - 1.0).abs() < 1e-9,
            "must open on 'so', got {}",
            clips[0].start_s
        );
    }

    #[test]
    fn completeness_extends_to_sentence_end() {
        // w29 ends a sentence mid-clip; raw end lands inside the next one.
        let mut text: Vec<String> = (0..40).map(|i| format!("w{i}")).collect();
        text[29] = "done.".into();
        let words = tw(&text.join(" "));
        let v = Validator {
            min_s: 10.0,
            ..Default::default()
        };
        let clips = v.normalize(vec![raw(0.0, 12.0)], &words, 20.0, 3, "t");
        assert_eq!(clips.len(), 1);
        assert!(
            (clips[0].end_s - 14.95).abs() < 1e-9,
            "must extend to sentence end, got {}",
            clips[0].end_s
        );
    }

    #[test]
    fn gates_are_noops_without_punctuation_or_filler() {
        let words = tw(&"word ".repeat(60)); // 30s, no punct, no filler
        let clips = Validator::default().normalize(vec![raw(0.0, 20.0)], &words, 30.0, 3, "t");
        assert_eq!(clips.len(), 1);
        assert!((clips[0].start_s - 0.0).abs() < 1e-9);
        assert!((clips[0].end_s - 20.0).abs() < 0.5);
    }
}
