//! Upload kit: per-clip copy-paste title, description and hashtags.
//!
//! Offline-first: prefers the picker's title/hashtags (LLM), otherwise
//! derives them from the clip's own words (first sentence -> title, top
//! frequent content words -> hashtags). Never fails a render — callers
//! log and continue on error.

use crate::validator::Clip;
use crate::whisper::Word;

const STOP: &[&str] = &[
    "the", "and", "that", "this", "with", "from", "have", "were", "they", "them", "then", "than",
    "what", "when", "where", "which", "while", "would", "could", "should", "your", "youre",
    "about", "into", "over", "after", "before", "because", "just", "like", "know", "think", "mean",
    "really", "very", "much", "more", "most", "some", "such", "only", "also", "even", "still",
    "back", "here", "there", "theyre", "were", "been", "being", "does", "doing", "dont", "cant",
    "wont", "isnt", "arent", "wasnt", "werent", "hasnt", "havent", "didnt", "doesnt",
];

fn clean(w: &str) -> String {
    w.trim()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

fn clip_words<'a>(clip: &Clip, words: &'a [Word]) -> Vec<&'a Word> {
    words
        .iter()
        .filter(|w| w.s >= clip.start_s - 0.05 && w.e <= clip.end_s + 0.35)
        .collect()
}

/// Title: picker title first, else the clip's first sentence (<=70 chars),
/// else the hook line.
pub fn title_for(clip: &Clip, words: &[Word]) -> String {
    if let Some(t) = &clip.title {
        let t = t
            .trim()
            .trim_matches(|c: char| c == '"' || c.is_whitespace());
        if !t.is_empty() {
            return t.chars().take(100).collect();
        }
    }
    let cw = clip_words(clip, words);
    let mut sent = String::new();
    for w in &cw {
        if !sent.is_empty() {
            sent.push(' ');
        }
        sent.push_str(&w.w);
        if w.w.ends_with(['.', '!', '?', '…']) {
            break;
        }
        if sent.len() >= 70 {
            break;
        }
    }
    if sent.is_empty() {
        sent = clip.hook_line.clone();
    }
    let sent = sent
        .trim()
        .trim_end_matches(['.', '!', '?', '…', ','])
        .trim()
        .to_string();
    // Word-boundary clamp to 70 chars.
    if sent.len() <= 70 {
        return sent;
    }
    match sent[..70].rfind(' ') {
        Some(i) => sent[..i].to_string(),
        None => sent[..70].to_string(),
    }
}

/// Openers that carry no meaning on a headline.
const FILLER_OPEN: &[&str] = &[
    "so",
    "and",
    "but",
    "like",
    "um",
    "uh",
    "uhm",
    "okay",
    "ok",
    "well",
    "yeah",
    "yes",
    "right",
    "now",
    "look",
    "listen",
    "honestly",
    "basically",
    "actually",
    "anyway",
    "oh",
    "hey",
];
/// Openers that point at something off-screen ("it", "that") — a headline
/// starting on one makes no sense out of context.
const DANGLING_OPEN: &[&str] = &[
    "it", "it's", "its", "that", "that's", "he", "she", "they", "him", "her", "them", "which",
    "because", "or", "then", "also", "too", "than",
];

/// Words a real question opens with.
const QUESTION_OPEN: &[&str] = &[
    "what",
    "why",
    "how",
    "who",
    "when",
    "where",
    "which",
    "is",
    "are",
    "was",
    "were",
    "do",
    "does",
    "did",
    "can",
    "could",
    "would",
    "should",
    "will",
    "have",
    "has",
    "am",
    "whats",
    "what's",
    "who's",
    "how's",
    "where's",
    "isn't",
    "aren't",
    "don't",
    "doesn't",
    "didn't",
    "can't",
    "won't",
    "wouldn't",
    "shouldn't",
];
/// Tails that make a "question" a tag on a statement ("…, right?").
const TAG_TAIL: &[&str] = &["right", "okay", "ok", "yeah", "huh", "no", "correct", "yes"];

/// A sentence as a standalone headline: `(score, text)`, or `None` when
/// it doesn't read well on its own. `t` = when it starts (s into the clip).
fn headline_candidate(sentence: &str, t: f64) -> Option<(f64, String)> {
    let mut ws: Vec<&str> = sentence.split_whitespace().collect();
    if !ws.last()?.ends_with(['.', '!', '?']) {
        return None;
    }
    while let Some(w) = ws.first() {
        if FILLER_OPEN.contains(&clean(w).as_str()) || w.ends_with(',') && ws.len() > 3 {
            ws.remove(0);
        } else {
            break;
        }
    }
    let text = ws.join(" ");
    if ws.len() < 4 || ws.len() > 9 || text.chars().count() > HEADLINE_CHARS {
        return None;
    }
    let lower: Vec<String> = ws.iter().map(|w| clean(w)).collect();
    if DANGLING_OPEN.contains(&lower[0].as_str())
        || lower
            .iter()
            .any(|w| matches!(w.as_str(), "um" | "uh" | "uhm" | "mm" | "hmm"))
        || ws
            .iter()
            .any(|w| w.contains("--") || w.contains('\u{2014}'))
    {
        return None;
    }
    let question = text.ends_with('?');
    if question
        && (!QUESTION_OPEN.contains(&lower[0].as_str())
            || TAG_TAIL.contains(&lower[lower.len() - 1].as_str()))
    {
        return None;
    }
    let keys = (1..ws.len())
        .filter(|&i| crate::captions::ass::is_keyword(ws[i], false))
        .count();
    let score = if question { 2.0 } else { 0.0 }
        + if text.ends_with('!') { 1.0 } else { 0.0 }
        + keys.min(2) as f64
        - 0.2 * (ws.len() as f64 - 6.0).abs()
        - 0.04 * t;
    Some((score, text))
}

/// Longest offline headline (chars): it's shown whole, never cut.
const HEADLINE_CHARS: usize = 48;

/// Offline on-screen headline: the clip's hook line when it stands on its
/// own (it's the opening sentence, straight from the transcript), else the
/// best short, complete sentence in the clip's first seconds — 4 to 9
/// words once filler openers are dropped, no context-dependent opener
/// ("it", "that"), real questions only (no "…, right?"). `None` when
/// nothing reads well on its own: no headline beats a nonsensical one.
pub fn headline_for(hook: &str, words: &[Word]) -> Option<String> {
    let pick = headline_candidate(hook, 0.0).or_else(|| {
        let mut best: Option<(f64, String)> = None;
        let mut sent: Vec<&Word> = Vec::new();
        for w in words {
            sent.push(w);
            if w.w.ends_with(['.', '!', '?', '\u{2026}']) {
                if sent[0].s <= 25.0 {
                    let text = sent
                        .iter()
                        .map(|w| w.w.trim())
                        .collect::<Vec<_>>()
                        .join(" ");
                    if let Some(c) = headline_candidate(&text, sent[0].s) {
                        if best.as_ref().is_none_or(|(b, _)| c.0 > *b) {
                            best = Some(c);
                        }
                    }
                }
                sent.clear();
            }
        }
        best
    });
    pick.map(|(_, t)| crate::captions::ass::headline_text(&t, HEADLINE_CHARS))
        .filter(|t| !t.is_empty())
}

/// Hashtags: picker tags first, topped up with the clip's most frequent
/// content words (len>=5, not stopwords), max 8, lowercased.
pub fn hashtags_for(clip: &Clip, words: &[Word]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for h in &clip.hashtags {
        let h = h.trim().trim_start_matches('#').to_lowercase();
        if h.len() > 1
            && h.chars().all(|c| c.is_alphanumeric() || c == '_')
            && !out.contains(&format!("#{h}"))
        {
            out.push(format!("#{h}"));
        }
    }
    let mut freq: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for w in clip_words(clip, words) {
        let c = clean(&w.w);
        if c.len() >= 5 && !STOP.contains(&c.as_str()) {
            *freq.entry(c).or_insert(0) += 1;
        }
    }
    let mut top: Vec<(String, usize)> = freq.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (w, n) in top {
        if out.len() >= 8 {
            break;
        }
        if n >= 2 && !out.contains(&format!("#{w}")) {
            out.push(format!("#{w}"));
        }
    }
    out
}

/// Copy-paste body: title, description (hook + why), hashtags.
pub fn body_for(clip: &Clip, words: &[Word]) -> String {
    let title = title_for(clip, words);
    let mut desc = clip.hook_line.trim().to_string();
    if !clip.why_it_works.trim().is_empty() {
        if !desc.is_empty() {
            desc.push(' ');
        }
        desc.push_str(clip.why_it_works.trim());
    }
    let tags = hashtags_for(clip, words).join(" ");
    if tags.is_empty() {
        format!("{title}\n\n{desc}\n")
    } else {
        format!("{title}\n\n{desc}\n\n{tags}\n")
    }
}

pub fn write(path: &std::path::Path, clip: &Clip, words: &[Word]) -> std::io::Result<()> {
    std::fs::write(path, body_for(clip, words))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn clip() -> Clip {
        // 40 words = 20s of talk.
        Clip {
            rank: 1,
            start_s: 0.0,
            end_s: 19.0,
            hook_line: "hook".into(),
            why_it_works: String::new(),
            score_total: 70.0,
            scores: None,
            title: None,
            hashtags: vec![],
            caption_style: "karaoke".into(),
            source: "t".into(),
        }
    }

    #[test]
    fn title_prefers_first_sentence() {
        let words = tw("Why is this airplane so fast today. It keeps flying higher now.");
        let t = title_for(&clip(), &words);
        assert_eq!(t, "Why is this airplane so fast today");
    }

    #[test]
    fn headline_is_a_short_complete_sentence_or_nothing() {
        // Skips the rambling opener and the context-dependent "It's…",
        // strips the filler opener, prefers the question.
        let words = tw(
            "Do we get to take all this home with him and during longer shoots we just keep going. \
             It's crazy. So, why do most creators quit in year one? Nobody talks about that.",
        );
        assert_eq!(
            headline_for("", &words).as_deref(),
            Some("Why do most creators quit in year one?")
        );
        // Nothing stands on its own: no headline.
        let words = tw("and then he said that it was um fine and we kept going with it");
        assert_eq!(headline_for("and then he said", &words), None);
        // A hook line that stands on its own wins; tag questions and
        // questions that aren't questions never make it.
        assert_eq!(
            headline_for("Who here's not using more than a $20 version?", &words).as_deref(),
            Some("Who here's not using more than a $20 version?")
        );
        assert_eq!(headline_for("Raise your hand, right?", &[]), None);
        // Too short to say anything (a transcription gap, usually).
        assert_eq!(headline_for("Who here before?", &[]), None);
        assert_eq!(
            headline_for("Here's not using more than $20 version?", &[]),
            None
        );
        assert_eq!(
            headline_for("You do not have-- OK, who here is using more", &[]),
            None
        );
    }

    #[test]
    fn hashtags_extract_frequent_content_words() {
        let words = tw("airplane airplane airplane the and jet jet runway");
        let tags = hashtags_for(&clip(), &words);
        assert!(tags.contains(&"#airplane".to_string()), "got {tags:?}");
        assert!(
            !tags.iter().any(|t| t == "#the" || t == "#and"),
            "stopwords out: {tags:?}"
        );
    }

    #[test]
    fn picker_title_and_tags_win() {
        let mut c = clip();
        c.title = Some("  My Title  ".into());
        c.hashtags = vec!["JetLife".into()];
        let words = tw("airplane airplane jet");
        assert_eq!(title_for(&c, &words), "My Title");
        assert!(hashtags_for(&c, &words).contains(&"#jetlife".to_string()));
    }
}
