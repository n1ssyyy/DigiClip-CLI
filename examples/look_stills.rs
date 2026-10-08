//! Stills for eyeballing a Look: burns the captions (and an optional
//! headline) of a fixed sample over a plain grey canvas with the engine's
//! own ffmpeg and `ass=`/`fontsdir` setup, and writes PNG stills along the
//! animation plus the `.ass` itself.
//!
//! Run: `cargo run --example look_stills -- <out_dir> [--look <json|@file>]
//!       [--style <name>] [--aspect 9:16] [--headline "<text>"]
//!       [--bar <#RRGGBB>] [--logo <png> [--logo-pos tl|tr|bl|br]]`
//!
//! `--bar` and `--logo` use the real compositor (`look.bar`) and the real
//! logo filter graph (`look.logo`) on the grey frame, so a bar position or
//! a logo placement can be seen exactly as a render draws it.
//!
//! Uses the ffmpeg the engine would use (bundled, provisioned or on PATH,
//! with libass); nothing is downloaded.

use std::path::{Path, PathBuf};

use digiclip_rs::captions::ass::{self, AssOpts, Clear};
use digiclip_rs::compose::{self, Canvas, Compositor};
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
         [--headline \"<text>\"] [--bar <#RRGGBB>] [--logo <png> [--logo-pos tr]]"
    );
    std::process::exit(2)
}

/// One grey yuv420p frame through the real compositor (progress bar and all),
/// converted to a PNG that ffmpeg loops as the stills' background.
fn composed_background(
    ffmpeg: &Path,
    canvas: Canvas,
    bar: Option<(u8, u8, u8)>,
    look: &Look,
    progress: f32,
    dir: &Path,
) -> Option<PathBuf> {
    let g = compose::Geom {
        w: canvas.w,
        h: canvas.h,
    };
    let mut src = vec![128u8; g.frame_len()];
    src[g.luma_len()..].fill(128);
    let rect = canvas.base_rect(canvas.w as f64, canvas.h as f64);
    let mut comp = Compositor::new(canvas.w, canvas.h, canvas)
        .with_bar(bar)
        .with_bar_look(look.bar.as_ref());
    let frame = comp.compose(&src, rect, 0.0, progress).ok()?;
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

fn main() {
    let mut out_dir: Option<PathBuf> = None;
    let (mut look_arg, mut style, mut aspect) =
        (None, String::from("karaoke"), String::from("9:16"));
    let (mut headline, mut bar, mut logo_file, mut logo_pos) =
        (None, None, None, String::from("tr"));
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
    let words = sample();
    let dur = words.last().map_or(5.0, |w| w.e) + 0.5;
    let opts = AssOpts {
        w: canvas.w,
        h: canvas.h,
        dur,
        headline: headline.clone(),
        captions: look.captions.clone(),
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
        .filter(|l| l.starts_with("Dialogue: 0,"))
        .filter_map(|l| l.split(',').nth(1))
        .map(secs)
        .collect();
    let (first, last) = match (starts.first(), starts.last()) {
        (Some(&f), Some(&l)) => (f, l),
        _ => (0.30, 3.90),
    };
    let mut marks = vec![
        ("1-start", first + 0.02),
        ("2-mid-pop", first + 0.11),
        ("3-settled", first + 0.60),
        ("4-later-line", last + 0.12),
        ("5-later-settled", last + 0.50),
    ];
    // The headline enters at 0: fading in, mid-pop, settled, and (when the
    // Look ends it early) just after it has gone.
    if text.contains("Dialogue: 1,") {
        marks.insert(0, ("h1-entrance", 0.06));
        marks.insert(1, ("h2-entrance-mid", 0.22));
        marks.insert(2, ("h3-settled", 0.80));
        if let Some(end) = text
            .lines()
            .find(|l| l.starts_with("Dialogue: 1,"))
            .and_then(|l| l.split(',').nth(2))
            .map(secs)
            .filter(|&e| e + 0.15 < dur)
        {
            marks.insert(3, ("h4-after-seconds", end + 0.15));
        }
    }
    let fonts = digiclip_rs::render::fonts_dir();
    let ass_filter = format!(
        "ass={}:fontsdir={}",
        digiclip_rs::render::filter_escape(&ass_path),
        digiclip_rs::render::filter_escape(&fonts)
    );
    // The background: a plain grey colour source, or (with a bar or a logo)
    // the compositor's frame with the engine's own logo overlay.
    let real = bar.is_some() || logo.is_some();
    let mut failed = false;
    for (name, t) in marks {
        let png = out_dir.join(format!("still-{name}.png"));
        let mut cmd = digiclip_rs::process::command(&ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
        if real {
            let bg = composed_background(&ffmpeg, canvas, bar, &look, (t / dur) as f32, &out_dir);
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
            graph.push_str(&format!(",{ass_filter},scale=540:-2[v]"));
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
                .arg(format!("{ass_filter},scale=540:-2"));
        }
        let out = cmd
            .args(["-ss", &format!("{t:.3}"), "-frames:v", "1"])
            .arg(&png)
            .output()
            .expect("run ffmpeg");
        if out.status.success() && Path::new(&png).is_file() {
            println!("{} (t={t:.2}s)", png.display());
        } else {
            failed = true;
            eprintln!(
                "ffmpeg failed for {name}:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    let _ = std::fs::remove_file(out_dir.join("background.png"));
    println!("{}", ass_path.display());
    if failed {
        std::process::exit(1);
    }
}
