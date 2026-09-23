//! whisper.cpp ggml model manager.
//!
//! Models live in the OS user-data dir (`%APPDATA%/digiclip/models` on
//! Windows, `~/.local/share/digiclip/models` on Linux) — never in the
//! bundle. Default `base.en` is 142MB; turbo upgrades are lazy.

use std::path::PathBuf;

pub struct ModelMeta {
    pub file: &'static str,
    pub size_mb: u64,
}

pub fn models() -> Vec<(&'static str, ModelMeta)> {
    vec![
        (
            "tiny.en",
            ModelMeta {
                file: "ggml-tiny.en.bin",
                size_mb: 75,
            },
        ),
        (
            "base.en",
            ModelMeta {
                file: "ggml-base.en.bin",
                size_mb: 142,
            },
        ),
        (
            "large-v3-turbo-q5_0",
            ModelMeta {
                file: "ggml-large-v3-turbo-q5_0.bin",
                size_mb: 574,
            },
        ),
        (
            "large-v3-turbo",
            ModelMeta {
                file: "ggml-large-v3-turbo.bin",
                size_mb: 1620,
            },
        ),
        (
            "large-v3",
            ModelMeta {
                file: "ggml-large-v3.bin",
                size_mb: 2950,
            },
        ),
    ]
}

pub fn meta(model: &str) -> anyhow::Result<ModelMeta> {
    for (id, m) in models() {
        if id == model {
            return Ok(m);
        }
    }
    anyhow::bail!("Unknown STT model [{model}].")
}

pub fn dir() -> PathBuf {
    let base = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("digiclip")
        .join("models");
    let _ = std::fs::create_dir_all(&base);
    base
}

pub fn file_for(model: &str) -> anyhow::Result<PathBuf> {
    Ok(dir().join(meta(model)?.file))
}

pub fn part_path(model: &str) -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(format!(
        "{}.part",
        file_for(model)?.display()
    )))
}

pub fn download_url(model: &str) -> anyhow::Result<String> {
    Ok(format!(
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
        meta(model)?.file
    ))
}

pub fn is_downloaded(model: &str) -> bool {
    let (path, expected_mb) = match (file_for(model), meta(model)) {
        (Ok(p), Ok(m)) => (p, m.size_mb),
        _ => return false,
    };
    let Ok(md) = std::fs::metadata(&path) else {
        return false;
    };
    if expected_mb == 0 {
        return true;
    }
    (md.len() as f64) > expected_mb as f64 * 1024.0 * 1024.0 * 0.9
}

pub fn require(model: &str) -> anyhow::Result<PathBuf> {
    if !is_downloaded(model) {
        anyhow::bail!(
            "STT model [{model}] is not on disk and auto-download failed. Run with network once so it can fetch from HuggingFace."
        );
    }
    file_for(model)
}

/// First-boot seeding: installers may stage ggml-base.en.bin read-only
/// under `resources/models`. Copy it into the user-writable models dir.
pub fn ensure_bundled() {
    if is_downloaded("base.en") {
        return;
    }
    let staged = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.to_path_buf()))
        .map(|d| d.join("resources").join("models").join("ggml-base.en.bin"));
    if let Some(staged) = staged {
        if staged.is_file() {
            if let Ok(dest) = file_for("base.en") {
                let _ = std::fs::copy(&staged, &dest);
            }
        }
    }
}

/// Stream-download with resume via Range + indicatif progress bar.
/// `progress` (serve hook) also gets `(done, total)` byte updates.
pub async fn download(model: &str) -> anyhow::Result<PathBuf> {
    download_with(model, None).await
}

pub async fn download_with(
    model: &str,
    progress: Option<&crate::progress::ByteFn>,
) -> anyhow::Result<PathBuf> {
    let url = download_url(model)?;
    let dest = part_path(model)?;
    let final_path = file_for(model)?;
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p)?;
    }
    let resume_from = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);

    let client = reqwest::Client::new();
    let mut req = client.get(&url);
    if resume_from > 0 {
        req = req.header("Range", format!("bytes={resume_from}-"));
    }
    let resp = req.send().await?;
    if !resp.status().is_success() && resp.status().as_u16() != 206 {
        anyhow::bail!("Model download failed: HTTP {}", resp.status());
    }
    let total = resp.content_length().unwrap_or(0) + resume_from;
    let bar = indicatif::ProgressBar::new(total.max(1));
    bar.set_style(
        indicatif::ProgressStyle::with_template("{msg} [{bar:40}] {bytes}/{total_bytes} ({eta})")
            .unwrap(),
    );
    bar.set_message(format!("downloading {model}"));

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(resume_from > 0)
        .write(true)
        .truncate(resume_from == 0)
        .open(&dest)
        .await?;
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let mut stream = resp.bytes_stream();
    let mut done = resume_from;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        bar.set_position(done);
        if let Some(p) = progress {
            p(done, total);
        }
    }
    file.flush().await?;
    drop(file);
    bar.finish_and_clear();
    tokio::fs::rename(&dest, &final_path).await?;
    Ok(final_path)
}
