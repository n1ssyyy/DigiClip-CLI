//! Provisioning smoke proof: the real downloads, on this system.
//!
//! Run: `cargo run --release --example provision_smoke`
//! Needs the internet. Touches nothing but a fresh folder in the temp dir.
//!
//! 1. **A portable ffmpeg.** The build the engine fetches when a machine has
//!    no ffmpeg is downloaded into an empty folder, unpacked, started, and
//!    must have libass (captions) and report a version; ffprobe must start.
//! 2. **A resumed download.** The face model is fetched with its first half
//!    already on disk as a partial file: the result must be the same bytes
//!    as a clean download.

use std::path::Path;
use std::process::Command;

use digiclip_rs::download::{self, Hooks};
use digiclip_rs::provision;

fn starts(exe: &Path) -> anyhow::Result<String> {
    let out = Command::new(exe).arg("-version").output()?;
    anyhow::ensure!(out.status.success(), "{} -version failed", exe.display());
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("digiclip-provision-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    // 1. ffmpeg
    let t = std::time::Instant::now();
    let ffmpeg = provision::ensure_ffmpeg_in(&dir.join("bin"), Hooks::default()).await?;
    println!(
        "ffmpeg: {} ({:.0}s)",
        starts(&ffmpeg)?,
        t.elapsed().as_secs_f64()
    );
    anyhow::ensure!(
        digiclip_rs::binaries::ffmpeg_has_libass(&ffmpeg),
        "the downloaded ffmpeg has no libass"
    );
    let ffprobe = ffmpeg.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });
    println!("ffprobe: {}", starts(&ffprobe)?);
    // Only the two programs stay: no archive, no partial file.
    let wanted = [&ffmpeg, &ffprobe].map(|p| p.file_name().unwrap().to_os_string());
    let left: Vec<_> = std::fs::read_dir(dir.join("bin"))?
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .filter(|n| !wanted.contains(n))
        .collect();
    anyhow::ensure!(left.is_empty(), "left behind in bin/: {left:?}");

    // 2. resume
    let clean = dir.join("clean.onnx");
    download::fetch(
        provision::YUNET_URL,
        &clean,
        "face model",
        100_001,
        Hooks::default(),
    )
    .await?;
    let whole = std::fs::read(&clean)?;
    let resumed = dir.join("resumed.onnx");
    std::fs::write(download::part_path(&resumed), &whole[..whole.len() / 2])?;
    download::fetch(
        provision::YUNET_URL,
        &resumed,
        "face model",
        100_001,
        Hooks::default(),
    )
    .await?;
    anyhow::ensure!(
        std::fs::read(&resumed)? == whole,
        "the resumed download differs from the clean one"
    );
    println!(
        "resume: {} bytes, the second half fetched alone, identical",
        whole.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
    println!("provision smoke OK");
    Ok(())
}
