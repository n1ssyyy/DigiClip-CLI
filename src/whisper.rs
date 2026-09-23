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
    let mut out = Vec::new();
    for t in tokens {
        let text = t.text.as_deref().unwrap_or("").trim().to_string();
        if text.is_empty() {
            continue;
        }
        if text.starts_with('[') && text.ends_with(']') {
            continue;
        }
        let (Some(f), Some(to)) = (
            t.timestamps.as_ref()?.from.as_deref(),
            t.timestamps.as_ref()?.to.as_deref(),
        ) else {
            continue;
        };
        out.push(Word {
            w: text,
            s: to_seconds(f),
            e: to_seconds(to),
            conf: t.p.map(|p| (p * 1000.0).round() / 1000.0),
        });
    }
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
