//! Clip → 1080×1920 captioned MP4 (port of RenderService.php).
//!
//! One continuous virtual-camera pass per timeline: the tracker segments
//! each output into Track (face-following), Wide (full horizontal frame)
//! and Punch (static VLM-chosen framing) ranges, render stitches them into
//! a single raw camera path and smooths it once — every framing change
//! arrives as a bezier dolly through an animated crop+scale+overlay comp,
//! never a dissolve. Audio is one contiguous slice, so A/V drift is
//! structurally impossible. Loudness-normalized mobile audio, H.264 +
//! faststart. GPU mode uses NVENC; CPU mode uses libx264.
//!
//! Windows notes: every path inside `-vf` goes through `filter_escape`
//! (backslash -> slash, wrap in single quotes, escape `':,`), which is
//! mandatory for `C:\...` drive colons. Thread count uses the same
//! `cpus-2` rule as transcription.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::framing::CamPose;
use crate::track::RawTarget;
use crate::validator::Clip;
use crate::whisper::Word;

/// One output chunk (absolute source seconds).
#[derive(Debug, Clone)]
pub struct Chunk {
    pub t0: f64,
    pub t1: f64,
    pub kind: ChunkKind,
}

#[derive(Debug, Clone)]
pub enum ChunkKind {
    /// Face-following raw camera targets (smoothed once, clip-wide, at render).
    Track(Vec<RawTarget>),
    /// Nobody on camera: dolly holds the full horizontal frame.
    Wide,
    /// Static punch-in at 0..1 frame position (VLM suggestion) — arrived
    /// at via dolly, not cut to.
    Punch(f64),
}

/// Concatenate same-codec segments with stream copy (no re-encode):
/// every segment comes from one encoder with identical settings, so the
/// join is sample-exact and A/V drift stays impossible.
pub fn concat_copy(
    ffmpeg: &Path,
    segs: &[std::path::PathBuf],
    out_mp4: &Path,
) -> anyhow::Result<()> {
    let list = out_mp4.parent().unwrap().join("concat.txt");
    let mut txt = String::new();
    for s in segs {
        txt.push_str(&format!(
            "file '{}'\n",
            s.display().to_string().replace('\'', "'\\''")
        ));
    }
    std::fs::write(&list, txt)?;
    let args: Vec<String> = vec![
        ffmpeg.display().to_string(),
        "-y".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        list.display().to_string(),
        "-c".into(),
        "copy".into(),
        "-movflags".into(),
        "+faststart".into(),
        out_mp4.display().to_string(),
    ];
    run_ffmpeg(&args, "concat segments")?;
    let _ = std::fs::remove_file(&list);
    if !out_mp4.is_file() {
        anyhow::bail!("concat produced no file.");
    }
    Ok(())
}

pub fn threads() -> usize {
    crate::whisper::cpu_count().saturating_sub(2).max(1)
}

/// Escape a path for use inside an ffmpeg -vf filter argument.
pub fn filter_escape(path: &Path) -> String {
    let s = path.display().to_string().replace('\\', "/");
    format!(
        "'{}'",
        s.replace('\'', "\\'")
            .replace(':', "\\:")
            .replace(',', "\\,")
    )
}

fn fonts_dir() -> PathBuf {
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
    let out = std::process::Command::new(ffmpeg)
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
/// build (plus an NVIDIA GPU for NVENC), else libx264. Never fails.
pub fn pick_encoder(gpu: bool) -> String {
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
    Command::new("nvidia-smi")
        .args(["-L"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn ass_filter(ass: &Path) -> String {
    format!(
        "ass={}:fontsdir={}",
        filter_escape(ass),
        filter_escape(&fonts_dir())
    )
}

/// Virtual-camera frame: one source rect per output frame. `crop` selects
/// the rect, `scale` fits it inside 1080x1920 (fit-width for wide rects,
/// fill for 9:16), `overlay` centers it over the blurred full-frame
/// backdrop. All three ride the same 30Hz sendcmd schedule, so
/// wide<->closeup moves render as continuous bezier dollies — never fades.
#[derive(Debug, Clone)]
pub struct Dolly {
    pub cw: i64,
    pub ch: i64,
    pub cx: i64,
    pub cy: i64,
    pub sw: i64,
    pub sh: i64,
    pub ox: i64,
    pub oy: i64,
}

/// Derive the comp from a source-px pose rect.
pub fn dolly_for(p: &CamPose, src_w: f64, src_h: f64) -> Dolly {
    let cw = ((p.w / 2.0).round() * 2.0).clamp(2.0, src_w) as i64;
    let ch = ((p.h / 2.0).round() * 2.0).clamp(2.0, src_h) as i64;
    let cx = p.x.round().clamp(0.0, (src_w - cw as f64).max(0.0)) as i64;
    let cy = p.y.round().clamp(0.0, (src_h - ch as f64).max(0.0)) as i64;
    let s = (1080.0 / cw as f64).min(1920.0 / ch as f64);
    let sw = ((cw as f64 * s / 2.0).round() * 2.0).clamp(2.0, 1080.0) as i64;
    let sh = ((ch as f64 * s / 2.0).round() * 2.0).clamp(2.0, 1920.0) as i64;
    Dolly {
        cw,
        ch,
        cx,
        cy,
        sw,
        sh,
        ox: ((1080 - sw) / 2).max(0),
        oy: ((1920 - sh) / 2).max(0),
    }
}

/// Dolly chain: sendcmd-driven crop + scale + overlay. `cmd_file=None` →
/// static comp.
fn vf_dolly(ass: &Path, cmd_file: Option<&Path>, init: &Dolly) -> String {
    let send = cmd_file
        .map(|p| format!("sendcmd=f={},", filter_escape(p)))
        .unwrap_or_default();
    format!(
        "{send}split=2[bg][fg];[bg]crop=ih*9/16:ih,scale=1080:1920:flags=lanczos,boxblur=luma_radius=20:luma_power=2[bg2];[fg]crop@c={cw}:{ch}:{cx}:{cy},scale@s={sw}:{sh}:flags=lanczos[fg2];[bg2][fg2]overlay@o={ox}:{oy},{},format=yuv420p",
        ass_filter(ass),
        cw = init.cw,
        ch = init.ch,
        cx = init.cx,
        cy = init.cy,
        sw = init.sw,
        sh = init.sh,
        ox = init.ox,
        oy = init.oy,
    )
}

/// Build the dolly `sendcmd` schedule: 8 commands per pose (crop x/y/w/h,
/// scale w/h, overlay x/y). Returns `(initial, commands)`; `offset` shifts
/// times (clip seeks reset frame timestamps to 0).
pub fn dolly_commands(poses: &[CamPose], src_w: f64, src_h: f64, offset: f64) -> (Dolly, String) {
    let base = Dolly {
        cw: (src_h * 9.0 / 16.0) as i64,
        ch: src_h as i64,
        cx: ((src_w - src_h * 9.0 / 16.0).max(0.0) / 2.0) as i64,
        cy: 0,
        sw: 1080,
        sh: 1920,
        ox: 0,
        oy: 0,
    };
    if poses.is_empty() {
        return (base, String::new());
    }
    let mut lines = String::with_capacity(poses.len() * 160);
    let mut initial = dolly_for(&poses[0], src_w, src_h);
    let mut last_t = f64::NEG_INFINITY;
    for p in poses {
        let t = (p.t - offset).max(0.0);
        if t - last_t < 0.005 && last_t > f64::NEG_INFINITY / 2.0 {
            continue; // sub-frame dup: sendcmd would step twice, not glide
        }
        last_t = t;
        let d = dolly_for(p, src_w, src_h);
        if p.t - offset <= 0.0 {
            initial = d.clone();
        }
        lines.push_str(&format!(
            "{t:.3} crop@c x {};{t:.3} crop@c y {};{t:.3} crop@c w {};{t:.3} crop@c h {};{t:.3} scale@s w {};{t:.3} scale@s h {};{t:.3} overlay@o x {};{t:.3} overlay@o y {};\n",
            d.cx, d.cy, d.cw, d.ch, d.sw, d.sh, d.ox, d.oy
        ));
    }
    (initial, lines)
}

fn run_ffmpeg(args: &[String], what: &str) -> anyhow::Result<()> {
    run_ffmpeg_progress(args, what, 0.0, None, &crate::progress::CancelFlag::never())
}

/// Streaming ffmpeg run with machine-readable progress.
///
/// Two live sources feed one deduping hook: `-progress pipe:1` parsed on
/// the caller (`out_time_ms=`) and stderr status lines (`time=HH:MM:SS`)
/// parsed on the drain thread. Either alone would do; together they
/// survive whichever stream a build buffers (seen live: `-progress`
/// arriving as one end-of-job burst on NVENC). On
/// [`CancelFlag`](crate::progress::CancelFlag) the child is killed and
/// this bails with "cancelled by user".
fn run_ffmpeg_progress(
    args: &[String],
    what: &str,
    total_s: f64,
    on_pct: Option<crate::progress::SharedPct>,
    cancel: &crate::progress::CancelFlag,
) -> anyhow::Result<()> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    tracing::info!("{what}");
    if args.len() < 2 {
        anyhow::bail!("{what}: empty ffmpeg command");
    }
    let (prog, rest) = args.split_first().unwrap();
    // Fast path (total unknown, e.g. concat): today's exact behavior.
    let stream = total_s > 0.0 && on_pct.is_some();
    let mut full: Vec<String> = rest.to_vec();
    if stream {
        let out = full.pop().unwrap();
        full.push("-progress".into());
        full.push("pipe:1".into());
        full.push(out);
    }
    let mut child = Command::new(prog)
        .args(&full)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let total_ms = (total_s * 1000.0).max(1.0);
    // Plain forward (callers clamp work-in-progress to 99 and finish
    // with exactly 100); the hook itself dedupes repeats.
    let report = |p: u8| {
        if let Some(f) = on_pct.as_deref() {
            f(p);
        }
    };
    // Drain stderr on a side thread (bounded tail for error reports);
    // without this a chatty ffmpeg could block on a full pipe. The
    // thread also mines `time=` status lines for live progress.
    let mut err_taker = child.stderr.take().map(|e| {
        let hook = on_pct.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut tail = vec![0u8; 0];
            let mut buf = [0u8; 8192];
            let mut e = e;
            let mut carry = String::new();
            loop {
                match e.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        tail.extend_from_slice(&buf[..n]);
                        let excess = tail.len().saturating_sub(4096);
                        if excess > 0 {
                            tail.drain(..excess);
                        }
                        if let Some(h) = hook.as_deref() {
                            carry.push_str(&String::from_utf8_lossy(&buf[..n]));
                            // Status lines end in \r (single line that
                            // rewrites itself); split on both separators.
                            let mut rest = String::new();
                            for chunk in carry.split(['\r', '\n']) {
                                if let Some(t) = parse_status_time(chunk) {
                                    let p = ((t * 1000.0 / total_ms * 100.0) as u8).min(99);
                                    h(p);
                                    rest.clear();
                                } else {
                                    // Keep only the tail: a `time=` token
                                    // never spans more than one line.
                                    rest = chunk[chunk.len().saturating_sub(32)..].to_string();
                                }
                            }
                            carry = rest;
                        }
                    }
                }
            }
            String::from_utf8_lossy(&tail).into_owned()
        })
    });
    let mut pct_emitted = false;
    if stream {
        let stdout = child.stdout.take().unwrap();
        let reader = std::io::BufReader::new(stdout);
        let total_ms = (total_s * 1000.0).max(1.0);
        for line in reader.lines() {
            if cancel.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(h) = err_taker.take() {
                    let _ = h.join();
                }
                anyhow::bail!("cancelled by user");
            }
            let Ok(line) = line else { break };
            if let Some(us) = line.strip_prefix("out_time_ms=") {
                if let Ok(us) = us.trim().parse::<f64>() {
                    report(out_time_pct(us, total_ms));
                    pct_emitted = true;
                }
            }
            if line.starts_with("progress=end") {
                break;
            }
        }
    }
    // Wait (poll so cancel still kills a silent child).
    let status = loop {
        match child.try_wait()? {
            Some(s) => break s,
            None => {
                if cancel.is_cancelled() {
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(h) = err_taker.take() {
                        let _ = h.join();
                    }
                    anyhow::bail!("cancelled by user");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    };
    let err_tail = err_taker
        .take()
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    if !status.success() {
        let tail: String = err_tail
            .chars()
            .rev()
            .take(600)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        anyhow::bail!("{what} failed (exit {status}):\n{tail}");
    }
    if pct_emitted {
        report(100);
    }
    Ok(())
}

/// Parse ffmpeg's stderr `time=HH:MM:SS.cs` status stamp into seconds.
/// `time=N/A` (head/tail of the run) and garbage parse to None.

/// Final quality encode. NOTE: `-crf` is x264-only — NVENC gets `-cq`
/// (a previous revision passed `-crf` to NVENC, which silently fell back
/// to a starved default bitrate).
fn video_codec_args(encoder: &str) -> Vec<String> {
    match encoder {
        "h264_nvenc" => vec![
            "-c:v".into(),
            "h264_nvenc".into(),
            "-preset".into(),
            "p5".into(),
            "-tune".into(),
            "hq".into(),
            "-cq".into(),
            "23".into(),
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            "192k".into(),
            "-movflags".into(),
            "+faststart".into(),
        ],
        _ => {
            let mut v = vec!["-c:v".into(), encoder.to_string()];
            if encoder == "libx264" {
                v.push("-preset".into());
                v.push("veryfast".into());
            }
            v.extend(
                [
                    "-crf",
                    "20",
                    "-c:a",
                    "aac",
                    "-b:a",
                    "192k",
                    "-movflags",
                    "+faststart",
                ]
                .into_iter()
                .map(str::to_string),
            );
            v
        }
    }
}

/// Loudness filter only (codec flags come from [`video_codec_args`]).
fn audio_args() -> Vec<String> {
    vec![
        "-af".into(),
        "loudnorm=I=-14:TP=-1.5:LRA=11,aresample=48000".into(),
    ]
}

fn words_in(words: &[Word], a: f64, b: f64) -> Vec<Word> {
    words
        .iter()
        .filter(|w| w.e > a && w.s < b)
        .cloned()
        .collect()
}

fn base_pose(src_w: f64, src_h: f64) -> CamPose {
    let w = src_h * 9.0 / 16.0;
    CamPose {
        t: 0.0,
        x: (src_w - w).max(0.0) / 2.0,
        y: 0.0,
        w,
        h: src_h,
    }
}

/// Stitch chunk framings into ONE raw camera timeline. Track chunks
/// contribute their raw targets; Wide synthesizes full-frame holds and
/// Punch a static focus window. Every kind change after the first is flagged
/// `cut` so arrival dollies in on a bezier bridge instead of dissolving.
fn assemble(chunks: &[Chunk], src_w: f64, src_h: f64) -> Vec<RawTarget> {
    let base_w = src_h * 9.0 / 16.0;
    let center = (src_w - base_w).max(0.0) / 2.0;
    let mut out = Vec::new();
    for (ci, chunk) in chunks.iter().enumerate() {
        let boundary = ci > 0;
        match &chunk.kind {
            ChunkKind::Track(raws) if raws.is_empty() => {
                for &t in &[chunk.t0, chunk.t1] {
                    out.push(RawTarget {
                        t,
                        x: center,
                        y: 0.0,
                        w: base_w,
                        h: src_h,
                        cut: false,
                        hard: false,
                        n_faces: 0,
                        pick_cx: f64::NAN,
                    });
                }
            }
            ChunkKind::Track(raws) => {
                for (i, r) in raws.iter().enumerate() {
                    let mut q = r.clone();
                    if i == 0 && boundary {
                        q.cut = true; // dolly in across the framing change
                    }
                    out.push(q);
                }
            }
            ChunkKind::Wide => {
                let mk = |t: f64, cut: bool| RawTarget {
                    t,
                    x: 0.0,
                    y: 0.0,
                    w: src_w,
                    h: src_h,
                    cut,
                    hard: false,
                    n_faces: 0,
                    pick_cx: f64::NAN,
                };
                out.push(mk(chunk.t0, boundary));
                if chunk.t1 - chunk.t0 > 2.0 {
                    out.push(mk((chunk.t0 + chunk.t1) / 2.0, false));
                }
                out.push(mk(chunk.t1, false));
            }
            ChunkKind::Punch(focus01) => {
                let x = (focus01 * src_w - base_w / 2.0).clamp(0.0, (src_w - base_w).max(0.0));
                let mk = |t: f64, cut: bool| RawTarget {
                    t,
                    x,
                    y: 0.0,
                    w: base_w,
                    h: src_h,
                    cut,
                    hard: false,
                    n_faces: 0,
                    pick_cx: f64::NAN,
                };
                out.push(mk(chunk.t0, boundary));
                if chunk.t1 - chunk.t0 > 2.0 {
                    out.push(mk((chunk.t0 + chunk.t1) / 2.0, false));
                }
                out.push(mk(chunk.t1, false));
            }
        }
    }
    out
}

/// Map a `-progress` `out_time_ms` stamp onto 0-99 against the expected
/// total. Despite the name the stamp is MICROseconds (29_960_000 ≈ 30s);
/// dividing by the millisecond total directly parks every encode at 99.
fn out_time_pct(us: f64, total_ms: f64) -> u8 {
    ((us / 1000.0 / total_ms.max(1.0) * 100.0) as u8).min(99)
}

/// Parse ffmpeg's stderr `time=HH:MM:SS.cs` status stamp into seconds.
/// `time=N/A` (head/tail of the run) and garbage parse to None.
fn parse_status_time(chunk: &str) -> Option<f64> {
    let i = chunk.find("time=")?;
    let t: String = chunk[i + 5..]
        .chars()
        .take_while(|c| *c == ':' || *c == '.' || c.is_ascii_digit())
        .collect();
    let mut it = t.split(':');
    let h: f64 = it.next()?.parse().ok()?;
    let m: f64 = it.next()?.parse().ok()?;
    let s: f64 = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some(h * 3600.0 + m * 60.0 + s)
}

#[cfg(test)]
mod progress_tests {
    use super::{out_time_pct, parse_status_time};

    #[test]
    fn status_time_parses() {
        let t = parse_status_time("frame=  42 fps=30 time=00:00:08.42 bitrate=100k").unwrap();
        assert!((t - 8.42).abs() < 1e-9, "got {t}");
        assert!(parse_status_time("frame=1 time=N/A bitrate=N/A").is_none());
        assert!(parse_status_time("nothing here").is_none());
        assert!(parse_status_time("time=-00:00:01.20 hmm").is_none());
    }

    #[test]
    fn out_time_microseconds_map_against_total() {
        // 30s encode: the stamp reads ~29_960_000 (µs, not ms).
        assert_eq!(out_time_pct(29_960_000.0, 30_000.0), 99);
        assert_eq!(out_time_pct(15_000_000.0, 30_000.0), 50);
        assert_eq!(out_time_pct(0.0, 30_000.0), 0);
        // Wild values clamp instead of wrapping the u8 cast.
        assert_eq!(out_time_pct(999_999_999.0, 30_000.0), 99);
    }
}

/// Render a full timeline (one clip or the whole video).
///
/// `words` cover the whole output range; `chunks` use absolute source
/// times. `style` is the caption preset. `progress`/`cancel` are serve
/// hooks (`None`/never on the CLI): progress reports 0-100 of the encode.
#[allow(clippy::too_many_arguments)]
pub fn render_timeline(
    source: &Path,
    words: &[Word],
    style: &str,
    chunks: &[Chunk],
    src_w: u32,
    src_h: u32,
    gpu: bool,
    out_mp4: &Path,
    threads: usize,
    label: &str,
    // White edge dips (fade-in at head, fade-out at tail): used for merge
    // flash joins, where each segment carries its own half of the dip.
    // Edge-anchored fades are exact; mid-stream `fade=t=in` would hold the
    // fade color over everything before it (all-white output — seen live).
    flash: Option<(bool, bool)>,
    progress: Option<crate::progress::SharedPct>,
    cancel: &crate::progress::CancelFlag,
) -> anyhow::Result<String> {
    // Segments of pure pause carry no words — still legal (empty sidecars
    // below); only the chunk list is required.
    let chunks: Vec<Chunk> = chunks.iter().filter(|c| c.t1 > c.t0).cloned().collect();
    if chunks.is_empty() {
        anyhow::bail!("No timeline chunks to render.");
    }
    if let Some(p) = out_mp4.parent() {
        std::fs::create_dir_all(p)?;
    }
    let dir = out_mp4.parent().unwrap().to_path_buf();
    let ffmpeg = crate::binaries::require("ffmpeg")?;
    let encoder = pick_encoder(gpu);
    let (sw, sh) = (src_w as f64, src_h as f64);

    // Whole-output SRT sidecar (absolute times, as before).
    let srt_path = dir.join(if label == "full" {
        "full.srt"
    } else {
        "clip.srt"
    });
    std::fs::write(&srt_path, crate::captions::srt::from_words(words))?;

    // ONE continuous pass for the whole timeline (no chunk files, no joins,
    // no dissolves): the camera path carries every framing change as a
    // bezier dolly, and audio is a single contiguous slice — A/V drift is
    // structurally impossible.
    let a0 = chunks[0].t0;
    let b1 = chunks.iter().map(|c| c.t1).fold(a0, f64::max);
    let total = (b1 - a0).max(1.0);
    let slice = words_in(words, a0, b1);
    let ass_name = if label == "full" {
        "full.ass"
    } else {
        "clip.ass"
    };
    let ass_path = dir.join(ass_name);
    std::fs::write(&ass_path, crate::captions::ass::build(&slice, style, a0))?;

    let raw_all = assemble(&chunks, sw, sh);
    let mut poses = if raw_all.is_empty() {
        vec![base_pose(sw, sh)]
    } else {
        crate::track::smooth_path(&raw_all, sw, sh)
    };
    if poses.len() > 12000 {
        poses = poses.into_iter().step_by(2).collect();
    }
    let (init, commands) = dolly_commands(&poses, sw, sh, a0);
    let vf = if commands.is_empty() {
        vf_dolly(&ass_path, None, &init)
    } else {
        let cp = dir.join("dolly.cmd");
        std::fs::write(&cp, &commands)?;
        vf_dolly(&ass_path, Some(&cp), &init)
    };
    // Optional white edge dips (merge flash joins), appended after the
    // caption burn so the whole frame dips.
    let vf = match flash {
        Some((fi, fo)) => {
            let d = 0.15f64.min((total / 2.0 - 0.02).max(0.0));
            let mut parts = vec![vf];
            if d > 0.01 {
                if fi {
                    parts.push(format!("fade=t=in:st=0:d={d:.2}:color=white"));
                }
                if fo {
                    parts.push(format!(
                        "fade=t=out:st={:.2}:d={d:.2}:color=white",
                        total - d
                    ));
                }
            }
            parts.join(",")
        }
        None => vf,
    };
    let mut args: Vec<String> = vec![
        ffmpeg.display().to_string(),
        "-y".into(),
        "-threads".into(),
        threads.to_string(),
        "-ss".into(),
        a0.to_string(),
        "-t".into(),
        total.to_string(),
        "-i".into(),
        source.display().to_string(),
        "-vf".into(),
        vf,
    ];
    args.extend(audio_args());
    args.extend(video_codec_args(&encoder));
    args.push(out_mp4.display().to_string());
    run_ffmpeg_progress(
        &args,
        &format!("render {label} ({total:.0}s, {encoder})"),
        total,
        progress,
        cancel,
    )?;
    if !out_mp4.is_file() {
        if cancel.is_cancelled() {
            anyhow::bail!("cancelled by user");
        }
        anyhow::bail!("ffmpeg produced no file.");
    }
    Ok(encoder)
}

/// Render one clip candidate to a captioned 9:16 MP4.
pub fn render_clip(
    source: &Path,
    clip: &Clip,
    words: &[Word],
    chunks: &[Chunk],
    src_w: u32,
    src_h: u32,
    gpu: bool,
    out_mp4: &Path,
    threads: usize,
    progress: Option<crate::progress::SharedPct>,
    cancel: &crate::progress::CancelFlag,
) -> anyhow::Result<String> {
    let slice: Vec<Word> = words_in(words, clip.start_s, clip.end_s);
    if slice.is_empty() {
        anyhow::bail!("No transcript words inside clip range.");
    }
    render_timeline(
        source,
        &slice,
        &clip.caption_style,
        chunks,
        src_w,
        src_h,
        gpu,
        out_mp4,
        threads,
        "clip",
        None,
        progress,
        cancel,
    )
}

/// Render the FULL video with subtitles burned in.
/// Same filter chain, no cuts, dialogue stamps unshifted.
pub fn render_full(
    source: &Path,
    words: &[Word],
    style: &str,
    chunks: &[Chunk],
    src_w: u32,
    src_h: u32,
    gpu: bool,
    out_mp4: &Path,
    threads: usize,
    progress: Option<crate::progress::SharedPct>,
    cancel: &crate::progress::CancelFlag,
) -> anyhow::Result<String> {
    render_timeline(
        source, words, style, chunks, src_w, src_h, gpu, out_mp4, threads, "full", None, progress,
        cancel,
    )
}
