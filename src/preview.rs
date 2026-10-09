//! One still of the real render: the engine side of the `preview_frame`
//! serve command (the desktop app's "Exact frame").
//!
//! A preview treats a stretch of the source as if a clip ran over it
//! (`start_s`, `len_s`) and draws the frame at `t` seconds into that clip
//! with the same pieces a render uses: the job options through
//! [`crate::serve`]'s `args_for` (so a preview and a job cannot drift
//! apart), [`crate::pipeline::look_for`] for the canvas and the Look, the
//! [`Compositor`] for framing, effects, split seam and progress bar, the
//! ASS writer for captions and headline, and the encoder graph's logo and
//! `ass=` steps. Framing is the centre of the frame: there is no face
//! tracking, no camera motion, no tightening.
//!
//! Cost: one ffprobe per source (cached), one ffmpeg that decodes a single
//! frame after an input-side seek, the compositor, one ffmpeg that burns
//! logo + captions and writes the JPEG.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::Mutex;

use crate::cli::Args;
use crate::compose::{Canvas, Compositor, Rect};
use crate::ffmpeg::Probe;
use crate::timeline::Keep;
use crate::whisper::Word;

/// Longest side of the JPEG (px), unless the canvas is smaller.
pub const LONG_SIDE: u32 = 720;
/// Names the previews rotate through in a job folder: none ever piles up,
/// and the next few replies always carry a name the app has not just seen.
const RING: u64 = 4;
/// Longest window a preview accepts (s).
const MAX_LEN_S: f64 = 3600.0;
/// Window the app's Studio shows when it does not say.
pub const DEFAULT_LEN_S: f64 = 12.0;

/// Serialises previews (one at a time, in arrival order) and numbers them.
#[derive(Default)]
pub struct Gate {
    pub lock: tokio::sync::Mutex<()>,
    pub seq: AtomicU64,
}

/// File name of preview number `seq` inside the job folder. `preview-N.jpg`
/// cannot be a clip file (`clip-NN-…`), the poster or a transcript, and only
/// [`RING`] of them ever exist.
pub fn file_name(seq: u64) -> String {
    format!("preview-{}.jpg", seq % RING)
}

/// The window and the playhead, made safe: `(start_s, len_s, t)`. A
/// non-finite or negative start is 0, `t` is clamped into `0..=len_s`; a
/// window with no length is the only thing that is an error.
pub fn window(start_s: f64, len_s: f64, t: f64) -> Result<(f64, f64, f64), String> {
    if !len_s.is_finite() || len_s <= 0.0 {
        return Err("len_s must be above 0".into());
    }
    let len = len_s.min(MAX_LEN_S);
    let start = if start_s.is_finite() {
        start_s.max(0.0)
    } else {
        0.0
    };
    let t = if t.is_finite() {
        t.clamp(0.0, len)
    } else {
        0.0
    };
    Ok((start, len, t))
}

/// The headline for a preview: nothing while the options have none
/// (`None`), the options' own text, else the first clip's title, else the
/// job's title.
pub fn headline_text(
    option: Option<&str>,
    first_clip_title: Option<&str>,
    job_title: &str,
) -> Option<String> {
    let given = option?.trim();
    if !given.is_empty() {
        return Some(given.to_string());
    }
    [first_clip_title.unwrap_or(""), job_title]
        .iter()
        .map(|t| t.trim())
        .find(|t| !t.is_empty())
        .map(str::to_string)
}

/// The transcript words of the window, on the window's clock (0 = its
/// start). The same retiming a render does for one keep.
pub fn window_words(words: &[Word], start_s: f64, len_s: f64) -> Vec<Word> {
    crate::timeline::retime(
        words,
        &[Keep {
            a: start_s,
            b: start_s + len_s,
        }],
    )
}

/// JPEG size for a canvas: [`LONG_SIDE`] on the long side, never upscaled,
/// both even.
pub fn out_size(c: Canvas) -> (u32, u32) {
    let s = (LONG_SIDE as f64 / c.w.max(c.h) as f64).min(1.0);
    let even = |v: f64| (((v * s) / 2.0).round() as u32 * 2).max(2);
    (even(c.w as f64), even(c.h as f64))
}

/// How the picture is laid out in the still.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// The centre crop that fills the canvas.
    Single,
    /// The whole frame over the blurred fill.
    Fill,
    /// Two panels around the seam.
    Split,
}

/// Does a job option `layout` ask for the whole frame over the blurred fill
/// (what a render does on a shot with nobody to follow)?
pub fn wants_fill(layout: Option<&str>) -> bool {
    matches!(
        layout.map(|l| l.trim().to_ascii_lowercase()).as_deref(),
        Some("fill" | "letterbox" | "wide")
    )
}

/// Everything one still needs.
pub struct Request {
    pub source: PathBuf,
    /// The job folder the JPEG (and the preview's `.ass`) is written to.
    pub dir: PathBuf,
    /// `transcript.json` of the job (absent or unreadable: no captions).
    pub transcript: PathBuf,
    /// The options as a job would run them.
    pub args: Args,
    pub headline: Option<String>,
    pub fill: bool,
    pub start_s: f64,
    pub len_s: f64,
    pub t: f64,
    pub seq: u64,
}

/// What a finished still reports.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Output {
    /// File name inside the job folder (`/art/<job>/<file>`).
    pub file: String,
    pub width: u32,
    pub height: u32,
    /// The playhead actually used (clamped into the window).
    pub t: f64,
    /// Source second the picture was taken at.
    pub src_t: f64,
    /// Transcript words inside the window.
    pub words: usize,
    pub layout: Layout,
    /// Things the preview could not do exactly as asked.
    pub warnings: Vec<String>,
}

/// ffprobe is a process spawn: a source is probed once and remembered.
fn probe_cached(path: &Path) -> Probe {
    type Entry = (PathBuf, u64, Option<std::time::SystemTime>, Probe);
    static CACHE: Mutex<Option<Entry>> = Mutex::new(None);
    let meta = std::fs::metadata(path).ok();
    let (len, mtime) = (
        meta.as_ref().map_or(0, |m| m.len()),
        meta.and_then(|m| m.modified().ok()),
    );
    if let Ok(c) = CACHE.lock() {
        if let Some((p, l, m, probe)) = c.as_ref() {
            if p == path && *l == len && *m == mtime {
                return probe.clone();
            }
        }
    }
    let probe = crate::ffmpeg::probe(path);
    if let Ok(mut c) = CACHE.lock() {
        *c = Some((path.to_path_buf(), len, mtime, probe.clone()));
    }
    probe
}

/// Stand-in people for a split preview, which has no tracker: one on the
/// left and one on the right of the frame, run through the same crop maths
/// a real split uses.
fn assumed_pair(sw: f64, sh: f64, canvas: Canvas, seam: f64) -> Option<(Rect, Rect)> {
    use crate::track::{Duo, Face};
    let h = sh * 0.22;
    let face = |cx: f64| Face {
        x: cx - h * 0.4,
        y: sh * 0.38 - h / 2.0,
        w: h * 0.8,
        h,
        score: 1.0,
    };
    let duo: Vec<Duo> = (0..3)
        .map(|i| Duo {
            t: i as f64,
            left: face(sw * 0.27),
            right: face(sw * 0.73),
        })
        .collect();
    let plan = crate::split::plan(&duo, &[], 0.0, 3.0, sw, sh, canvas, seam);
    crate::split::at(&plan, 0.0)
}

/// Render the still. Blocking. Two ffmpeg processes, started together: the
/// one that burns logo and captions and writes the JPEG waits on its stdin
/// while the other decodes the frame, so their (slow, on some PCs) start-ups
/// overlap.
pub fn render(req: &Request) -> anyhow::Result<Output> {
    let mut args = req.args.clone();
    let mut warnings: Vec<String> = Vec::new();
    // A missing logo is a note, not a failure (a render would refuse the
    // job); music has no picture.
    if args.logo.as_ref().is_some_and(|p| !p.is_file()) {
        warnings.push("logo file not found: shown without".into());
        args.logo = None;
    }
    args.music = None;
    let canvas = args.canvas();
    let look = crate::pipeline::look_for(&args)?;
    // A font the Look names that is not installed: said, and the default used.
    if let Some(raw) = args.look.as_deref() {
        warnings.extend(crate::look::Look::font_notes(raw));
    }
    // libass reads the fonts folder: the bundled fonts must be in it.
    crate::provision::ensure_fonts()?;

    let ffmpeg = crate::binaries::require("ffmpeg")?;
    if !req.source.is_file() {
        anyhow::bail!("source video not found: {}", req.source.display());
    }

    // --- layout, captions + headline (ASS): nothing here needs the video -----------
    let wants_split = args.layout == crate::split::Mode::Split;
    let layout = if wants_split && crate::split::fits(canvas) {
        Layout::Split
    } else if req.fill {
        Layout::Fill
    } else {
        Layout::Single
    };
    if wants_split && layout != Layout::Split {
        warnings.push("split needs a tall canvas: shown as one camera".into());
    }
    let all_words = std::fs::read(&req.transcript)
        .ok()
        .and_then(|b| serde_json::from_slice::<crate::whisper::Transcription>(&b).ok())
        .map(|t| t.words)
        .unwrap_or_default();
    let words = window_words(&all_words, req.start_s, req.len_s);
    let style = args.style.clone().unwrap_or_else(|| "karaoke".into());
    let ass_text = crate::captions::ass::build_for(
        &words,
        &style,
        0.0,
        &crate::pipeline::ass_opts(
            &look,
            req.headline.clone(),
            req.len_s,
            layout == Layout::Split,
        ),
    );
    std::fs::create_dir_all(&req.dir)?;
    let ass = req.dir.join("preview.ass");
    std::fs::write(&ass, ass_text)?;

    // --- the encoder: logo, captions, scale, JPEG ---------------------------------
    let (ow, oh) = out_size(canvas);
    let file = file_name(req.seq);
    let out = req.dir.join(&file);
    let mut graph = String::from(
        "[0:v]setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv",
    );
    let logo = look.logo.as_ref().filter(|l| l.path.is_file());
    if let Some(l) = logo {
        graph.push_str(&crate::render::logo_chain(1, l, canvas));
    }
    // The single frame sits at the playhead, so libass draws the line (and
    // the animation state) of that moment.
    graph.push_str(&format!(
        ",setpts=PTS+{:.4}/TB,ass={}:fontsdir={},\
         scale={ow}:{oh}:flags=lanczos:in_color_matrix=bt709:in_range=tv:out_color_matrix=bt601:out_range=pc,\
         format=yuvj420p[v]",
        req.t,
        crate::render::filter_escape(&ass),
        crate::render::filter_escape(&crate::render::fonts_dir()),
    ));
    let mut cmd = crate::process::command(&ffmpeg);
    cmd.args(["-nostdin", "-hide_banner", "-v", "error", "-y"])
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-s"])
        .arg(format!("{}x{}", canvas.w, canvas.h))
        .args(["-framerate", "30", "-i", "pipe:0"]);
    if let Some(l) = logo {
        cmd.arg("-i").arg(&l.path);
    }
    let mut child = cmd
        .arg("-filter_complex")
        .arg(&graph)
        .args(["-map", "[v]", "-frames:v", "1", "-fps_mode", "passthrough"])
        .args(["-q:v", "2"])
        .arg(&out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take();

    // --- decode one frame and compose it (the encoder is starting meanwhile) -------
    let made = (|| -> anyhow::Result<(Vec<u8>, f64)> {
        let probe = probe_cached(&req.source);
        let fps = probe.render_fps();
        let rate = crate::render::rate(fps);
        // The moment on the source clock, held inside the video.
        let mut src_t = req.start_s + req.t;
        if let Some(d) = probe.duration_s.filter(|d| *d > 0.0) {
            let last = (d - 4.0 / rate).max(0.0);
            if src_t > last {
                src_t = last;
                warnings.push("past the end of the video: showing its last frame".into());
            }
        }
        let dg = crate::render::decode_geom(&probe, fps);
        let src_g = crate::compose::Geom { w: dg.w, h: dg.h };
        // A quarter frame early, as the render's decoder does: the frame on
        // the grid must not be lost to float rounding. A seek that finds no
        // frame (the very end of a file) tries again a little earlier.
        let mut decoded = None;
        let mut last_err = String::new();
        for back in [0.0, 1.5] {
            let ss = (src_t - back - 0.25 / rate).max(0.0);
            let dec = crate::process::command(&ffmpeg)
                .args(["-nostdin", "-hide_banner", "-v", "error", "-ss"])
                .arg(format!("{ss:.6}"))
                .arg("-i")
                .arg(&req.source)
                .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-vf", &dg.vf])
                .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "yuv420p"])
                .arg("pipe:1")
                .stdin(Stdio::null())
                .output()?;
            if dec.stdout.len() >= src_g.frame_len() {
                if back > 0.0 {
                    warnings.push("end of the video: showing a frame just before it".into());
                }
                decoded = Some(dec.stdout);
                break;
            }
            last_err = stderr_note(&dec.stderr);
        }
        let Some(frame) = decoded else {
            anyhow::bail!("no frame at {src_t:.2}s in the source{last_err}");
        };
        let (sw, sh) = (
            probe.width.unwrap_or(dg.w) as f64,
            probe.height.unwrap_or(dg.h) as f64,
        );
        let scaled = |r: Rect| Rect {
            x: r.x * dg.sx,
            y: r.y * dg.sy,
            w: r.w * dg.sx,
            h: r.h * dg.sy,
        };
        let progress = (req.t / req.len_s) as f32;
        let mut comp = Compositor::new(dg.w, dg.h, canvas)
            .with_bar(look.bar)
            .with_bar_look(look.bar_look.as_ref())
            .with_effects(look.effects.as_ref())
            .with_split(look.layout.as_ref());
        let frame = &frame[..src_g.frame_len()];
        let composed = match layout {
            Layout::Split => {
                let seam = look.layout.as_ref().and_then(|l| l.split).unwrap_or(0.5);
                let (top, bottom) = assumed_pair(sw, sh, canvas, seam)
                    .ok_or_else(|| anyhow::anyhow!("no split crops for this frame"))?;
                comp.compose_split(frame, scaled(top), scaled(bottom), 0.0, progress)?
            }
            Layout::Fill => {
                let whole = Rect {
                    x: 0.0,
                    y: 0.0,
                    w: sw,
                    h: sh,
                };
                comp.compose(frame, scaled(whole), 0.0, progress)?
            }
            Layout::Single => {
                comp.compose(frame, scaled(canvas.base_rect(sw, sh)), 0.0, progress)?
            }
        };
        Ok((composed.to_vec(), src_t))
    })();
    let (raw, src_t) = match made {
        Ok(m) => m,
        Err(e) => {
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };

    // Written from a thread: the encoder may answer (or fail) before it has
    // read the whole frame.
    let mut stdin = stdin.ok_or_else(|| anyhow::anyhow!("encoder stdin missing"))?;
    let writer = std::thread::spawn(move || stdin.write_all(&raw));
    let res = child.wait_with_output()?;
    let _ = writer.join();
    if !res.status.success() || !out.is_file() {
        let _ = std::fs::remove_file(&out);
        anyhow::bail!("could not write the still{}", stderr_note(&res.stderr));
    }
    Ok(Output {
        file,
        width: ow,
        height: oh,
        t: req.t,
        src_t,
        words: words.len(),
        layout,
        warnings,
    })
}

/// `: <last ffmpeg lines>` for an error message, or nothing.
fn stderr_note(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let t = text.trim();
    if t.is_empty() {
        return String::new();
    }
    let tail: String = t
        .chars()
        .rev()
        .take(300)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!(": {tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t_is_clamped_into_the_window() {
        assert_eq!(window(10.0, 12.0, 5.0), Ok((10.0, 12.0, 5.0)));
        assert_eq!(window(10.0, 12.0, -3.0), Ok((10.0, 12.0, 0.0)));
        assert_eq!(window(10.0, 12.0, 99.0), Ok((10.0, 12.0, 12.0)));
        assert_eq!(window(10.0, 12.0, f64::NAN), Ok((10.0, 12.0, 0.0)));
        assert_eq!(window(-4.0, 12.0, 1.0), Ok((0.0, 12.0, 1.0)));
        assert_eq!(window(f64::INFINITY, 12.0, 1.0), Ok((0.0, 12.0, 1.0)));
        assert_eq!(window(0.0, 1e9, 5000.0), Ok((0.0, MAX_LEN_S, MAX_LEN_S)));
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(window(0.0, bad, 0.0).is_err(), "{bad}");
        }
    }

    #[test]
    fn previews_rotate_through_a_few_names_that_are_not_clip_files() {
        let names: Vec<String> = (0..40).map(file_name).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len() as u64, RING);
        // Neighbours differ (the app can tell a new picture from the last).
        for w in names.windows(2) {
            assert_ne!(w[0], w[1]);
        }
        for n in &names {
            assert!(n.starts_with("preview-") && n.ends_with(".jpg"), "{n}");
            assert!(!n.starts_with("clip-") && n != "poster.jpg", "{n}");
            assert!(!n.contains('/') && !n.contains('\\'));
        }
    }

    #[test]
    fn headline_falls_back_option_then_first_clip_then_job() {
        // Off while the options have none.
        assert_eq!(headline_text(None, Some("Clip"), "Job"), None);
        // Text given.
        assert_eq!(
            headline_text(Some("  My line "), Some("Clip"), "Job").as_deref(),
            Some("My line")
        );
        // Bare: the first clip's title, else the job's.
        assert_eq!(
            headline_text(Some(""), Some("Clip one"), "Job").as_deref(),
            Some("Clip one")
        );
        assert_eq!(
            headline_text(Some("  "), Some("  "), "Job").as_deref(),
            Some("Job")
        );
        assert_eq!(headline_text(Some(""), None, "Job").as_deref(), Some("Job"));
        assert_eq!(headline_text(Some(""), None, " "), None);
    }

    fn w(s: &str, a: f64, b: f64) -> Word {
        Word {
            w: s.into(),
            s: a,
            e: b,
            conf: None,
        }
    }

    #[test]
    fn the_window_words_start_at_zero() {
        let words = vec![
            w("before", 98.0, 98.4),
            w("one", 100.5, 100.9),
            w("two", 105.0, 105.4),
            w("after", 112.2, 112.6),
        ];
        let got = window_words(&words, 100.0, 12.0);
        let texts: Vec<&str> = got.iter().map(|x| x.w.as_str()).collect();
        assert_eq!(texts, ["one", "two"]);
        assert!((got[0].s - 0.5).abs() < 1e-9 && (got[1].e - 5.4).abs() < 1e-9);
    }

    #[test]
    fn jpeg_is_720_on_the_long_side_and_never_upscaled() {
        assert_eq!(out_size(Canvas::TALL), (406, 720));
        assert_eq!(out_size(Canvas::PORTRAIT), (576, 720));
        assert_eq!(out_size(Canvas::SQUARE), (720, 720));
        assert_eq!(out_size(Canvas::WIDE), (720, 406));
        let small = Canvas { w: 400, h: 300 };
        assert_eq!(out_size(small), (400, 300));
    }

    #[test]
    fn only_fill_like_layouts_letterbox() {
        for l in ["fill", "Fill", " letterbox ", "wide"] {
            assert!(wants_fill(Some(l)), "{l}");
        }
        for l in [None, Some("auto"), Some("single"), Some("split"), Some("")] {
            assert!(!wants_fill(l), "{l:?}");
        }
    }

    #[test]
    fn a_split_without_a_tracker_assumes_one_person_per_side() {
        let (top, bottom) = assumed_pair(1920.0, 1080.0, Canvas::TALL, 0.6).unwrap();
        assert!(top.cx() < 960.0 && bottom.cx() > 960.0);
        for r in [top, bottom] {
            assert!(r.x >= 0.0 && r.x + r.w <= 1920.0 + 1e-9);
            assert!(r.y >= 0.0 && r.y + r.h <= 1080.0 + 1e-9);
        }
    }
}
