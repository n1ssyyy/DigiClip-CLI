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
    let base = crate::provision::root().join("models");
    let _ = std::fs::create_dir_all(&base);
    base
}

pub fn file_for(model: &str) -> anyhow::Result<PathBuf> {
    Ok(dir().join(meta(model)?.file))
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

/// Fetch the model (see [`crate::download`]: resumed, retried, checked).
pub async fn download(model: &str) -> anyhow::Result<PathBuf> {
    download_with(model, crate::download::Hooks::default()).await
}

pub async fn download_with(
    model: &str,
    hooks: crate::download::Hooks<'_>,
) -> anyhow::Result<PathBuf> {
    let url = download_url(model)?;
    let dest = file_for(model)?;
    // Anything `is_downloaded` would turn down is not the model.
    let min = meta(model)?.size_mb * 1024 * 1024 / 10 * 9 + 1;
    crate::download::fetch(&url, &dest, &format!("speech model {model}"), min, hooks).await?;
    Ok(dest)
}
