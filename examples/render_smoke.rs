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
//! 3. **Split screen and other canvases.** A red|blue source renders as a
//!    split (red on top, blue below, checked by pixel) and on a square
//!    canvas (checked by size and frame count).
//! 4. **A full Look.** The camera + captions scene again, dressed by a Look
//!    through the CLI's own path (`--look`, `--progress-bar`, `--headline`):
//!    moved and resized captions with a box, a placed headline, the bar on
//!    top, a vignette and a warm grade; then the split source with the seam
//!    at 0.6. Checked by pixel: the bar is at the top in its exact colour, a
//!    corner is darker than in the same render without the vignette, the
//!    seam sits on its row. PNG frames land in `<tmp>/look/`.
//!    `-- --look <json|@file>` swaps the camera scene's Look for another.

use std::process::Command;

use clap::Parser;
use digiclip_rs::camera::{self, Kind, PlanInput, Target};
use digiclip_rs::cli::Args;
use digiclip_rs::compose::Rect;
use digiclip_rs::pipeline;
use digiclip_rs::progress::CancelFlag;
use digiclip_rs::render::{self, Job, Span};

/// The Look of stage 4: moved and resized captions with a box, a placed
/// headline, the bar on top, a vignette and a warm grade.
const LOOK: &str = r##"{"v":1,
  "captions":{"x":0.5,"y":0.3,"size":1.4,"box":"#0B1D3A","box_opacity":0.85,"active":"#FFD400"},
  "headline":{"x":0.5,"y":0.62,"size":1.2,"card":"#FFFFFF","ink":"#101010"},
  "bar":{"pos":"top","height":2},
  "effects":{"vignette":0.8,"grade":"warm"}}"##;

/// `--look <json|@file>` on the command line replaces [`LOOK`].
fn look_arg() -> String {
    let mut it = std::env::args().skip_while(|a| a != "--look");
    it.next();
    match it.next() {
        Some(v) => match v.strip_prefix('@') {
            Some(p) => std::fs::read_to_string(p).expect("read --look file"),
            None => v,
        },
        None => LOOK.to_string(),
    }
}

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
            split: &[],
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
        .as_chunks::<{ 32 * 18 }>()
        .0
        .iter()
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
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&b| i16::from_le_bytes(b))
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
            split: &[],
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

    let cam = (probe.clone(), spans.clone(), poses.clone(), total);

    // ------------------------------------------------ 3. split screen + square
    // Left half red, right half blue: the split puts red on top, blue below.
    let duo = tmp.join("duo.mp4");
    run(
        Command::new("ffmpeg").args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=640x720:r=30:d=3",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=640x720:r=30:d=3",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=3",
            "-filter_complex",
            "[0:v][1:v]hstack=inputs=2[v]",
            "-map",
            "[v]",
            "-map",
            "2:a",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            &duo.display().to_string(),
        ]),
        "make duo source",
    );
    let probe = digiclip_rs::ffmpeg::probe(&duo);
    let spans = render::spans_for(&[(0.0, 3.0, 0)], fps);
    let total: usize = spans.iter().map(|s| s.frames).sum();
    let wide = camera::Pose {
        rect: Rect {
            x: 0.0,
            y: 0.0,
            w: 1280.0,
            h: 720.0,
        },
        ax: 640.0,
        ay: 360.0,
        kind: Kind::Wide,
    };
    let poses = vec![wide; total];
    // Half-canvas aspect 1080x960 inside each color.
    let half = |cx: f64| Rect::from_center(cx, 360.0, 540.0 * 1.125, 540.0);
    let split = vec![(half(320.0), half(960.0)); total];
    // Grab one frame as `w x h` RGB.
    let rgb = |path: &std::path::Path, w: usize, h: usize| {
        run(
            Command::new("ffmpeg").args([
                "-v",
                "error",
                "-ss",
                "1.5",
                "-i",
                &path.display().to_string(),
                "-frames:v",
                "1",
                "-vf",
                &format!("scale={w}:{h},format=rgb24"),
                "-f",
                "rawvideo",
                "pipe:1",
            ]),
            "decode rgb",
        )
    };
    let dims = |path: &std::path::Path| {
        let out = run(
            Command::new("ffprobe").args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height",
                "-of",
                "csv=p=0",
                &path.display().to_string(),
            ]),
            "ffprobe dims",
        );
        String::from_utf8_lossy(&out).trim().to_string()
    };
    let out = tmp.join("clip-split.mp4");
    render::render(
        &Job {
            source: &duo,
            probe: &probe,
            fps,
            spans: &spans,
            poses: &poses,
            flash: &[],
            split: &split,
            ass: None,
            out: &out,
            gpu: false,
            threads: 4,
            label: "split",
            look: &Default::default(),
        },
        None,
        &cancel,
    )
    .expect("split render");
    assert_eq!(dims(&out), "1080,1920");
    assert_eq!(probe_counts(&out).0, total);
    // 1x2 pixels: top then bottom.
    let px = rgb(&out, 1, 2);
    let (top, bot) = (&px[0..3], &px[3..6]);
    println!("split: top rgb {top:?}, bottom rgb {bot:?}");
    assert!(
        top[0] > 180 && top[2] < 80,
        "top half must show the left (red) person"
    );
    assert!(
        bot[2] > 180 && bot[0] < 80,
        "bottom half must show the right (blue) person"
    );

    let out = tmp.join("clip-square.mp4");
    render::render(
        &Job {
            source: &duo,
            probe: &probe,
            fps,
            spans: &spans,
            poses: &poses,
            flash: &[],
            split: &[],
            ass: None,
            out: &out,
            gpu: false,
            threads: 4,
            label: "square",
            look: &render::Look {
                canvas: digiclip_rs::compose::Canvas::SQUARE,
                ..Default::default()
            },
        },
        None,
        &cancel,
    )
    .expect("square render");
    assert_eq!(dims(&out), "1080,1080");
    assert_eq!(probe_counts(&out).0, total);
    println!("split + square renders OK");

    // ------------------------------------------------------------ 4. a full Look
    let look_dir = tmp.join("look");
    let _ = std::fs::remove_dir_all(&look_dir);
    std::fs::create_dir_all(&look_dir).unwrap();
    let look_json = look_arg();
    let (cam_probe, cam_spans, cam_poses, cam_total) = cam;
    let cam_dur = cam_total as f64 / 30.0;
    let bar_hex = "#FF3B30";
    let args_with = |json: &str| -> Args {
        Args::try_parse_from([
            "digiclip",
            "in.mp4",
            "--look",
            json,
            "--progress-bar",
            bar_hex,
            "--headline",
            "Look test headline",
        ])
        .unwrap()
    };
    // Render the camera scene dressed by `json`; returns the mp4.
    let render_look = |json: &str, name: &str| -> std::path::PathBuf {
        let args = args_with(json);
        let look = pipeline::look_for(&args).unwrap();
        let ass = look_dir.join(format!("{name}.ass"));
        std::fs::write(
            &ass,
            digiclip_rs::captions::ass::build_for(
                &words,
                "hormozi",
                0.0,
                &pipeline::ass_opts(&look, args.headline.clone(), cam_dur, false),
            ),
        )
        .unwrap();
        let out = look_dir.join(format!("{name}.mp4"));
        let t = std::time::Instant::now();
        render::render(
            &Job {
                source: &src,
                probe: &cam_probe,
                fps,
                spans: &cam_spans,
                poses: &cam_poses,
                flash: &[],
                split: &[],
                ass: Some(&ass),
                out: &out,
                gpu: false,
                threads: 4,
                label: name,
                look: &look,
            },
            None,
            &cancel,
        )
        .unwrap_or_else(|e| panic!("{name} render: {e:#}"));
        println!(
            "{name}: {cam_total} frames in {:.2}s",
            t.elapsed().as_secs_f64()
        );
        assert_eq!(probe_counts(&out).0, cam_total);
        out
    };
    // One frame as full-size RGB (the stream is tagged BT.709, limited).
    let frame_rgb = |path: &std::path::Path, ts: f64, w: usize, h: usize| -> Vec<u8> {
        run(
            Command::new("ffmpeg").args([
                "-v",
                "error",
                "-ss",
                &format!("{ts:.3}"),
                "-i",
                &path.display().to_string(),
                "-frames:v",
                "1",
                "-vf",
                &format!("scale={w}:{h}:in_color_matrix=bt709:in_range=tv,format=rgb24"),
                "-f",
                "rawvideo",
                "pipe:1",
            ]),
            "decode rgb frame",
        )
    };
    let px = |buf: &[u8], w: usize, x: usize, y: usize| -> [i32; 3] {
        let i = (y * w + x) * 3;
        [buf[i] as i32, buf[i + 1] as i32, buf[i + 2] as i32]
    };
    let png = |path: &std::path::Path, ts: f64, name: &str| {
        run(
            Command::new("ffmpeg").args([
                "-y",
                "-v",
                "error",
                "-ss",
                &format!("{ts:.3}"),
                "-i",
                &path.display().to_string(),
                "-frames:v",
                "1",
                "-vf",
                "scale=540:960",
                &look_dir.join(name).display().to_string(),
            ]),
            "grab look frame",
        );
    };

    let looked = render_look(&look_json, "look-camera");
    // The same Look without the vignette (the warm grade stays).
    let mut plain_look: serde_json::Value = serde_json::from_str(&look_json).expect("look is JSON");
    if let Some(fx) = plain_look
        .get_mut("effects")
        .and_then(|e| e.as_object_mut())
    {
        fx.remove("vignette");
    }
    let plain = render_look(&plain_look.to_string(), "look-novignette");
    for (i, ts) in ["1.0", "3.4", "7.0", "10.4"].iter().enumerate() {
        png(
            &looked,
            ts.parse().unwrap(),
            &format!("look-camera-{i}.png"),
        );
    }
    png(&plain, 7.0, "look-novignette-7.0.png");

    // Bar: on top, exact colour, filled to the playhead (frame 180 of 360).
    let (w, h) = (1080usize, 1920usize);
    let mid = frame_rgb(&looked, 6.0, w, h);
    let want = digiclip_rs::compose::parse_hex(bar_hex).unwrap();
    let want = [want.0 as i32, want.1 as i32, want.2 as i32];
    let near =
        |a: [i32; 3], b: [i32; 3], tol: i32| a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tol);
    let thick =
        digiclip_rs::compose::bar_thickness(digiclip_rs::compose::Canvas::TALL, 2.0) as usize;
    let top_filled = px(&mid, w, w / 4, thick / 2);
    let top_track = px(&mid, w, w * 3 / 4, thick / 2);
    println!("bar: filled {top_filled:?}, track {top_track:?}, want {want:?}, {thick} px thick");
    assert!(
        near(top_filled, want, 8),
        "the bar must be at the top in its colour"
    );
    assert!(
        !near(top_track, want, 40),
        "the track past the playhead must not be bar-coloured"
    );
    // Not at the bottom edge, and the row under the bar is picture.
    assert!(
        !near(px(&mid, w, w / 4, h - thick / 2), want, 40),
        "the Look moved the bar off the bottom"
    );
    assert!(
        !near(px(&mid, w, w / 4, thick + 6), want, 40),
        "the bar is {thick} px thick"
    );

    // Vignette: some corner is clearly darker than without it.
    let flat = frame_rgb(&plain, 6.0, w, h);
    let luma = |buf: &[u8], x0: usize, y0: usize| -> f64 {
        let mut sum = 0.0;
        for y in y0..y0 + 24 {
            for x in x0..x0 + 24 {
                let p = px(buf, w, x, y);
                sum += 0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64;
            }
        }
        sum / (24.0 * 24.0)
    };
    // Bottom corners only: the top ones sit under the bar.
    let mut best = (0.0f64, 0.0f64, "");
    for (name, x0, y0) in [("bottom-left", 0, h - 24), ("bottom-right", w - 24, h - 24)] {
        let (a, b) = (luma(&flat, x0, y0), luma(&mid, x0, y0));
        println!("corner {name}: luma {a:.1} without vignette, {b:.1} with");
        if a > best.0 {
            best = (a, b, name);
        }
    }
    assert!(
        best.0 > 30.0,
        "the test picture is too dark in both corners to tell"
    );
    assert!(
        best.1 < best.0 * 0.8,
        "the {} corner must be darker with the vignette ({:.1} vs {:.1})",
        best.2,
        best.1,
        best.0
    );

    // Split seam at 0.6 on the red|blue source: the colour changes on its row.
    let seam_json = r#"{"layout":{"split":0.6},"bar":{"pos":"top"}}"#;
    let args = args_with(seam_json);
    let look = pipeline::look_for(&args).unwrap();
    let duo_spans = render::spans_for(&[(0.0, 3.0, 0)], fps);
    let duo_total: usize = duo_spans.iter().map(|s| s.frames).sum();
    let duo_probe = digiclip_rs::ffmpeg::probe(&duo);
    let seam_ass = look_dir.join("look-split.ass");
    std::fs::write(
        &seam_ass,
        digiclip_rs::captions::ass::build_for(
            &words[..6],
            "hormozi",
            0.0,
            &pipeline::ass_opts(&look, args.headline.clone(), 3.0, true),
        ),
    )
    .unwrap();
    // The panels are 1080x1152 and 1080x768: crop each to its own shape.
    let panel = |cx: f64, aspect: f64| Rect::from_center(cx, 360.0, 400.0 * aspect, 400.0);
    let seam_rows = digiclip_rs::compose::split_rows(1920, Some(0.6)) as usize;
    let top_aspect = 1080.0 / seam_rows as f64;
    let bottom_aspect = 1080.0 / (1920 - seam_rows) as f64;
    let seam_crops = vec![(panel(320.0, top_aspect), panel(960.0, bottom_aspect)); duo_total];
    let seam_out = look_dir.join("look-split.mp4");
    render::render(
        &Job {
            source: &duo,
            probe: &duo_probe,
            fps,
            spans: &duo_spans,
            poses: &vec![wide; duo_total],
            flash: &[],
            split: &seam_crops,
            ass: Some(&seam_ass),
            out: &seam_out,
            gpu: false,
            threads: 4,
            label: "split seam",
            look: &look,
        },
        None,
        &cancel,
    )
    .expect("split seam render");
    png(&seam_out, 1.5, "look-split.png");
    // Column 6 is clear of the captions, which are centred on the seam.
    let col = frame_rgb(&seam_out, 1.5, w, h);
    let first_blue = (0..h)
        .find(|&y| {
            let p = px(&col, w, 6, y);
            p[2] > p[0] + 60
        })
        .expect("a blue row");
    println!("seam: first blue row {first_blue}, expected {seam_rows}");
    assert!(
        first_blue.abs_diff(seam_rows) <= 2,
        "seam at row {first_blue}, not {seam_rows}"
    );
    let above = px(&col, w, 6, first_blue.saturating_sub(8).max(thick + 2));
    assert!(
        above[0] > above[2] + 60,
        "red above the seam, got {above:?}"
    );
    println!("look frames in {}", look_dir.display());

    println!("render smoke OK ({})", tmp.display());
}
