//! First-run provisioning: keep the exe small, download the heavy bits.
//!
//! Layout under the OS user-data dir (`%APPDATA%/digiclip` on Windows,
//! `~/.local/share/digiclip` on Linux, `~/Library/Application Support/digiclip`
//! on macOS — see [`root`]):
//! - `bin/ffmpeg(.exe)`, `bin/ffprobe(.exe)` (Windows: gyan essentials,
//!   ~80MB auto-download; macOS/Linux: system install, never downloaded)
//! - `yunet_2026may.onnx` (face detector, ~230KB)
//! - `models/ggml-*.bin` (whisper weights, via [`crate::models`])
//! - `fonts/*.ttf` (embedded in the exe, 1MB — written out for libass)
//!
//! Everything is cached; later runs are fully offline. If a download fails
//! (offline machine), callers fall back gracefully: system PATH ffmpeg,
//! center-crop framing, heuristic scoring.

use std::path::{Path, PathBuf};

pub const FFMPEG_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
pub const YUNET_URL: &str = "https://github.com/opencv/opencv_zoo/raw/main/models/face_detection_yunet/face_detection_yunet_2026may.onnx";
pub const YUNET_FILE: &str = "yunet_2026may.onnx";

const FONTS: &[(&str, &[u8])] = &[
    (
        "Anton-Regular.ttf",
        include_bytes!("../resources/fonts/Anton-Regular.ttf"),
    ),
    (
        "ArchivoBlack-Regular.ttf",
        include_bytes!("../resources/fonts/ArchivoBlack-Regular.ttf"),
    ),
    (
        "Inter-Medium.ttf",
        include_bytes!("../resources/fonts/Inter-Medium.ttf"),
    ),
    (
        "JetBrainsMono-Variable.ttf",
        include_bytes!("../resources/fonts/JetBrainsMono-Variable.ttf"),
    ),
];

/// Root of the provisioned user-data dir.
pub fn root() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("digiclip")
}

pub fn bin_dir() -> PathBuf {
    root().join("bin")
}

pub fn fonts_dir() -> PathBuf {
    root().join("fonts")
}

pub fn yunet_path() -> PathBuf {
    root().join(YUNET_FILE)
}

/// Write embedded fonts out for libass (no-op when already present).
pub fn ensure_fonts() -> anyhow::Result<PathBuf> {
    let dir = fonts_dir();
    std::fs::create_dir_all(&dir)?;
    for (name, bytes) in FONTS {
        let p = dir.join(name);
        if !p.is_file() {
            std::fs::write(&p, bytes)?;
        }
    }
    Ok(dir)
}

async fn download_to(
    url: &str,
    dest: &Path,
    label: &str,
    progress: Option<&crate::progress::ByteFn>,
) -> anyhow::Result<()> {
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p)?;
    }
    let part = dest.with_extension("part");
    let resume_from = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let client = reqwest::Client::new();
    let mut req = client.get(url);
    if resume_from > 0 {
        req = req.header("Range", format!("bytes={resume_from}-"));
    }
    let resp = req.send().await?;
    if !resp.status().is_success() && resp.status().as_u16() != 206 {
        anyhow::bail!("{label} download failed: HTTP {}", resp.status());
    }
    let total = resp.content_length().unwrap_or(0) + resume_from;
    let bar = indicatif::ProgressBar::new(total.max(1));
    bar.set_style(
        indicatif::ProgressStyle::with_template("{msg} [{bar:40}] {bytes}/{total_bytes} ({eta})")
            .unwrap(),
    );
    bar.set_message(label.to_string());
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(resume_from > 0)
        .write(true)
        .truncate(resume_from == 0)
        .open(&part)
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
    tokio::fs::rename(&part, dest).await?;
    Ok(())
}

/// Ensure the YuNet face model is on disk (downloads ~230KB once).
pub async fn ensure_yunet() -> anyhow::Result<PathBuf> {
    ensure_yunet_with(None).await
}

pub async fn ensure_yunet_with(
    progress: Option<&crate::progress::ByteFn>,
) -> anyhow::Result<PathBuf> {
    let dest = yunet_path();
    if dest.is_file() && std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) > 100_000 {
        return Ok(dest);
    }
    download_to(YUNET_URL, &dest, "yunet face model", progress).await?;
    Ok(dest)
}

/// Ensure a usable ffmpeg: provisioned copy wins, else system PATH.
///
/// Windows downloads the gyan essentials zip once (~80MB) when nothing
/// resolves. On macOS/Linux there is no portable zip bundled — install
/// ffmpeg via the system package manager (see the error hint) so the
/// build always carries libass + the platform encoder.
pub async fn ensure_ffmpeg() -> anyhow::Result<PathBuf> {
    ensure_ffmpeg_with(None).await
}

pub async fn ensure_ffmpeg_with(
    progress: Option<&crate::progress::ByteFn>,
) -> anyhow::Result<PathBuf> {
    if let Some(p) = crate::binaries::resolve("ffmpeg") {
        return Ok(p);
    }
    if !cfg!(windows) {
        anyhow::bail!(
            "ffmpeg not found (checked provisioned bin/ and PATH). Install it first: \
             macOS `brew install ffmpeg`, Debian/Ubuntu `sudo apt install ffmpeg`, \
             Fedora `sudo dnf install ffmpeg`. It must include libass \
             (`ffmpeg -hide_banner -buildconf | grep libass`) or caption burn-in fails."
        );
    }
    tracing::info!("ffmpeg not found — downloading portable build (once)…");
    let dir = bin_dir();
    std::fs::create_dir_all(&dir)?;
    let zip_path = dir.join("ffmpeg-essentials.zip");
    download_to(FFMPEG_URL, &zip_path, "ffmpeg", progress).await?;
    tracing::info!("extracting ffmpeg…");
    let file = std::fs::File::open(&zip_path)?;
    let mut zip = zip::ZipArchive::new(file)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        let is_ffmpeg = name.ends_with("/bin/ffmpeg.exe") || name == "bin/ffmpeg.exe";
        let is_probe = name.ends_with("/bin/ffprobe.exe") || name == "bin/ffprobe.exe";
        if cfg!(windows) && (is_ffmpeg || is_probe) {
            let out = dir.join(if is_ffmpeg {
                "ffmpeg.exe"
            } else {
                "ffprobe.exe"
            });
            let mut out_f = std::fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut out_f)?;
        } else if !cfg!(windows)
            && (name.ends_with("/bin/ffmpeg") || name.ends_with("/bin/ffprobe"))
        {
            let out = dir.join(name.rsplit('/').next().unwrap());
            let mut out_f = std::fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut out_f)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755));
            }
        }
    }
    let _ = std::fs::remove_file(&zip_path);
    crate::binaries::require("ffmpeg")
}

/// Prefetch everything (ffmpeg + fonts + YuNet + STT model), then report.
pub async fn provision_all(model: &str) -> anyhow::Result<()> {
    ensure_fonts()?;
    println!("fonts: {}", fonts_dir().display());
    let ff = ensure_ffmpeg().await?;
    println!("ffmpeg: {}", ff.display());
    let yu = ensure_yunet().await?;
    println!(
        "yunet: {} ({} bytes)",
        yu.display(),
        std::fs::metadata(&yu).map(|m| m.len()).unwrap_or(0)
    );
    if !crate::models::is_downloaded(model) {
        crate::models::download(model).await?;
    }
    println!(
        "model {model}: {}",
        crate::models::file_for(model)?.display()
    );
    println!("provisioned: {}", root().display());
    Ok(())
}
