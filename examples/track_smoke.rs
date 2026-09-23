//! Tracker smoke: load YuNet, run detection on sampled frames.
//!
//! Run: `cargo run --example track_smoke`
//! Uses the vendored dev model (models/yunet_2026may.onnx) and the ffmpeg
//! testsrc clip (no faces — expect 0, proving the inference path runs).

fn main() -> anyhow::Result<()> {
    let model = std::path::PathBuf::from("models/yunet_2026may.onnx");
    assert!(model.is_file(), "run from the repo root");
    let mut tracker = digiclip_rs::track::Tracker::load(&model, false)?;
    println!("yunet session loaded");

    let ffmpeg = digiclip_rs::binaries::require("ffmpeg")?;
    let src = std::env::temp_dir().join("digiclip-smoke.mp4");
    if !src.is_file() {
        println!("no smoke clip at {}; generating…", src.display());
        let out = std::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=1280x720:rate=30",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                &src.display().to_string(),
            ])
            .output()?;
        assert!(out.status.success());
    }
    let frames = digiclip_rs::track::sample_frames(&ffmpeg, &src, 1, 320, 180, None)?;
    println!("sampled {} frames", frames.len());
    for (t, rgb) in &frames {
        let faces = tracker.detect(rgb, 320, 180)?;
        println!("t={t:.1}s: {} faces", faces.len());
        for f in faces.iter().take(3) {
            println!(
                "  x={:.0} y={:.0} {}x{:.0} score={:.2}",
                f.x, f.y, f.w, f.h, f.score
            );
        }
    }
    println!("TRACK SMOKE OK");
    Ok(())
}
