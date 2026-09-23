//! LLM prompt builders (port of ClipPrompt.php).

use crate::whisper::{Segment, Word};

pub fn stamp(s: f64) -> String {
    let s = s.max(0.0);
    format!("{:02}:{:02}", (s / 60.0) as u64, (s % 60.0) as u64)
}

pub fn system(count: usize, min_s: u64, max_s: u64) -> String {
    let target = if min_s == max_s {
        format!("each clip exactly {min_s}s long,")
    } else {
        format!("each clip {min_s}-{max_s}s long,")
    };
    format!(
        "You are a short-form video editor for TikTok/Reels/Shorts. \
         Pick the {count} most viral-worthy moments. Rules: {target} \
         hook in the first 2s, self-contained payoff, no mid-sentence cuts. \
         Use the submit_clips tool to return your results."
    )
}

pub fn user(words: &[Word], segments: &[Segment], duration_s: f64, max_chars: usize) -> String {
    let mut body = if !segments.is_empty() {
        let mut out = String::new();
        for seg in segments {
            let line = format!("[{}] {}\n", stamp(seg.s), seg.text.trim());
            if out.len() + line.len() > max_chars {
                out.push_str("[...truncated...]");
                break;
            }
            out.push_str(&line);
        }
        out
    } else {
        // Fallback: 10s word buckets.
        let mut out = String::new();
        let mut bucket = 0u64;
        let mut cur = String::new();
        for w in words {
            let b = (w.s / 10.0) as u64;
            if b != bucket && !cur.is_empty() {
                let line = format!("[{}] {}\n", stamp(bucket as f64 * 10.0), cur.trim());
                if out.len() + line.len() > max_chars {
                    out.push_str("[...truncated...]");
                    break;
                }
                out.push_str(&line);
                cur.clear();
                bucket = b;
            }
            cur.push_str(&w.w);
            cur.push(' ');
        }
        if !cur.is_empty() && out.len() < max_chars {
            out.push_str(&format!(
                "[{}] {}\n",
                stamp(bucket as f64 * 10.0),
                cur.trim()
            ));
        }
        out
    };
    if body.len() > max_chars {
        body.truncate(max_chars);
        body.push_str("[...truncated...]");
    }
    format!("Video duration: {:.0}s\n{body}", duration_s)
}
