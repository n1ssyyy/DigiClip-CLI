//! Transcript cache: a rerun on the same source (a retry, a clip edit, a
//! clip made from the transcript, a CLI rerun into the same out dir)
//! reuses `audio.wav` + `transcript.json` instead of extracting and
//! transcribing again. Keyed on the source's size + mtime and on the model
//! and language, so any of those changing transcribes fresh.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::whisper::Transcription;

pub const KEY_FILE: &str = "transcript.key";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceKey {
    pub size: u64,
    pub mtime_ms: u64,
    pub model: String,
    pub lang: String,
}

/// Fingerprint of `input` for this model + language (None when the file
/// can't be stat'ed; then nothing is cached).
pub fn source_key(input: &Path, model: &str, lang: &str) -> Option<SourceKey> {
    let m = std::fs::metadata(input).ok()?;
    let mtime_ms = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    Some(SourceKey {
        size: m.len(),
        mtime_ms,
        model: model.to_string(),
        lang: lang.to_string(),
    })
}

/// The cached transcript when `out` holds one made from exactly this key
/// (and still has the wav it came from).
pub fn load(out: &Path, key: &SourceKey) -> Option<Transcription> {
    let saved: SourceKey = serde_json::from_slice(&std::fs::read(out.join(KEY_FILE)).ok()?).ok()?;
    if &saved != key || !out.join("audio.wav").is_file() {
        return None;
    }
    let tr: Transcription =
        serde_json::from_slice(&std::fs::read(out.join("transcript.json")).ok()?).ok()?;
    (!tr.words.is_empty()).then_some(tr)
}

/// Mark the transcript in `out` as made from `key`.
pub fn store(out: &Path, key: &SourceKey) {
    if let Ok(v) = serde_json::to_vec_pretty(key) {
        let _ = std::fs::write(out.join(KEY_FILE), v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whisper::Word;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("digiclip-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn transcript() -> Transcription {
        Transcription {
            words: vec![Word {
                w: "hello".into(),
                s: 0.0,
                e: 0.4,
                conf: None,
            }],
            segments: vec![],
            language: "en".into(),
            model: "base.en".into(),
        }
    }

    #[test]
    fn hit_needs_same_key_and_the_wav() {
        let out = tmp("hit");
        let src = out.join("in.mp4");
        std::fs::write(&src, b"video").unwrap();
        let key = source_key(&src, "base.en", "en").unwrap();
        std::fs::write(
            out.join("transcript.json"),
            serde_json::to_vec(&transcript()).unwrap(),
        )
        .unwrap();
        store(&out, &key);
        // No wav yet: miss.
        assert!(load(&out, &key).is_none());
        std::fs::write(out.join("audio.wav"), b"wav").unwrap();
        assert_eq!(load(&out, &key).unwrap().words.len(), 1);
        // Another model or language: miss.
        let other = SourceKey {
            model: "large-v3".into(),
            ..key.clone()
        };
        assert!(load(&out, &other).is_none());
        let other = SourceKey {
            lang: "de".into(),
            ..key.clone()
        };
        assert!(load(&out, &other).is_none());
        // The source changed size: miss.
        std::fs::write(&src, b"a longer video").unwrap();
        let changed = source_key(&src, "base.en", "en").unwrap();
        assert!(load(&out, &changed).is_none());
        let _ = std::fs::remove_dir_all(&out);
    }
}
