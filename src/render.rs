//! Render engine: kept source spans + a per-frame camera path → one
//! 1080x1920 captioned MP4, in a single pass.
//!
//! ```text
//!  ffmpeg decode ──yuv420p──▶ compositor (Rust) ──yuv420p──▶ ffmpeg encode
//!  (one per span,              f64 crop, SIMD resize,        (ass burn-in,
//!   CFR via fps filter)        blur fill, flash)              audio graph)
//! ```
//!
//! Why not an ffmpeg filtergraph: `crop` only takes whole (even) pixels and
//! `scale` rebuilds itself whenever the crop size changes, so pans stepped
//! several output pixels at a time and zooms flickered bars; `sendcmd`
//! schedules had to be regenerated per keep and the keeps concatenated.
//! Here every output frame gets its exact fractional camera rect, all keeps
//! of a clip decode back to back into ONE encoder (no segment files, no
//! concat, no re-encode), and the three stages run concurrently on bounded
//! queues, so memory stays at a handful of frames.
//!
//! A/V sync is by construction: every span renders a whole number of
//! frames `n` and exactly `n / fps` seconds of audio (sample-exact `atrim`
//! on a sample-count clock), so the two can never drift, however many
//! jump cuts a clip has. Joins get 8 ms fades (no clicks, no gap you can
//! hear), clip edges a soft in/out. Loudness is two-pass: measure the
//! assembled audio, then one linear gain to -14 LUFS with a true-peak-safe
//! limiter — no pumping.
//!
//! Windows notes: every path inside a filter goes through
//! [`filter_escape`] (backslash → slash, quotes, escaped `':,`), which is
//! mandatory for `C:\...` drive colons.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc;

use crate::camera::Pose;
use crate::compose::{Canvas, Compositor, Geom, Rect};
use crate::ffmpeg::Probe;
use crate::progress::{CancelFlag, SharedPct};

pub fn threads() -> usize {
    crate::whisper::cpu_count().saturating_sub(2).max(1)
}

/// Escape a path for use inside an ffmpeg -vf filter argument.
///
/// Two parsing levels see it: the option value (`\:`, `\,`, `\'` escapes)
/// and then the filtergraph, where it sits in single quotes. Nothing can be
/// escaped inside graph-level quotes, so a quote is written close-quote,
/// escaped quote, reopen (`'\''`) — e.g. `C:\Users\O'Brien`.
pub fn filter_escape(path: &Path) -> String {
    let s = path.display().to_string().replace('\\', "/");
    let option_level = s
        .replace('\'', "\\'")
        .replace(':', "\\:")
        .replace(',', "\\,");
    format!("'{}'", option_level.replace('\'', "'\\''"))
}

/// Where libass finds the caption fonts.
pub fn fonts_dir() -> PathBuf {
    // Provisioned first (single-exe installs), then alongside the exe,
    // then the repo checkout (cargo run).
    let prov = crate::provision::fonts_dir();
    if prov.is_dir() {
        return prov;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            let p = d.join("resources").join("fonts");
            if p.is_dir() {
                return p;
            }
        }
    }
    PathBuf::from("resources/fonts")
}

fn encoder_available(ffmpeg: &Path, name: &str) -> bool {
    let out = crate::process::command(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output();
    let Ok(out) = out else { return false };
    let list = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    list.contains(name)
}

/// Encoder pick follows the master GPU switch (see `GpuMode`): VideoToolbox
/// on macOS, NVENC on Windows/Linux when actually present in the ffmpeg
/// build (plus an NVIDIA GPU for NVENC), else libx264. Never fails. The
/// probe (two child processes) runs once per process and mode.
pub fn pick_encoder(gpu: bool) -> String {
    static CACHE: std::sync::Mutex<[Option<String>; 2]> = std::sync::Mutex::new([None, None]);
    let slot = gpu as usize;
    if let Some(e) = CACHE.lock().ok().and_then(|c| c[slot].clone()) {
        return e;
    }
    let picked = probe_encoder(gpu);
    if let Ok(mut c) = CACHE.lock() {
        c[slot] = Some(picked.clone());
    }
    picked
}

fn probe_encoder(gpu: bool) -> String {
    if gpu {
        if let Some(ffmpeg) = crate::binaries::resolve("ffmpeg") {
            if cfg!(target_os = "macos") {
                if encoder_available(&ffmpeg, "h264_videotoolbox") {
                    return "h264_videotoolbox".into();
                }
                tracing::warn!("GPU mode on but VideoToolbox unavailable: CPU render fallback");
            } else if encoder_available(&ffmpeg, "h264_nvenc") && has_nvidia() {
                return "h264_nvenc".into();
            } else {
                tracing::warn!("GPU mode on but NVENC unavailable: CPU render fallback");
            }
        }
    }
    "libx264".into()
}

fn has_nvidia() -> bool {
    crate::process::command("nvidia-smi")
        .args(["-L"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Quality encode flags. NVENC runs constant-quality VBR with no bitrate
/// cap (`-b:v 0`; without it the 2 Mbit/s default starves 1080x1920) and
/// spatial AQ; x264 `veryfast` CRF 20; VideoToolbox (no CRF) a generous
/// fixed bitrate.
fn video_codec_args(encoder: &str, threads: usize, fps: (u32, u32)) -> Vec<String> {
    let gop = ((fps.0 as f64 / fps.1 as f64) * 2.0).round().max(1.0) as u32;
    let mut v: Vec<String> = match encoder {
        "h264_nvenc" => [
            "-c:v",
            "h264_nvenc",
            "-preset",
            "p5",
            "-tune",
            "hq",
            "-rc",
            "vbr",
            "-cq",
            "21",
            "-b:v",
            "0",
            "-spatial-aq",
            "1",
            "-profile:v",
            "high",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        "h264_videotoolbox" => [
            "-c:v",
            "h264_videotoolbox",
            "-b:v",
            "12M",
            "-maxrate",
            "16M",
            "-profile:v",
            "high",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        other => {
            let mut v: Vec<String> = vec!["-c:v".into(), other.to_string()];
            if other == "libx264" {
                v.extend(
                    ["-preset", "veryfast", "-crf", "20", "-profile:v", "high"]
                        .iter()
                        .map(|s| s.to_string()),
                );
                v.push("-threads".into());
                v.push(threads.to_string());
            }
            v
        }
    };
    v.extend([
        "-g".to_string(),
        gop.to_string(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-color_primaries".into(),
        "bt709".into(),
        "-color_trc".into(),
        "bt709".into(),
        "-colorspace".into(),
        "bt709".into(),
        "-color_range".into(),
        "tv".into(),
    ]);
    v
}

/// Output rate as f64.
pub fn rate(fps: (u32, u32)) -> f64 {
    fps.0 as f64 / fps.1.max(1) as f64
}

/// One kept source span: `frames` output frames starting at source time
/// `a`. `part` numbers merge parts (a new part = a different moment of
/// the video; joins between parts get longer audio fades).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub a: f64,
    pub frames: usize,
    pub part: usize,
}

impl Span {
    pub fn secs(&self, fps: (u32, u32)) -> f64 {
        self.frames as f64 / rate(fps)
    }
}

/// Frame-count each kept `(a, b, part)` range on one output clock:
/// cumulative rounding, so the total is exact and no span drifts.
/// Callers snap keeps to the frame grid first (see
/// [`crate::timeline::snap_keeps`]), which makes this exact per span too.
pub fn spans_for(keeps: &[(f64, f64, usize)], fps: (u32, u32)) -> Vec<Span> {
    let r = rate(fps);
    let mut out = Vec::with_capacity(keeps.len());
    let mut acc = 0.0f64;
    let mut done = 0usize;
    for &(a, b, part) in keeps {
        acc += (b - a).max(0.0);
        let end = (acc * r).round() as usize;
        let n = end.saturating_sub(done);
        if n > 0 {
            out.push(Span { a, frames: n, part });
        }
        done = end.max(done);
    }
    out
}

/// Where a logo sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    /// `tl`, `tr`, `bl`, `br`.
    pub fn parse(s: &str) -> Option<Corner> {
        match s.trim().to_ascii_lowercase().as_str() {
            "tl" => Some(Corner::TopLeft),
            "tr" => Some(Corner::TopRight),
            "bl" => Some(Corner::BottomLeft),
            "br" => Some(Corner::BottomRight),
            _ => None,
        }
    }

    pub fn is_top(self) -> bool {
        matches!(self, Corner::TopLeft | Corner::TopRight)
    }

    pub fn is_left(self) -> bool {
        matches!(self, Corner::TopLeft | Corner::BottomLeft)
    }
}

/// A corner logo.
#[derive(Debug, Clone)]
pub struct Logo {
    pub path: PathBuf,
    pub corner: Corner,
    /// Image width / height (1.0 when unknown).
    pub aspect: f64,
}

impl Logo {
    /// A logo from an image file; its shape is read from the header.
    pub fn new(path: PathBuf, corner: Corner) -> Logo {
        let aspect = image_size(&path)
            .map(|(w, h)| w as f64 / h.max(1) as f64)
            .filter(|a| a.is_finite() && *a > 0.0)
            .unwrap_or(1.0);
        Logo {
            path,
            corner,
            aspect,
        }
    }

    /// Drawn size (even px): fits a box 14% of the short canvas side tall
    /// and 26% wide, so marks and wide wordmarks read at the same weight.
    pub fn size(&self, c: Canvas) -> (u32, u32) {
        let s = c.w.min(c.h) as f64;
        let (bw, bh) = (s * 0.26, s * 0.14);
        let w = bw.min(bh * self.aspect);
        let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(2);
        (even(w), even(w / self.aspect))
    }

    /// Corner insets (x, y) in px.
    pub fn inset(c: Canvas) -> (u32, u32) {
        (
            (c.w as f64 * 0.04).round() as u32,
            (c.h as f64 * 0.035).round() as u32,
        )
    }
}

/// Pixel size of a PNG or JPEG from its header (no decoder needed).
pub fn image_size(p: &Path) -> Option<(u32, u32)> {
    let b = std::fs::read(p).ok()?;
    let be16 =
        |i: usize| -> Option<u32> { Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32) };
    if b.len() >= 24 && b.starts_with(b"\x89PNG\r\n\x1a\n") {
        let w = u32::from_be_bytes(b[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(b[20..24].try_into().ok()?);
        return (w > 0 && h > 0).then_some((w, h));
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xFF {
                return None;
            }
            let m = b[i + 1];
            // SOF0..SOF15 carry the size (C4/C8/CC are other tables).
            if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
                let (h, w) = (be16(i + 5)?, be16(i + 7)?);
                return (w > 0 && h > 0).then_some((w, h));
            }
            i += 2 + be16(i + 2)? as usize;
        }
    }
    None
}

/// Output canvas and overlays shared by every render of a run.
#[derive(Debug, Clone, Default)]
pub struct Look {
    pub canvas: Canvas,
    /// Progress bar color (sRGB); `None` = off.
    pub bar: Option<(u8, u8, u8)>,
    /// Corner logo.
    pub logo: Option<Logo>,
    /// Background music and its bed level (dB relative to the speech).
    pub music: Option<(PathBuf, f64)>,
    /// Caption motion.
    pub anim: crate::captions::ass::Anim,
    /// The Look's captions section (position, size, colours, ...).
    pub captions: Option<crate::look::CaptionsLook>,
}

/// Everything one render needs.
pub struct Job<'a> {
    pub source: &'a Path,
    pub probe: &'a Probe,
    pub fps: (u32, u32),
    pub spans: &'a [Span],
    /// One camera pose per output frame (source px).
    pub poses: &'a [Pose],
    /// Split-screen crops per output frame (top, bottom; source px). When
    /// set they replace `poses`; empty = one camera.
    pub split: &'a [(Rect, Rect)],
    /// Per-frame white flash 0..1 (merge joins); empty = none.
    pub flash: &'a [f32],
    /// Captions to burn (output clock), if any.
    pub ass: Option<&'a Path>,
    pub out: &'a Path,
    pub gpu: bool,
    pub threads: usize,
    pub label: &'a str,
    /// Canvas + overlays (default: 9:16, none).
    pub look: &'a Look,
}

impl Job<'_> {
    pub fn total_frames(&self) -> usize {
        self.spans.iter().map(|s| s.frames).sum()
    }
}

/// Render a job. Returns the encoder used. NVENC failures (session limits
/// on consumer cards, driver hiccups) retry once on libx264.
pub fn render(
    job: &Job,
    progress: Option<SharedPct>,
    cancel: &CancelFlag,
) -> anyhow::Result<String> {
    if job.total_frames() == 0 {
        anyhow::bail!("nothing to render ({})", job.label);
    }
    if let Some(p) = job.out.parent() {
        std::fs::create_dir_all(p)?;
    }
    let ffmpeg = crate::binaries::require("ffmpeg")?;
    let audio = AudioPlan::new(job.spans, job.fps, job.probe.has_audio);
    let gain = if audio.inputs.is_empty() {
        Loudness::Silent
    } else {
        match measure_loudness(&ffmpeg, job.source, &audio, cancel) {
            Ok(l) => l,
            Err(e) => {
                cancel.check()?;
                tracing::warn!("loudness measure failed ({e}): single-pass loudnorm");
                Loudness::Dynamic
            }
        }
    };
    let encoder = pick_encoder(job.gpu);
    match run(
        job,
        &ffmpeg,
        &audio,
        &gain,
        &encoder,
        progress.clone(),
        cancel,
    ) {
        Ok(()) => Ok(encoder),
        Err(e) if encoder != "libx264" && !cancel.is_cancelled() => {
            tracing::warn!("{encoder} render failed ({e:#}): retrying on libx264");
            let _ = std::fs::remove_file(job.out);
            run(job, &ffmpeg, &audio, &gain, "libx264", progress, cancel)?;
            Ok("libx264".into())
        }
        Err(e) => {
            let _ = std::fs::remove_file(job.out);
            Err(e)
        }
    }
}

/// Decoded-frame geometry and the decoder's filter chain.
struct DecodeGeom {
    w: u32,
    h: u32,
    sx: f64,
    sy: f64,
    vf: String,
}

/// Decode at source size (capped at 2160 on the short side — 8K sources
/// would only burn bandwidth), as limited-range BT.709 4:2:0 on a CFR grid.
fn decode_geom(probe: &Probe, fps: (u32, u32)) -> DecodeGeom {
    let sw = probe.width.unwrap_or(1280).max(2);
    let sh = probe.height.unwrap_or(720).max(2);
    let s = (2160.0 / sw.min(sh) as f64).min(1.0);
    let even = |v: f64| (((v / 2.0).round() as u32) * 2).max(2);
    let (w, h) = (even(sw as f64 * s), even(sh as f64 * s));
    let mut vf = format!(
        "fps={}/{},scale={w}:{h}:flags=bicubic:out_range=tv,format=yuv420p",
        fps.0, fps.1
    );
    if probe.is_bt601() {
        // Composited in YUV and tagged BT.709: SD matrices must convert or
        // skin tones shift.
        vf.push_str(",colorspace=all=bt709:iall=bt601-6-625:fast=1:format=yuv420p");
    }
    DecodeGeom {
        w,
        h,
        sx: w as f64 / sw as f64,
        sy: h as f64 / sh as f64,
        vf,
    }
}

/// Audio of the whole job on a sample-count clock.
struct AudioPlan {
    /// One source input per stretch of nearby spans (empty: no audio).
    inputs: Vec<AudioInput>,
    /// Spans in output order.
    n_spans: usize,
    total_samples: u64,
}

/// One `-ss/-t -i source` input and the spans cut from it.
struct AudioInput {
    ss: f64,
    dur: f64,
    /// `(span index, start sample in this input, samples, fade-in, fade-out)`.
    cuts: Vec<(usize, u64, u64, u64, u64)>,
}

const SR: f64 = 48000.0;
/// Fade at jump-cut joins (s): long enough to kill the click, short enough
/// that no gap is heard.
const JOIN_FADE: f64 = 0.008;
/// Fade at merge-part joins (s).
const PART_FADE: f64 = 0.03;
/// Clip head fade-in (s).
const HEAD_FADE: f64 = 0.015;
/// Clip tail fade-out (s): the room tone lands instead of stopping dead.
const TAIL_FADE: f64 = 0.12;

impl AudioPlan {
    fn new(spans: &[Span], fps: (u32, u32), has_audio: bool) -> Self {
        let r = rate(fps);
        // Output sample boundaries from cumulative frames (exact).
        let mut bounds = vec![0u64];
        let mut frames = 0usize;
        for s in spans {
            frames += s.frames;
            bounds.push((frames as f64 * SR / r).round() as u64);
        }
        let total_samples = *bounds.last().unwrap_or(&0);
        if !has_audio || spans.is_empty() {
            return Self {
                inputs: vec![],
                n_spans: spans.len(),
                total_samples,
            };
        }
        // Spans that sit close together in the source share one input
        // (one seek + decode per stretch, not per keep).
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (i, s) in spans.iter().enumerate() {
            let fits = groups.last().is_some_and(|g| {
                let first = &spans[g[0]];
                let last = &spans[*g.last().unwrap()];
                let last_end = last.a + last.secs(fps);
                s.a >= last_end - 1e-3
                    && s.a - last_end < 20.0
                    && s.a + s.secs(fps) - first.a < 600.0
            });
            if fits {
                groups.last_mut().unwrap().push(i);
            } else {
                groups.push(vec![i]);
            }
        }
        let fade = |a: f64, n: u64| ((a * SR) as u64).min(n / 3);
        let inputs = groups
            .iter()
            .map(|g| {
                let first = &spans[g[0]];
                let last = &spans[*g.last().unwrap()];
                let ss = (first.a - 0.2).max(0.0);
                let dur = last.a + last.secs(fps) - ss + 0.5;
                let cuts = g
                    .iter()
                    .map(|&j| {
                        let s = &spans[j];
                        let n = bounds[j + 1] - bounds[j];
                        let s0 = ((s.a - ss) * SR).round().max(0.0) as u64;
                        let fin = if j == 0 {
                            HEAD_FADE
                        } else if spans[j - 1].part != s.part {
                            PART_FADE
                        } else {
                            JOIN_FADE
                        };
                        let fout = if j + 1 == spans.len() {
                            TAIL_FADE
                        } else if spans[j + 1].part != s.part {
                            PART_FADE
                        } else {
                            JOIN_FADE
                        };
                        (j, s0, n, fade(fin, n), fade(fout, n))
                    })
                    .collect();
                AudioInput { ss, dur, cuts }
            })
            .collect();
        Self {
            inputs,
            n_spans: spans.len(),
            total_samples,
        }
    }

    /// `-ss/-t/-i` args for every audio input.
    fn input_args(&self, source: &Path) -> Vec<String> {
        let mut v = Vec::new();
        for i in &self.inputs {
            v.extend([
                "-ss".to_string(),
                format!("{:.6}", i.ss),
                "-t".into(),
                format!("{:.6}", i.dur),
                "-i".into(),
                source.display().to_string(),
            ]);
        }
        v
    }

    /// Filter chains from the audio inputs (numbered from `first`) to
    /// `[cat]`: sample-count clock, exact per-span trims (padded if the
    /// source runs short, so later spans never shift), short fades at every
    /// join, then one concat.
    fn graph(&self, first: usize) -> String {
        if self.inputs.is_empty() {
            return format!(
                "anullsrc=r=48000:cl=stereo,atrim=end_sample={}[cat]",
                self.total_samples
            );
        }
        let mut g = String::new();
        for (ii, inp) in self.inputs.iter().enumerate() {
            g.push_str(&format!(
                "[{}:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,asetpts=N/SR/TB,asplit={}",
                first + ii,
                inp.cuts.len()
            ));
            for c in &inp.cuts {
                g.push_str(&format!("[g{}]", c.0));
            }
            g.push(';');
            for &(j, s0, n, fi, fo) in &inp.cuts {
                g.push_str(&format!(
                    "[g{j}]atrim=start_sample={s0}:end_sample={},asetpts=PTS-STARTPTS,apad=whole_len={n},atrim=end_sample={n},afade=t=in:ss=0:ns={fi},afade=t=out:ss={}:ns={fo}[s{j}];",
                    s0 + n,
                    n - fo
                ));
            }
        }
        for j in 0..self.n_spans {
            g.push_str(&format!("[s{j}]"));
        }
        g.push_str(&format!("concat=n={}:v=0:a=1[cat]", self.n_spans));
        g
    }

    /// Final chain `[cat]` → `[a]`: gain, 48 kHz, exact total length.
    /// With `music` (input index, bed level dB): the bed is normalized to
    /// the level below the speech target, looped to length, faded in and
    /// out, ducked by the speech (sidechain), then mixed under it; one
    /// limiter guards the sum.
    fn finish(&self, loud: &Loudness, music: Option<(usize, f64)>) -> String {
        let n = self.total_samples;
        let Some((mi, db)) = music else {
            return format!(
                "[cat]{}aresample=48000,apad=whole_len={n},atrim=end_sample={n}[a]",
                loud.chain()
            );
        };
        let fin = ((0.4 * SR) as u64).min(n / 4);
        let fout = ((1.5 * SR) as u64).min(n / 3);
        let bed = (TARGET_LUFS + db.clamp(-40.0, 0.0)).max(-70.0);
        format!(
            "[cat]{voice}asplit=2[vo][vk];\
             [{mi}:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,\
             loudnorm=I={bed:.1}:TP=-6:LRA=11,aresample=48000,asetpts=N/SR/TB,\
             atrim=end_sample={n},afade=t=in:ss=0:ns={fin},afade=t=out:ss={fo}:ns={fout}[mb];\
             [mb][vk]sidechaincompress=threshold=0.03:ratio=2.5:attack=25:release=450:makeup=1[md];\
             [vo][md]amix=inputs=2:duration=first:dropout_transition=0:normalize=0,\
             alimiter=limit={LIMIT}:attack=5:release=60:level=0,\
             aresample=48000,apad=whole_len={n},atrim=end_sample={n}[a]",
            voice = loud.voice(),
            fo = n.saturating_sub(fout),
        )
    }
}

/// Gain decision from the measuring pass.
#[derive(Debug, Clone)]
enum Loudness {
    /// Linear gain (dB) + limiter.
    Linear(f64),
    /// Measurement failed: classic single-pass loudnorm.
    Dynamic,
    /// No audio stream.
    Silent,
}

const TARGET_LUFS: f64 = -14.0;
/// Limiter ceiling (linear, ≈ -2 dBFS: headroom for AAC inter-sample peaks).
const LIMIT: f64 = 0.794;

impl Loudness {
    /// Speech gain alone (the limiter comes after a music mix).
    fn voice(&self) -> String {
        match self {
            Loudness::Linear(g) => format!("volume={g:.2}dB,"),
            Loudness::Dynamic => "loudnorm=I=-14:TP=-1.5:LRA=11,aresample=48000,".into(),
            Loudness::Silent => String::new(),
        }
    }

    fn chain(&self) -> String {
        match self {
            Loudness::Linear(g) => {
                format!("volume={g:.2}dB,alimiter=limit={LIMIT}:attack=5:release=60:level=0,")
            }
            Loudness::Dynamic => "loudnorm=I=-14:TP=-1.5:LRA=11,".into(),
            Loudness::Silent => String::new(),
        }
    }
}

/// Pass 1: integrated loudness of the assembled audio (exactly what the
/// render will play, joins and fades included).
fn measure_loudness(
    ffmpeg: &Path,
    source: &Path,
    audio: &AudioPlan,
    cancel: &CancelFlag,
) -> anyhow::Result<Loudness> {
    let child = crate::process::command(ffmpeg)
        .args(["-nostdin", "-hide_banner", "-nostats"])
        .args(audio.input_args(source))
        .arg("-filter_complex")
        .arg(format!(
            "{};[cat]loudnorm=I={TARGET_LUFS}:TP=-1.5:LRA=11:print_format=json[m]",
            audio.graph(0)
        ))
        .args(["-map", "[m]", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    crate::process::gentle(&child);
    let (status, err) = wait_cancellable(child, cancel)?;
    if !status.success() {
        anyhow::bail!("measure pass failed: {}", tail(&err, 400));
    }
    let (Some(a), Some(b)) = (err.rfind('{'), err.rfind('}')) else {
        anyhow::bail!("no loudnorm report");
    };
    let v: serde_json::Value = serde_json::from_str(&err[a..=b])?;
    let input_i: f64 = v
        .get("input_i")
        .and_then(|x| x.as_str())
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| anyhow::anyhow!("no input_i"))?;
    if !input_i.is_finite() || input_i < -70.0 {
        return Ok(Loudness::Linear(0.0)); // silence: leave it alone
    }
    let gain = (TARGET_LUFS - input_i).clamp(-20.0, 24.0);
    tracing::info!("loudness {input_i:.1} LUFS -> gain {gain:+.1} dB");
    Ok(Loudness::Linear(gain))
}

/// The render pass proper: decoder thread → compositor (this thread) →
/// writer thread → encoder, on bounded queues.
fn run(
    job: &Job,
    ffmpeg: &Path,
    audio: &AudioPlan,
    loud: &Loudness,
    encoder: &str,
    progress: Option<SharedPct>,
    cancel: &CancelFlag,
) -> anyhow::Result<()> {
    let t_start = std::time::Instant::now();
    let total = job.total_frames();
    let dg = decode_geom(job.probe, job.fps);
    let src_g = Geom { w: dg.w, h: dg.h };
    let canvas = job.look.canvas;
    let out_g = Geom {
        w: canvas.w,
        h: canvas.h,
    };

    // --- encoder ---------------------------------------------------------
    // Inputs: 0 = composed frames, then the audio stretches, then the logo
    // and music (when set).
    let mut next_input = 1 + audio.inputs.len();
    let logo = job.look.logo.as_ref().filter(|l| l.path.is_file());
    let logo_idx = logo.map(|_| {
        next_input += 1;
        next_input - 1
    });
    let music = job.look.music.as_ref().filter(|(p, _)| p.is_file());
    let music_idx = music.map(|(_, db)| {
        next_input += 1;
        (next_input - 1, *db)
    });
    let mut vchain = String::from(
        "[0:v]setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv",
    );
    if let (Some(l), Some(li)) = (logo, logo_idx) {
        vchain.push_str(&logo_chain(li, l, canvas));
    }
    if let Some(ass) = job.ass {
        vchain.push_str(&format!(
            ",ass={}:fontsdir={}",
            filter_escape(ass),
            filter_escape(&fonts_dir())
        ));
    }
    vchain.push_str("[v]");
    let graph = format!(
        "{vchain};{};{}",
        audio.graph(1),
        audio.finish(loud, music_idx)
    );
    let mut enc_cmd = crate::process::command(ffmpeg);
    enc_cmd
        .args(["-hide_banner", "-v", "error", "-y"])
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p"])
        .arg("-s")
        .arg(format!("{}x{}", canvas.w, canvas.h))
        .arg("-framerate")
        .arg(format!("{}/{}", job.fps.0, job.fps.1))
        .args(["-i", "pipe:0"])
        .args(audio.input_args(job.source));
    if let Some(l) = logo {
        enc_cmd.arg("-i").arg(&l.path);
    }
    if let Some((p, _)) = music {
        enc_cmd.args(["-stream_loop", "-1", "-i"]).arg(p);
    }
    enc_cmd
        .arg("-filter_complex")
        .arg(&graph)
        .args(["-map", "[v]", "-map", "[a]"])
        .args(video_codec_args(encoder, job.threads, job.fps))
        .args(["-c:a", "aac", "-b:a", "192k", "-ar", "48000", "-ac", "2"])
        .args(["-movflags", "+faststart"])
        .arg(job.out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    tracing::info!(
        "render {} ({:.1}s, {total} frames @ {}/{}, {encoder})",
        job.label,
        total as f64 / rate(job.fps),
        job.fps.0,
        job.fps.1
    );
    let mut enc = enc_cmd.spawn()?;
    crate::process::gentle(&enc);
    let enc_err = enc.stderr.take().map(drain);
    let enc_in = enc
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("encoder stdin missing"))?;

    // --- writer thread -----------------------------------------------------
    let (wtx, wrx) = mpsc::sync_channel::<Vec<u8>>(3);
    let (opool_tx, opool_rx) = mpsc::channel::<Vec<u8>>();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        let mut enc_in = enc_in;
        for buf in wrx {
            enc_in.write_all(&buf)?;
            let _ = opool_tx.send(buf);
        }
        enc_in.flush()?;
        Ok(()) // dropping stdin = EOF for the encoder
    });

    // --- decoder thread ------------------------------------------------------
    let (dtx, drx) = mpsc::sync_channel::<anyhow::Result<Vec<u8>>>(3);
    let (ipool_tx, ipool_rx) = mpsc::channel::<Vec<u8>>();
    let dec_spans: Vec<Span> = job.spans.to_vec();
    let (dec_ff, dec_src) = (ffmpeg.to_path_buf(), job.source.to_path_buf());
    let dec_vf = dg.vf.clone();
    let dec_threads = (job.threads / 2).clamp(1, 8);
    let fps = job.fps;
    let decoder = std::thread::spawn(move || {
        decode_spans(
            &dec_ff,
            &dec_src,
            &dec_spans,
            fps,
            &dec_vf,
            src_g,
            dec_threads,
            dtx,
            ipool_rx,
        )
    });

    // --- compose loop (this thread) ----------------------------------------
    let mut comp = Compositor::new(dg.w, dg.h, canvas).with_bar(job.look.bar);
    let base = canvas.base_rect(
        job.probe.width.unwrap_or(dg.w) as f64,
        job.probe.height.unwrap_or(dg.h) as f64,
    );
    let mut failure: Option<anyhow::Error> = None;
    let mut last_pct = u8::MAX;
    let mut compose_s = 0.0f64;
    for i in 0..total {
        if cancel.is_cancelled() {
            failure = Some(anyhow::anyhow!("cancelled by user"));
            break;
        }
        let frame = match drx.recv() {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => {
                failure = Some(e);
                break;
            }
            Err(_) => {
                failure = Some(anyhow::anyhow!("decoder stopped at frame {i}/{total}"));
                break;
            }
        };
        let rect = job
            .poses
            .get(i)
            .or(job.poses.last())
            .map(|p| p.rect)
            .unwrap_or(base);
        let dec = |r: Rect| Rect {
            x: r.x * dg.sx,
            y: r.y * dg.sy,
            w: r.w * dg.sx,
            h: r.h * dg.sy,
        };
        let flash = job.flash.get(i).copied().unwrap_or(0.0);
        let t0 = std::time::Instant::now();
        let done = (i + 1) as f32 / total as f32;
        let composed = match job.split.get(i).or(job.split.last()) {
            Some(&(top, bottom)) => comp.compose_split(&frame, dec(top), dec(bottom), flash, done),
            None => comp.compose(&frame, dec(rect), flash, done),
        };
        let composed = match composed {
            Ok(c) => c,
            Err(e) => {
                failure = Some(e);
                break;
            }
        };
        let mut ob = opool_rx
            .try_recv()
            .unwrap_or_else(|_| vec![0u8; out_g.frame_len()]);
        ob.copy_from_slice(composed);
        compose_s += t0.elapsed().as_secs_f64();
        let _ = ipool_tx.send(frame);
        if wtx.send(ob).is_err() {
            failure = Some(anyhow::anyhow!("encoder stopped accepting frames"));
            break;
        }
        if let Some(p) = progress.as_deref() {
            let pct = ((i + 1) * 100 / total).min(99) as u8;
            if pct != last_pct {
                last_pct = pct;
                p(pct);
            }
        }
    }
    // Unblock and settle every stage.
    drop(drx);
    drop(ipool_tx);
    drop(wtx);
    if failure.is_some() {
        let _ = enc.kill();
    }
    let dec_res = decoder.join();
    let wr_res = writer.join();
    let status = enc.wait()?;
    let enc_tail = enc_err
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    if let Some(e) = failure {
        if cancel.is_cancelled() {
            anyhow::bail!("cancelled by user");
        }
        if !status.success() && !enc_tail.trim().is_empty() {
            anyhow::bail!("{e:#}; encoder: {}", tail(&enc_tail, 600));
        }
        return Err(e);
    }
    match dec_res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => anyhow::bail!("decoder thread panicked"),
    }
    if let Ok(Err(e)) = wr_res {
        anyhow::bail!(
            "writing frames failed ({e}); encoder: {}",
            tail(&enc_tail, 600)
        );
    }
    if !status.success() {
        anyhow::bail!("encoder failed (exit {status}): {}", tail(&enc_tail, 600));
    }
    if !job.out.is_file() {
        anyhow::bail!("encoder produced no file");
    }
    if let Some(p) = progress.as_deref() {
        p(100);
    }
    let took = t_start.elapsed().as_secs_f64();
    tracing::info!(
        "render {} done: {:.1} fps ({:.1}s; compose {:.1} ms/frame)",
        job.label,
        total as f64 / took.max(1e-6),
        took,
        compose_s * 1000.0 / total.max(1) as f64
    );
    Ok(())
}

/// Logo overlay (input `li`) on the video chain: sized by `Logo::size`,
/// lightly translucent, inset from its corner. Captions burn on top.
fn logo_chain(li: usize, logo: &Logo, c: Canvas) -> String {
    let (lw, lh) = logo.size(c);
    let (mx, my) = Logo::inset(c);
    let x = if logo.corner.is_left() {
        format!("{mx}")
    } else {
        format!("W-w-{mx}")
    };
    // Bottom corners sit higher: clear of the progress bar and the
    // platform's bottom UI.
    let y = if logo.corner.is_top() {
        format!("{my}")
    } else {
        format!("H-h-{}", my * 2)
    };
    format!(
        "[vpre];[{li}:v]format=rgba,scale={lw}:{lh}:flags=lanczos,colorchannelmixer=aa=0.9[logo];\
         [vpre][logo]overlay=x={x}:y={y}:format=auto,format=yuv420p"
    )
}

/// Decoder stage: one ffmpeg per span, frames streamed in output order.
/// A span that decodes short (source ends early, damaged tail) is padded
/// with its last frame so the frame count — and with it A/V sync — holds.
#[allow(clippy::too_many_arguments)]
fn decode_spans(
    ffmpeg: &Path,
    source: &Path,
    spans: &[Span],
    fps: (u32, u32),
    vf: &str,
    g: Geom,
    threads: usize,
    tx: mpsc::SyncSender<anyhow::Result<Vec<u8>>>,
    pool: mpsc::Receiver<Vec<u8>>,
) -> anyhow::Result<()> {
    let flen = g.frame_len();
    let r = rate(fps);
    let fresh =
        |pool: &mpsc::Receiver<Vec<u8>>| pool.try_recv().unwrap_or_else(|_| vec![0u8; flen]);
    for s in spans {
        // Seek a quarter frame early: the span's first frame sits exactly
        // on the grid and must not be dropped by float rounding.
        let ss = (s.a - 0.25 / r).max(0.0);
        let mut child = crate::process::command(ffmpeg)
            .args(["-nostdin", "-hide_banner", "-v", "error"])
            .arg("-threads")
            .arg(threads.to_string())
            .arg("-ss")
            .arg(format!("{ss:.6}"))
            .arg("-i")
            .arg(source)
            .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-vf", vf])
            .arg("-frames:v")
            .arg(s.frames.to_string())
            .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        crate::process::gentle(&child);
        let err = child.stderr.take().map(drain);
        let mut out = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("decoder stdout missing"))?;
        // Hold one frame back so a short read can repeat it.
        let mut held: Option<Vec<u8>> = None;
        let mut sent = 0usize;
        let mut decoded = 0usize;
        while decoded < s.frames {
            let mut buf = fresh(&pool);
            if read_full(&mut out, &mut buf)? < flen {
                drop(buf);
                break;
            }
            decoded += 1;
            if let Some(h) = held.replace(buf) {
                if tx.send(Ok(h)).is_err() {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(()); // consumer stopped (cancel/failure)
                }
                sent += 1;
            }
        }
        drop(out);
        let status = child.wait()?;
        let err = err
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        let last = match held {
            Some(h) => h,
            None => {
                if !status.success() {
                    let e = anyhow::anyhow!("decode at {:.2}s failed: {}", s.a, tail(&err, 400));
                    let _ = tx.send(Err(anyhow::anyhow!("{e}")));
                    return Err(e);
                }
                // Nothing decodable here: black.
                let mut b = fresh(&pool);
                b[..g.luma_len()].fill(16);
                b[g.luma_len()..].fill(128);
                b
            }
        };
        if decoded < s.frames {
            tracing::warn!(
                "span at {:.2}s decoded {decoded}/{} frames: holding the last one",
                s.a,
                s.frames
            );
        }
        while sent + 1 < s.frames {
            let mut b = fresh(&pool);
            b.copy_from_slice(&last);
            if tx.send(Ok(b)).is_err() {
                return Ok(());
            }
            sent += 1;
        }
        if tx.send(Ok(last)).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

/// Read until `buf` is full or EOF; returns bytes read.
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}
fn tail(s: &str, n: usize) -> String {
    let t: Vec<char> = s.chars().rev().take(n).collect();
    t.into_iter().rev().collect()
}

/// Wait for a child (cancel kills it), collecting stderr.
fn wait_cancellable(
    mut child: Child,
    cancel: &CancelFlag,
) -> anyhow::Result<(std::process::ExitStatus, String)> {
    let err = child.stderr.take().map(drain);
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("cancelled by user");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let err = err
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    Ok((status, err))
}

/// Drain a pipe on a thread, keeping the last 16 KiB (a chatty ffmpeg must
/// never block on a full stderr pipe — Windows pipes hold only 64 KiB).
fn drain<R: Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut keep: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    keep.extend_from_slice(&buf[..n]);
                    let excess = keep.len().saturating_sub(16384);
                    if excess > 0 {
                        keep.drain(..excess);
                    }
                }
            }
        }
        String::from_utf8_lossy(&keep).into_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_size_reads_png_and_jpeg_headers() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend(640u32.to_be_bytes());
        b.extend(160u32.to_be_bytes());
        b.extend([8, 6, 0, 0, 0]);
        std::fs::write(&png, &b).unwrap();
        assert_eq!(image_size(&png), Some((640, 160)));
        // JPEG: SOI, an APP0 segment, then SOF0 with h=300, w=500.
        let jpg = dir.path().join("a.jpg");
        let mut j = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        j.extend([0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0x2C, 0x01, 0xF4, 0x03]);
        j.extend([0u8; 12]);
        std::fs::write(&jpg, &j).unwrap();
        assert_eq!(image_size(&jpg), Some((500, 300)));
        let junk = dir.path().join("a.webp");
        std::fs::write(&junk, b"RIFF....WEBP").unwrap();
        assert_eq!(image_size(&junk), None);
    }

    #[test]
    fn logos_fit_one_box_whatever_their_shape() {
        let mark = Logo {
            path: PathBuf::new(),
            corner: Corner::TopRight,
            aspect: 1.0,
        };
        let word = Logo {
            aspect: 4.0,
            ..mark.clone()
        };
        let tall = Canvas::TALL;
        // 14% of 1080 tall for a square mark, width capped at 26% for a
        // wordmark (so it is shorter, never wider than the box).
        assert_eq!(mark.size(tall), (152, 152));
        let (w, h) = word.size(tall);
        assert_eq!(w, 280);
        assert_eq!(h, 70);
        assert!(w % 2 == 0 && h % 2 == 0);
        assert_eq!(Corner::parse("BL"), Some(Corner::BottomLeft));
        assert!(Corner::BottomLeft.is_left() && !Corner::BottomLeft.is_top());
    }

    #[test]
    fn logo_chain_places_the_logo_in_its_corner() {
        let l = Logo {
            path: PathBuf::new(),
            corner: Corner::BottomRight,
            aspect: 1.0,
        };
        let f = logo_chain(3, &l, Canvas::SQUARE);
        assert!(f.contains("[3:v]format=rgba,scale=152:152"), "{f}");
        assert!(f.contains("overlay=x=W-w-43:y=H-h-76"), "{f}");
    }
}
