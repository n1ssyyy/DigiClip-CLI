//! `--redo`: re-render exact clips instead of picking. Serve uses it for
//! clip edits (new range, title, caption style, fixed caption words) and
//! for clips made by hand from the transcript; the CLI takes the same JSON.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::openrouter::Scores;
use crate::validator::Clip;
use crate::whisper::Word;

/// One caption fix: the word starting at `s` (source seconds, ±10 ms)
/// reads `w` instead; an empty `w` drops the word from the captions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fix {
    pub s: f64,
    pub w: String,
}

/// One clip to render. Everything but the range is optional: an edit
/// carries the original's title/why/scores so they survive, a new clip
/// gets a title from its own words.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Spec {
    pub rank: usize,
    pub start_s: f64,
    pub end_s: f64,
    pub title: Option<String>,
    pub hook: Option<String>,
    pub style: Option<String>,
    pub why: Option<String>,
    pub score: Option<f64>,
    pub scores: Option<Scores>,
    pub hashtags: Vec<String>,
    pub source: Option<String>,
    pub fixes: Vec<Fix>,
}

/// Shortest clip a redo may ask for.
pub const MIN_LEN_S: f64 = 1.0;

pub fn load(path: &Path) -> anyhow::Result<Vec<Spec>> {
    let raw = std::fs::read(path).map_err(|e| anyhow::anyhow!("--redo {}: {e}", path.display()))?;
    let specs: Vec<Spec> = serde_json::from_slice(&raw)
        .map_err(|e| anyhow::anyhow!("--redo {}: bad JSON ({e})", path.display()))?;
    if specs.is_empty() {
        anyhow::bail!("--redo {}: no clips listed", path.display());
    }
    let mut seen = std::collections::HashSet::new();
    for s in &specs {
        if s.rank == 0 || !seen.insert(s.rank) {
            anyhow::bail!(
                "--redo: every clip needs its own rank >= 1 (got {})",
                s.rank
            );
        }
    }
    Ok(specs)
}

/// Apply caption fixes to the transcript. Returns how many words changed.
pub fn apply_fixes(words: &mut Vec<Word>, fixes: &[Fix]) -> usize {
    let mut changed = 0;
    for f in fixes {
        let Some(i) = words.iter().position(|w| (w.s - f.s).abs() <= 0.010 + 1e-9) else {
            tracing::warn!("caption fix at {:.2}s matches no word", f.s);
            continue;
        };
        let text = f.w.trim();
        if text.is_empty() {
            words.remove(i);
            changed += 1;
        } else if words[i].w != text {
            words[i].w = text.to_string();
            changed += 1;
        }
    }
    changed
}

/// The spoken text of `[a, b)` (first `max` words).
fn text_of(words: &[Word], a: f64, b: f64, max: usize) -> String {
    words
        .iter()
        .filter(|w| w.e > a && w.s < b)
        .take(max)
        .map(|w| w.w.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build the clip for one spec: range clamped to the video, at least
/// [`MIN_LEN_S`] long and holding some speech.
pub fn to_clip(spec: &Spec, words: &[Word], dur: f64, style: &str) -> anyhow::Result<Clip> {
    let a = spec.start_s.max(0.0);
    let b = if dur > 0.0 {
        spec.end_s.min(dur)
    } else {
        spec.end_s
    };
    if b - a < MIN_LEN_S || (b - a).is_nan() {
        anyhow::bail!(
            "clip #{}: range {:.2}-{:.2}s is shorter than {MIN_LEN_S}s",
            spec.rank,
            spec.start_s,
            spec.end_s
        );
    }
    if !words.iter().any(|w| w.e > a && w.s < b) {
        anyhow::bail!("clip #{}: no speech between {a:.1}s and {b:.1}s", spec.rank);
    }
    let hook = spec
        .hook
        .clone()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| text_of(words, a, b, 14));
    let title = spec
        .title
        .clone()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let style = crate::captions::ass::valid_preset(spec.style.as_deref().unwrap_or(style));
    Ok(Clip {
        rank: spec.rank,
        start_s: a,
        end_s: b,
        hook_line: hook,
        why_it_works: spec
            .why
            .clone()
            .unwrap_or_else(|| "Picked by hand from the transcript.".into()),
        score_total: spec.score.unwrap_or(0.0),
        scores: spec.scores.clone(),
        title,
        hashtags: spec.hashtags.clone(),
        caption_style: style,
        source: spec.source.clone().unwrap_or_else(|| "custom".into()),
    })
}

/// Merge `fresh` entries into the JSON array at `path` by `rank` (a
/// redo rewrites only its own clips in `clips.json` / `cut_plan.json`).
pub fn merge_by_rank(path: &Path, fresh: &[serde_json::Value]) -> anyhow::Result<()> {
    let mut all: Vec<serde_json::Value> = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let rank = |v: &serde_json::Value| v.get("rank").and_then(|r| r.as_u64()).unwrap_or(0);
    for f in fresh {
        let r = rank(f);
        match all.iter_mut().find(|v| rank(v) == r) {
            Some(slot) => *slot = f.clone(),
            None => all.push(f.clone()),
        }
    }
    all.sort_by_key(rank);
    std::fs::write(path, serde_json::to_string_pretty(&all)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(t: &str, s: f64, e: f64) -> Word {
        Word {
            w: t.into(),
            s,
            e,
            conf: None,
        }
    }

    fn words() -> Vec<Word> {
        vec![
            w("So", 0.0, 0.2),
            w("the", 0.3, 0.4),
            w("secret", 0.5, 0.9),
            w("is", 1.0, 1.1),
            w("compounding.", 1.2, 1.9),
        ]
    }

    #[test]
    fn fixes_rewrite_and_drop_words_by_start() {
        let mut ws = words();
        let n = apply_fixes(
            &mut ws,
            &[
                Fix {
                    s: 0.505,
                    w: "SECRET".into(),
                },
                Fix {
                    s: 0.0,
                    w: " ".into(),
                },
                Fix {
                    s: 7.0,
                    w: "nothing".into(),
                },
            ],
        );
        assert_eq!(n, 2);
        assert_eq!(ws.len(), 4);
        assert_eq!(ws[0].w, "the");
        assert_eq!(ws[1].w, "SECRET");
        // Timing never moves.
        assert_eq!((ws[1].s, ws[1].e), (0.5, 0.9));
    }

    #[test]
    fn spec_becomes_a_clip_with_defaults() {
        let spec = Spec {
            rank: 4,
            start_s: 0.25,
            end_s: 30.0,
            ..Default::default()
        };
        let c = to_clip(&spec, &words(), 2.0, "hormozi").unwrap();
        assert_eq!(c.rank, 4);
        assert_eq!((c.start_s, c.end_s), (0.25, 2.0));
        assert_eq!(c.caption_style, "hormozi");
        assert_eq!(c.source, "custom");
        assert_eq!(c.hook_line, "the secret is compounding.");
        assert!(c.title.is_none());
    }

    #[test]
    fn edits_keep_what_the_picker_said() {
        let spec = Spec {
            rank: 1,
            start_s: 0.0,
            end_s: 1.9,
            title: Some("  The secret  ".into()),
            why: Some("Strong payoff.".into()),
            score: Some(81.5),
            source: Some("llm:x".into()),
            style: Some("minimal".into()),
            ..Default::default()
        };
        let c = to_clip(&spec, &words(), 10.0, "karaoke").unwrap();
        assert_eq!(c.title.as_deref(), Some("The secret"));
        assert_eq!(c.why_it_works, "Strong payoff.");
        assert_eq!(c.score_total, 81.5);
        assert_eq!(c.source, "llm:x");
        assert_eq!(c.caption_style, "minimal");
    }

    #[test]
    fn empty_or_silent_ranges_are_refused() {
        let short = Spec {
            rank: 1,
            start_s: 1.0,
            end_s: 1.5,
            ..Default::default()
        };
        assert!(to_clip(&short, &words(), 10.0, "karaoke").is_err());
        let silent = Spec {
            rank: 1,
            start_s: 3.0,
            end_s: 9.0,
            ..Default::default()
        };
        assert!(to_clip(&silent, &words(), 10.0, "karaoke").is_err());
    }

    #[test]
    fn load_rejects_duplicate_ranks() {
        let d = std::env::temp_dir().join(format!("digiclip-redo-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("redo.json");
        std::fs::write(
            &p,
            r#"[{"rank":1,"start_s":0,"end_s":5},{"rank":1,"start_s":6,"end_s":9}]"#,
        )
        .unwrap();
        assert!(load(&p).is_err());
        std::fs::write(
            &p,
            r#"[{"rank":2,"start_s":0,"end_s":5,"fixes":[{"s":1,"w":"x"}]}]"#,
        )
        .unwrap();
        let specs = load(&p).unwrap();
        assert_eq!(specs[0].fixes.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn merge_by_rank_replaces_and_appends() {
        let d = std::env::temp_dir().join(format!("digiclip-merge-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("clips.json");
        std::fs::write(&p, r#"[{"rank":1,"t":"a"},{"rank":2,"t":"b"}]"#).unwrap();
        merge_by_rank(
            &p,
            &[
                serde_json::json!({"rank": 2, "t": "B"}),
                serde_json::json!({"rank": 3, "t": "c"}),
            ],
        )
        .unwrap();
        let v: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        let ts: Vec<&str> = v.iter().map(|x| x["t"].as_str().unwrap()).collect();
        assert_eq!(ts, ["a", "B", "c"]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
