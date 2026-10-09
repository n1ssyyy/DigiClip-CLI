//! Does the engine's font reader agree with libass? For every bundled font
//! (and any font given with `--add`), draw one line of text with libass at a
//! known size, measure the ink box of the picture, and compare it with what
//! `captions::metrics` says the same line should measure: the advances give
//! the ink's left and right edge, the cap height and the line box give its
//! top and bottom.
//!
//! Run: `cargo run --example font_check -- [--dir <scratch dir>] [--add <font file>]...`
//!
//! The fonts folder used is `<scratch dir>/fonts` (a temp folder by default),
//! so the bundled set and the added fonts are exactly what a render would
//! see, and the real data dir is never touched. Exits non-zero when a font's
//! width is more than 1 % off on the kern-free line. Needs ffmpeg with libass.

use std::path::{Path, PathBuf};

use digiclip_rs::captions::metrics;
use digiclip_rs::fonts::{self, Library};

const W: usize = 1920;
const H: usize = 300;
const SIZE: f64 = 100.0;
const ORIGIN: (f64, f64) = (100.0, 60.0);

/// Ink box (x0, y0, x1, y1) of a libass picture of `text` in `family`.
fn measure(
    ffmpeg: &Path,
    dir: &Path,
    family: &str,
    text: &str,
) -> Option<(usize, usize, usize, usize)> {
    let ass = format!(
        "[Script Info]\nScriptType: v4.00+\nPlayResX: {W}\nPlayResY: {H}\nWrapStyle: 2\n\n\
         [V4+ Styles]\nFormat: Name,Fontname,Fontsize,PrimaryColour,SecondaryColour,OutlineColour,BackColour,Bold,Italic,Underline,StrikeOut,ScaleX,ScaleY,Spacing,Angle,BorderStyle,Outline,Shadow,Alignment,MarginL,MarginR,MarginV,Encoding\n\
         Style: T,{family},{SIZE},&H00FFFFFF,&H00FFFFFF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,0,0,7,0,0,0,1\n\n\
         [Events]\nFormat: Layer,Start,End,Style,Name,MarginL,MarginR,MarginV,Effect,Text\n\
         Dialogue: 0,0:00:00.00,0:00:05.00,T,,0,0,0,,{{\\pos({},{})}}{text}\n",
        ORIGIN.0, ORIGIN.1
    );
    let ass_path = dir.join("check.ass");
    std::fs::write(&ass_path, ass).ok()?;
    let out = digiclip_rs::process::command(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg(format!("color=c=black:s={W}x{H}:r=10:d=1"))
        .arg("-vf")
        .arg(format!(
            "ass={}:fontsdir={}",
            digiclip_rs::render::filter_escape(&ass_path),
            digiclip_rs::render::filter_escape(&dir.join("fonts"))
        ))
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gray", "-"])
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.len() != W * H {
        eprintln!("ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (W, H, 0, 0);
    for (i, &v) in out.stdout.iter().enumerate() {
        if v > 64 {
            let (x, y) = (i % W, i / W);
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
    }
    (x1 >= x0).then_some((x0, y0, x1, y1))
}

fn main() {
    let mut dir: Option<PathBuf> = None;
    let mut add: Vec<PathBuf> = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dir" => dir = it.next().map(PathBuf::from),
            "--add" => add.extend(it.next().map(PathBuf::from)),
            _ => {
                eprintln!("usage: font_check [--dir <scratch dir>] [--add <font file>]...");
                std::process::exit(2)
            }
        }
    }
    let dir = dir.unwrap_or_else(|| std::env::temp_dir().join("digiclip-font-check"));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let fonts_dir = dir.join("fonts");
    digiclip_rs::provision::ensure_fonts_in(&fonts_dir).expect("write the bundled fonts");
    let lib = Library::new(&fonts_dir);
    let mut families: Vec<String> = fonts::bundled_families().map(String::from).collect();
    for f in &add {
        match lib.add(f) {
            Ok(e) => {
                println!("added {} ({})", e.family, e.file);
                families.push(e.family);
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1)
            }
        }
    }
    let ffmpeg = digiclip_rs::binaries::require("ffmpeg").expect("ffmpeg");
    println!(
        "{:<22} {:>9} {:>9} {:>7} | {:>9} {:>9} | {:>7} {:>7}   (px at {SIZE} px; flat = HIMN OMNH, free = Hello world)",
        "family", "flat meas", "flat calc", "diff %", "free meas", "free calc", "top d", "base d"
    );
    let mut bad = 0;
    for fam in &families {
        let Some(face) = metrics::face(fam) else {
            println!("{fam:<22} NO METRICS");
            bad += 1;
            continue;
        };
        // Expected ink box of a line drawn from ORIGIN with `\an7`.
        let expect = |text: &str| {
            let (l, r) = face.ink_x(text, SIZE);
            let w = face.width(text, SIZE);
            let (asc, _) = face.line_box(SIZE);
            let (top, bottom) = face.ink_y(text, SIZE).unwrap_or((0.0, 0.0));
            (
                ORIGIN.0 + l,
                ORIGIN.0 + w - r,
                ORIGIN.1 + asc - top,
                ORIGIN.1 + asc - bottom,
            )
        };
        let flat = "HIMN OMNH";
        let free = "Hello world";
        let (Some(m1), Some(m2)) = (
            measure(&ffmpeg, &dir, fam, flat),
            measure(&ffmpeg, &dir, fam, free),
        ) else {
            println!("{fam:<22} NOT DRAWN");
            bad += 1;
            continue;
        };
        let (e1, e2) = (expect(flat), expect(free));
        let mw = (m1.2 - m1.0 + 1) as f64;
        let ew = e1.1 - e1.0;
        let diff = (mw - ew) / ew * 100.0;
        let fw = (m2.2 - m2.0 + 1) as f64;
        let fe = e2.1 - e2.0;
        // Vertical: the flat line's top (cap height; round letters overshoot a
        // little) and its baseline.
        let top_d = m1.1 as f64 - e1.2;
        let base_d = (m1.3 + 1) as f64 - e1.3;
        println!(
            "{fam:<22} {mw:>9.1} {ew:>9.1} {diff:>7.2} | {fw:>9.1} {fe:>9.1} | {top_d:>7.1} {base_d:>7.1}{}",
            if face.is_cff() { "   (CFF: bearings and ink height estimated)" } else { "" }
        );
        if diff.abs() > 1.0 && !face.is_cff() {
            bad += 1;
        }
    }
    if bad > 0 {
        eprintln!("{bad} font(s) off");
        std::process::exit(1);
    }
}
