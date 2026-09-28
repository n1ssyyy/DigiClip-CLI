//! Full mode: YouTube chapters and a short summary for the upload kit.
//! With an OpenRouter key the model reads a timestamped transcript;
//! without one (or when it fails) chapters fall on the longest pauses and
//! the summary is the opening of the talk.

use crate::whisper::Word;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Chapter {
    pub start_s: f64,
    pub title: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Outline {
    pub summary: String,
    pub chapters: Vec<Chapter>,
    pub source: String,
}

/// YouTube wants 3+ chapters, the first at 0:00, each 10 s or longer.
const MIN_CHAPTERS: usize = 3;
const MIN_CHAPTER_S: f64 = 10.0;
/// Shorter videos get a summary but no chapters.
const MIN_VIDEO_S: f64 = 90.0;

/// `M:SS`, or `H:MM:SS` from an hour.
pub fn stamp(t: f64) -> String {
    let t = t.max(0.0) as u64;
    let (h, m, s) = (t / 3600, t / 60 % 60, t % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Sorted, first at 0, each at least [`MIN_CHAPTER_S`] long, titles
/// trimmed; empty when fewer than [`MIN_CHAPTERS`] survive.
pub fn tidy(mut cs: Vec<Chapter>, dur: f64) -> Vec<Chapter> {
    cs.retain(|c| c.start_s.is_finite() && !c.title.trim().is_empty());
    cs.sort_by(|a, b| a.start_s.total_cmp(&b.start_s));
    let mut out: Vec<Chapter> = Vec::new();
    for mut c in cs {
        c.title = c
            .title
            .trim()
            .trim_end_matches(['.', ':'])
            .chars()
            .take(80)
            .collect();
        c.start_s = c.start_s.max(0.0).floor();
        if out.is_empty() {
            c.start_s = 0.0;
        } else if c.start_s - out.last().unwrap().start_s < MIN_CHAPTER_S
            || dur - c.start_s < MIN_CHAPTER_S
        {
            continue;
        }
        out.push(c);
    }
    if out.len() < MIN_CHAPTERS {
        out.clear();
    }
    out
}

/// `0:00 Title` lines.
pub fn chapters_txt(cs: &[Chapter]) -> String {
    cs.iter()
        .map(|c| format!("{} {}\n", stamp(c.start_s), c.title))
        .collect()
}

/// Transcript as `[m:ss] text` blocks of about 20 s (the model's view).
fn blocks(words: &[Word]) -> String {
    let mut out = String::new();
    let mut t0: Option<f64> = None;
    let mut cur: Vec<&str> = Vec::new();
    for w in words {
        let start = *t0.get_or_insert(w.s);
        cur.push(&w.w);
        if w.e - start >= 20.0 && w.w.ends_with(['.', '?', '!']) || w.e - start >= 40.0 {
            out += &format!("[{}] {}\n", stamp(start), cur.join(" "));
            cur.clear();
            t0 = None;
        }
    }
    if let (Some(t), false) = (t0, cur.is_empty()) {
        out += &format!("[{}] {}\n", stamp(t), cur.join(" "));
    }
    out
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Offline outline: chapters on the longest pauses (spread out), titled
/// with the words that follow; the summary is the opening sentences.
pub fn heuristic(words: &[Word], dur: f64) -> Outline {
    let summary = {
        let mut s = Vec::new();
        for w in words.iter().take(60) {
            s.push(w.w.as_str());
            if s.len() >= 25 && w.w.ends_with(['.', '?', '!']) {
                break;
            }
        }
        s.join(" ")
    };
    let mut chapters = Vec::new();
    if dur >= MIN_VIDEO_S && words.len() > 1 {
        let n = ((dur / 240.0).round() as usize).clamp(MIN_CHAPTERS, 10);
        let spacing = dur / (n as f64 * 2.0);
        let mut gaps: Vec<(f64, usize)> = words
            .windows(2)
            .enumerate()
            .map(|(i, p)| (p[1].s - p[0].e, i + 1))
            .collect();
        gaps.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut starts: Vec<usize> = vec![0];
        for (_, i) in gaps {
            if starts.len() >= n {
                break;
            }
            let t = words[i].s;
            if t > spacing && starts.iter().all(|&j| (words[j].s - t).abs() >= spacing) {
                starts.push(i);
            }
        }
        starts.sort_unstable();
        chapters = starts
            .iter()
            .map(|&i| {
                let title: Vec<&str> = words[i..]
                    .iter()
                    .take(6)
                    .map(|w| w.w.trim_end_matches([',', '.', '?', '!', ';']))
                    .collect();
                Chapter {
                    start_s: if i == 0 { 0.0 } else { words[i].s },
                    title: capitalize(&title.join(" ")),
                }
            })
            .collect();
    }
    Outline {
        summary,
        chapters: tidy(chapters, dur),
        source: "heuristic".into(),
    }
}

/// Outline from the model.
async fn llm(cfg: &crate::openrouter::Config, words: &[Word], dur: f64) -> anyhow::Result<Outline> {
    let want = if dur >= MIN_VIDEO_S {
        format!(
            "and {}-{} chapters. Each chapter: \"start_s\" (seconds, taken from \
             the [m:ss] stamps; the first is 0) and a 2-6 word \"title\" in \
             sentence case",
            MIN_CHAPTERS,
            ((dur / 180.0).round() as usize).clamp(MIN_CHAPTERS + 1, 15)
        )
    } else {
        "and an empty chapters list".into()
    };
    let system = format!(
        "You write YouTube descriptions. From the timestamped transcript, \
         return a 2-3 sentence \"summary\" that makes someone want to watch \
         (no hashtags, no emojis) {want}. Write in the transcript's language. \
         Reply with JSON only: {{\"summary\": \"...\", \"chapters\": \
         [{{\"start_s\": 0, \"title\": \"...\"}}]}}"
    );
    let v = crate::openrouter::chat_json(cfg, &system, &blocks(words)).await?;
    let summary = v
        .get("summary")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    let chapters: Vec<Chapter> = v
        .get("chapters")
        .cloned()
        .and_then(|c| serde_json::from_value(c).ok())
        .unwrap_or_default();
    if summary.is_empty() {
        anyhow::bail!("no summary in the reply");
    }
    Ok(Outline {
        summary,
        chapters: tidy(chapters, dur),
        source: format!("llm:{}", cfg.model),
    })
}

/// Summary + chapters for a full render.
pub async fn outline(cfg: &crate::openrouter::Config, words: &[Word], dur: f64) -> Outline {
    if cfg.has_key() {
        match llm(cfg, words, dur).await {
            Ok(mut o) => {
                if o.chapters.is_empty() && dur >= MIN_VIDEO_S {
                    o.chapters = heuristic(words, dur).chapters;
                }
                return o;
            }
            Err(e) => tracing::warn!("chapters via OpenRouter failed ({e}): offline outline"),
        }
    }
    heuristic(words, dur)
}

/// The full render's upload kit: title, summary, chapters.
pub fn kit_text(title: &str, o: &Outline) -> String {
    let mut s = format!("{}\n\n{}\n", title.trim(), o.summary.trim());
    if !o.chapters.is_empty() {
        s += "\nChapters\n";
        s += &chapters_txt(&o.chapters);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(t: f64, title: &str) -> Chapter {
        Chapter {
            start_s: t,
            title: title.into(),
        }
    }

    #[test]
    fn stamps() {
        assert_eq!(stamp(0.0), "0:00");
        assert_eq!(stamp(75.9), "1:15");
        assert_eq!(stamp(3725.0), "1:02:05");
    }

    #[test]
    fn tidy_meets_youtube_rules() {
        let out = tidy(
            vec![
                ch(130.0, "Pricing."),
                ch(4.0, "Intro"),
                ch(60.0, "The setup"),
                ch(65.0, "Too close"),
                ch(295.0, "Too near the end"),
                ch(200.0, "  "),
            ],
            300.0,
        );
        assert_eq!(
            out,
            [
                ch(0.0, "Intro"),
                ch(60.0, "The setup"),
                ch(130.0, "Pricing")
            ]
        );
        // Fewer than three: no chapters at all.
        assert!(tidy(vec![ch(0.0, "A"), ch(50.0, "B")], 300.0).is_empty());
    }

    fn talk(dur: f64) -> Vec<Word> {
        // A word every 0.5 s, with long pauses at 100 s and 200 s.
        let mut ws = Vec::new();
        let mut t = 0.0;
        let mut i = 0;
        while t < dur {
            if (t - 100.0).abs() < 0.25 || (t - 200.0).abs() < 0.25 {
                t += 3.0;
            }
            ws.push(Word {
                w: if i % 12 == 11 {
                    "end.".into()
                } else {
                    format!("w{i}")
                },
                s: t,
                e: t + 0.4,
                conf: None,
            });
            t += 0.5;
            i += 1;
        }
        ws
    }

    #[test]
    fn heuristic_chapters_land_on_long_pauses() {
        let ws = talk(300.0);
        let o = heuristic(&ws, 306.0);
        let starts: Vec<f64> = o.chapters.iter().map(|c| c.start_s.round()).collect();
        assert_eq!(starts, [0.0, 103.0, 203.0]);
        assert!(o.chapters[0].title.starts_with("W0"));
        assert!(o.summary.ends_with("end."));
        let kit = kit_text("My video", &o);
        assert!(kit.starts_with("My video\n\n"));
        assert!(kit.contains("\nChapters\n0:00 "));
        // Short videos: summary only.
        assert!(heuristic(&talk(60.0), 60.0).chapters.is_empty());
    }

    #[test]
    fn blocks_are_timestamped() {
        let b = blocks(&talk(60.0));
        assert!(b.starts_with("[0:00] w0"));
        assert!(b.lines().count() >= 2);
    }
}
