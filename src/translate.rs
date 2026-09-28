//! `--subs-lang`: captions in another language. The transcript is cut
//! into subtitle lines (pauses, sentence ends), the lines are translated
//! in batches through OpenRouter, and each translated line's words are
//! spread over the time its original words were spoken (by length), so
//! karaoke highlighting, cuts and punch-ins keep working on the same clock.
//! Picking and cutting always use the original words.

use std::hash::{Hash, Hasher};
use std::path::Path;

use crate::whisper::{Transcription, Word};

/// Lines per translation request.
const BATCH: usize = 40;

/// English name for a language code (for the prompt); unknown codes pass
/// through as-is.
pub fn lang_name(code: &str) -> &str {
    match code {
        "en" => "English",
        "sq" => "Albanian",
        "de" => "German",
        "fr" => "French",
        "es" => "Spanish",
        "it" => "Italian",
        "tr" => "Turkish",
        "pt" => "Portuguese",
        "nl" => "Dutch",
        "pl" => "Polish",
        "ro" => "Romanian",
        "el" => "Greek",
        "sr" => "Serbian",
        "hr" => "Croatian",
        "bs" => "Bosnian",
        "mk" => "Macedonian",
        "sv" => "Swedish",
        "ru" => "Russian",
        "uk" => "Ukrainian",
        "ar" => "Arabic",
        "hi" => "Hindi",
        "ja" => "Japanese",
        "ko" => "Korean",
        "zh" => "Chinese (Simplified)",
        other => other,
    }
}

/// Subtitle lines as word index ranges `[a, b)`: a new line after a pause
/// over 0.6 s, after a sentence end (once the line has 4+ words), or at 16
/// words.
pub fn lines(words: &[Word]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut a = 0;
    for i in 1..=words.len() {
        let n = i - a;
        let brk = i == words.len() || {
            let (prev, next) = (&words[i - 1], &words[i]);
            next.s - prev.e > 0.6 || n >= 16 || (n >= 4 && prev.w.ends_with(['.', '?', '!']))
        };
        if brk && n > 0 {
            out.push((a, i));
            a = i;
        }
    }
    out
}

/// Text of one line.
pub fn line_text(words: &[Word], (a, b): (usize, usize)) -> String {
    words[a..b]
        .iter()
        .map(|w| w.w.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Lay `text`'s words over the time the line `[a, b)` was spoken, each
/// word's share by its length.
fn respread(words: &[Word], (a, b): (usize, usize), text: &str) -> Vec<Word> {
    let toks: Vec<&str> = text.split_whitespace().collect();
    if toks.is_empty() || a >= b {
        return vec![];
    }
    let (s0, e1) = (words[a].s, words[b - 1].e.max(words[a].s + 0.05));
    let weight = |t: &str| t.chars().count().max(1) as f64 + 1.0;
    let total: f64 = toks.iter().map(|t| weight(t)).sum();
    let span = e1 - s0;
    let mut cum = 0.0;
    toks.iter()
        .map(|t| {
            let s = s0 + span * cum / total;
            cum += weight(t);
            // Leave the inter-word gap (the "+1") silent.
            let e = s0 + span * (cum - 1.0) / total;
            Word {
                w: t.to_string(),
                s: (s * 1000.0).round() / 1000.0,
                e: (e.max(s + 0.01) * 1000.0).round() / 1000.0,
                conf: None,
            }
        })
        .collect()
}

/// Rebuild the word list from translated lines (a missing line keeps its
/// original words).
fn assemble(words: &[Word], ls: &[(usize, usize)], texts: &[Option<String>]) -> Vec<Word> {
    let mut out = Vec::with_capacity(words.len());
    for (i, &l) in ls.iter().enumerate() {
        match texts.get(i).and_then(|t| t.as_deref()) {
            Some(t) if !t.trim().is_empty() => out.extend(respread(words, l, t)),
            _ => out.extend_from_slice(&words[l.0..l.1]),
        }
    }
    out
}

fn system_prompt(target: &str) -> String {
    format!(
        "You translate video subtitles into {}. Keep the meaning, tone and \
         slang; keep each line about as short as the original so it can be \
         read on screen. No notes, no quotes around lines. Reply with JSON \
         only: {{\"lines\": [\"...\", ...]}} with exactly one translated \
         string per input line, in the same order.",
        lang_name(target)
    )
}

/// Translate one batch; `None` when the model's line count never matched.
async fn batch(
    cfg: &crate::openrouter::Config,
    target: &str,
    src: &[String],
) -> anyhow::Result<Option<Vec<String>>> {
    let user = serde_json::json!({ "lines": src }).to_string();
    for attempt in 1..=2 {
        let v = crate::openrouter::chat_json(cfg, &system_prompt(target), &user).await?;
        let got: Vec<String> = v
            .get("lines")
            .and_then(|l| l.as_array())
            .map(|a| {
                a.iter()
                    .map(|x| x.as_str().unwrap_or_default().trim().to_string())
                    .collect()
            })
            .unwrap_or_default();
        if got.len() == src.len() {
            return Ok(Some(got));
        }
        tracing::warn!(
            "translation batch came back with {} of {} lines (attempt {attempt}/2)",
            got.len(),
            src.len()
        );
    }
    Ok(None)
}

/// Translate every word of `words` into `target`.
pub async fn translate(
    cfg: &crate::openrouter::Config,
    words: &[Word],
    target: &str,
    cancel: &crate::progress::CancelFlag,
) -> anyhow::Result<Vec<Word>> {
    let ls = lines(words);
    let mut texts: Vec<Option<String>> = Vec::with_capacity(ls.len());
    let mut kept = 0;
    for chunk in ls.chunks(BATCH) {
        cancel.check()?;
        let src: Vec<String> = chunk.iter().map(|&l| line_text(words, l)).collect();
        match batch(cfg, target, &src).await? {
            Some(t) => texts.extend(t.into_iter().map(Some)),
            None => {
                kept += chunk.len();
                texts.extend(std::iter::repeat_n(None, chunk.len()));
            }
        }
    }
    if kept > 0 {
        tracing::warn!("{kept} subtitle line(s) kept in the original language");
    }
    Ok(assemble(words, &ls, &texts))
}

/// Fingerprint of the words a translation was made from.
fn fingerprint(words: &[Word]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for w in words {
        w.w.hash(&mut h);
        ((w.s * 1000.0).round() as i64).hash(&mut h);
    }
    h.finish()
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Cached {
    src: u64,
    words: Vec<Word>,
}

fn cache_path(out: &Path, target: &str) -> std::path::PathBuf {
    out.join(format!("transcript.{target}.json"))
}

/// Normalize a `--subs-lang` value (`DE` -> `de`); `None` for "off".
pub fn target_code(v: Option<&str>) -> Option<String> {
    let v = v?.trim().to_lowercase();
    (!v.is_empty() && v != "off" && v != "none" && v != "original").then_some(v)
}

/// Caption words in `target`, or `None` to caption in the original
/// language (same language, no key, or the translation failed — a failed
/// translation never fails the job). Cached per language in `out`.
pub async fn for_captions(
    cfg: &crate::openrouter::Config,
    tr: &Transcription,
    target: &str,
    out: &Path,
    cancel: &crate::progress::CancelFlag,
) -> Option<Vec<Word>> {
    if target == tr.language {
        tracing::info!("subtitles already in {}: no translation", lang_name(target));
        return None;
    }
    let fp = fingerprint(&tr.words);
    let path = cache_path(out, target);
    if let Some(c) = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Cached>(&b).ok())
        .filter(|c| c.src == fp && !c.words.is_empty())
    {
        tracing::info!("subtitles in {} (cached)", lang_name(target));
        return Some(c.words);
    }
    if !cfg.has_key() {
        tracing::warn!(
            "subtitles in {} need an OpenRouter key: keeping the original language",
            lang_name(target)
        );
        return None;
    }
    let t = std::time::Instant::now();
    match translate(cfg, &tr.words, target, cancel).await {
        Ok(words) => {
            tracing::info!(
                "translated subtitles to {} in {:.1}s",
                lang_name(target),
                t.elapsed().as_secs_f64()
            );
            let c = Cached { src: fp, words };
            if let Ok(b) = serde_json::to_vec(&c) {
                let _ = std::fs::write(&path, b);
            }
            let _ = std::fs::write(
                out.join(format!("transcript.{target}.srt")),
                crate::captions::srt::from_words(&c.words),
            );
            Some(c.words)
        }
        Err(e) => {
            tracing::warn!("subtitle translation failed ({e}): keeping the original language");
            None
        }
    }
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

    fn talk() -> Vec<Word> {
        vec![
            w("So", 0.0, 0.2),
            w("the", 0.3, 0.4),
            w("secret", 0.5, 0.9),
            w("is", 1.0, 1.1),
            w("compounding.", 1.2, 1.9),
            // Long pause: new line.
            w("Really", 3.0, 3.4),
            w("slowly.", 3.5, 4.0),
        ]
    }

    #[test]
    fn lines_break_on_sentences_and_pauses() {
        assert_eq!(lines(&talk()), [(0, 5), (5, 7)]);
        assert_eq!(line_text(&talk(), (5, 7)), "Really slowly.");
        let many: Vec<Word> = (0..40)
            .map(|i| w("word", i as f64 * 0.3, i as f64 * 0.3 + 0.2))
            .collect();
        let ls = lines(&many);
        assert_eq!(ls.len(), 3);
        assert!(ls.iter().all(|(a, b)| b - a <= 16));
        assert!(lines(&[]).is_empty());
    }

    #[test]
    fn translated_words_fill_the_original_time() {
        let ws = talk();
        let out = respread(&ws, (0, 5), "Das Geheimnis ist Zinseszins.");
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].w, "Das");
        assert!((out[0].s - 0.0).abs() < 1e-9);
        assert!(out.last().unwrap().e <= 1.9 + 1e-9);
        // Ordered, non-overlapping, longer words get more time.
        for p in out.windows(2) {
            assert!(p[0].e <= p[1].s + 1e-9);
        }
        assert!(out[3].e - out[3].s > out[0].e - out[0].s);
    }

    #[test]
    fn missing_lines_keep_the_original() {
        let ws = talk();
        let ls = lines(&ws);
        let out = assemble(&ws, &ls, &[None, Some("Wirklich langsam.".into())]);
        assert_eq!(out[0].w, "So");
        assert_eq!(out.len(), 5 + 2);
        assert_eq!(out[5].w, "Wirklich");
        assert!(out[5].s >= 3.0 - 1e-9);
    }

    #[test]
    fn subs_lang_off_values() {
        assert_eq!(target_code(Some(" DE ")).as_deref(), Some("de"));
        assert!(target_code(Some("off")).is_none());
        assert!(target_code(Some("")).is_none());
        assert!(target_code(None).is_none());
        assert_eq!(lang_name("sq"), "Albanian");
        assert_eq!(lang_name("xx"), "xx");
    }
}
