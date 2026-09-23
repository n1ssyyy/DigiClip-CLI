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
