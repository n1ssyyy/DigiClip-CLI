//! whisper.cpp sidecar transcriber — no Python, no torch.
//!
//! Requires 16kHz mono WAV in; emits word-level timestamps out.
//! `whisper.cpp --output-json` segment schema varies by release
//! (timestamps "HH:MM:SS,mmm" + text; newer builds add per-token timing).
//! We prefer token timing when present, else distribute each segment
//! evenly across its words (captions still work, pop-style).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Word {
    pub w: String,
    pub s: f64,
    pub e: f64,
    pub conf: Option<f64>,
}

/// Whisper tokens are word *pieces* (byte-level BPE): a piece that begins
/// with a space opens a new word, anything else continues the one before —
/// "It" + "'d", "Urs" + "ula", "don" + "'t", or the two halves of a
/// multi-byte character. Taking every piece as a word put those splits
/// straight into the captions ("IT 'D", "URS ULA"). Pieces are joined as
/// raw bytes and decoded once per word.
#[derive(Default)]
pub struct WordBuilder {
    words: Vec<Word>,
    cur: Option<PendingWord>,
    /// The current word's opening piece had no timing: drop the whole word.
    skipping: bool,
}

struct PendingWord {
    bytes: Vec<u8>,
    s: f64,
    e: f64,
    conf: Option<f64>,
}

impl WordBuilder {
    /// Add one token piece. `timing` is (start, end) in seconds when known.
    pub fn push(&mut self, raw: &[u8], timing: Option<(f64, f64)>, conf: Option<f64>) {
        let text = String::from_utf8_lossy(raw);
        let t = text.trim();
        // Whitespace-only pieces carry nothing; [_BEG_]/[_TT_n] are markers.
        if t.is_empty() || (t.starts_with('[') && t.ends_with(']')) {
            return;
        }
        let opens = raw.first().is_some_and(|b| b.is_ascii_whitespace())
            || (self.cur.is_none() && !self.skipping);
        if opens {
            self.flush();
            match timing {
                Some((s, e)) => {
                    let start = raw
                        .iter()
                        .position(|b| !b.is_ascii_whitespace())
                        .unwrap_or(0);
                    self.cur = Some(PendingWord {
                        bytes: raw[start..].to_vec(),
                        s,
                        e,
                        conf,
                    });
                    self.skipping = false;
                }
                None => self.skipping = true,
            }
            return;
        }
        if self.skipping {
            return;
        }
        if let Some(p) = self.cur.as_mut() {
            p.bytes.extend_from_slice(raw);
            if let Some((_, e)) = timing {
                p.e = p.e.max(e);
            }
            p.conf = match (p.conf, conf) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
    }

    /// Segment boundary: the next piece always opens a new word.
    pub fn break_word(&mut self) {
        self.flush();
        self.skipping = false;
    }

    pub fn finish(mut self) -> Vec<Word> {
        self.flush();
        self.words
    }

    fn flush(&mut self) {
        if let Some(p) = self.cur.take() {
            let w = String::from_utf8_lossy(&p.bytes).trim().to_string();
            if !w.is_empty() {
                self.words.push(Word {
                    w,
                    s: p.s,
                    e: p.e,
                    conf: p.conf,
                });
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub s: f64,
    pub e: f64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcription {
    pub words: Vec<Word>,
    pub segments: Vec<Segment>,
    pub language: String,
    pub model: String,
}

pub fn cpu_count() -> usize {
    // Windows has no nproc; the process env always carries this.
    if let Ok(n) = std::env::var("NUMBER_OF_PROCESSORS") {
        if let Ok(v) = n.trim().parse::<usize>() {
            if v > 0 {
                return v;
            }
        }
    }
    if let Ok(out) = crate::process::command("nproc").output() {
        if out.status.success() {
            let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if let Ok(v) = t.parse::<usize>() {
                if v > 0 {
                    return v;
                }
            }
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1)
}

/// Pure command builder (no I/O): whisper binary + flags.
/// This whisper.cpp release uses -ng/-dev, not the older -ngl.
pub fn build_command(
    whisper: &Path,
    model: &Path,
    wav: &Path,
    lang: &str,
    out_prefix: &Path,
    threads: usize,
    is_vulkan: bool,
    dev_index: Option<usize>,
) -> Vec<String> {
    let mut cmd = vec![
        whisper.display().to_string(),
        "-m".into(),
        model.display().to_string(),
        "-f".into(),
        wav.display().to_string(),
        "-l".into(),
        lang.into(),
        "-oj".into(),
        "-ojf".into(),
        "-of".into(),
        out_prefix.display().to_string(),
        "-t".into(),
        threads.to_string(),
    ];
    if is_vulkan {
        if let Some(d) = dev_index {
            cmd.push("-dev".into());
            cmd.push(d.to_string());
        }
    } else {
        cmd.push("-ng".into());
    }
    cmd
}

pub fn to_seconds(ts: &str) -> f64 {
    // "HH:MM:SS,mmm" or "MM:SS.mmm"
    let ts = ts.trim().replace(',', ".");
    let parts: Vec<&str> = ts.split(':').collect();
    let (h, m, s) = match parts.len() {
        3 => (
            parts[0].parse::<f64>().unwrap_or(0.0),
            parts[1].parse::<f64>().unwrap_or(0.0),
            parts[2].parse::<f64>().unwrap_or(0.0),
        ),
        2 => (
            0.0,
            parts[0].parse::<f64>().unwrap_or(0.0),
            parts[1].parse::<f64>().unwrap_or(0.0),
        ),
        _ => return 0.0,
    };
    h * 3600.0 + m * 60.0 + s
}

#[derive(Deserialize)]
struct TokenTs {
    from: Option<String>,
    to: Option<String>,
}
#[derive(Deserialize)]
struct Token {
    text: Option<String>,
    timestamps: Option<TokenTs>,
    p: Option<f64>,
}
#[derive(Deserialize)]
struct SegTs {
    from: Option<String>,
    to: Option<String>,
}
#[derive(Deserialize)]
struct Seg {
    text: Option<String>,
    timestamps: Option<SegTs>,
    tokens: Option<Vec<Token>>,
}

fn token_words(seg: &Seg) -> Option<Vec<Word>> {
    let tokens = seg.tokens.as_ref()?;
    if tokens.is_empty() {
        return None;
    }
    let mut wb = WordBuilder::default();
    for t in tokens {
        let raw = t.text.as_deref().unwrap_or("");
        let ts = t.timestamps.as_ref()?;
        let timing = match (ts.from.as_deref(), ts.to.as_deref()) {
            (Some(f), Some(to)) => Some((to_seconds(f), to_seconds(to))),
            _ => None,
        };
        wb.push(
            raw.as_bytes(),
            timing,
            t.p.map(|p| (p * 1000.0).round() / 1000.0),
        );
    }
    let out = wb.finish();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub fn parse_output(data: &serde_json::Value, model: &str) -> Transcription {
    let mut words = Vec::new();
    let mut segments = Vec::new();
    let arr = data
        .get("transcription")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for raw in arr {
        let seg: Seg = serde_json::from_value(raw).unwrap_or(Seg {
            text: None,
            timestamps: None,
            tokens: None,
        });
        let from = seg
            .timestamps
            .as_ref()
            .and_then(|t| t.from.as_deref())
            .map(to_seconds)
            .unwrap_or(0.0);
        let to = seg
            .timestamps
            .as_ref()
            .and_then(|t| t.to.as_deref())
            .map(to_seconds)
            .unwrap_or(from);
        let text = seg.text.as_deref().unwrap_or("").trim().to_string();
        if text.is_empty() {
            continue;
        }
        segments.push(Segment {
            s: from,
            e: to,
            text: text.clone(),
        });
        if let Some(tw) = token_words(&seg) {
            words.extend(tw);
            continue;
        }
        let parts: Vec<&str> = text.split_whitespace().collect();
        let n = parts.len().max(1);
        let dur = (to - from).max(0.0);
        for (i, w) in parts.iter().enumerate() {
            words.push(Word {
                w: w.to_string(),
                s: ((from + dur * i as f64 / n as f64) * 1000.0).round() / 1000.0,
                e: ((from + dur * (i + 1) as f64 / n as f64) * 1000.0).round() / 1000.0,
                conf: None,
            });
        }
    }
    let language = data
        .pointer("/result/language")
        .and_then(|v| v.as_str())
        .unwrap_or("en")
        .to_string();
    Transcription {
        words,
        segments,
        language,
        model: model.into(),
    }
}

pub struct TranscribeOptions {
    pub model: String,
    pub lang: String,
    pub gpu: bool,
    pub threads: usize,
    pub timeout_s: u64,
}

/// Run the sidecar. Auto-downloads weights on first run.
/// timeout_for_model mirrors the PHP tiers: <=150MB 1800s, <=700MB 7200s.
pub fn timeout_for_model(model: &str) -> u64 {
    let mb = crate::models::meta(model).map(|m| m.size_mb).unwrap_or(142);
    if mb <= 150 {
        1800
    } else if mb <= 700 {
        7200
    } else {
        14400
    }
}

pub async fn transcribe(
    wav: &Path,
    out_prefix: &Path,
    opts: &TranscribeOptions,
    cancel: &crate::progress::CancelFlag,
) -> anyhow::Result<Transcription> {
    if !wav.is_file() {
        anyhow::bail!("Audio not found: {}", wav.display());
    }
    if !crate::models::is_downloaded(&opts.model) {
        tracing::info!("STT model '{}' missing — downloading…", opts.model);
        crate::models::download(&opts.model).await?;
    }
    let model_path = crate::models::require(&opts.model)?;
    let vulkan = if opts.gpu {
        crate::binaries::resolve("whisper-cli-vulkan")
    } else {
        None
    };
    let whisper = match vulkan.clone() {
        Some(p) => p,
        // Eager `unwrap_or` would evaluate (and fail) even on the Vulkan
        // path — the fallback must stay lazy.
        None => crate::binaries::require("whisper-cli")?,
    };
    let is_vulkan = vulkan.is_some();

    // Map ranked-best GPU to whisper `-dev N`. We pass the sidecar's own
    // stderr sample when available; without it we omit -dev (default).
    let dev_index: Option<usize> = None;

    let cmd = build_command(
        &whisper,
        &model_path,
        wav,
        &opts.lang,
        out_prefix,
        opts.threads,
        is_vulkan,
        dev_index,
    );
    tracing::info!(
        "whisper: {} (threads={}, gpu={})",
        whisper.display(),
        opts.threads,
        is_vulkan
    );
    let (prog, args) = cmd.split_first().unwrap();
    // Enforce the model-tier timeout (PHP used Process::setTimeout).
    // Poll the child; kill on expiry so a stuck sidecar can't hang the CLI.
    let mut child = crate::process::command(prog).args(args).spawn()?;
    let deadline = std::time::Duration::from_secs(opts.timeout_s.max(60));
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait()? {
            Some(s) => break s,
            None => {
                if cancel.is_cancelled() {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!("cancelled by user");
                }
                if start.elapsed() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!(
                        "whisper-cli timed out after {}s (model '{}').",
                        deadline.as_secs(),
                        opts.model
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    };
    if !status.success() {
        if cancel.is_cancelled() {
            anyhow::bail!("cancelled by user");
        }
        anyhow::bail!("whisper-cli exited with {status}");
    }
    let json_path = PathBuf::from(format!("{}.json", out_prefix.display()));
    if !json_path.is_file() {
        anyhow::bail!("whisper-cli produced no JSON output.");
    }
    let data: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&json_path)?)?;
    let _ = std::fs::remove_file(&json_path);
    Ok(parse_output(&data, &opts.model))
}

#[cfg(test)]
mod word_builder_tests {
    use super::WordBuilder;

    fn words(pieces: &[(&str, Option<(f64, f64)>)]) -> Vec<(String, f64, f64)> {
        let mut wb = WordBuilder::default();
        for (raw, t) in pieces {
            wb.push(raw.as_bytes(), *t, None);
        }
        wb.finish().into_iter().map(|w| (w.w, w.s, w.e)).collect()
    }

    #[test]
    fn pieces_without_a_leading_space_continue_the_word() {
        // Real splits from a base.en transcript: "It'd", "Ursula", "don't".
        let got = words(&[
            (" It", Some((1.0, 1.2))),
            ("'d", Some((1.2, 1.3))),
            (" be", Some((1.3, 1.5))),
            (" Urs", Some((2.0, 2.2))),
            ("ula", Some((2.2, 2.5))),
            (" don", Some((3.0, 3.1))),
            ("'t", Some((3.1, 3.2))),
            (".", Some((3.2, 3.2))),
        ]);
        let text: Vec<&str> = got.iter().map(|w| w.0.as_str()).collect();
        assert_eq!(text, ["It'd", "be", "Ursula", "don't."]);
        // A joined word spans all of its pieces.
        assert_eq!((got[0].1, got[0].2), (1.0, 1.3));
        assert_eq!((got[2].1, got[2].2), (2.0, 2.5));
    }

    #[test]
    fn multibyte_characters_split_across_pieces_rejoin() {
        let mut wb = WordBuilder::default();
        wb.push(b" caf", Some((0.0, 0.2)), None);
        wb.push(&[0xC3], Some((0.2, 0.3)), None); // first byte of "é"
        wb.push(&[0xA9], Some((0.3, 0.4)), None); // second byte
        let w = wb.finish();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].w, "café");
    }

    #[test]
    fn markers_and_untimed_words_are_dropped() {
        let got = words(&[
            ("[_BEG_]", Some((0.0, 0.0))),
            (" hello", Some((0.0, 0.4))),
            (" ghost", None),         // no timing: can't be placed
            ("ly", Some((0.5, 0.6))), // ...and neither can its tail
            (" world", Some((0.7, 1.0))),
            ("[_TT_50]", Some((1.0, 1.0))),
        ]);
        let text: Vec<&str> = got.iter().map(|w| w.0.as_str()).collect();
        assert_eq!(text, ["hello", "world"]);
    }

    #[test]
    fn segment_breaks_open_a_new_word() {
        let mut wb = WordBuilder::default();
        wb.push(b" end", Some((0.0, 0.3)), None);
        wb.break_word();
        wb.push(b"start", Some((0.4, 0.6)), None); // no leading space after a break
        let text: Vec<String> = wb.finish().into_iter().map(|w| w.w).collect();
        assert_eq!(text, ["end", "start"]);
    }
}
