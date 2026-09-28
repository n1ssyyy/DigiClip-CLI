//! Clip judging with a System One model: each candidate window gets a
//! handful of typed questions (hook, stands alone, finished thought,
//! payoff, shareable, and optionally focus and caption look). The answers
//! become the virality scorecard (0-100 per dimension) and the "why this
//! clip" line, so ranking works the same with or without an LLM.

use super::{Answer, Decider, Question};
use crate::openrouter::{RawClip, Scores};
use crate::whisper::Word;

/// Caption looks, described by the content each suits.
pub const LOOKS: &[(&str, &str)] = &[
    ("tiktok", "casual everyday talk, vlogs, chatting"),
    ("karaoke", "storytelling people follow word by word"),
    ("hormozi", "business, money, motivation, bold claims"),
    ("minimal", "calm, serious, educational or emotional"),
    ("beast", "hype, challenges, big reactions, high energy"),
    ("neon", "gaming, tech, music, nightlife"),
    ("highlight", "lists, tips and key takeaways"),
    ("ghost", "quiet, reflective, personal moments"),
];

const HOOK: [&str; 4] = [
    "no hook: slow, vague or confusing start",
    "weak hook: gets going but nothing grabs",
    "decent hook: a clear reason to keep watching",
    "strong hook: a bold claim, surprise, question or tension right away",
];

const VALUE: [&str; 4] = [
    "no payoff: filler or small talk",
    "small payoff: a minor point",
    "solid payoff: a useful idea, a laugh or a feeling",
    "big payoff: a memorable insight, story climax or punchline",
];

/// The question set for one candidate.
pub fn questions(focus: Option<&str>, pick_look: bool) -> Vec<(&'static str, Question)> {
    let mut q = vec![
        (
            "hook",
            Question::score(
                "How strongly does the opening of this short video clip grab a scrolling viewer?",
                &HOOK,
            ),
        ),
        (
            "standalone",
            Question::noul("The clip makes sense on its own, without the rest of the video."),
        ),
        (
            "complete",
            Question::noul("The clip ends on a finished thought, not in the middle of a point."),
        ),
        (
            "value",
            Question::score("How much does this clip deliver by the end?", &VALUE),
        ),
        (
            "share",
            Question::noul("Viewers would likely share, save or comment on this clip."),
        ),
    ];
    if let Some(f) = focus.filter(|f| !f.trim().is_empty()) {
        q.push((
            "focus",
            Question::Noul {
                instructions: format!("This clip is about {}.", f.trim()),
            },
        ));
    }
    if pick_look {
        q.push((
            "look",
            Question::choice("Which caption look suits this clip's content?", LOOKS),
        ));
    }
    q
}

/// The text a candidate is judged on: its length and words (the opening
/// questions read the start; keeping it short keeps Laya fast on CPU).
pub fn state_for(words: &[Word], s: f64, e: f64) -> String {
    let inside: Vec<&str> = words
        .iter()
        .filter(|w| w.s >= s - 0.05 && w.e <= e + 0.05)
        .map(|w| w.w.trim())
        .filter(|w| !w.is_empty())
        .collect();
    format!(
        "Short video clip, {:.0} seconds. Transcript: {}",
        (e - s).max(0.0),
        inside.join(" ")
    )
}

/// One candidate's answers, as probabilities (`hook`/`value` scaled 0-1).
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub hook: f64,
    pub standalone: f64,
    pub complete: f64,
    pub value: f64,
    pub share: f64,
    pub focus: Option<f64>,
    pub look: Option<String>,
}

fn verdict(names: &[(&'static str, Question)], answers: &[Answer]) -> Verdict {
    let get = |n: &str| {
        names
            .iter()
            .position(|(k, _)| *k == n)
            .and_then(|i| answers.get(i))
    };
    let level = |n: &str, levels: f64| {
        get(n)
            .and_then(Answer::score)
            .map_or(0.5, |s| (s / levels).clamp(0.0, 1.0))
    };
    let p = |n: &str| get(n).and_then(Answer::noul).unwrap_or(0.5);
    Verdict {
        hook: level("hook", (HOOK.len() - 1) as f64),
        standalone: p("standalone"),
        complete: p("complete"),
        value: level("value", (VALUE.len() - 1) as f64),
        share: p("share"),
        focus: get("focus").and_then(Answer::noul),
        look: get("look").and_then(Answer::choice).map(str::to_string),
    }
}

fn pct(x: f64) -> i64 {
    (x * 100.0).round().clamp(1.0, 100.0) as i64
}

/// The scorecard on the 0-100 scale.
pub fn scores(v: &Verdict) -> Scores {
    Scores {
        hook: pct(v.hook),
        retention: pct(0.5 * v.standalone + 0.5 * v.complete),
        value: pct(v.value),
        share: pct(v.share),
    }
}

/// "Why this clip", from the strongest answers.
pub fn why(v: &Verdict, by: &str) -> String {
    let mut good: Vec<(f64, String)> = vec![];
    let mut weak: Vec<&str> = vec![];
    let mut add = |x: f64, yes: &str, no: &'static str| {
        if x >= 0.6 {
            good.push((x, format!("{yes} ({:.0}%)", x * 100.0)));
        } else if x < 0.35 {
            weak.push(no);
        }
    };
    add(v.hook, "strong opening hook", "slow opening");
    add(v.value, "clear payoff", "light payoff");
    add(v.standalone, "stands on its own", "needs context");
    add(v.complete, "ends on a finished thought", "ends mid-point");
    add(v.share, "likely to be shared", "low share pull");
    good.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut s = if good.is_empty() {
        "No standout strengths".to_string()
    } else {
        let parts: Vec<String> = good.into_iter().take(3).map(|g| g.1).collect();
        let mut t = parts.join(", ");
        if let Some(c) = t.get(..1) {
            t = c.to_uppercase() + &t[1..];
        }
        t
    };
    if !weak.is_empty() {
        s += &format!("; watch for: {}", weak.join(", "));
    }
    format!("{s}. Judged by {by}.")
}

/// Heuristic "why" placeholders the verdict may replace.
fn is_placeholder(why: Option<&str>) -> bool {
    why.is_none_or(|w| w.trim().is_empty() || w.starts_with("Offline heuristic"))
}

/// Fold a verdict into a candidate. LLM picks keep their own "why" and
/// look, and their 1-10 scorecard is averaged with the verdict's.
pub fn apply(c: &mut RawClip, v: &Verdict, by: &str, from_llm: bool) {
    let mut s = scores(v);
    if let (true, Some(o)) = (from_llm, c.scores.as_ref()) {
        let blend = |llm: i64, sys: i64| {
            let llm = if llm <= 10 { llm * 10 } else { llm };
            ((llm + sys) as f64 / 2.0).round() as i64
        };
        s = Scores {
            hook: blend(o.hook, s.hook),
            retention: blend(o.retention, s.retention),
            value: blend(o.value, s.value),
            share: blend(o.share, s.share),
        };
    }
    c.scores = Some(s);
    if !from_llm || is_placeholder(c.why_it_works.as_deref()) {
        c.why_it_works = Some(why(v, by));
    }
    if !from_llm {
        if let Some(l) = &v.look {
            c.caption_style = Some(l.clone());
        }
    }
}

/// What judging did.
#[derive(Debug, Default)]
pub struct Report {
    pub judged: usize,
    /// Candidates on the focus topic, when a focus was judged.
    pub on_topic: Option<usize>,
}

/// Most candidates judged per run (the rest keep their picker's scores).
pub const MAX: usize = 24;

/// Judge `raw` in place. Focus hits (the model's yes, or a keyword
/// mention) get the same rank bump as [`crate::scorer::apply_focus`].
/// Failures leave a candidate as its picker scored it; an auth error
/// stops judging.
pub async fn judge(
    dec: &Decider,
    raw: &mut [RawClip],
    words: &[Word],
    focus: Option<&str>,
    from_llm: bool,
    pick_look: bool,
    cancel: &crate::progress::CancelFlag,
) -> Report {
    let focus = focus.filter(|f| !f.trim().is_empty());
    let terms = focus.map(crate::scorer::focus_terms).unwrap_or_default();
    let qs = questions(focus, pick_look && !from_llm);
    let by = dec.label();
    let mut rep = Report {
        on_topic: focus.map(|_| 0),
        ..Default::default()
    };
    let t0 = std::time::Instant::now();
    for c in raw.iter_mut().take(MAX) {
        if cancel.is_cancelled() {
            break;
        }
        let state = state_for(words, c.start_s, c.end_s);
        let answers = match dec.ask(&state, &qs).await {
            Ok(a) => a,
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!(
                    "{by}: judging {:.0}s-{:.0}s failed: {msg}",
                    c.start_s,
                    c.end_s
                );
                if msg.contains("refused the key") {
                    break;
                }
                continue;
            }
        };
        let v = verdict(&qs, &answers);
        apply(c, &v, &by, from_llm);
        rep.judged += 1;
        if focus.is_some() {
            let text = state.split_once("Transcript: ").map_or("", |t| t.1);
            if v.focus.unwrap_or(0.0) >= 0.5 || crate::scorer::mentions_focus(text, &terms) {
                *rep.on_topic.get_or_insert(0) += 1;
                if let Some(s) = c.scores.as_mut() {
                    for x in [&mut s.hook, &mut s.retention, &mut s.value, &mut s.share] {
                        *x = (*x + 15).min(100);
                    }
                }
            }
        }
    }
    tracing::info!(
        "{by}: judged {}/{} candidates in {:.1}s",
        rep.judged,
        raw.len(),
        t0.elapsed().as_secs_f64()
    );
    rep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(t: &str, s: f64) -> Word {
        Word {
            w: t.into(),
            s,
            e: s + 0.4,
            conf: None,
        }
    }

    fn v() -> Verdict {
        Verdict {
            hook: 0.9,
            standalone: 0.8,
            complete: 0.2,
            value: 0.7,
            share: 0.5,
            focus: None,
            look: Some("neon".into()),
        }
    }

    /// Same request as the reference TS port (receptron/laya), same numbers:
    /// `cargo test --release --lib laya_clip_parity -- --ignored`.
    #[test]
    #[ignore]
    fn laya_clip_parity() {
        let l = super::super::laya::Laya::load().unwrap();
        let state = "Short video clip, 31 seconds.
Opening: Most founders price their product way too low, and here is why that kills you.
Full transcript: Most founders price their product way too low, and here is why that kills you. When you charge more, you can afford to actually serve customers well. We doubled our price and churn went down, not up.";
        let qs = questions(Some("pricing"), true);
        let a = l.ask(state, &qs).unwrap();
        eprintln!("{a:?}");
        let close = |x: f64, y: f64| (x - y).abs() < 2e-3;
        assert!(close(a[0].score().unwrap(), 2.2042));
        assert!(close(a[1].noul().unwrap(), 0.8185));
        assert!(close(a[2].noul().unwrap(), 0.663));
        assert!(close(a[3].score().unwrap(), 1.6375));
        assert!(close(a[4].noul().unwrap(), 0.2744));
        assert!(close(a[5].noul().unwrap(), 0.829));
        assert_eq!(a[6].choice(), Some("minimal"));
    }

    #[test]
    fn state_holds_opening_and_words_in_range() {
        let words = vec![
            w("before", 0.0),
            w("Did", 1.0),
            w("you", 1.5),
            w("know", 2.0),
            w("after", 9.0),
        ];
        let s = state_for(&words, 1.0, 3.0);
        assert_eq!(s, "Short video clip, 2 seconds. Transcript: Did you know");
        assert!(!s.contains("before") && !s.contains("after"));
    }

    #[test]
    fn verdict_maps_answers_by_name() {
        let qs = questions(Some("pricing"), true);
        let names: Vec<&str> = qs.iter().map(|q| q.0).collect();
        assert_eq!(
            names,
            [
                "hook",
                "standalone",
                "complete",
                "value",
                "share",
                "focus",
                "look"
            ]
        );
        let a = vec![
            Answer::Score {
                score: 3.0,
                confidence: 1.0,
            },
            Answer::Noul { p: 0.9 },
            Answer::Noul { p: 0.8 },
            Answer::Score {
                score: 1.5,
                confidence: 0.5,
            },
            Answer::Noul { p: 0.4 },
            Answer::Noul { p: 0.7 },
            Answer::Choice {
                choice: "hormozi".into(),
                confidence: 0.3,
            },
        ];
        let v = verdict(&qs, &a);
        assert_eq!(v.hook, 1.0);
        assert_eq!(v.value, 0.5);
        assert_eq!(v.focus, Some(0.7));
        assert_eq!(v.look.as_deref(), Some("hormozi"));
        let s = scores(&v);
        assert_eq!((s.hook, s.retention, s.value, s.share), (100, 85, 50, 40));
    }

    #[test]
    fn why_names_strengths_and_weak_spots() {
        let y = why(&v(), "laya");
        assert!(y.starts_with("Strong opening hook (90%)"), "{y}");
        assert!(y.contains("stands on its own (80%)"));
        assert!(y.contains("watch for: ends mid-point"));
        assert!(y.ends_with("Judged by laya."));
    }

    #[test]
    fn heuristic_picks_take_the_verdict_llm_picks_blend() {
        let mut h = RawClip {
            start_s: 0.0,
            end_s: 30.0,
            hook_line: None,
            why_it_works: Some("Offline heuristic pick (no LLM key set).".into()),
            scores: Some(Scores {
                hook: 70,
                retention: 65,
                value: 62,
                share: 60,
            }),
            title: None,
            hashtags: None,
            caption_style: Some("karaoke".into()),
        };
        apply(&mut h, &v(), "laya", false);
        assert_eq!(h.scores.as_ref().unwrap().hook, 90);
        assert_eq!(h.caption_style.as_deref(), Some("neon"));
        assert!(h.why_it_works.as_ref().unwrap().contains("Judged by laya"));

        let mut l = RawClip {
            why_it_works: Some("Sharp contrarian take.".into()),
            scores: Some(Scores {
                hook: 6,
                retention: 8,
                value: 7,
                share: 5,
            }),
            caption_style: Some("hormozi".into()),
            ..h.clone()
        };
        apply(&mut l, &v(), "jev:jev-latest", true);
        let s = l.scores.unwrap();
        assert_eq!(s.hook, 75); // (60 + 90) / 2
        assert_eq!(l.why_it_works.as_deref(), Some("Sharp contrarian take."));
        assert_eq!(l.caption_style.as_deref(), Some("hormozi"));
    }
}
