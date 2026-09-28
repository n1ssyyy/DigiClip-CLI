//! Render smoke proof: real ffmpeg, real engine, measured results.
//!
//! Run: `cargo run --release --example render_smoke`
//! Requires ffmpeg+ffprobe on PATH (with libass).
//!
//! 1. **A/V sync across jump cuts.** A synthetic source flashes white and
//!    clicks at every whole second. It renders with three keeps whose
//!    edges are off the frame grid; the output is decoded and every flash
//!    must coincide with its click (within one frame), with the exact frame
//!    count and audio length the plan promised.
//! 2. **Camera + captions.** A 1280x720 testsrc renders through a planned
//!    camera (speaker left → speaker right handoff → wide → back, plus an
//!    emphasis punch) with burned captions; a few frames are dumped as PNG
//!    for eyeballing.

use std::process::Command;

use digiclip_rs::camera::{self, Kind, PlanInput, Target};
use digiclip_rs::compose::Rect;
use digiclip_rs::progress::CancelFlag;
use digiclip_rs::render::{self, Job, Span};

fn run(cmd: &mut Command, what: &str) -> Vec<u8> {
    let out = cmd.output().unwrap_or_else(|e| panic!("spawn {what}: {e}"));
    assert!(
        out.status.success(),
        "{what} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn probe_counts(path: &std::path::Path) -> (usize, f64) {
    let out = run(
        Command::new("ffprobe").args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
            &path.display().to_string(),
        ]),
        "ffprobe frames",
    );
    let frames: usize = String::from_utf8_lossy(&out).trim().parse().unwrap();
    let out = run(
        Command::new("ffprobe").args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=duration",
            "-of",
            "csv=p=0",
            &path.display().to_string(),
        ]),
        "ffprobe audio",
    );
    let adur: f64 = String::from_utf8_lossy(&out).trim().parse().unwrap();
    (frames, adur)
}

fn main() {
    let tmp = std::env::temp_dir().join("digiclip-render-smoke");
    std::fs::create_dir_all(&tmp).unwrap();
    let cancel = CancelFlag::never();
    let fps = (30u32, 1u32);

    // ---------------------------------------------------------------- 1. sync
    let sync_src = tmp.join("sync-src.mp4");
    run(
        Command::new("ffmpeg").args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=1280x720:r=30:d=12,drawbox=x=0:y=0:w=iw:h=ih:color=white:t=fill:enable='lt(mod(t\\,1)\\,0.03)'",
            "-f",
            "lavfi",
            "-i",
            "aevalsrc='if(lt(mod(t\\,1)\\,0.02)\\,0.8*sin(2*PI*1000*t)\\,0)':s=48000:d=12",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "15",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            &sync_src.display().to_string(),
        ]),
        "make sync source",
    );
    let probe = digiclip_rs::ffmpeg::probe(&sync_src);
    // Off-grid keep edges on purpose; clicks sit inside every keep.
    let mut keeps = vec![
        digiclip_rs::timeline::Keep { a: 0.51, b: 3.217 },
        digiclip_rs::timeline::Keep { a: 4.683, b: 7.12 },
        digiclip_rs::timeline::Keep { a: 8.405, b: 11.61 },
    ];
    digiclip_rs::timeline::snap_keeps(&mut keeps, fps);
    let spans: Vec<Span> = render::spans_for(
        &keeps.iter().map(|k| (k.a, k.b, 0)).collect::<Vec<_>>(),
        fps,
    );
    let total: usize = spans.iter().map(|s| s.frames).sum();
    let full = Rect {
        x: 0.0,
        y: 0.0,
        w: 1280.0,
        h: 720.0,
    };
    let poses = vec![
        camera::Pose {
            rect: full,
            ax: 640.0,
            ay: 360.0,
            kind: Kind::Wide,
        };
        total
    ];
    let sync_out = tmp.join("sync-out.mp4");
    let t = std::time::Instant::now();
    let enc = render::render(
        &Job {
            source: &sync_src,
            probe: &probe,
            fps,
            spans: &spans,
            poses: &poses,
            flash: &[],
            ass: None,
            out: &sync_out,
            gpu: false,
            threads: 4,
            label: "sync",
            look: &Default::default(),
        },
        None,
        &cancel,
    )
    .expect("sync render");
    println!(
        "sync render: {total} frames in {:.2}s ({enc})",
        t.elapsed().as_secs_f64()
    );
    let (frames, adur) = probe_counts(&sync_out);
    assert_eq!(frames, total, "frame count must match the plan exactly");
    let vdur = total as f64 / 30.0;
    assert!(
        (adur - vdur).abs() < 0.03,
        "audio {adur:.3}s vs video {vdur:.3}s"
    );
    // Flash frames (mean luma of a tiny gray decode).
    let luma = run(
        Command::new("ffmpeg").args([
            "-v",
            "error",
            "-i",
            &sync_out.display().to_string(),
            "-vf",
            "scale=32:18,format=gray",
            "-f",
            "rawvideo",
            "pipe:1",
        ]),
        "decode luma",
    );
    let flashes: Vec<f64> = luma
        .chunks_exact(32 * 18)
        .enumerate()
        .filter(|(_, f)| f.iter().map(|&v| v as u32).sum::<u32>() / (32 * 18) > 128)
        .map(|(i, _)| i as f64 / 30.0)
        .collect();
    // Click onsets.
    let pcm = run(
        Command::new("ffmpeg").args([
            "-v",
            "error",
            "-i",
            &sync_out.display().to_string(),
            "-ac",
            "1",
            "-ar",
            "48000",
            "-f",
            "s16le",
            "pipe:1",
        ]),
        "decode audio",
    );
    let samples: Vec<i16> = pcm
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    let mut clicks = Vec::new();
    let mut quiet = 0usize;
    for (i, &s) in samples.iter().enumerate() {
        if (s as i32).abs() > 6000 {
            if quiet > 9600 {
                clicks.push(i as f64 / 48000.0);
            }
            quiet = 0;
        } else {
            quiet += 1;
        }
    }
    println!("flashes at {flashes:.3?}");
    println!("clicks  at {clicks:.3?}");
    assert!(
        flashes.len() >= 8,
        "expected ~9 flashes, got {}",
        flashes.len()
    );
    let mut worst = 0.0f64;
    for f in &flashes {
        let d = clicks
            .iter()
            .map(|c| (c - f).abs())
            .fold(f64::INFINITY, f64::min);
        worst = worst.max(d);
    }
    println!("worst flash/click offset: {:.1} ms", worst * 1000.0);
    assert!(worst < 0.034, "A/V must line up within a frame");

    // ------------------------------------------------------- 2. camera + captions
    let src = tmp.join("src.mp4");
    run(
        Command::new("ffmpeg").args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=12:size=1280x720:rate=30",
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
    let probe = digiclip_rs::ffmpeg::probe(&src);
    let words: Vec<digiclip_rs::whisper::Word> = (0..24)
        .map(|i| digiclip_rs::whisper::Word {
            w: format!("word{i}"),
            s: i as f64 * 0.5,
            e: i as f64 * 0.5 + 0.45,
            conf: Some(0.9),
        })
        .collect();
    let spans = render::spans_for(&[(0.0, 12.0, 0)], fps);
    let total: usize = spans.iter().map(|s| s.frames).sum();
    let face = |cx: f64| Rect::from_center(cx, 360.0, 720.0 * 9.0 / 16.0 / 1.2, 720.0 / 1.2);
    let mut targets: Vec<Target> = Vec::new();
    for i in 0..(12 * 15) {
        let t = i as f64 / 15.0;
        let (rect, kind, cut) = if t < 3.0 {
            (face(380.0), Kind::Subject, false)
        } else if t < 6.0 {
            (face(900.0), Kind::Subject, i == 45)
        } else if t < 8.5 {
            (
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 1280.0,
                    h: 720.0,
                },
                Kind::Wide,
                i == 90,
            )
        } else {
            (face(420.0), Kind::Subject, i == 128)
        };
        targets.push(Target {
            t,
            rect,
            ax: rect.cx(),
            ay: rect.y + rect.h * 0.4,
            kind,
            cut,
            weak: false,
        });
    }
    let mut poses = camera::plan(
        &PlanInput {
            targets: &targets,
            frames: total,
            fps: 30.0,
            src_w: 1280.0,
            src_h: 720.0,
            hards: &[],
            jumps: &[],
            onsets: &[2.9],
            canvas: Default::default(),
        },
        &camera::CamCfg::default(),
    );
    let landed = camera::apply_punches(
        &mut poses,
        &[(10.0, 10.8)],
        30.0,
        1280.0,
        720.0,
        &camera::PunchCfg::default(),
    );
    println!("camera: {} poses, {landed} punch", poses.len());
    let ass = tmp.join("clip.ass");
    std::fs::write(
        &ass,
        digiclip_rs::captions::ass::build(&words, "tiktok", 0.0),
    )
    .unwrap();
    let out = tmp.join("clip-camera.mp4");
    let t = std::time::Instant::now();
    let enc = render::render(
        &Job {
            source: &src,
            probe: &probe,
            fps,
            spans: &spans,
            poses: &poses,
            flash: &[],
            ass: Some(&ass),
            out: &out,
            gpu: true,
            threads: 4,
            label: "camera",
            look: &Default::default(),
        },
        None,
        &cancel,
    )
    .expect("camera render");
    println!(
        "camera render: {total} frames in {:.2}s ({enc}) -> {}",
        t.elapsed().as_secs_f64(),
        out.display()
    );
    let (frames, _) = probe_counts(&out);
    assert_eq!(frames, total);
    for (i, ts) in ["1.0", "3.4", "7.0", "10.4"].iter().enumerate() {
        let png = tmp.join(format!("frame-{i}.png"));
        run(
            Command::new("ffmpeg").args([
                "-y",
                "-v",
                "error",
                "-ss",
                ts,
                "-i",
                &out.display().to_string(),
                "-frames:v",
                "1",
                "-vf",
                "scale=270:480",
                &png.display().to_string(),
            ]),
            "grab frame",
        );
    }
    println!("render smoke OK ({})", tmp.display());
}
