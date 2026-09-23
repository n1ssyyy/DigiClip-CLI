//! Tracking EP benchmark: CPU vs DirectML on identical frames.
//!
//! Run: `cargo run --release --example track_bench`
//! Samples 60 frames from the real video (if present, else testsrc),
//! times 60 detections per EP, prints ms/frame. Data drives the default.

fn main() -> anyhow::Result<()> {
    let src = {
        let p = std::path::PathBuf::from("C:\\Users\\kleod\\Downloads\\videoplayback.mp4");
        if p.is_file() {
            p
        } else {
            anyhow::bail!("need C:\\Users\\kleod\\Downloads\\videoplayback.mp4 for bench");
        }
    };
    let ffmpeg = digiclip_rs::binaries::require("ffmpeg")?;
    // 60 frames @6fps over the first 10s.
    let frames = digiclip_rs::track::sample_frames(&ffmpeg, &src, 6, 320, 180, Some((20.0, 10.0)))?;
    let frames: Vec<_> = frames.into_iter().take(60).collect();
    println!("frames: {}", frames.len());

    let model = std::path::PathBuf::from("models/yunet_2026may.onnx");
    for gpu in [false, true] {
        let mut t = digiclip_rs::track::Tracker::load(&model, gpu)?;
        // Warmup.
        let _ = t.detect(&frames[0].1, 320, 180)?;
        let start = std::time::Instant::now();
        let mut faces = 0;
        for (_, rgb) in &frames {
            faces += t.detect(rgb, 320, 180)?.len();
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0 / frames.len() as f64;
        println!(
            "{}: {ms:.1}ms/frame ({} faces total)",
            if gpu { "DirectML" } else { "CPU     " },
            faces
        );
    }
    Ok(())
}
