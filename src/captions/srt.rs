//! Words -> SRT subtitles (port of SrtBuilder.php).

use crate::whisper::Word;

fn stamp(s: f64) -> String {
    let s = s.max(0.0);
    let ms = (s * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        (ms % 3_600_000) / 60_000,
        (ms % 60_000) / 1000,
        ms % 1000
    )
}

/// Group like AssBuilder but looser (8 words / 42 chars / 0.8s / 5s),
/// splitting >42ch cues into two lines.
pub fn from_words(words: &[Word]) -> String {
    let mut lines: Vec<Vec<Word>> = Vec::new();
    let mut cur: Vec<Word> = Vec::new();
    for w in words {
        let flush = if cur.is_empty() {
            false
        } else {
            let last = cur.last().unwrap();
            let len: usize = cur.iter().map(|x| x.w.len() + 1).sum::<usize>() + w.w.len();
            cur.len() >= 8 || len > 42 || (w.s - last.e) > 0.8 || (w.e - cur[0].s) > 5.0
        };
        if flush {
            lines.push(std::mem::take(&mut cur));
        }
        cur.push(w.clone());
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        let text = line
            .iter()
            .map(|w| w.w.clone())
            .collect::<Vec<_>>()
            .join(" ");
        let body = if text.len() > 42 {
            // Split near the middle on a space.
            let mid = text.len() / 2;
            let mut at = text.len();
            for (j, _) in text.match_indices(' ') {
                if (j as i64 - mid as i64).abs() < (at as i64 - mid as i64).abs() {
                    at = j;
                }
            }
            if at < text.len() {
                format!("{}\n{}", text[..at].trim(), text[at..].trim())
            } else {
                text
            }
        } else {
            text
        };
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            stamp(line[0].s),
            stamp(line[line.len() - 1].e),
            body
        ));
    }
    out
}
