//! Offline heuristic clip scorer (port of HeuristicScorer.php).
//!
//! Used when no OpenRouter key is set, or when the LLM refuses/fails.
//! Splits the transcript into sentences, scores them, grows tight
//! windows around the best ones.

use crate::openrouter::{RawClip, Scores};
use crate::whisper::Word;

pub struct Heuristic {
    #[allow(dead_code)]
    pub target_min_s: f64,
    pub target_max_s: f64,
}

impl Default for Heuristic {
    fn default() -> Self {
        Self {
            target_min_s: 20.0,
            target_max_s: 45.0,
        }
    }
}

struct Sent {
    idx: usize,
    start: usize,
    end: usize, // exclusive word index
    #[allow(dead_code)]
    s: f64,
    #[allow(dead_code)]
    e: f64,
    text: String,
    score: i64,
}

pub fn split_sentences(words: &[Word]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, w) in words.iter().enumerate() {
        let ends = w.w.ends_with(['.', '!', '?', '…']) || (i + 1 - start) >= 30;
        if ends {
            out.push((start, i + 1));
            start = i + 1;
        }
    }
    if start < words.len() {
        out.push((start, words.len()));
    }
    out
}

fn score_sentence(text: &str, nwords: usize) -> i64 {
    let mut s: i64 = 55;
    if text.contains('?') {
        s += 12;
    }
    if text.contains('!') {
        s += 6;
    }
    if text.chars().any(|c| c.is_ascii_digit()) {
        s += 6;
    }
    if (8..=25).contains(&nwords) {
        s += 6;
    }
    s.min(92)
}

pub fn propose(words: &[Word], count: usize, ctx: &Heuristic) -> Vec<RawClip> {
    if words.is_empty() || count == 0 {
        return vec![];
    }
    let mut sents = Vec::new();
    for (idx, (a, b)) in split_sentences(words).into_iter().enumerate() {
        let slice = &words[a..b.min(words.len())];
        if slice.is_empty() {
            continue;
        }
        let text = slice
            .iter()
            .map(|w| w.w.clone())
            .collect::<Vec<_>>()
            .join(" ");
        sents.push(Sent {
            idx,
            start: a,
            end: b.min(words.len()),
            s: slice.first().map(|w| w.s).unwrap_or(0.0),
            e: slice.last().map(|w| w.e).unwrap_or(0.0),
            text: text.clone(),
            score: score_sentence(&text, slice.len()),
        });
    }
    sents.sort_by(|a, b| b.score.cmp(&a.score).then(a.idx.cmp(&b.idx)));
    let mut clips = Vec::new();
    // Over-propose (up to 3x): the validator dedupes overlaps down to
    // `count`, so handing it exactly `count` raw picks guarantees
    // shortfalls whenever two cluster. Ranked best-first, it keeps the
    // best distinct N.
    for sent in sents.iter().take(count * 3) {
        // Grow to ~3x sentence words, clamped to target window.
        let want = ((sent.end - sent.start) * 3).clamp(8, 60);
        let mut a = sent.start;
        let mut b = sent.end;
        while b - a < want && (a > 0 || b < words.len()) {
            if a > 0 {
                a -= 1;
            }
            if b - a >= want || b >= words.len() {
                break;
            }
            b += 1;
        }
        let s = words[a].s;
        let mut e = words[b - 1].e;
        if e - s < 8.0 && b < words.len() {
            e = words[b.min(words.len() - 1)].e;
        }
        if e - s > ctx.target_max_s + 20.0 {
            e = s + ctx.target_max_s;
        }
        let hook_line = sent.text.chars().take(200).collect::<String>();
        let sc = sent.score;
        clips.push(RawClip {
            start_s: s,
            end_s: e,
            hook_line: Some(hook_line),
            why_it_works: Some("Offline heuristic pick (no LLM key set).".into()),
            scores: Some(Scores {
                hook: sc,
                retention: sc - 5,
                value: sc - 8,
                share: sc - 10,
            }),
            title: None,
            hashtags: None,
            caption_style: Some("karaoke".into()),
        });
    }
    clips
}

/// Merit bar for `--count 0` (auto): candidates at/above this total are
/// "worth clipping". Calibrated to the heuristic scale (base 55 + bonuses
/// to 92): questions/digits/emphatic lines clear it, plain filler doesn't.
/// The best candidate always survives even if nothing clears the bar.
pub const AUTO_KEEP_SCORE: f64 = 62.0;

/// Words that must never open a clip. The hook guard shifts the start past
/// them (whisper glues punctuation onto words, so matching strips edge
/// punctuation and case first). Deliberately conservative — only pure
/// fillers, never real openers like "so"/"and"/"but".
pub const HOOK_FILLERS: &[&str] = &[
    "um", "uh", "umm", "uhh", "hmm", "mm", "mhm", "er", "ah", "eh",
];

pub fn is_hook_filler(w: &str) -> bool {
    let t = w
        .trim()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    !t.is_empty() && HOOK_FILLERS.contains(&t.as_str())
}

/// Complete-thought picks: top sentences grown by WHOLE sentences to >=20s
/// (forward first so the payoff stays inside, then backward), capped at
/// ~120s. Boundaries are sentence ends by construction — length flexes,
/// the ending never lands mid-thought.
pub fn propose_complete(words: &[Word], count: usize) -> Vec<RawClip> {
    if words.is_empty() || count == 0 {
        return vec![];
    }
    struct CS {
        s: f64,
        e: f64,
        text: String,
        score: i64,
    }
    let mut ss: Vec<CS> = Vec::new();
    for (a, b) in split_sentences(words) {
        let slice = &words[a..b.min(words.len())];
        if slice.is_empty() {
            continue;
        }
        let text = slice
            .iter()
            .map(|w| w.w.clone())
            .collect::<Vec<_>>()
            .join(" ");
        ss.push(CS {
            s: slice.first().map(|w| w.s).unwrap_or(0.0),
            e: slice.last().map(|w| w.e).unwrap_or(0.0),
            score: score_sentence(&text, slice.len()),
            text,
        });
    }
    let mut order: Vec<usize> = (0..ss.len()).collect();
    order.sort_by(|x, y| ss[*y].score.cmp(&ss[*x].score).then(x.cmp(y)));
    let mut clips = Vec::new();
    // Same over-proposal contract as `propose`: up to 3x ranked
    // candidates, the validator keeps the best distinct `count`.
    for si in order.into_iter().take(count * 3) {
        let (mut a, mut b) = (si, si);
        while ss[b].e - ss[a].s < 20.0 && b + 1 < ss.len() && ss[b + 1].e - ss[a].s <= 120.0 {
            b += 1;
        }
        while ss[b].e - ss[a].s < 20.0 && a > 0 && ss[b].e - ss[a - 1].s <= 120.0 {
            a -= 1;
        }
        let s = ss[a].s;
        let e = ss[b].e;
        if e - s < 8.0 {
            continue;
        }
        let hook_line = ss[a].text.chars().take(200).collect::<String>();
        let full = ss[a..=b]
            .iter()
            .map(|c| c.text.clone())
            .collect::<Vec<_>>()
            .join(" ");
        let sc = score_sentence(&full, full.split_whitespace().count());
        clips.push(RawClip {
            start_s: s,
            end_s: e,
            hook_line: Some(hook_line),
            why_it_works: Some("Complete thought, sentence-snapped boundaries.".into()),
            scores: Some(Scores {
                hook: sc,
                retention: sc - 5,
                value: sc - 8,
                share: sc - 10,
            }),
            title: None,
            hashtags: None,
            caption_style: Some("karaoke".into()),
        });
    }
    clips
}

/// Seeded RNG (xorshift64 — no dependency for one call site).
struct Xor64(u64);

impl Xor64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0.max(1);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (self.next() as f64 / u64::MAX as f64) * (hi - lo).max(0.0)
    }
}

/// Random-moment picks: K seeded windows (15–45s). Light scoring only —
/// the validator's hook guard + dedupe still apply downstream.
pub fn propose_moments(words: &[Word], count: usize, seed: u64) -> Vec<RawClip> {
    if words.is_empty() || count == 0 {
        return vec![];
    }
    let dur = words.last().map(|w| w.e).unwrap_or(0.0);
    if dur <= 8.0 {
        return propose(words, count, &Heuristic::default());
    }
    let mut rng = Xor64(seed);
    let mut out = Vec::new();
    for i in 0..count {
        let len = rng.range(15.0, 45.0).min(dur);
        let s = rng.range(0.0, (dur - len).max(0.0));
        let hook = words
            .iter()
            .filter(|w| w.s >= s && w.s < s + 6.0)
            .take(12)
            .map(|w| w.w.clone())
            .collect::<Vec<_>>()
            .join(" ");
        out.push(RawClip {
            start_s: s,
            end_s: (s + len).min(dur),
            hook_line: Some(if hook.is_empty() {
                format!("Random moment {}", i + 1)
            } else {
                hook.chars().take(200).collect()
            }),
            why_it_works: Some(format!("Random moment #{} (seed {seed}).", i + 1)),
            scores: None,
            title: None,
            hashtags: None,
            caption_style: Some("karaoke".into()),
        });
    }
    out
}

/// Uniform timecut: slice [start, end) into len_s parts. Returns the parts
/// (caller reports `parts.len() × len_s`); word-edge nudging happens in
/// the validator. Parts shorter than 5s (ragged tail) are dropped.
pub fn propose_timecut(words: &[Word], start: f64, end: f64, len_s: f64) -> Vec<RawClip> {
    let len_s = len_s.max(5.0);
    let (s0, e0) = (start.max(0.0), end.max(0.0));
    if e0 - s0 < 5.0 {
        return vec![];
    }
    let n = ((e0 - s0) / len_s).ceil().max(1.0) as usize;
    (0..n)
        .filter_map(|i| {
            let a = s0 + i as f64 * len_s;
            let b = (a + len_s).min(e0);
            if b - a < 5.0 {
                return None;
            }
            let hook = words
                .iter()
                .filter(|w| w.s >= a && w.s < a + 8.0)
                .take(14)
                .map(|w| w.w.clone())
                .collect::<Vec<_>>()
                .join(" ");
            Some(RawClip {
                start_s: a,
                end_s: b,
                hook_line: Some(if hook.is_empty() {
                    format!("Part {}", i + 1)
                } else {
                    hook.chars().take(200).collect()
                }),
                why_it_works: Some(format!(
                    "Uniform timecut part {}/{} ({:.0}s).",
                    i + 1,
                    n,
                    len_s
                )),
                scores: None,
                title: None,
                hashtags: None,
                caption_style: Some("karaoke".into()),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whisper::Word;

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

    #[test]
    fn filler_match_strips_punct_and_case() {
        assert!(is_hook_filler("Um,"));
        assert!(is_hook_filler("UH"));
        assert!(!is_hook_filler("under"));
        assert!(!is_hook_filler("so"));
        assert!(!is_hook_filler(""));
    }

    #[test]
    fn timecut_counts_parts() {
        let words = tw(&"word ".repeat(200)); // 100s
        let parts = propose_timecut(&words, 0.0, 100.0, 15.0);
        assert_eq!(parts.len(), 7, "ceil(100/15) = 7 parts");
        assert!((parts[0].start_s - 0.0).abs() < 1e-9);
        assert!((parts[6].end_s - 100.0).abs() < 1e-6);
    }

    #[test]
    fn moments_are_seeded_and_bounded() {
        let words = tw(&"word ".repeat(120)); // 60s
        let a = propose_moments(&words, 3, 42);
        let b = propose_moments(&words, 3, 42);
        assert_eq!(a.len(), 3);
        assert!(
            (a[0].start_s - b[0].start_s).abs() < 1e-9,
            "same seed, same picks"
        );
        for c in &a {
            assert!(c.end_s <= 60.0 + 1e-9 && c.end_s - c.start_s >= 15.0 - 1e-9);
        }
    }

    #[test]
    fn complete_snaps_to_sentence_ends() {
        let words = tw("This is a complete thought here. And another follows right after it ends today for sure. The audience always loves a clean finish to the story arc today.");
        let clips = propose_complete(&words, 1);
        // Over-proposal: all 3 ranked sentences reach the validator.
        assert_eq!(clips.len(), 3);
        assert!(
            (clips[0].start_s - 0.0).abs() < 1e-9,
            "starts at first sentence"
        );
        let last_e = words.last().unwrap().e;
        assert!(
            (clips[0].end_s - last_e).abs() < 1e-9,
            "ends at sentence end, got {}",
            clips[0].end_s
        );
    }

    #[test]
    fn pickers_over_propose_for_validator_backfill() {
        // 12Roomy sentences: ask 2, expect the full 3x pool, not 2.
        let text = (0..12)
            .map(|i| format!("Moment number {i} lands right here today."))
            .collect::<Vec<_>>()
            .join(" ");
        let words = tw(&text);
        assert_eq!(propose(&words, 2, &Heuristic::default()).len(), 6);
        assert_eq!(propose_complete(&words, 2).len(), 6);
    }
}
