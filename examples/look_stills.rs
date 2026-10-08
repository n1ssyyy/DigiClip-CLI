//! Stills for eyeballing a Look: burns the captions of a fixed sample over a
//! plain grey canvas with the engine's own ffmpeg and `ass=`/`fontsdir`
//! setup, and writes PNG stills along the animation plus the `.ass` itself.
//!
//! Run: `cargo run --example look_stills -- <out_dir> [--look <json|@file>]
//!       [--style <name>] [--aspect 9:16]`
//!
//! Uses the ffmpeg the engine would use (bundled, provisioned or on PATH,
//! with libass); nothing is downloaded.

use std::path::{Path, PathBuf};

use digiclip_rs::captions::ass::{self, AssOpts};
use digiclip_rs::compose::Canvas;
use digiclip_rs::look::Look;
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
        "usage: look_stills <out_dir> [--look <json|@file>] [--style <name>] [--aspect 9:16]"
    );
    std::process::exit(2)
}

fn main() {
    let mut out_dir: Option<PathBuf> = None;
    let (mut look_arg, mut style, mut aspect) =
        (None, String::from("karaoke"), String::from("9:16"));
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--look" => look_arg = Some(it.next().unwrap_or_else(|| usage())),
            "--style" => style = it.next().unwrap_or_else(|| usage()),
            "--aspect" => aspect = it.next().unwrap_or_else(|| usage()),
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
    let words = sample();
    let dur = words.last().map_or(5.0, |w| w.e) + 0.5;
    let opts = AssOpts {
        w: canvas.w,
        h: canvas.h,
        dur,
        captions: look.captions.clone(),
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
    let marks = [
        ("1-start", first + 0.02),
        ("2-mid-pop", first + 0.11),
        ("3-settled", first + 0.60),
        ("4-later-line", last + 0.12),
        ("5-later-settled", last + 0.50),
    ];
    let fonts = digiclip_rs::render::fonts_dir();
    let filter = format!(
        "ass={}:fontsdir={},scale=540:-2",
        digiclip_rs::render::filter_escape(&ass_path),
        digiclip_rs::render::filter_escape(&fonts)
    );
    let mut failed = false;
    for (name, t) in marks {
        let png = out_dir.join(format!("still-{name}.png"));
        let out = digiclip_rs::process::command(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg(format!(
                "color=c=0x808080:s={}x{}:r=30:d={:.2}",
                canvas.w,
                canvas.h,
                t + 0.5
            ))
            .arg("-vf")
            .arg(&filter)
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
    println!("{}", ass_path.display());
    if failed {
        std::process::exit(1);
    }
}
