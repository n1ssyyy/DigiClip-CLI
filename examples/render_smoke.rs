//! Render smoke proof: synthetic transcript -> real ffmpeg render.
//!
//! Run: `cargo run --example render_smoke`
//! Requires ffmpeg+ffprobe on PATH (with libass). Uses a generated 12s
//! testsrc video + 24 synthetic words, renders full + clip + tracked +
//! wide outputs, then probes them. Proves on real Windows paths:
//! filter escaping (drive colon), fontsdir, encoder auto-pick (NVENC on
//! NVIDIA), loudnorm + faststart, sendcmd schedules, blur-bg fill.

use std::process::Command;

use digiclip_rs::render::{Chunk, ChunkKind};
use digiclip_rs::track::RawTarget;

fn run(cmd: &mut Command, what: &str) {
    let out = cmd.output().unwrap_or_else(|e| panic!("spawn {what}: {e}"));
    assert!(
        out.status.success(),
        "{what} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn main() {
    let tmp = std::env::temp_dir().join("digiclip-render-smoke");
    std::fs::create_dir_all(&tmp).unwrap();
    let src = tmp.join("src.mp4");

    // 12s 1280x720 testsrc + tone so audio filters have input.
    run(
        Command::new("ffmpeg").args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=12:size=1280x720:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=12",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            &src.display().to_string(),
        ]),
        "make testsrc",
    );

    // 24 words, 0.5s each => 12s of speech-shaped timings.
    let words: Vec<digiclip_rs::whisper::Word> = (0..24)
        .map(|i| digiclip_rs::whisper::Word {
            w: format!("word{i}"),
            s: i as f64 * 0.5,
            e: i as f64 * 0.5 + 0.45,
            conf: Some(0.9),
        })
        .collect();

    let threads = 2;
    let gpu = false;
    let center = vec![Chunk {
        t0: 0.0,
        t1: 12.0,
        kind: ChunkKind::Track(vec![]),
    }];
    let full_mp4 = tmp.join("full-9x16.mp4");
    let enc_full = digiclip_rs::render::render_full(
        &src,
        &words,
        "tiktok",
        &center,
        1280,
        720,
        gpu,
        &full_mp4,
        threads,
        None,
        &digiclip_rs::progress::CancelFlag::never(),
    )
    .expect("render_full");
    println!("full: {} (encoder {enc_full})", full_mp4.display());

    let clip = digiclip_rs::validator::Clip {
        rank: 1,
        start_s: 1.0,
        end_s: 11.0,
        hook_line: "smoke hook".into(),
        why_it_works: String::new(),
        score_total: 80.0,
        scores: None,
        title: None,
        hashtags: vec![],
        caption_style: "hormozi".into(),
        source: "smoke".into(),
    };
    let one = vec![Chunk {
        t0: 1.0,
        t1: 11.0,
        kind: ChunkKind::Track(vec![]),
    }];
    let clip_mp4 = tmp.join("clip-01-9x16.mp4");
    let enc_clip = digiclip_rs::render::render_clip(
        &src,
        &clip,
        &words,
        &one,
        1280,
        720,
        gpu,
        &clip_mp4,
        threads,
        None,
        &digiclip_rs::progress::CancelFlag::never(),
    )
    .expect("render_clip");
    println!("clip: {} (encoder {enc_clip})", clip_mp4.display());

    for (label, p) in [("full", &full_mp4), ("clip", &clip_mp4)] {
        assert!(p.is_file(), "{label} missing");
        let probe = digiclip_rs::ffmpeg::probe(p);
        println!(
            "{label}: {:.1?}s {:?}x{:?} ({} bytes)",
            probe.duration_s,
            probe.width,
            probe.height,
            std::fs::metadata(p).unwrap().len()
        );
        assert_eq!((probe.width, probe.height), (Some(1080), Some(1920)));
    }

    // Tracked framing (wandering window): raw targets, smoothed once at render.
    let plan: Vec<RawTarget> = (0..24)
        .map(|i| {
            let w = if i % 2 == 0 { 405.0 } else { 300.0 };
            let x = if i % 2 == 0 { 50.0 } else { 200.0 };
            RawTarget {
                t: i as f64 * 0.5,
                x,
                y: 0.0,
                w,
                h: w * 16.0 / 9.0,
                cut: false,
                hard: false,
                n_faces: 1,
                pick_cx: x + w / 2.0,
            }
        })
        .collect();
    let tracked = vec![Chunk {
        t0: 1.0,
        t1: 11.0,
        kind: ChunkKind::Track(plan),
    }];
    let tracked_mp4 = tmp.join("clip-01-tracked.mp4");
    digiclip_rs::render::render_clip(
        &src,
        &clip,
        &words,
        &tracked,
        1280,
        720,
        gpu,
        &tracked_mp4,
        threads,
        None,
        &digiclip_rs::progress::CancelFlag::never(),
    )
    .expect("render_clip tracked");
    let probe = digiclip_rs::ffmpeg::probe(&tracked_mp4);
    assert_eq!((probe.width, probe.height), (Some(1080), Some(1920)));
    println!(
        "tracked: {:.1?}s ({} bytes)",
        probe.duration_s,
        std::fs::metadata(&tracked_mp4).unwrap().len()
    );

    // Wide fill (blur background) + static punch-in.
    let mixed = vec![
        Chunk {
            t0: 1.0,
            t1: 6.0,
            kind: ChunkKind::Wide,
        },
        Chunk {
            t0: 6.0,
            t1: 11.0,
            kind: ChunkKind::Punch(0.7),
        },
    ];
    let mixed_mp4 = tmp.join("clip-01-mixed.mp4");
    digiclip_rs::render::render_clip(
        &src,
        &clip,
        &words,
        &mixed,
        1280,
        720,
        gpu,
        &mixed_mp4,
        threads,
        None,
        &digiclip_rs::progress::CancelFlag::never(),
    )
    .expect("render_clip mixed");
    let probe = digiclip_rs::ffmpeg::probe(&mixed_mp4);
    assert_eq!((probe.width, probe.height), (Some(1080), Some(1920)));
    assert!(
        (probe.duration_s.unwrap_or(0.0) - 10.0).abs() < 0.5,
        "concat duration"
    );
    println!(
        "mixed wide+punch: {:.1?}s ({} bytes)",
        probe.duration_s,
        std::fs::metadata(&mixed_mp4).unwrap().len()
    );
    println!("RENDER SMOKE OK");
}
