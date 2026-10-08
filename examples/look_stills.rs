//! Stills for eyeballing a Look: burns the captions (and an optional
//! headline) of a fixed sample over a plain grey canvas with the engine's
//! own ffmpeg and `ass=`/`fontsdir` setup, and writes PNG stills along the
//! animation plus the `.ass` itself.
//!
//! Run: `cargo run --example look_stills -- <out_dir> [--look <json|@file>]
//!       [--style <name>] [--aspect 9:16] [--headline "<text>"]
//!       [--bar <#RRGGBB>] [--logo <png> [--logo-pos tl|tr|bl|br]]
//!       [--scene flat|crop|letterbox|split] [--width 540]`
//!
//! `--strip <n>` writes a strip instead of the usual five moments: stills
//! every 40 ms from 200 ms before sample word `n` (0-based) starts to one
//! second after it ends, `still-strip-<ms>ms.png` (the time is the clip clock
//! in the name), and `strip-sheet.png`, the caption band of all of them in
//! time order (3 across, 40 ms per tile), to judge a transition at a glance.
//!
//! `--at <s>[,<s>...]` writes stills at those clip times only
//! (`still-at-<ms>ms.png`).
//!
//! `--width <px>` is the width of the stills (540 by default; the full canvas
//! width shows glow and edges pixel for pixel).
//!
//! `--bar` and `--logo` use the real compositor (`look.bar`) and the real
//! logo filter graph (`look.logo`) on the grey frame, so a bar position or
//! a logo placement can be seen exactly as a render draws it.
//!
//! `--scene` picks what is under the text. `flat` (the default) is the plain
//! grey frame. A look with an `effects` section switches to `crop`: a test
//! picture with real colour and brightness range (grey ramp, colour bars,
//! a colour field, skin tones, a dark gradient, grey steps) through the real
//! compositor, so a vignette or a grade can be judged by eye. `letterbox`
//! shows the same picture as a 16:9 frame over the blurred fill
//! (`effects.fill_dim`); `split` stacks two crops as the split-screen layout
//! does (`layout.split`, captions on the seam). A look with a `layout`
//! section switches to `split`.
//!
//! Uses the ffmpeg the engine would use (bundled, provisioned or on PATH,
//! with libass); nothing is downloaded.

use std::path::{Path, PathBuf};

use digiclip_rs::captions::ass::{self, AssOpts, Clear};
use digiclip_rs::compose::{self, Canvas, Compositor, Rect};
use digiclip_rs::look::Look;
use digiclip_rs::render::{self, Corner, Logo};
use digiclip_rs::whisper::Word;

/// About ten words with natural timings; "never" is a power word, so it
/// gets the keyword colour and bump.
fn sample() -> Vec<Word> {
    [
        ("Most", 0.30, 0.62),
        ("people", 0.62, 1.00),
        ("never", 1.05, 1.50),
        ("learn", 1.50, 1.85),
        ("how", 1.85, 2.05),
        ("to", 2.05, 2.20),
        ("speak", 2.20, 2.60),
        ("on", 2.60, 2.75),
        ("camera.", 2.75, 3.30),
        ("Try", 3.90, 4.15),
        ("it", 4.15, 4.30),
        ("today.", 4.30, 4.90),
    ]
    .iter()
    .map(|&(w, s, e)| Word {
        w: w.into(),
        s,
        e,
        conf: Some(0.95),
    })
    .collect()
}

/// `H:MM:SS.cc` -> seconds.
fn secs(stamp: &str) -> f64 {
    let p: Vec<&str> = stamp.split(':').collect();
    if p.len() != 3 {
        return 0.0;
    }
    p[0].parse::<f64>().unwrap_or(0.0) * 3600.0
        + p[1].parse::<f64>().unwrap_or(0.0) * 60.0
        + p[2].parse::<f64>().unwrap_or(0.0)
}

fn usage() -> ! {
    eprintln!(
        "usage: look_stills <out_dir> [--look <json|@file>] [--style <name>] [--aspect 9:16] \
         [--headline \"<text>\"] [--bar <#RRGGBB>] [--logo <png> [--logo-pos tr]] \
         [--scene flat|crop|letterbox|split] [--strip <word index>] [--at <s>,<s>...]          [--width <px>]"
    );
    std::process::exit(2)
}

/// What sits under the text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scene {
    /// A plain grey frame.
    Flat,
    /// The test picture filling the canvas.
    Crop,
    /// The test picture as a 16:9 frame over the blurred fill.
    Letterbox,
    /// Two crops of the test picture, stacked.
    Split,
}

/// sRGB of the test picture at `(u, v)` (0..1) inside one tile.
fn picture_rgb(u: f64, v: f64) -> (u8, u8, u8) {
    let c = |x: f64| x.round().clamp(0.0, 255.0) as u8;
    let steps =
        |n: usize, f: &dyn Fn(usize) -> (u8, u8, u8)| f(((u * n as f64) as usize).min(n - 1));
    if v < 0.08 {
        // Full-range grey ramp.
        let g = c(255.0 * u);
        (g, g, g)
    } else if v < 0.20 {
        // Colour bars, a little under full saturation.
        const BARS: [(u8, u8, u8); 6] = [
            (222, 48, 48),
            (48, 200, 64),
            (52, 84, 226),
            (48, 206, 214),
            (214, 58, 200),
            (232, 214, 52),
        ];
        steps(6, &|i| BARS[i])
    } else if v < 0.45 {
        // A smooth colour field: red across, blue back, green down.
        let t = (v - 0.20) / 0.25;
        (
            c(255.0 * u),
            c(190.0 * (1.0 - t) + 40.0),
            c(255.0 * (1.0 - u) * (0.4 + 0.6 * t)),
        )
    } else if v < 0.62 {
        // Skin tones, fair to deep.
        const SKIN: [(u8, u8, u8); 4] = [
            (246, 214, 190),
            (224, 172, 140),
            (170, 112, 84),
            (110, 70, 52),
        ];
        steps(4, &|i| SKIN[i])
    } else if v < 0.75 {
        // Shadows: a dark gradient, where banding shows first.
        let g = c(6.0 + 84.0 * u);
        (g, g, c(g as f64 * 1.08))
    } else if v < 0.88 {
        // Eleven grey steps.
        steps(11, &|i| {
            let g = c(255.0 * i as f64 / 10.0);
            (g, g, g)
        })
    } else {
        // Highlights: bright gradient, warm to cool.
        (
            c(255.0 - 40.0 * u),
            c(245.0 - 15.0 * u),
            c(215.0 + 40.0 * u),
        )
    }
}

/// The test picture as a yuv420p frame. A landscape frame holds two tiles
/// side by side (so a split-screen crop of each half still shows a full set).
fn picture(w: u32, h: u32) -> Vec<u8> {
    let g = compose::Geom { w, h };
    let tiles = if w > h { 2.0 } else { 1.0 };
    let at = |x: u32, y: u32| {
        let u = ((x as f64 + 0.5) * tiles / w as f64).fract();
        let (r, g, b) = picture_rgb(u, (y as f64 + 0.5) / h as f64);
        compose::yuv709(r, g, b)
    };
    let mut f = vec![128u8; g.frame_len()];
    for y in 0..h {
        for x in 0..w {
            f[(y * w + x) as usize] = at(x, y).0;
        }
    }
    let (cw, ch) = (g.cw(), g.ch());
    for y in 0..ch {
        for x in 0..cw {
            let (_, u, v) = at((x * 2).min(w - 1), (y * 2).min(h - 1));
            f[g.luma_len() + (y * cw + x) as usize] = u;
            f[g.luma_len() + g.chroma_len() + (y * cw + x) as usize] = v;
        }
    }
    f
}

/// The source frame for a scene, and its size.
fn scene_source(scene: Scene, canvas: Canvas) -> (u32, u32, Vec<u8>) {
    match scene {
        Scene::Flat => {
            let g = compose::Geom {
                w: canvas.w,
                h: canvas.h,
            };
            (canvas.w, canvas.h, vec![128u8; g.frame_len()])
        }
        Scene::Crop => (canvas.w, canvas.h, picture(canvas.w, canvas.h)),
        Scene::Letterbox | Scene::Split => (1280, 720, picture(1280, 720)),
    }
}

/// One yuv420p frame of the scene through the real compositor (effects,
/// split seam and progress bar included), converted to a PNG that ffmpeg
/// loops as the stills' background.
fn composed_background(
    ffmpeg: &Path,
    canvas: Canvas,
    bar: Option<(u8, u8, u8)>,
    look: &Look,
    progress: f32,
    dir: &Path,
    scene: (Scene, &(u32, u32, Vec<u8>)),
) -> Option<PathBuf> {
    let (scene, (sw, sh, src)) = scene;
    let mut comp = Compositor::new(*sw, *sh, canvas)
        .with_bar(bar)
        .with_bar_look(look.bar.as_ref())
        .with_effects(look.effects.as_ref())
        .with_split(look.layout.as_ref());
    let whole = Rect {
        x: 0.0,
        y: 0.0,
        w: *sw as f64,
        h: *sh as f64,
    };
    let frame = match scene {
        Scene::Flat | Scene::Crop => {
            let rect = canvas.base_rect(*sw as f64, *sh as f64);
            comp.compose(src, rect, 0.0, progress)
        }
        Scene::Letterbox => comp.compose(src, whole, 0.0, progress),
        Scene::Split => {
            let half = |x: f64| Rect {
                x,
                w: *sw as f64 / 2.0,
                ..whole
            };
            comp.compose_split(src, half(0.0), half(*sw as f64 / 2.0), 0.0, progress)
        }
    }
    .ok()?;
    let yuv = dir.join("background.yuv");
    std::fs::write(&yuv, frame).ok()?;
    let png = dir.join("background.png");
    let ok = digiclip_rs::process::command(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-s"])
        .arg(format!("{}x{}", canvas.w, canvas.h))
        .arg("-i")
        .arg(&yuv)
        .args(["-frames:v", "1"])
        .arg(&png)
        .status()
        .ok()?
        .success();
    let _ = std::fs::remove_file(&yuv);
    ok.then_some(png)
}

/// The caption band of a still (rows of the `--width`-wide stills): around the
/// first line's position, else where the style's alignment puts it.
fn caption_band(ass: &str, canvas: Canvas, width: u32) -> (u32, u32) {
    let k = width as f64 / canvas.w as f64;
    let field = |l: &str, i: usize| l.split(',').nth(i).and_then(|v| v.parse::<f64>().ok());
    let style = ass
        .lines()
        .find(|l| l.starts_with("Style: ") && !l.contains("Headline"));
    let size = style.and_then(|l| field(l, 2)).unwrap_or(80.0);
    let first = ass
        .lines()
        .find(|l| l.starts_with("Dialogue: 0,"))
        .unwrap_or("");
    let tag_y = ["\\pos(", "\\move("].iter().find_map(|t| {
        first
            .split(t)
            .nth(1)
            .and_then(|r| r.split(',').nth(1))
            .and_then(|y| y.trim_end_matches(')').parse::<f64>().ok())
    });
    let h = canvas.h as f64;
    let cy = tag_y.unwrap_or_else(|| {
        let (align, mv) = style
            .map(|l| (field(l, 18).unwrap_or(2.0), field(l, 21).unwrap_or(0.0)))
            .unwrap_or((2.0, 400.0));
        match align as u32 {
            1..=3 => h - mv - size / 2.0,
            7..=9 => mv + size / 2.0,
            _ => h / 2.0,
        }
    });
    let band = (size * 2.6 * k).round().max(40.0);
    let top = (cy * k - band / 2.0).clamp(0.0, (h * k - band).max(0.0));
    (top.round() as u32 & !1, band as u32 & !1)
}

/// All of a strip's stills as one sheet, caption band only, 3 across.
fn strip_sheet(
    ffmpeg: &Path,
    dir: &Path,
    stills: &[PathBuf],
    ass: &str,
    canvas: Canvas,
    width: u32,
) {
    let (top, band) = caption_band(ass, canvas, width);
    let list = dir.join("strip-list.txt");
    let body: String = stills
        .iter()
        .map(|p| {
            format!(
                "file '{}'\nduration 0.04\n",
                p.display().to_string().replace('\\', "/")
            )
        })
        .collect();
    if std::fs::write(&list, body).is_err() {
        return;
    }
    let rows = stills.len().div_ceil(3);
    let sheet = dir.join("strip-sheet.png");
    let ok = digiclip_rs::process::command(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "concat", "-safe", "0", "-i"])
        .arg(&list)
        .arg("-vf")
        .arg(format!(
            "crop=iw:{band}:0:{top},tile=3x{rows}:padding=3:color=0x202020"
        ))
        .args(["-frames:v", "1"])
        .arg(&sheet)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&list);
    if ok {
        println!("{} (3 across, 40 ms per tile)", sheet.display());
    }
}

fn main() {
    let mut out_dir: Option<PathBuf> = None;
    let (mut look_arg, mut style, mut aspect) =
        (None, String::from("karaoke"), String::from("9:16"));
    let (mut headline, mut bar, mut logo_file, mut logo_pos) =
        (None, None, None, String::from("tr"));
    let mut scene_arg: Option<String> = None;
    let mut strip: Option<usize> = None;
    let mut at: Vec<f64> = Vec::new();
    let mut width = 540u32;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--look" => look_arg = Some(it.next().unwrap_or_else(|| usage())),
            "--style" => style = it.next().unwrap_or_else(|| usage()),
            "--aspect" => aspect = it.next().unwrap_or_else(|| usage()),
            "--headline" => headline = Some(it.next().unwrap_or_else(|| usage())),
            "--bar" => bar = Some(it.next().unwrap_or_else(|| usage())),
            "--logo" => logo_file = Some(PathBuf::from(it.next().unwrap_or_else(|| usage()))),
            "--logo-pos" => logo_pos = it.next().unwrap_or_else(|| usage()),
            "--scene" => scene_arg = Some(it.next().unwrap_or_else(|| usage())),
            "--at" => {
                at = it
                    .next()
                    .unwrap_or_else(|| usage())
                    .split(',')
                    .filter_map(|v| v.trim().parse().ok())
                    .collect()
            }
            "--width" => {
                width = it
                    .next()
                    .and_then(|v| v.parse::<u32>().ok())
                    .map(|v| v.clamp(120, 4096) & !1)
                    .unwrap_or_else(|| usage())
            }
            "--strip" => strip = it.next().and_then(|v| v.parse().ok()).or_else(|| usage()),
            _ if a.starts_with("--") => usage(),
            _ if out_dir.is_none() => out_dir = Some(PathBuf::from(a)),
            _ => usage(),
        }
    }
    let out_dir = out_dir.unwrap_or_else(|| usage());
    let canvas = Canvas::parse(&aspect).unwrap_or_else(|| {
        eprintln!("unknown aspect {aspect} (9:16, 4:5, 1:1, 16:9)");
        std::process::exit(2)
    });
    let bar = bar.map(|c| {
        compose::parse_hex(&c).unwrap_or_else(|| {
            eprintln!("--bar wants a colour like #FFD400");
            std::process::exit(2)
        })
    });
    let ffmpeg = match digiclip_rs::binaries::resolve("ffmpeg") {
        Some(p) if digiclip_rs::binaries::ffmpeg_has_libass(&p) => p,
        Some(p) => {
            eprintln!(
                "ffmpeg at {} has no libass; cannot burn captions",
                p.display()
            );
            std::process::exit(1)
        }
        None => {
            eprintln!("ffmpeg is not provisioned on this machine (nothing downloaded); run `digiclip --provision` or put ffmpeg on PATH");
            std::process::exit(1)
        }
    };
    std::fs::create_dir_all(&out_dir).expect("create out_dir");
    // Absolute, minus the verbatim prefix canonicalize adds on Windows (not a
    // filter-graph friendly path).
    let out_dir = std::fs::canonicalize(&out_dir)
        .ok()
        .and_then(|p| {
            p.to_str()
                .map(|s| s.trim_start_matches(r"\\?\").to_string())
        })
        .map_or(out_dir, PathBuf::from);

    let look = look_arg.as_deref().map(Look::from_arg).unwrap_or_default();
    // The logo as a render would build it: corner, then the Look's section.
    let logo = logo_file.as_ref().map(|p| {
        if !p.is_file() {
            eprintln!("--logo not found: {}", p.display());
            std::process::exit(2)
        }
        let l = Logo::new(
            p.clone(),
            Corner::parse(&logo_pos).unwrap_or(Corner::TopRight),
        );
        match &look.logo {
            Some(g) => l.with_look(g),
            None => l,
        }
    });
    // The scene: asked for, else the picture for an effects look, the split
    // for a layout look, else the plain grey frame.
    let scene = match scene_arg.as_deref() {
        Some("flat") => Scene::Flat,
        Some("crop") => Scene::Crop,
        Some("letterbox") => Scene::Letterbox,
        Some("split") => Scene::Split,
        Some(_) => usage(),
        None if look.layout.is_some() => Scene::Split,
        None if look.effects.is_some() => Scene::Crop,
        None => Scene::Flat,
    };
    if scene == Scene::Split && !digiclip_rs::split::fits(canvas) {
        eprintln!("the split layout needs a tall canvas (9:16 or 4:5)");
        std::process::exit(2)
    }
    let source = scene_source(scene, canvas);
    // Split screen: the captions sit on the seam, wherever the look put it.
    let mut captions = look.captions.clone();
    let seam_at = look.layout.as_ref().and_then(|l| l.split);
    if let (Scene::Split, Some(s)) = (scene, seam_at) {
        let c = captions.get_or_insert_with(Default::default);
        if (s - 0.5).abs() > 1e-9 && c.x.is_none() && c.y.is_none() {
            c.y = Some(compose::split_rows(canvas.h, Some(s)) as f64 / canvas.h as f64);
        }
    }
    let words = sample();
    let dur = words.last().map_or(5.0, |w| w.e) + 0.5;
    let opts = AssOpts {
        w: canvas.w,
        h: canvas.h,
        dur,
        headline: headline.clone(),
        seam: scene == Scene::Split,
        captions,
        headline_look: look.headline.clone(),
        // Text keeps clear of a corner logo, unless the Look placed it.
        clear: logo.as_ref().filter(|l| l.at.is_none()).map(|l| Clear {
            top: l.corner.is_top(),
            left: l.corner.is_left(),
            px: l.size(canvas).0 + Logo::inset(canvas).0 + (canvas.w as f64 * 0.025).round() as u32,
        }),
        ..AssOpts::default()
    };
    let text = ass::build_for(&words, &style, 0.0, &opts);
    let ass_path = out_dir.join("captions.ass");
    std::fs::write(&ass_path, &text).expect("write .ass");

    // Moments along the animation: a line's first frames, mid-pop, settled,
    // and a later line.
    let starts: Vec<f64> = text
        .lines()
        .filter(|l| l.starts_with("Dialogue: 0,") || l.starts_with("Dialogue: 3,"))
        .filter_map(|l| l.split(',').nth(1))
        .map(secs)
        .collect();
    let (first, last) = match (starts.first(), starts.last()) {
        (Some(&f), Some(&l)) => (f, l),
        _ => (0.30, 3.90),
    };
    let mut marks: Vec<(String, f64)> = [
        ("1-start", first + 0.02),
        ("2-mid-pop", first + 0.11),
        ("3-settled", first + 0.60),
        ("4-later-line", last + 0.12),
        ("5-later-settled", last + 0.50),
    ]
    .iter()
    .map(|&(n, t)| (n.to_string(), t))
    .collect();
    // The headline enters at 0: fading in, mid-pop, settled, and (when the
    // Look ends it early) just after it has gone.
    if text.contains("Dialogue: 1,") {
        marks.insert(0, ("h1-entrance".into(), 0.06));
        marks.insert(1, ("h2-entrance-mid".into(), 0.22));
        marks.insert(2, ("h3-settled".into(), 0.80));
        if let Some(end) = text
            .lines()
            .find(|l| l.starts_with("Dialogue: 1,"))
            .and_then(|l| l.split(',').nth(2))
            .map(secs)
            .filter(|&e| e + 0.15 < dur)
        {
            marks.insert(3, ("h4-after-seconds".into(), end + 0.15));
        }
    }
    // A strip: fine steps around one word instead.
    if let Some(n) = strip {
        let Some(w) = words.get(n) else {
            eprintln!("--strip: the sample has words 0..{}", words.len() - 1);
            std::process::exit(2)
        };
        let (a, b) = ((w.s - 0.2).max(0.0), w.e + 1.0);
        marks = (0..)
            .map(|i| a + 0.04 * i as f64)
            .take_while(|t| *t <= b + 1e-9)
            .map(|t| (format!("strip-{:05}ms", (t * 1000.0).round() as i64), t))
            .collect();
    }
    if !at.is_empty() && strip.is_none() {
        marks = at
            .iter()
            .map(|&t| (format!("at-{:05}ms", (t * 1000.0).round() as i64), t))
            .collect();
    }
    let fonts = digiclip_rs::render::fonts_dir();
    let ass_filter = format!(
        "ass={}:fontsdir={}",
        digiclip_rs::render::filter_escape(&ass_path),
        digiclip_rs::render::filter_escape(&fonts)
    );
    // The background: a plain grey colour source, or (with a bar or a logo)
    // the compositor's frame with the engine's own logo overlay.
    let real = bar.is_some() || logo.is_some() || scene != Scene::Flat || look.effects.is_some();
    let mut failed = false;
    let mut made: Vec<PathBuf> = Vec::new();
    for (name, t) in marks {
        let png = out_dir.join(format!("still-{name}.png"));
        let mut cmd = digiclip_rs::process::command(&ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
        if real {
            let bg = composed_background(
                &ffmpeg,
                canvas,
                bar,
                &look,
                (t / dur) as f32,
                &out_dir,
                (scene, &source),
            );
            let Some(bg) = bg else {
                eprintln!("could not compose the background for {name}");
                std::process::exit(1)
            };
            cmd.args(["-loop", "1", "-framerate", "30", "-t"])
                .arg(format!("{:.2}", t + 0.5))
                .arg("-i")
                .arg(&bg);
            let mut graph = String::from(
                "[0:v]setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv",
            );
            if let Some(l) = &logo {
                cmd.arg("-i").arg(&l.path);
                graph.push_str(&render::logo_chain(1, l, canvas));
            }
            graph.push_str(&format!(",{ass_filter},scale={width}:-2[v]"));
            cmd.arg("-filter_complex").arg(graph).args(["-map", "[v]"]);
        } else {
            cmd.args(["-f", "lavfi", "-i"])
                .arg(format!(
                    "color=c=0x808080:s={}x{}:r=30:d={:.2}",
                    canvas.w,
                    canvas.h,
                    t + 0.5
                ))
                .arg("-vf")
                .arg(format!("{ass_filter},scale={width}:-2"));
        }
        let out = cmd
            .args(["-ss", &format!("{t:.3}"), "-frames:v", "1"])
            .arg(&png)
            .output()
            .expect("run ffmpeg");
        if out.status.success() && Path::new(&png).is_file() {
            println!("{} (t={t:.2}s)", png.display());
            made.push(png.clone());
        } else {
            failed = true;
            eprintln!(
                "ffmpeg failed for {name}:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    let _ = std::fs::remove_file(out_dir.join("background.png"));
    if strip.is_some() && !made.is_empty() {
        strip_sheet(&ffmpeg, &out_dir, &made, &text, canvas, width);
    }
    println!("{}", ass_path.display());
    if failed {
        std::process::exit(1);
    }
}
