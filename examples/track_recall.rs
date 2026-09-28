//! Detection recall vs sample width: how often YuNet finds a face, and at
//! what cost, when frames are sampled at different widths.
//!
//! Run: `cargo run --release --example track_recall -- <video> [start] [dur]`

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let src = std::path::PathBuf::from(args.get(1).expect("video path"));
    let start: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let dur: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(60.0);
    let ffmpeg = digiclip_rs::binaries::require("ffmpeg")?;
    let pr = digiclip_rs::ffmpeg::probe(&src);
    let (sw, sh) = (pr.width.unwrap_or(1920), pr.height.unwrap_or(1080));
    let model = std::path::PathBuf::from("models/yunet_2026may.onnx");
    for w in [480u32, 640, 768, 800] {
        let h = ((sh as f64 * w as f64 / sw as f64) / 2.0).round() as u32 * 2;
        let mut t = digiclip_rs::track::Tracker::load(&model, std::env::var("CPU").is_err())?;
        let frames = digiclip_rs::track::sample_frames(&ffmpeg, &src, 8, w, h, Some((start, dur)))?;
        let _ = t.detect(&frames[0].1, w as usize, h as usize)?;
        let t0 = std::time::Instant::now();
        let (mut hit, mut faces, mut small) = (0, 0, 0usize);
        for (_, rgb) in &frames {
            let f = t.detect(rgb, w as usize, h as usize)?;
            if !f.is_empty() {
                hit += 1;
            }
            faces += f.len();
            small += f.iter().filter(|f| f.w < w as f64 * 0.06).count();
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0 / frames.len() as f64;
        println!(
            "{w:4}x{h:<4} recall {:5.1}%  faces/frame {:.2}  small {small:4}  {ms:5.1} ms/frame",
            100.0 * hit as f64 / frames.len() as f64,
            faces as f64 / frames.len() as f64
        );
    }
    Ok(())
}
