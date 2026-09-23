//! Speaker-aware face tracking (YuNet via ONNX Runtime).
//!
//! Offline, in-process: frames are piped from the provisioned ffmpeg,
//! faces detected with YuNet (`yunet_2026may.onnx`, MIT, auto-downloaded
//! once to the provision dir), the primary speaker tracked across samples,
//! and the crop path rendered in a single ffmpeg pass via a per-frame
//! `sendcmd` schedule (30Hz: `sendcmd` steps discretely, so frame-rate
//! entries are what make the motion read as continuous).
//!
//! Camera model: sparse per-sample window targets (float, never rounded)
//! feed [`smooth_path`]: continuous motion rides Catmull-Rom, speaker
//! handoffs ride a cubic-bezier ease-in-out glide, dropouts hold position.
//!
//! Trust model (the camera only moves on evidence): the speaker is whoever's
//! mouth moves; newcomers must win twice running and respect a 2.5s floor;
//! weak detections never earn zoom; reframes freeze while the transcript is
//! silent; 2+ close confident talkers share one window (never the empty
//! middle, never a crowd); shot cuts (hash jump + face turnover) snap
//! instead of gliding, while handheld motion keeps gliding by face veto.
//!
//! Decode math is ported exactly from OpenCV's `FaceDetectorYN`
//! (`modules/objdetect/src/face_detect.cpp`): pad input to a multiple of
//! 32, RGB 0-255 NCHW input, per-stride `{cls,obj,bbox,kps}_{8,16,32}`
//! heads, `score = sqrt(clamp(cls)*clamp(obj))`, center-size box decode,
//! greedy NMS. Output rows are `[x, y, w, h, 5 landmarks, score]`
//! (top-left + size, like the OpenCV demo).

use std::path::Path;

use crate::framing::{CamPose, CropPlan, TrackPoint};
use crate::whisper::Word;

const STRIDES: [usize; 3] = [8, 16, 32];
const DIVISOR: usize = 32;

/// One decoded face, in the coordinate space it was detected in.
#[derive(Debug, Clone)]
pub struct Face {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub score: f64,
}

impl Face {
    pub fn cx(&self) -> f64 {
        self.x + self.w / 2.0
    }
    pub fn area(&self) -> f64 {
        (self.w * self.h).max(0.0)
    }
}

pub fn iou(a: &Face, b: &Face) -> f64 {
    let x1 = a.x.max(b.x);
    let y1 = a.y.max(b.y);
    let x2 = (a.x + a.w).min(b.x + b.w);
    let y2 = (a.y + a.h).min(b.y + b.h);
    let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
    let union = (a.area() + b.area() - inter).max(1e-9);
    inter / union
}

/// Temporal confirmation: a low-confidence detection survives only if it
/// overlaps a box from the previous sample. Single-sample hallucinations
/// die here, at the detector level, instead of downstream.
pub fn confirm(faces: Vec<Face>, prev: &[Face]) -> Vec<Face> {
    faces
        .into_iter()
        .filter(|f| f.score >= 0.65 || prev.iter().any(|p| iou(f, p) > 0.3))
        .collect()
}

/// Average-hash of a sample frame (8x8 luma) for shot-cut detection.
/// Pure (unit-tested).
pub fn ahash(rgb: &[u8], w: usize, h: usize) -> u64 {
    if rgb.len() != w * h * 3 || w == 0 || h == 0 {
        return 0;
    }
    let mut cells = [0u64; 64];
    let mut counts = [0u64; 64];
    for y in 0..h {
        for x in 0..w {
            let idx = (y * w + x) * 3;
            let luma = rgb[idx] as u64 + rgb[idx + 1] as u64 + rgb[idx + 2] as u64;
            let c = (y * 8 / h) * 8 + (x * 8 / w);
            cells[c] += luma;
            counts[c] += 1;
        }
    }
    let mut means = [0u64; 64];
    let mut total = 0u64;
    for (i, cell) in cells.iter().enumerate() {
        means[i] = cell / counts[i].max(1);
        total += means[i];
    }
    let avg = total / 64;
    let mut hash = 0u64;
    for (i, m) in means.iter().enumerate() {
        if *m > avg {
            hash |= 1 << i;
        }
    }
    hash
}

/// Hamming distance between two hashes.
pub fn hamdist(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Face continuity veto: any box overlapping last sample's boxes means the
/// same people are still on screen — not a shot change. Handheld pans and
/// laughing fits spike frame hashes without cutting; snapping there is far
/// worse than gliding. A real cut (shot/reverse-shot, new scene) changes
/// the faces too, so it still confirms. Pure (unit-tested).
pub fn faces_overlap(cur: &[Face], prev: &[Face]) -> bool {
    cur.iter().any(|f| prev.iter().any(|p| iou(f, p) >= 0.15))
}

/// Shot-cut threshold (bits): hard scene changes score 25+; same-scene
/// motion stays well under. Between them lies the occasional whip-pan —
/// snapping there is still safer than gliding across a cut.
pub const SHOT_CUT_BITS: u32 = 16;

/// Mouth-band motion 0..1 between consecutive sample frames: mean abs pixel
/// diff over the lower-face band, scaled so talking reads ~0.4-1.0 and
/// stillness ~0. Pure (unit-tested). This is the speaker signal — the face
/// whose mouth moves is the face holding the floor.
pub fn mouth_motion(prev: &[u8], cur: &[u8], w: usize, h: usize, f: &Face) -> f64 {
    if prev.len() != cur.len() || prev.len() != w * h * 3 {
        return 0.0;
    }
    let x0 = (f.x + f.w * 0.25).clamp(0.0, w as f64) as usize;
    let x1 = (f.x + f.w * 0.75).clamp(0.0, w as f64) as usize;
    let y0 = (f.y + f.h * 0.55).clamp(0.0, h as f64) as usize;
    let y1 = (f.y + f.h * 0.92).clamp(0.0, h as f64) as usize;
    if x1 <= x0 || y1 <= y0 {
        return 0.0;
    }
    let mut sad: u64 = 0;
    let mut n: u64 = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            let idx = (y * w + x) * 3;
            for ch in 0..3 {
                sad += prev[idx + ch].abs_diff(cur[idx + ch]) as u64;
            }
            n += 1;
        }
    }
    if n == 0 {
        return 0.0;
    }
    ((sad as f64 / (n * 3) as f64 / 255.0) * 8.0).clamp(0.0, 1.0)
}

/// Greedy NMS over score-sorted faces.
pub fn nms(mut faces: Vec<Face>, thresh: f64, top_k: usize) -> Vec<Face> {
    faces.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    faces.truncate(top_k.max(1));
    let mut kept = Vec::new();
    for f in faces {
        if kept.iter().all(|k: &Face| iou(&f, k) < thresh) {
            kept.push(f);
        }
    }
    kept
}

/// Raw per-stride head tensors, flattened row-major as `(rows*cols)` long
/// for cls/obj and `(rows*cols*4 / *10)` for bbox/kps.
pub struct StrideHeads<'a> {
    pub stride: usize,
    pub cols: usize,
    pub rows: usize,
    pub cls: &'a [f32],
    pub obj: &'a [f32],
    pub bbox: &'a [f32],
    pub kps: &'a [f32],
}

/// Decode one stride head exactly like OpenCV's postProcess().
pub fn decode_stride(head: &StrideHeads, score_threshold: f32) -> Vec<Face> {
    let mut out = Vec::new();
    let s = head.stride as f64;
    for r in 0..head.rows {
        for c in 0..head.cols {
            let idx = r * head.cols + c;
            let cls = head.cls[idx].clamp(0.0, 1.0);
            let obj = head.obj[idx].clamp(0.0, 1.0);
            let score = (cls * obj).sqrt();
            if score < score_threshold {
                continue;
            }
            let cx = (c as f64 + head.bbox[idx * 4] as f64) * s;
            let cy = (r as f64 + head.bbox[idx * 4 + 1] as f64) * s;
            let w = (head.bbox[idx * 4 + 2] as f64).exp() * s;
            let h = (head.bbox[idx * 4 + 3] as f64).exp() * s;
            out.push(Face {
                x: cx - w / 2.0,
                y: cy - h / 2.0,
                w,
                h,
                score: score as f64,
            });
        }
    }
    out
}

fn pad_to(x: usize, div: usize) -> usize {
    ((x + div - 1) / div) * div
}

/// Padded dims for an input size (OpenCV pads to multiples of 32 with 0).
pub fn padded_dims(w: usize, h: usize) -> (usize, usize) {
    (pad_to(w, DIVISOR), pad_to(h, DIVISOR))
}

/// Grid cells per stride for padded dims (validates the anchor math:
/// total cells must equal the model's loc rows).
pub fn grid_cells(pad_w: usize, pad_h: usize) -> [(usize, usize, usize); 3] {
    [
        (STRIDES[0], pad_w / STRIDES[0], pad_h / STRIDES[0]),
        (STRIDES[1], pad_w / STRIDES[1], pad_h / STRIDES[1]),
        (STRIDES[2], pad_w / STRIDES[2], pad_h / STRIDES[2]),
    ]
}

pub struct Tracker {
    session: ort::session::Session,
    /// True when the DirectML EP is active (drives sample rate).
    pub gpu: bool,
}

impl Tracker {
    /// Load YuNet. With `gpu` on Windows, tries the DirectML EP first and
    /// falls back to CPU with a warning (never fatal). On macOS/Linux there
    /// is no DirectML EP — GPU tracking means the CPU session (the Ort
    /// build still uses its bundled CPU/CoreML kernels where available).
    pub fn load(model_path: &Path, gpu: bool) -> anyhow::Result<Self> {
        #[cfg(windows)]
        if gpu {
            match (|| -> anyhow::Result<ort::session::Session> {
                let b = ort::session::Session::builder()
                    .map_err(|e| anyhow::anyhow!("ort builder: {e:?}"))?;
                let mut b = b
                    .with_execution_providers([ort::ep::DirectML::default().build()])
                    .map_err(|e| anyhow::anyhow!("directml ep: {e:?}"))?;
                Ok(b.commit_from_file(model_path)?)
            })() {
                Ok(session) => {
                    tracing::info!("tracker: DirectML EP active");
                    return Ok(Self { session, gpu: true });
                }
                Err(e) => tracing::warn!("DirectML session failed ({e:#}): CPU fallback"),
            }
        }
        #[cfg(not(windows))]
        if gpu {
            tracing::info!("tracker: no DirectML EP on this OS — CPU session");
        }
        let session = ort::session::Session::builder()?.commit_from_file(model_path)?;
        Ok(Self {
            session,
            gpu: false,
        })
    }

    /// Detect faces in an RGB frame. Returns faces in frame pixels.
    pub fn detect(&mut self, rgb: &[u8], w: usize, h: usize) -> anyhow::Result<Vec<Face>> {
        let (pad_w, pad_h) = padded_dims(w, h);
        // NCHW f32, RGB 0-255 (blobFromImage defaults: scale 1.0, swapRB).
        let mut input = ndarray::Array4::<f32>::zeros((1, 3, pad_h as usize, pad_w as usize));
        for y in 0..h {
            for x in 0..w {
                let src = (y * w + x) * 3;
                for ch in 0..3 {
                    input[[0, ch, y, x]] = rgb[src + ch] as f32;
                }
            }
        }
        let outputs = self
            .session
            .run(ort::inputs![ort::value::TensorRef::from_array_view(
                &input
            )?])?;

        let mut faces = Vec::new();
        for (si, s) in STRIDES.iter().enumerate() {
            let tag = format!("_{s}");
            let suffixes = ["cls", "obj", "bbox", "kps"];
            let mut flat: Vec<Vec<f32>> = Vec::new();
            for suf in suffixes {
                let name = format!("{suf}{tag}");
                let (shape, data) = outputs[name.as_str()].try_extract_tensor::<f32>()?;
                let _ = shape;
                flat.push(data.to_vec());
            }
            let cols = pad_w / s;
            let rows = pad_h / s;
            let n = cols * rows;
            if flat[0].len() < n
                || flat[1].len() < n
                || flat[2].len() < n * 4
                || flat[3].len() < n * 10
            {
                anyhow::bail!("unexpected YuNet head shape for stride {s} (pad {pad_w}x{pad_h})");
            }
            let _ = si;
            faces.extend(decode_stride(
                &StrideHeads {
                    stride: *s,
                    cols,
                    rows,
                    cls: &flat[0],
                    obj: &flat[1],
                    bbox: &flat[2],
                    kps: &flat[3],
                },
                0.5,
            ));
        }
        Ok(nms(faces, 0.3, 5000))
    }
}

/// Sample frames via ffmpeg: `fps` frames/sec, `width` px wide RGB24.
/// With `seek = Some((start, dur))`, only that span is decoded (returns
/// absolute timestamps). Used to track picked clip ranges instead of the
/// whole source.
pub fn sample_frames(
    ffmpeg: &Path,
    source: &Path,
    fps: u32,
    width: u32,
    sample_h: u32,
    seek: Option<(f64, f64)>,
) -> anyhow::Result<Vec<(f64, Vec<u8>)>> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(ffmpeg);
    cmd.arg("-hide_banner").arg("-v").arg("error");
    let t0 = seek.map(|(s, _)| s).unwrap_or(0.0);
    if let Some((s, d)) = seek {
        cmd.arg("-ss")
            .arg(s.to_string())
            .arg("-t")
            .arg(d.max(0.5).to_string());
    }
    cmd.arg("-i").arg(source.display().to_string());
    let child = cmd
        .args([
            "-vf",
            &format!("fps={fps},scale={width}:{sample_h}"),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!("frame sampling failed");
    }
    let frame_len = width as usize * sample_h as usize * 3;
    if frame_len == 0 {
        anyhow::bail!("bad sample dims");
    }
    let mut frames = Vec::new();
    for (i, chunk) in out.stdout.chunks(frame_len).enumerate() {
        if chunk.len() < frame_len {
            break;
        }
        frames.push((t0 + i as f64 / fps as f64, chunk.to_vec()));
    }
    Ok(frames)
}

/// A timeline segment: tracked close-up or wide fill.
#[derive(Debug, Clone, PartialEq)]
pub enum SegKind {
    Track,
    Wide,
}

#[derive(Debug, Clone)]
pub struct Seg {
    pub t0: f64,
    pub t1: f64,
    pub kind: SegKind,
}

pub struct Tracked {
    pub segments: Vec<Seg>,
    pub ever_seen: bool,
    /// Raw per-sample targets (render smooths the whole clip once).
    pub raw: Vec<RawTarget>,
    /// Seconds held in two-shot (both talkers framed).
    pub group_secs: f64,
}

/// Butter-smooth reframing camera.
///
/// The old exponential follower looked choppy for three compounding reasons,
/// all fixed here:
/// - `sendcmd` steps the crop window discretely (zero interpolation), so the
///   schedule must run at ~output frame rate ([`EMIT_HZ`]): 33ms sub-pixel
///   steps read as continuous motion, 83ms steps read as stepping.
/// - Rounding happens once, at emit time — never before smoothing (rounding
///   first bakes quantization judder into the path).
/// - Continuous motion rides Catmull-Rom through denoised targets, while
///   discontinuities (speaker handoffs) ride a cubic-bezier ease-in-out
///   glide ([`GLIDE_S`]): slow attack, fast middle, gentle landing.
/// - Brief detector dropouts hold the last target instead of drifting home
///   and gliding back (that drift-then-return was half the visible jerk).
/// - Zoom (window w/h) is a first-class eased channel, not a chased
///   side-effect: reframing moves ease *into* faces and regions.
pub const EMIT_HZ: f64 = 30.0;
/// Speaker-handoff glide duration (s) — scaled by move distance so big
/// cross-frame moves get more time (constant peak velocity, like a real
/// operator): `dur = clamp(0.5 + dist/600, 0.5, 1.25)`.
pub const GLIDE_S: f64 = 0.7;
/// Runs shorter than this are detector flap, not a speaker change: the
/// handoff is absorbed (camera holds) instead of whipping over and back.
/// Without this, rapid primary alternation compresses each bezier bridge
/// into a tiny run and the camera visibly whips.
pub const MIN_RUN_S: f64 = 0.4;
/// Runs shorter than this that hand off AGAIN (A→B→C banter chains) are
/// middle stops, not settled speakers: they collapse so the chain renders
/// as ONE full glide A→C instead of stacked compressed whips. Genuine
/// turns (each speaker holds the floor ≥0.7s) always survive, as do group
/// boundaries and end-of-clip arrivals.
pub const CHAIN_S: f64 = 0.7;
/// Post-cut settle (s): after a hard shot cut the camera lands WIDE and
/// holds while the challenger gate confirms the new shot's speaker, instead
/// of snapping to the first (unconfirmed, usually wrong) pick and whipping
/// to the correction a beat later. The first move in a new shot is ONE
/// glide from the establishing wide to the settled speaker.
pub const SETTLE_S: f64 = 0.4;
/// Handoff trigger: a primary-target center jump beyond this fraction of the
/// frame width between consecutive samples starts a bezier glide.
/// Small enough that reacquires whip as glides, not chases (no human moves
/// 64px+ per sample); flap is absorbed downstream by [`MIN_RUN_S`].
const CUT_FRAC: f64 = 0.10;
/// Pending-challenger match radius (fraction of frame width): box centers
/// are stable under size jitter where IoU is not.
const PENDING_FRAC: f64 = 0.08;
/// Detection denoiser: soft EMA admits no visible lag but irons out box
/// jitter (the butter comes from median + resample + bezier path). Kept
/// responsive on purpose — attack latency is what reads as "laggy".
const DENOISE_ALPHA: f64 = 0.6;

/// Cubic-bezier easing (CSS-style): solve y for progress x with control
/// points (x1,y1),(x2,y2). `ease_in_out` = (0.42, 0, 0.58, 1).
pub fn cubic_bezier(x1: f64, y1: f64, x2: f64, y2: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let cx = 3.0 * x1;
    let bx = 3.0 * (x2 - x1) - cx;
    let ax = 1.0 - cx - bx;
    let cy = 3.0 * y1;
    let by = 3.0 * (y2 - y1) - cy;
    let ay = 1.0 - cy - by;
    // Newton-Raphson on x(t), then sample y(t).
    let mut t = x;
    for _ in 0..5 {
        let xt = ((ax * t + bx) * t + cx) * t - x;
        let dx = (3.0 * ax * t + 2.0 * bx) * t + cx;
        if dx.abs() < 1e-6 {
            break;
        }
        t = (t - xt / dx).clamp(0.0, 1.0);
    }
    ((ay * t + by) * t + cy) * t
}

/// Standard ease-in-out: slow attack, fast middle, gentle landing.
pub fn ease_in_out(u: f64) -> f64 {
    cubic_bezier(0.42, 0.0, 0.58, 1.0, u)
}

/// One raw per-sample framing target (source px, f64 — never rounded).
/// `cut` marks a speaker handoff: the path glides here on a bezier curve
/// instead of chasing through intermediate frames. `hard` marks a shot
/// change: the path SNAPS (no glide — panning across a hard cut implies a
/// continuous space that isn't there).
#[derive(Debug, Clone)]
pub struct RawTarget {
    pub t: f64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub cut: bool,
    pub hard: bool,
    /// Faces detected at this sample (drives future two-shot logic).
    pub n_faces: usize,
    /// Picked primary center-x (NaN when no face).
    pub pick_cx: f64,
}

/// Transcript gate: is anyone audibly speaking at `t` (word spans padded)?
/// Reframes freeze during silence — the camera never chases quiet faces.
/// Pure (unit-tested).
pub fn speech_active(words: &[Word], t: f64) -> bool {
    words.iter().any(|w| t > w.s - 0.4 && t < w.e + 0.4)
}

/// Desired 9:16 window for a face — (center-x, center-y, height-fraction) in
/// source px → window geometry. Zoom stays generous (≤1.3x) and weak
/// detections never earn more than 1.15x: the camera doesn't punch into
/// faces it isn't sure about. Face rides ~40% from the window top.
pub fn window_for_face(
    cx: f64,
    cy: f64,
    frac: f64,
    src_w: f64,
    src_h: f64,
    base_w: f64,
    score: f64,
) -> (f64, f64, f64, f64) {
    let zmax = if score >= 0.65 { 1.3 } else { 1.15 };
    let z = (0.38 / frac.max(0.05)).clamp(1.0, zmax);
    let w = base_w / z;
    let h = src_h / z;
    let x = (cx - w / 2.0).clamp(0.0, (src_w - w).max(0.0));
    let y = (cy - h * 0.4).clamp(0.0, (src_h - h).max(0.0));
    (x, y, w, h)
}

/// Closeness budget: a talking cluster must span at most this fraction of
/// the base window. Window margins eat the rest, so the midpoint always
/// lands near faces — never a zoom on the empty middle. Wider crews fall
/// back to the primary speaker (a midpoint there would frame nobody).
/// Incumbents get a looser budget (see [`cluster_members`]) so boundary
/// dither can't strobe the group on and off.
const CLUSTER_SPAN_FRAC: f64 = 0.75;
const CLUSTER_KEEP_FRAC: f64 = 0.9;

/// Cluster members, rank-ordered: walk down score+motion rank, admit anyone
/// confident and distinct while the running span stays within budget. No
/// head-count cap — two talkers or six, closeness decides, not number.
/// A far face neither joins nor vetoes the close crew.
/// Hysteresis: faces already in `keep` (last sample's members) are admitted
/// under the looser keep budget, so a crew breathing on the boundary line
/// doesn't flap in and out. Pure (unit-tested).
/// Motion is NOT filtered here — entry strictness lives at the call site
/// (members moving now) so pauses can hold.
pub fn cluster_members<'a>(
    scored: &[(&'a Face, f64)],
    base_w: f64,
    keep: &[Face],
) -> Vec<(&'a Face, f64)> {
    let mut members: Vec<(&Face, f64)> = Vec::new();
    let mut x1 = f64::INFINITY;
    let mut x2 = f64::NEG_INFINITY;
    for &(f, m) in scored {
        if f.score < 0.6 {
            continue;
        }
        if !members
            .iter()
            .all(|(g, _)| iou(f, g) < 0.15 && (f.cx() - g.cx()).abs() > 0.6 * f.w.max(g.w))
        {
            continue; // duplicate box on an admitted head
        }
        let incumbent = keep.iter().any(|k| iou(f, k) > 0.25);
        let budget = if incumbent {
            CLUSTER_KEEP_FRAC
        } else {
            CLUSTER_SPAN_FRAC
        } * base_w;
        let (nx1, nx2) = (x1.min(f.x), x2.max(f.x + f.w));
        if nx2 - nx1 > budget {
            continue; // too far out — neither joins nor vetoes
        }
        x1 = nx1;
        x2 = nx2;
        members.push((f, m));
    }
    if members.len() >= 2 {
        members
    } else {
        Vec::new()
    }
}

/// Group window for 2+ simultaneous talkers: the base (widest) 9:16 window
/// over the whole span — or `None` when they don't fit, in which case the
/// camera stays on the primary speaker. Never half-frames anyone: if
/// edge-clamping would cut a member out, `None`. Pure (unit-tested).
pub fn group_window(
    faces: &[&Face],
    src_w: f64,
    src_h: f64,
    base_w: f64,
) -> Option<(f64, f64, f64, f64)> {
    if faces.len() < 2 {
        return None;
    }
    let margin = 0.12 * base_w;
    let mut x1 = f64::INFINITY;
    let mut x2 = f64::NEG_INFINITY;
    for f in faces {
        x1 = x1.min(f.x - margin);
        x2 = x2.max(f.x + f.w + margin);
    }
    if x2 - x1 > base_w {
        return None; // too spread for one vertical window
    }
    if x1 < 0.0 {
        x2 -= x1;
        x1 = 0.0;
    }
    if x2 > src_w {
        x1 -= x2 - src_w;
        x2 = src_w;
    }
    let w = base_w.min(src_w);
    let h = (w * 16.0 / 9.0).min(src_h);
    let x = ((x1 + x2) / 2.0 - w / 2.0).clamp(0.0, (src_w - w).max(0.0));
    // Every member must survive inside (with 1px tolerance).
    for f in faces {
        if f.x < x - 1.0 || f.x + f.w > x + w + 1.0 {
            return None;
        }
    }
    let cy = faces.iter().map(|f| f.y + f.h / 2.0).sum::<f64>() / faces.len() as f64;
    let y = (cy - h * 0.4).clamp(0.0, (src_h - h).max(0.0));
    Some((x, y, w, h))
}

/// Group state machine: enter after 4 straight qualifying samples (~0.3s —
/// lone blips can't engage it), leave after 8 straight misses (~0.5s —
/// engaged groups ride through flicker). Membership hysteresis and the
/// speech latch do the slow stabilizing; the state just needs to be slower
/// than a blink and faster than a thought. Returns `(active, edge)`; `edge`
/// is true on enter AND exit so arrival dollies in both directions. Pure
/// (unit-tested).
#[derive(Debug, Default)]
pub struct GroupState {
    pub on: bool,
    yes: u32,
    no: u32,
}

impl GroupState {
    pub fn step(&mut self, qualifies: bool) -> (bool, bool) {
        if qualifies {
            self.yes += 1;
            self.no = 0;
        } else {
            self.no += 1;
            self.yes = 0;
        }
        let was = self.on;
        if !self.on && self.yes >= 4 {
            self.on = true;
        }
        if self.on && self.no >= 8 {
            self.on = false;
        }
        (self.on, self.on != was)
    }
}

/// Catmull-Rom sample of one channel at fractional position `pos`.
fn cr_sample(vals: &[f64], pos: f64) -> f64 {
    let n = vals.len();
    if n == 0 {
        return 0.0;
    }
    if n == 1 {
        return vals[0];
    }
    let pos = pos.clamp(0.0, (n - 1) as f64);
    let i = (pos.floor() as usize).min(n - 2);
    let u = pos - i as f64;
    let at = |k: isize| vals[k.clamp(0, n as isize - 1) as usize];
    let (p0, p1, p2, p3) = (
        at(i as isize - 1),
        at(i as isize),
        at(i as isize + 1),
        at(i as isize + 2),
    );
    0.5 * ((2.0 * p1)
        + (-p0 + p2) * u
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * u * u
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * u * u * u)
}

/// Turn sparse raw targets into a butter-smooth [`EMIT_HZ`] pose path.
/// Pure (unit-tested): continuous runs ride Catmull-Rom, `cut` boundaries
/// ride a [`GLIDE_S`] cubic-bezier glide, across all channels (x, y, zoom).
pub fn smooth_path(targets: &[RawTarget], src_w: f64, src_h: f64) -> Vec<CamPose> {
    if targets.is_empty() {
        return Vec::new();
    }
    let single = |r: &RawTarget| {
        snap_pose(
            &CamPose {
                t: r.t,
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
            },
            src_w,
            src_h,
        )
    };
    if targets.len() == 1 {
        return vec![single(&targets[0])];
    }
    // Absorb excursions: after a cut, if the path returns to (near) the
    // pre-cut framing quickly, the excursion was flap — hold the pre-cut
    // framing through it instead of swinging over and back. Genuine
    // handoffs never return, so they always survive. Processed
    // left-to-right so chains collapse one blip at a time.
    let n = targets.len();
    let mut work: Vec<RawTarget> = targets.to_vec();
    let mut s = 1;
    while s < n {
        if !work[s].cut || work[s].hard {
            s += 1;
            continue; // hard shot cuts are always kept, never absorbed
        }
        let x0 = work[s - 1].x;
        let jump = (work[s].x - x0).abs();
        let mut e = s + 1;
        while e < n
            && !work[e].hard
            && (work[e].t - work[s].t) <= 1.5
            && (work[e].x - x0).abs() >= (8.0f64).max(0.25 * jump)
        {
            e += 1;
        }
        let returned = e < n && (work[e].t - work[s].t) <= 1.5;
        if jump > 1e-9 && returned && work[e].t - work[s].t < MIN_RUN_S {
            work[s].cut = false;
            for k in s..e {
                work[k].x = work[s - 1].x;
                work[k].y = work[s - 1].y;
                work[k].w = work[s - 1].w;
                work[k].h = work[s - 1].h;
            }
        }
        s += 1;
    }
    // Collapse rapid handoff chains (see CHAIN_S): walk the cuts; when a
    // non-hard single cut's run is shorter than CHAIN_S and the next
    // boundary is another non-hard single cut, the middle stop was banter
    // — hold the pre-chain framing through it. Left-to-right so A→B→C→D
    // collapses iteratively and the settled speaker gets one full glide.
    let mut c = 1;
    while c < n {
        if !work[c].cut || work[c].hard || work[c].n_faces > 1 {
            c += 1;
            continue;
        }
        let mut j = c + 1;
        while j < n && !work[j].cut && !work[j].hard {
            j += 1;
        }
        let run_end_t = if j < n { work[j].t } else { work[n - 1].t };
        let next_is_single = j < n && !work[j].hard && work[j].n_faces <= 1;
        if next_is_single && run_end_t - work[c].t < CHAIN_S {
            work[c].cut = false;
            for k in c..j {
                work[k].x = work[c - 1].x;
                work[k].y = work[c - 1].y;
                work[k].w = work[c - 1].w;
                work[k].h = work[c - 1].h;
            }
        }
        c += 1;
    }
    let targets = &work;
    // Split into continuous runs at cuts (the first run never bridges).
    // Hard shot cuts split too, but never bridge (see below).
    let mut runs: Vec<&[RawTarget]> = Vec::new();
    let mut run_hard: Vec<bool> = vec![false];
    let mut start = 0;
    for i in 1..targets.len() {
        if targets[i].cut || targets[i].hard {
            runs.push(&targets[start..i]);
            run_hard.push(targets[i].hard);
            start = i;
        }
    }
    runs.push(&targets[start..]);

    let dt = EMIT_HZ.recip();
    let mut out: Vec<CamPose> = Vec::new();
    for (ri, run) in runs.iter().enumerate() {
        let t0 = run[0].t;
        let t1 = run[run.len() - 1].t;
        let xs: Vec<f64> = run.iter().map(|r| r.x).collect();
        let ys: Vec<f64> = run.iter().map(|r| r.y).collect();
        let ws: Vec<f64> = run.iter().map(|r| r.w).collect();
        let hs: Vec<f64> = run.iter().map(|r| r.h).collect();
        let span = (t1 - t0).max(1e-9);
        // Exact (float) channel values at any time in this run.
        let at = |t: f64| -> (f64, f64, f64, f64) {
            let p = ((t - t0) / span * (run.len() - 1) as f64).clamp(0.0, (run.len() - 1) as f64);
            (
                cr_sample(&xs, p),
                cr_sample(&ys, p),
                cr_sample(&ws, p).max(2.0),
                cr_sample(&hs, p).max(2.0),
            )
        };
        // Full-rate resample of this run on its own grid.
        let mut grid: Vec<f64> = Vec::new();
        if t1 <= t0 {
            grid.push(t0);
        } else {
            let n = ((t1 - t0) / dt).round() as usize;
            for k in 0..=n {
                grid.push(t0 + k as f64 * dt);
            }
            if *grid.last().unwrap() < t1 - 1e-9 {
                grid.push(t1);
            }
        }
        if ri == 0 || run_hard[ri] {
            // First run, or a fresh shot: trim any tail at/after the
            // boundary, then append resampled with NO bridge (snap, not pan:
            // gliding across a hard cut implies continuity that isn't there).
            out.retain(|p| p.t < t0 - 1e-9);
            for &t in &grid {
                let (x, y, w, h) = at(t);
                out.push(snap_pose(&CamPose { t, x, y, w, h }, src_w, src_h));
            }
            continue;
        }
        // Bezier glide: bridge from the previous run's tail into this run.
        // Trim any tail at/after the cut, then ease (tc, t_end] on the
        // cubic curve and resume the run's own samples after t_end.
        let tc = t0;
        out.retain(|p| p.t < tc - 1e-9);
        let p0 = out.last().cloned().unwrap_or_else(|| single(&run[0]));
        // Big moves get more time (constant peak velocity); never shorter
        // than a flap (absorption guarantees real runs exceed it).
        let (eex, _, eew, _) = at(t1);
        let dist = ((eex + eew / 2.0) - (p0.x + p0.w / 2.0)).abs();
        let dur = (0.5 + dist / 600.0).clamp(0.5, 1.25);
        let t_end = (tc + dur).min(t1);
        if t_end > tc + 1e-9 {
            let (ex, ey, ew, eh) = at(t_end);
            // Directed zoom: closeup-to-closeup handoffs arrive with a
            // push-in — a sine bump over the ease (zero at both ends, so
            // the seam is seamless and the landing exact). Moves that
            // change framing class (wide<->track dollies) ride straight —
            // the dolly itself is the move there.
            let base_w = src_h * 9.0 / 16.0;
            let push = ew < base_w * 0.95 && p0.w < base_w * 0.95 && ew > p0.w * 0.5;
            let aw = if push {
                (ew * 1.3).min(base_w) - ew
            } else {
                0.0
            };
            let ah = if push {
                (eh * 1.3).min(src_h) - eh
            } else {
                0.0
            };
            let (cx0, cx1) = (p0.x + p0.w / 2.0, ex + ew / 2.0);
            let span = (t_end - tc).max(1e-9);
            let mut t = tc + dt;
            while t < t_end - 1e-9 {
                let u = (t - tc) / span;
                let e = ease_in_out(u);
                let bump = (std::f64::consts::PI * u).sin();
                let w = p0.w + (ew - p0.w) * e + aw * bump;
                let h = p0.h + (eh - p0.h) * e + ah * bump;
                let cx = cx0 + (cx1 - cx0) * e;
                let x = (cx - w / 2.0).clamp(0.0, (src_w - w).max(0.0));
                let y = (p0.y + (ey - p0.y) * e).clamp(0.0, (src_h - h).max(0.0));
                out.push(snap_pose(&CamPose { t, x, y, w, h }, src_w, src_h));
                t += dt;
            }
        }
        for &t in &grid {
            if t >= t_end - 1e-9 {
                let (x, y, w, h) = at(t);
                out.push(snap_pose(&CamPose { t, x, y, w, h }, src_w, src_h));
            }
        }
    }
    out
}

/// Track the primary speaker and segment the timeline.
/// Returns smoothed states + Track/Wide segments (short wides <1.5s are
/// absorbed so the framing never strobes).
pub fn plan_tracks(
    tracker: &mut Tracker,
    ffmpeg: &Path,
    source: &Path,
    src_w: u32,
    src_h: u32,
    duration: f64,
    range: Option<(f64, f64)>,
    words: &[Word],
    // Forced jump-cut times (source clock, sorted): tighten/merge
    // boundaries inside the tracked range. They snap exactly like shot
    // cuts, but identity persists (same scene).
    cuts: &[f64],
    // Emphasis punch windows (source clock): brief eased zoom bumps.
    punches: &[(f64, f64)],
    // Serve hooks (`None` on the CLI): per-frame progress + cancel.
    progress: Option<crate::progress::SharedPct>,
    cancel: &crate::progress::CancelFlag,
) -> anyhow::Result<Tracked> {
    // Sample as fast as practical: DirectML does 15fps comfortably, CPU
    // YuNet + pipe decode ~8fps. 480px-wide samples (vs 320) resolve small
    // faces with far less box jitter; detections stay sparse (cheap) and the
    // camera path resamples to EMIT_HZ (30Hz) with bezier handoff glides.
    let fps: u32 = if tracker.gpu { 15 } else { 8 };
    let sample_w: u32 = 480;
    let sample_h: u32 =
        ((src_h as f64 * sample_w as f64 / src_w as f64).round() as u32).max(2) & !1;
    // Track only the requested span (picked clip ranges, not the source).
    let (r0, r1) = range.unwrap_or((0.0, duration));
    let frames = sample_frames(
        ffmpeg,
        source,
        fps,
        sample_w,
        sample_h,
        Some((r0, (r1 - r0).max(0.5))),
    )?;
    let sx = src_w as f64 / sample_w as f64;

    let crop_w = src_h as f64 * 9.0 / 16.0;
    let center = (src_w as f64 - crop_w).max(0.0) / 2.0;
    let (src_wf, src_hf) = (src_w as f64, src_h as f64);
    let cut_dist = CUT_FRAC * src_wf;

    let mut primary: Option<Face> = None;
    let mut last_seen_t = f64::NEG_INFINITY;
    // Challenger waiting to dethrone the primary (face + consecutive wins).
    let mut pending: Option<(Face, u32)> = None;
    // No ping-pong: a fresh switch holds the floor ~2.5s (a full turn of
    // banter) unless the challenger holds ~0.75s of consecutive wins (a
    // real turn-take, not alternation). Isolated handoffs are instant
    // regardless.
    let mut last_switch_t = f64::NEG_INFINITY;
    // Last hard shot cut (drives the post-cut settle window).
    let mut last_shot_t = f64::NEG_INFINITY;
    // Next forced jump cut to fire.
    let mut cut_idx = 0;
    // Previous sample (pixels + kept boxes) for temporal confirm + mouth.
    let mut prev_rgb: Option<Vec<u8>> = None;
    let mut prev_kept: Vec<Face> = Vec::new();
    // Previous frame hash for shot-cut detection.
    let mut prev_hash: Option<u64> = None;
    let mut raw: Vec<RawTarget> = Vec::new();
    let mut seen: Vec<(f64, bool)> = Vec::new();
    let mut ever_seen = false;
    let mut group_samples = 0u32;
    // Denoiser state + last pushed target (dropouts hold it).
    let mut ema: Option<(f64, f64, f64, f64)> = None;
    let mut prev_raw: Option<(f64, f64, f64, f64)> = None;
    // Median-3 spike rejector over raw windows (lone box jumps never reach
    // the path). Cleared across handoffs and gaps so reacquires stay snappy.
    let mut med: Vec<(f64, f64, f64, f64)> = Vec::with_capacity(5);
    // Previous members (incumbency for span hysteresis).
    let mut prev_members: Vec<Face> = Vec::new();
    // Group state (shared framing while a talking crew holds together).
    let mut group_state = GroupState::default();
    // Speech memory (samples): refreshed while either top face talks.
    let mut speech_live = 0u32;

    let n_frames = frames.len();
    let every = (n_frames / 25).max(1);
    for (fi, (t, rgb)) in frames.iter().enumerate() {
        cancel.check()?;
        // Serve progress (throttled: ~25 updates per range).
        if let Some(p) = progress.as_deref() {
            if fi % every == 0 || fi + 1 == n_frames {
                p(((fi + 1) as f64 / n_frames.max(1) as f64 * 100.0) as u8);
            }
        }
        // Stale identity after 2s unseen: re-pick fresh (avoids latching
        // onto the wrong person when the speaker changes off-screen).
        if *t - last_seen_t > 2.0 {
            primary = None;
            pending = None;
        }
        // Shot change: every identity assumption dies here. Tracking,
        // groups and latches restart in the new shot, and the camera path
        // snaps instead of panning across the cut. Confirmed by BOTH a
        // frame-hash jump AND face discontinuity — the veto keeps handheld
        // pans and laughing fits (same faces, spiky hash) gliding.
        let hash = ahash(rgb, sample_w as usize, sample_h as usize);
        let hash_cut = match prev_hash {
            Some(h) => hamdist(hash, h) > SHOT_CUT_BITS,
            None => false,
        };
        prev_hash = Some(hash);
        // Faces in SAMPLE coords first: temporal confirmation kills lone
        // hallucinations, mouth motion scores who is talking — both need
        // the pixels. Only survivors scale up to source coords.
        let sample_faces = tracker.detect(rgb, sample_w as usize, sample_h as usize)?;
        let kept: Vec<Face> = confirm(sample_faces, &prev_kept);
        let motions: Vec<f64> = match &prev_rgb {
            Some(prev) => kept
                .iter()
                .map(|f| {
                    let same = prev_kept.iter().map(|p| iou(f, p)).fold(0.0f64, f64::max);
                    if same > 0.35 {
                        mouth_motion(prev, rgb, sample_w as usize, sample_h as usize, f)
                    } else {
                        0.0
                    }
                })
                .collect(),
            None => vec![0.0; kept.len()],
        };
        // A hash jump is only a cut when the faces turn over too.
        let shot = hash_cut && !faces_overlap(&kept, &prev_kept);
        if shot {
            primary = None;
            pending = None;
            group_state = GroupState::default();
            speech_live = 0;
            last_switch_t = f64::NEG_INFINITY;
            last_shot_t = *t;
            ema = None;
            med.clear();
        }
        // Forced jump cut: same snap, no identity reset (med/EMA reset via
        // `cut` below; the settle window stays dark — no wide flash).
        let forced = cut_idx < cuts.len() && *t >= cuts[cut_idx] - 1e-9;
        prev_rgb = Some(rgb.clone());
        prev_kept = kept.clone();
        let faces: Vec<Face> = kept
            .into_iter()
            .map(|mut f| {
                f.x *= sx;
                f.y *= sx;
                f.w *= sx;
                f.h *= sx;
                f
            })
            .collect();
        // Two-shot candidate: a real pair sharing the scene while the
        // conversation is alive. Entry is strict (both confident, distinct
        // and moving now); holding is lenient (2s speech memory covers
        // pauses and turn-taking). Duplicates and crowds stay single.
        let mut scored: Vec<(&Face, f64)> = faces
            .iter()
            .zip(motions.iter())
            .map(|(f, m)| (f, *m))
            .collect();
        scored.sort_by(|a, b| {
            (b.0.score + b.1)
                .partial_cmp(&(a.0.score + a.1))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Group candidate: the talking cluster shares one window (if it
        // fits); holding is lenient via speech memory so turn-taking and
        // pauses don't strobe. Loners, duplicates and spread crews stay
        // single.
        let members = cluster_members(&scored, crop_w, &prev_members);
        prev_members = members.iter().map(|(f, _)| (*f).clone()).collect();
        // Count now: `members` borrows `faces`, which contender may move.
        let n_group = members.len();
        let all_talking = members.len() >= 2 && members.iter().all(|(_, m)| *m > 0.12);
        if all_talking {
            speech_live = 30;
        } else if speech_live > 0 {
            speech_live -= 1;
        }
        let group_candidate: Option<(f64, f64, f64, f64)> = if members.len() >= 2 && speech_live > 0
        {
            let fs: Vec<&Face> = members.iter().map(|(f, _)| *f).collect();
            let w = group_window(&fs, src_wf, src_hf, crop_w);
            if w.is_some() {
                let mut s = format!(
                    "group t={:.2} n={}:",
                    raw.last().map(|r| r.t).unwrap_or(0.0),
                    members.len()
                );
                for (f, m) in &members {
                    s.push_str(&format!(
                        " [{:.0},{:.0} {:.0}x{:.0} s={:.2} m={:.2}]",
                        f.x, f.y, f.w, f.h, f.score, *m
                    ));
                }
                tracing::debug!("{s}");
            }
            w
        } else {
            None
        };
        // Transcript gate: reframes freeze while nobody is audibly speaking.
        // Presence still counts (no Wide strobing), identity is retained —
        // the camera just holds instead of chasing quiet faces.
        let frozen = !speech_active(words, *t);
        if frozen {
            pending = None;
        }
        let (group_now, group_edge) = if frozen {
            (group_state.on, false)
        } else {
            group_state.step(group_candidate.is_some())
        };
        if group_now {
            group_samples += 1;
        }
        // Contender = persistence + mouth + detector score. A different
        // face must win twice running before it dethrones the primary, so a
        // single-sample teleport detection can never take over the camera.
        // Breaking a fresh 2.5s lock takes SUSTAINED winning (~0.75s of
        // consecutive wins, not 4 lucky samples): overlapping banter no
        // longer whips A→B→C in under a second.
        let contender: Option<Face> = if faces.is_empty() {
            None
        } else if let Some(prev) = &primary {
            let mut best: Option<(Face, f64)> = None;
            for (i, f) in faces.iter().enumerate() {
                let score = f.score * 1.5 + iou(f, prev) + motions[i] * 2.5;
                if best.as_ref().map(|(_, b)| score > *b).unwrap_or(true) {
                    best = Some((f.clone(), score));
                }
            }
            best.map(|(f, _)| f)
        } else {
            faces
                .into_iter()
                .enumerate()
                .max_by(|(ia, a), (ib, b)| {
                    (a.score * (1.0 + motions[*ia]) * a.w * a.h)
                        .partial_cmp(&(b.score * (1.0 + motions[*ib]) * b.w * b.h))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(_, f)| f)
        };
        let pick: Option<Face> = match (&primary, contender) {
            (_, None) => {
                pending = None;
                None
            }
            (None, c) => {
                pending = None;
                c
            }
            (Some(prev), Some(c)) if iou(&c, prev) >= 0.2 => {
                pending = None;
                Some(c)
            }
            (Some(prev), Some(c)) => match &pending {
                Some((p, hits))
                    if iou(&c, p) > 0.25 || (c.cx() - p.cx()).abs() < PENDING_FRAC * src_wf =>
                {
                    // Sustained floor-hold: ~0.75s of consecutive wins to
                    // steal inside the lock (12 hits @15fps, 6 @8fps).
                    let steal_hits: u32 = ((0.75 * fps as f64).ceil() as u32).max(4);
                    if *hits + 1 >= 2 && (*t - last_switch_t >= 2.5 || *hits + 1 >= steal_hits) {
                        pending = None;
                        last_switch_t = *t;
                        Some(c)
                    } else {
                        pending = Some((c, *hits + 1));
                        Some(prev.clone())
                    }
                }
                _ => {
                    pending = Some((c, 1));
                    Some(prev.clone())
                }
            },
        };

        let face = pick
            .as_ref()
            .map(|f| (f.cx(), f.y + f.h / 2.0, f.h / src_hf));
        let pick_cx = face.map(|(cx, _, _)| cx).unwrap_or(f64::NAN);
        let pick_score = pick.as_ref().map(|f| f.score).unwrap_or(0.0);
        let has_face = face.is_some() || group_now;
        if has_face {
            ever_seen = true;
            last_seen_t = *t;
        }
        // Retain identity through gaps (the 2s stale-reset expires it), but
        // never reframe on silence: frozen samples keep the old identity.
        // Wiping on every faceless sample would defeat the challenger gate
        // and let teleport detections take over after any dropout.
        if !frozen && pick.is_some() {
            primary = pick;
        }
        // Raw window target (float, unrounded). Silence holds the last
        // target; a talking group shares one window; dropouts hold too —
        // no drift home, no return glide, no zoom on quiet faces.
        // `grouped` is true only when THIS target frames the group (not
        // merely when faces are present) — it drives the n_faces flag.
        // Post-cut settle (SETTLE_S): land wide, confirm behind it, glide
        // once to the settled speaker — never snap to an unconfirmed pick.
        let settling = *t - last_shot_t < SETTLE_S;
        let (gx, gy, gw, gh, grouped) = if settling {
            (center, 0.0, crop_w, src_hf, false)
        } else if frozen {
            let (hx, hy, hw, hh) = prev_raw.unwrap_or((center, 0.0, crop_w, src_hf));
            (hx, hy, hw, hh, false)
        } else if group_now {
            match group_candidate {
                Some((dx, dy, dw, dh)) => (dx, dy, dw, dh, true),
                None => {
                    let (hx, hy, hw, hh) = prev_raw.unwrap_or((center, 0.0, crop_w, src_hf));
                    (hx, hy, hw, hh, false)
                }
            }
        } else {
            match face {
                Some((cx, cy, frac)) => {
                    let (wx, wy, ww, wh) =
                        window_for_face(cx, cy, frac, src_wf, src_hf, crop_w, pick_score);
                    (wx, wy, ww, wh, false)
                }
                None => {
                    let (hx, hy, hw, hh) = prev_raw.unwrap_or((center, 0.0, crop_w, src_hf));
                    (hx, hy, hw, hh, false)
                }
            }
        };
        // Speaker handoff (or group enter/exit): the desired window jumps.
        // A shot change always cuts hard (snap, never glide).
        let ncx = gx + gw / 2.0;
        let cut = shot
            || forced
            || group_edge
            || match (prev_raw, has_face) {
                (Some((px, _, pw, _)), true) => (ncx - (px + pw / 2.0)).abs() > cut_dist,
                _ => false,
            };
        // Median-3 first: lone spikes die here, settled motion passes through
        // (median-5's extra ~130ms of attack lag read as "laggy"; the 2-win
        // challenger gate already rejects single-sample teleports).
        if cut || !has_face {
            med.clear();
        }
        med.push((gx, gy, gw, gh));
        if med.len() > 3 {
            med.remove(0);
        }
        let median = |get: fn(&(f64, f64, f64, f64)) -> f64| -> f64 {
            let mut v: Vec<f64> = med.iter().map(get).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            v[v.len() / 2]
        };
        let (mx, my, mw, mh) = (
            median(|q| q.0),
            median(|q| q.1),
            median(|q| q.2),
            median(|q| q.3),
        );
        // Soft EMA after it (reset across handoffs so the glide starts clean).
        let (fx, fy, fw, fh) = match (ema, cut) {
            (Some((ex, ey, ew, eh)), false) => (
                DENOISE_ALPHA * mx + (1.0 - DENOISE_ALPHA) * ex,
                DENOISE_ALPHA * my + (1.0 - DENOISE_ALPHA) * ey,
                DENOISE_ALPHA * mw + (1.0 - DENOISE_ALPHA) * ew,
                DENOISE_ALPHA * mh + (1.0 - DENOISE_ALPHA) * eh,
            ),
            _ => (mx, my, mw, mh),
        };
        ema = Some((fx, fy, fw, fh));
        prev_raw = Some((fx, fy, fw, fh));
        raw.push(RawTarget {
            t: *t,
            x: fx,
            y: fy,
            w: fw,
            h: fh,
            cut,
            hard: shot || forced,
            n_faces: if grouped { n_group } else { 1 },
            pick_cx,
        });
        seen.push((*t, has_face));
        if forced {
            cut_idx += 1;
        }
    }
    if raw.is_empty() {
        seen.push((r0, false));
    }

    // Emphasis punch-ins: ease a brief zoom bump over each window. The
    // bump is C1-smooth (raised cosine, zero slope at both ends) so the
    // resample carries it as butter, not steps. Groups and cut
    // neighborhoods (±0.8s) are exempt — never punch a crowd or a cut.
    if !punches.is_empty() && !raw.is_empty() {
        let cut_ts: Vec<f64> = raw
            .iter()
            .filter(|r| r.cut || r.hard)
            .map(|r| r.t)
            .collect();
        for (p0, p1) in punches {
            let mut n = 0u32;
            for r in raw.iter_mut() {
                if r.n_faces > 1 {
                    continue;
                }
                if cut_ts.iter().any(|c| (c - r.t).abs() < 0.8) {
                    continue;
                }
                if r.t >= *p0 && r.t <= *p1 && *p1 > *p0 {
                    let u = (r.t - p0) / (p1 - p0);
                    let bump = (std::f64::consts::PI * u).sin().powi(2);
                    let k = 1.0 - crate::punch::PUNCH_DEPTH * bump;
                    let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
                    r.w = (r.w * k).max(2.0);
                    r.h = (r.h * k).max(2.0);
                    r.x = cx - r.w / 2.0;
                    r.y = cy - r.h / 2.0;
                    n += 1;
                }
            }
            tracing::info!("punch applied {p0:.1}s-{p1:.1}s ({n} samples)");
        }
    }

    let end = r1.max(raw.last().map(|s| s.t).unwrap_or(r1));
    let merged = build_segments(&seen, end);
    let group_secs = group_samples as f64 / fps as f64;

    Ok(Tracked {
        segments: merged,
        ever_seen,
        raw,
        group_secs,
    })
}

/// Build Track/Wide segments from per-sample face flags.
/// Pure (unit-tested): hysteresis via majority vote, short wides absorbed.
pub fn build_segments(seen: &[(f64, bool)], end: f64) -> Vec<Seg> {
    if seen.is_empty() {
        return vec![Seg {
            t0: 0.0,
            t1: end.max(0.5),
            kind: SegKind::Track,
        }];
    }
    // Hysteresis: majority vote over ±2 samples so single-frame detector
    // flicker can't strobe the framing mode.
    let n = seen.len();
    let filt: Vec<(f64, bool)> = seen
        .iter()
        .enumerate()
        .map(|(i, (t, _))| {
            let lo = i.saturating_sub(2);
            let hi = (i + 3).min(n);
            let votes = seen[lo..hi].iter().filter(|(_, f)| *f).count();
            (*t, votes * 2 >= hi - lo)
        })
        .collect();

    let flush = |segments: &mut Vec<Seg>, t0: f64, t1: f64, face: bool| {
        if t1 > t0 {
            segments.push(Seg {
                t0,
                t1,
                kind: if face { SegKind::Track } else { SegKind::Wide },
            });
        }
    };
    let mut segments = Vec::new();
    let mut run_start = 0.0f64;
    let mut run_face = filt[0].1;
    for (t, face) in filt.iter().skip(1) {
        if *face != run_face {
            flush(&mut segments, run_start, *t, run_face);
            run_start = *t;
            run_face = *face;
        }
    }
    flush(&mut segments, run_start, end, run_face);
    // Absorb short wides.
    let mut merged: Vec<Seg> = Vec::new();
    for seg in segments {
        let short_wide = seg.kind == SegKind::Wide && seg.t1 - seg.t0 < 1.5 && !merged.is_empty();
        if short_wide {
            if let Some(prev) = merged.last_mut() {
                prev.t1 = seg.t1;
                continue;
            }
        }
        // Merge same-kind neighbors.
        if let Some(prev) = merged.last_mut() {
            if prev.kind == seg.kind {
                prev.t1 = seg.t1;
                continue;
            }
        }
        merged.push(seg);
    }
    // A leading short wide has no Track to join — drop it into the next.
    if merged.len() > 1 && merged[0].kind == SegKind::Wide && merged[0].t1 - merged[0].t0 < 1.5 {
        let t1 = merged[0].t1;
        merged.remove(0);
        merged[0].t0 = merged[0].t0.min(t1);
    }

    merged
}

/// Resample a window-left series to `out_hz` with Catmull-Rom interpolation.
/// Detections stay sparse (cheap) while the rendered camera moves every
/// frame — like a face filter, not a slideshow. Clamped to >=0 (upper
/// clamp happens against max_x at emit time).
pub fn resample(states: &[TrackPoint], out_hz: f64) -> Vec<TrackPoint> {
    if states.len() < 2 || out_hz <= 0.0 {
        return states.to_vec();
    }
    let t0 = states[0].t;
    let t1 = states[states.len() - 1].t;
    if t1 <= t0 {
        return states.to_vec();
    }
    let n = ((t1 - t0) * out_hz).round() as usize + 1;
    let at = |i: isize| -> f64 { states[i.clamp(0, states.len() as isize - 1) as usize].x };
    let mut out = Vec::with_capacity(n.min(8192));
    // Locate the segment by index (states are uniform at sample fps).
    let dt = (t1 - t0) / (states.len() - 1) as f64;
    for k in 0..n {
        let t = t0 + k as f64 / out_hz;
        let pos = ((t - t0) / dt).clamp(0.0, (states.len() - 1) as f64);
        let i = (pos.floor() as usize).min(states.len() - 2);
        let u = pos - i as f64;
        let (p0, p1, p2, p3) = (
            at(i as isize - 1),
            at(i as isize),
            at(i as isize + 1),
            at(i as isize + 2),
        );
        // Catmull-Rom, then clamp (avoids overshoot past frame edges).
        let x = 0.5
            * ((2.0 * p1)
                + (-p0 + p2) * u
                + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * u * u
                + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * u * u * u);
        out.push(TrackPoint { t, x: x.max(0.0) });
        if out.len() >= 8192 {
            break;
        }
    }
    out
}
///
/// Returns `(initial_x, commands)` where `commands` is the file body:
/// one `{t} crop x {x};` line per track point. `offset` shifts plan times
/// (clip renders seek with `-ss`, which resets frame timestamps to 0, so
/// clip-absolute times must be shifted back by the clip start).
/// Points shifted below 0 collapse onto `t=0` in file order.
pub fn crop_commands(plan: &CropPlan, max_x: f64, offset: f64) -> (f64, String) {
    if plan.tracks.is_empty() {
        return (max_x / 2.0, String::new());
    }
    // Sparse plans (external files, low sample rates) are upsampled to a
    // 30Hz schedule so the camera moves every frame instead of stepping.
    // Dense native plans pass through untouched.
    let native_hz = if plan.tracks.len() > 1 {
        let span = (plan.tracks[plan.tracks.len() - 1].t - plan.tracks[0].t).max(1e-6);
        (plan.tracks.len() - 1) as f64 / span
    } else {
        0.0
    };
    let pts: Vec<TrackPoint> = if native_hz < EMIT_HZ - 0.1 {
        resample(&plan.tracks, EMIT_HZ)
    } else {
        plan.tracks.clone()
    };
    // Bound absurd schedules (10min+ full-video renders).
    let pts: Vec<TrackPoint> = if pts.len() > 16384 {
        pts.into_iter().step_by(2).collect()
    } else {
        pts
    };
    let mut lines = String::with_capacity(pts.len() * 24);
    let mut initial_x = pts[0].x.clamp(0.0, max_x);
    for p in &pts {
        let t = (p.t - offset).max(0.0);
        let x = p.x.clamp(0.0, max_x);
        if p.t - offset <= 0.0 {
            initial_x = x;
        }
        lines.push_str(&format!("{t:.3} crop x {x:.1};\n"));
    }
    (initial_x, lines)
}

/// Catmull-Rom resample of poses to `out_hz` (x, y, w, h channels).
/// Sparse detections become a filter-smooth schedule.
pub fn resample_poses(poses: &[CamPose], out_hz: f64) -> Vec<CamPose> {
    if poses.len() < 2 || out_hz <= 0.0 {
        return poses.to_vec();
    }
    let t0 = poses[0].t;
    let t1 = poses[poses.len() - 1].t;
    if t1 <= t0 {
        return poses.to_vec();
    }
    let n = ((t1 - t0) * out_hz).round() as usize + 1;
    let dt = (t1 - t0) / (poses.len() - 1) as f64;
    let chan = |get: fn(&CamPose) -> f64, pos: f64| -> f64 {
        let i = (pos.floor() as usize).min(poses.len() - 2);
        let u = pos - i as f64;
        let at = |k: isize| -> f64 { get(&poses[k.clamp(0, poses.len() as isize - 1) as usize]) };
        let (p0, p1, p2, p3) = (
            at(i as isize - 1),
            at(i as isize),
            at(i as isize + 1),
            at(i as isize + 2),
        );
        0.5 * ((2.0 * p1)
            + (-p0 + p2) * u
            + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * u * u
            + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * u * u * u)
    };
    let mut out = Vec::with_capacity(n.min(8192));
    for k in 0..n {
        let t = t0 + k as f64 / out_hz;
        let pos = ((t - t0) / dt).clamp(0.0, (poses.len() - 1) as f64);
        out.push(CamPose {
            t,
            x: chan(|p| p.x, pos).max(0.0),
            y: chan(|p| p.y, pos).max(0.0),
            w: chan(|p| p.w, pos).max(2.0),
            h: chan(|p| p.h, pos).max(2.0),
        });
        if out.len() >= 8192 {
            break;
        }
    }
    out
}

/// Snap a pose to encoder-safe ints (even w/h) within the frame.
pub fn snap_pose(p: &CamPose, src_w: f64, src_h: f64) -> CamPose {
    let w = ((p.w / 2.0).round() * 2.0).clamp(2.0, src_w);
    let h = ((p.h / 2.0).round() * 2.0).clamp(2.0, src_h);
    CamPose {
        t: p.t,
        x: p.x.round().clamp(0.0, (src_w - w).max(0.0)),
        y: p.y.round().clamp(0.0, (src_h - h).max(0.0)),
        w,
        h,
    }
}

/// Build a full-geometry `sendcmd` schedule from poses.
/// Returns `(initial_pose, commands)`; `offset` shifts times (clip seeks
/// reset frame timestamps to 0). Sparse series ride Catmull-Rom up to
/// 30Hz; near-duplicate timestamps are dropped (sendcmd steps discretely,
/// so sub-frame dups would only add stepping noise).
pub fn pose_commands(poses: &[CamPose], src_w: f64, src_h: f64, offset: f64) -> (CamPose, String) {
    let base = CamPose {
        t: 0.0,
        x: (src_w - src_h * 9.0 / 16.0).max(0.0) / 2.0,
        y: 0.0,
        w: src_h * 9.0 / 16.0,
        h: src_h,
    };
    if poses.is_empty() {
        return (snap_pose(&base, src_w, src_h), String::new());
    }
    let native_hz = if poses.len() > 1 {
        let span = (poses[poses.len() - 1].t - poses[0].t).max(1e-6);
        (poses.len() - 1) as f64 / span
    } else {
        0.0
    };
    let mut pts: Vec<CamPose> = if native_hz < EMIT_HZ - 0.1 {
        resample_poses(poses, EMIT_HZ)
    } else {
        poses.to_vec()
    };
    if pts.len() > 16384 {
        pts = pts.into_iter().step_by(2).collect();
    }
    let mut lines = String::with_capacity(pts.len() * 72);
    let mut initial = snap_pose(&pts[0], src_w, src_h);
    let mut last_t = f64::NEG_INFINITY;
    for p in &pts {
        let t = (p.t - offset).max(0.0);
        if t - last_t < 0.005 && last_t > f64::NEG_INFINITY / 2.0 {
            continue; // sub-frame dup: sendcmd would step twice, not glide
        }
        last_t = t;
        let q = snap_pose(p, src_w, src_h);
        if p.t - offset <= 0.0 {
            initial = q.clone();
        }
        lines.push_str(&format!(
            "{t:.3} crop x {};{t:.3} crop y {};{t:.3} crop w {};{t:.3} crop h {};\n",
            q.x as i64, q.y as i64, q.w as i64, q.h as i64
        ));
    }
    (initial, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_run(x0: f64, x1: f64, n: usize, dt: f64, cut_at: Option<usize>) -> Vec<RawTarget> {
        (0..n)
            .map(|i| {
                let u = if n > 1 {
                    i as f64 / (n - 1) as f64
                } else {
                    0.0
                };
                RawTarget {
                    t: i as f64 * dt,
                    x: x0 + (x1 - x0) * u,
                    y: 10.0,
                    w: 150.0,
                    h: 260.0,
                    cut: cut_at == Some(i),
                    hard: false,
                    n_faces: 1,
                    pick_cx: x0 + (x1 - x0) * u,
                }
            })
            .collect()
    }

    #[test]
    fn bezier_ease_has_slow_attack_fast_middle_gentle_landing() {
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(1.0), 1.0);
        let (mut prev, mut monotonic) = (0.0, true);
        for k in 1..=20 {
            let v = ease_in_out(k as f64 / 20.0);
            monotonic &= v >= prev;
            prev = v;
        }
        assert!(monotonic, "ease must be monotonic");
        let attack = ease_in_out(0.1) - ease_in_out(0.0);
        let middle = ease_in_out(0.5) - ease_in_out(0.4);
        let landing = ease_in_out(1.0) - ease_in_out(0.9);
        assert!(attack < middle, "slow attack: {attack} vs {middle}");
        assert!(landing < middle, "gentle landing: {landing} vs {middle}");
        // Identity control points reproduce the line.
        assert!((cubic_bezier(0.0, 0.0, 1.0, 1.0, 0.37) - 0.37).abs() < 1e-6);
    }

    #[test]
    fn continuous_run_is_butter_smooth() {
        // 200px pan over 1s at 15Hz samples, no cuts.
        let path = smooth_path(&raw_run(100.0, 300.0, 16, 1.0 / 15.0, None), 640.0, 360.0);
        assert!(
            (path.len() as f64 - 30.0).abs() <= 2.0,
            "30Hz emit, got {}",
            path.len()
        );
        let mut max_step = 0.0f64;
        for w in path.windows(2) {
            assert!(w[1].t > w[0].t, "times must stay monotonic");
            max_step = max_step.max((w[1].x - w[0].x).abs());
        }
        // 200px/s eased ≈ 6.7px/frame; anything near a 12Hz-style 17px+ jump fails.
        assert!(max_step < 12.0, "per-frame step too big: {max_step}");
        assert!(
            (path.last().unwrap().x - 300.0).abs() < 3.0,
            "must land on target"
        );
    }

    #[test]
    fn handoff_glides_on_bezier_without_jumps() {
        // Static left, then a hard cut to the right (speaker change).
        // Run B is long enough for the full distance-scaled glide.
        let mut tg = raw_run(100.0, 100.0, 6, 1.0 / 15.0, None);
        let mut right = raw_run(450.0, 450.0, 18, 1.0 / 15.0, Some(0));
        for r in &mut right {
            r.t += 6.0 / 15.0;
        }
        tg.extend(right);
        let path = smooth_path(&tg, 640.0, 360.0);
        let mut max_step = 0.0f64;
        for w in path.windows(2) {
            assert!(w[1].t > w[0].t, "times must stay monotonic");
            max_step = max_step.max((w[1].x - w[0].x).abs());
        }
        // 350px over a 0.7s eased glide peaks ~27px/frame — a step/chase
        // follower would jump 100px+ in a single frame here.
        assert!(max_step < 45.0, "handoff must glide, max step {max_step}");
        // Slow attack: no jumps anywhere in the first 0.2s of the glide
        // (the push-in widens first, so the crossing point isn't the metric).
        let tc = 6.0 / 15.0;
        for wn in path.windows(2) {
            if wn[0].t >= tc - 1e-9 && wn[1].t <= tc + 0.2 + 1e-9 {
                assert!(
                    (wn[1].x - wn[0].x).abs() < 8.0,
                    "slow attack step at t={}: {} -> {}",
                    wn[1].t,
                    wn[0].x,
                    wn[1].x
                );
            }
        }
        assert!(
            (path.last().unwrap().x - 450.0).abs() < 3.0,
            "must land on new speaker"
        );
    }

    #[test]
    fn rapid_chain_collapses_to_single_glide() {
        // A→B→C inside 0.55s (banter, never returning): B is a middle stop,
        // not a settled speaker — the camera holds A, then rides ONE full
        // glide to C. No stacked compressed whips.
        let mk = |t: f64, x: f64, cut: bool| RawTarget {
            t,
            x,
            y: 10.0,
            w: 150.0,
            h: 260.0,
            cut,
            hard: false,
            n_faces: 1,
            pick_cx: x + 75.0,
        };
        let tg = vec![
            mk(0.0, 80.0, false),
            mk(0.15, 80.0, false),
            mk(0.30, 360.0, true), // B: 0.25s stop, hands off again
            mk(0.45, 360.0, false),
            mk(0.55, 420.0, true), // C: settles (keeps going, no return)
            mk(0.80, 420.0, false),
            mk(1.10, 420.0, false),
            mk(1.50, 420.0, false),
            mk(2.00, 420.0, false),
        ];
        let path = smooth_path(&tg, 640.0, 360.0);
        // B's span holds A: nothing moves before C's cut.
        for p in path.iter().filter(|p| p.t < 0.5) {
            assert!(
                (p.x - 80.0).abs() < 3.0,
                "banter stop leaked at t={}: x={}",
                p.t,
                p.x
            );
        }
        let mut max_step = 0.0f64;
        for w in path.windows(2) {
            assert!(w[1].t > w[0].t, "times must stay monotonic");
            max_step = max_step.max((w[1].x - w[0].x).abs());
        }
        assert!(
            max_step < 45.0,
            "chain must glide once, max step {max_step}"
        );
        assert!(
            (path.last().unwrap().x - 420.0).abs() < 3.0,
            "must land on settled speaker"
        );
    }

    #[test]
    fn zoom_channel_eases_with_position() {
        let mut tg: Vec<RawTarget> = (0..16)
            .map(|i| RawTarget {
                t: i as f64 / 15.0,
                x: 200.0,
                y: 10.0,
                w: 200.0 - i as f64 * 5.0, // punch in over the run
                h: 360.0 - i as f64 * 9.0,
                cut: false,
                hard: false,
                n_faces: 1,
                pick_cx: 320.0,
            })
            .collect();
        let _ = &mut tg;
        let path = smooth_path(&tg, 640.0, 360.0);
        let mut prev_w = path[0].w;
        for p in &path[1..] {
            assert!(
                p.w <= prev_w + 1e-9,
                "zoom must ease monotonically in, {} -> {}",
                prev_w,
                p.w
            );
            prev_w = p.w;
        }
        assert!(path.last().unwrap().w < path[0].w, "must punch in overall");
    }

    #[test]
    fn cluster_members_wants_close_confident_crews() {
        let mk = |x: f64, w: f64, score: f64| Face {
            x,
            y: 100.0,
            w,
            h: 80.0,
            score,
        };
        let base_w = 202.5; // budget = 151.9
                            // Near-duplicate boxes on one face (survive NMS, still one face).
        let a = mk(200.0, 60.0, 0.9);
        let b = mk(240.0, 60.0, 0.8);
        assert!(iou(&a, &b) < 0.3, "test setup: must survive NMS");
        assert!(cluster_members(&[(&a, 0.5), (&b, 0.5)], base_w, &[]).is_empty());
        // Low-confidence second: clutter, never joins.
        let c = mk(320.0, 60.0, 0.9);
        let d = mk(380.0, 60.0, 0.55);
        assert!(cluster_members(&[(&c, 0.5), (&d, 0.5)], base_w, &[]).is_empty());
        // Close confident pair: the classic two-shot.
        let e = mk(220.0, 60.0, 0.9);
        let f = mk(300.0, 60.0, 0.85);
        assert_eq!(
            cluster_members(&[(&e, 0.5), (&f, 0.4)], base_w, &[]).len(),
            2
        );
        // Trio in range: all three belong.
        let g = mk(250.0, 40.0, 0.9);
        let h = mk(290.0, 40.0, 0.85);
        let i = mk(330.0, 40.0, 0.8);
        assert_eq!(
            cluster_members(&[(&g, 0.5), (&h, 0.5), (&i, 0.5)], base_w, &[]).len(),
            3,
            "close trio all join"
        );
        // Four clustered talkers: no head-count cap.
        let q = mk(230.0, 30.0, 0.9);
        let r = mk(270.0, 30.0, 0.88);
        let s = mk(310.0, 30.0, 0.85);
        let u = mk(350.0, 30.0, 0.82);
        assert_eq!(
            cluster_members(&[(&q, 0.5), (&r, 0.5), (&s, 0.5), (&u, 0.5)], base_w, &[]).len(),
            4
        );
        // Far fourth: neither joins nor vetoes the close trio.
        let k = mk(500.0, 60.0, 0.9);
        let crew = cluster_members(&[(&g, 0.5), (&h, 0.5), (&i, 0.5), (&k, 0.5)], base_w, &[]);
        assert_eq!(crew.len(), 3);
        assert!(crew.iter().all(|(f, _)| f.x < 400.0));
        // Giant close-up face plus a small one: span blown, stay single.
        let big = Face {
            x: 100.0,
            y: 50.0,
            w: 300.0,
            h: 250.0,
            score: 0.95,
        };
        let small = mk(450.0, 50.0, 0.9);
        assert!(cluster_members(&[(&big, 0.5), (&small, 0.5)], base_w, &[]).is_empty());
    }

    #[test]
    fn cluster_holds_incumbents_on_the_boundary() {
        // Pair spanning 180px: too wide to FORM (strict 151.9) but fine to
        // HOLD (loose 182.25) — boundary breathing must not flap the group.
        let base_w = 202.5;
        let a = Face {
            x: 200.0,
            y: 100.0,
            w: 60.0,
            h: 80.0,
            score: 0.9,
        };
        let b = Face {
            x: 320.0,
            y: 100.0,
            w: 60.0,
            h: 80.0,
            score: 0.9,
        };
        assert!(cluster_members(&[(&a, 0.5), (&b, 0.5)], base_w, &[]).is_empty());
        let ka = Face {
            x: 205.0,
            y: 100.0,
            w: 60.0,
            h: 80.0,
            score: 0.9,
        };
        let kb = Face {
            x: 315.0,
            y: 100.0,
            w: 60.0,
            h: 80.0,
            score: 0.9,
        };
        assert_eq!(
            cluster_members(&[(&a, 0.5), (&b, 0.5)], base_w, &[ka, kb]).len(),
            2
        );
    }

    #[test]
    fn group_window_frames_crews_that_fit() {
        let src_w = 640.0;
        let src_h = 360.0;
        let base_w = 202.5;
        let f = |x: f64, w: f64| Face {
            x,
            y: 100.0,
            w,
            h: 80.0,
            score: 0.9,
        };
        // Pair: shared base window, both inside.
        let (a, b) = (f(220.0, 60.0), f(300.0, 60.0));
        let (x, _y, w, h) = group_window(&[&a, &b], src_w, src_h, base_w).expect("must fit");
        assert!((w - base_w).abs() < 1e-9 && (h - src_h).abs() < 1e-9);
        assert!(x <= 220.0 && x + w >= 360.0, "both faces inside: x={x}");
        // Trio: same deal.
        let (g, h2, i) = (f(230.0, 40.0), f(280.0, 40.0), f(330.0, 40.0));
        let (x3, _, _, _) = group_window(&[&g, &h2, &i], src_w, src_h, base_w).expect("trio fits");
        assert!(x3 <= 230.0 && x3 + base_w >= 370.0, "trio inside: x={x3}");
        // Spread crew: no shared window (camera stays on primary).
        let far = f(500.0, 60.0);
        assert!(group_window(&[&a, &far], src_w, src_h, base_w).is_none());
        // Pair at the right edge: the window may overhang the frame (the
        // renderer clamps), but both members must survive inside it.
        let (c, d) = (f(520.0, 50.0), f(580.0, 40.0));
        // NOTE: c faces have different h here; containment is x-only.
        if let Some((x2, _, w2, _)) = group_window(&[&c, &d], src_w, src_h, base_w) {
            assert!(c.x >= x2 - 1.0 && d.x + d.w <= x2 + w2 + 1.0);
        }
        // Single face: never a group.
        assert!(group_window(&[&a], src_w, src_h, base_w).is_none());
    }

    #[test]
    fn group_state_has_slow_hysteresis() {
        let mut s = GroupState::default();
        assert_eq!(s.step(false), (false, false));
        for _ in 0..3 {
            assert_eq!(s.step(true).0, false, "blips must not engage");
        }
        // Fourth straight yes: on + edge (arrival dollies in).
        assert_eq!(s.step(true), (true, true));
        assert_eq!(s.step(true), (true, false));
        // Brief misses don't drop it...
        for _ in 0..7 {
            assert_eq!(s.step(false).0, true);
        }
        // ...but eight straight misses exit with an edge.
        assert_eq!(s.step(false), (false, true));
        assert_eq!(s.step(false), (false, false));
    }

    #[test]
    fn face_zoom_stays_generous() {
        // Small confident face: bounded punch, never a postage stamp.
        let (.., w, h) = window_for_face(320.0, 180.0, 0.15, 640.0, 360.0, 202.5, 0.9);
        assert!(202.5 / w <= 1.3 + 1e-9, "z={}", 202.5 / w);
        assert!(w >= 150.0, "no more postage stamps: w={w}");
        let _ = h;
        // Same face, weak detection: stays wider (no punch on doubt).
        let (.., weak_w, _) = window_for_face(320.0, 180.0, 0.15, 640.0, 360.0, 202.5, 0.5);
        assert!(weak_w > w, "doubt stays wide: {weak_w} vs {w}");
        // Big face: no zoom at all.
        let (.., w2, _) = window_for_face(320.0, 180.0, 0.6, 640.0, 360.0, 202.5, 0.9);
        assert!((w2 - 202.5).abs() < 1e-9);
    }

    #[test]
    fn speech_gate_follows_words() {
        let words = vec![
            Word {
                w: "hey".into(),
                s: 1.0,
                e: 1.4,
                conf: Some(0.9),
            },
            Word {
                w: "you".into(),
                s: 1.6,
                e: 2.0,
                conf: Some(0.9),
            },
        ];
        assert!(speech_active(&words, 1.2));
        assert!(speech_active(&words, 2.05)); // +0.4 padding
        assert!(!speech_active(&words, 2.6)); // real silence: camera holds
        assert!(!speech_active(&words, 0.3));
        assert!(!speech_active(&[], 1.2));
    }

    #[test]
    fn hold_targets_stay_put() {
        let tg = raw_run(250.0, 250.0, 10, 1.0 / 15.0, None);
        let path = smooth_path(&tg, 640.0, 360.0);
        for p in &path {
            assert!((p.x - 250.0).abs() < 3.0, "holds must not wander: {}", p.x);
        }
    }

    #[test]
    fn confirm_kills_lone_hallucinations() {
        let prev = vec![Face {
            x: 100.0,
            y: 100.0,
            w: 50.0,
            h: 50.0,
            score: 0.8,
        }];
        // Same face, low score but overlapping → kept.
        let same = vec![Face {
            x: 102.0,
            y: 101.0,
            w: 50.0,
            h: 50.0,
            score: 0.55,
        }];
        assert_eq!(confirm(same, &prev).len(), 1);
        // Teleport, low score, no overlap → dropped.
        let lone = vec![Face {
            x: 400.0,
            y: 100.0,
            w: 50.0,
            h: 50.0,
            score: 0.55,
        }];
        assert!(confirm(lone, &prev).is_empty());
        // High score anywhere → kept (new speaker entering).
        let fresh = vec![Face {
            x: 400.0,
            y: 100.0,
            w: 50.0,
            h: 50.0,
            score: 0.9,
        }];
        assert_eq!(confirm(fresh, &prev).len(), 1);
        assert!(confirm(vec![], &prev).is_empty());
    }

    #[test]
    fn mouth_motion_sees_talking() {
        // 100x100 gray frames; mouth band of `cur` flips bright.
        let w = 100usize;
        let h = 100usize;
        let prev = vec![128u8; w * h * 3];
        let mut cur = prev.clone();
        let f = Face {
            x: 10.0,
            y: 10.0,
            w: 80.0,
            h: 80.0,
            score: 0.9,
        };
        // Mouth band: x 30..70, y 54..83.6 → paint it white in cur.
        for y in 54..84 {
            for x in 30..70 {
                for ch in 0..3 {
                    cur[(y * w + x) * 3 + ch] = 255;
                }
            }
        }
        let m = mouth_motion(&prev, &cur, w, h, &f);
        assert!(m > 0.4, "talking must read high, got {m}");
        assert_eq!(
            mouth_motion(&prev, &prev, w, h, &f),
            0.0,
            "stillness reads zero"
        );
        assert_eq!(
            mouth_motion(&[], &cur, w, h, &f),
            0.0,
            "shape mismatch reads zero"
        );
    }

    #[test]
    fn handoff_arrives_with_push_in() {
        // Closeup-to-closeup cut: bridge must start wider, end exact.
        let mut tg = raw_run(100.0, 100.0, 6, 1.0 / 15.0, None);
        for r in &mut tg {
            r.w = 140.0;
            r.h = 248.0;
        }
        let mut right = raw_run(450.0, 450.0, 14, 1.0 / 15.0, Some(0));
        for r in &mut right {
            r.t += 6.0 / 15.0;
            r.w = 140.0;
            r.h = 248.0;
        }
        tg.extend(right);
        let path = smooth_path(&tg, 640.0, 360.0);
        let cut_idx = path.iter().position(|p| p.x > 105.0).unwrap();
        // Early bridge frames are punched OUT (wider than destination).
        assert!(
            path[cut_idx].w > 140.0,
            "push-in must start wide, got {}",
            path[cut_idx].w
        );
        let last = path.last().unwrap();
        assert!(
            (last.x - 450.0).abs() < 3.0 && (last.w - 140.0).abs() < 3.0,
            "must land exact"
        );
        let mut mono = true;
        for wn in path.windows(2) {
            if wn[1].t < wn[0].t {
                mono = false;
            }
        }
        assert!(mono, "times must stay monotonic");
    }

    #[test]
    fn micro_handoff_flap_is_absorbed_not_whipped() {
        // A holds 1s, B blips for 0.2s (3 samples), A resumes: flap.
        let mut tg = raw_run(100.0, 100.0, 16, 1.0 / 15.0, None);
        let mut blip = raw_run(450.0, 450.0, 3, 1.0 / 15.0, Some(0));
        for r in &mut blip {
            r.t += 16.0 / 15.0;
        }
        let t_end = blip.last().unwrap().t;
        tg.extend(blip);
        for i in 0..16 {
            tg.push(RawTarget {
                t: t_end + (i + 1) as f64 / 15.0,
                x: 100.0,
                y: 10.0,
                w: 150.0,
                h: 260.0,
                cut: false,
                hard: false,
                n_faces: 1,
                pick_cx: 100.0,
            });
        }
        let path = smooth_path(&tg, 640.0, 360.0);
        let mut max_dev = 0.0f64;
        let mut max_step = 0.0f64;
        for (i, p) in path.iter().enumerate() {
            max_dev = max_dev.max((p.x - 100.0).abs());
            if i > 0 {
                max_step = max_step.max((p.x - path[i - 1].x).abs());
            }
        }
        assert!(max_dev < 25.0, "flap must not swing the camera: {max_dev}");
        assert!(max_step < 12.0, "no whipping steps: {max_step}");
    }

    #[test]
    fn segments_absorb_flicker_and_short_wides() {
        // 4Hz samples: faces, 2-frame dropout, faces, 3s gap, faces.
        let mut seen = Vec::new();
        for i in 0..40 {
            seen.push((i as f64 * 0.25, true));
        }
        seen[20] = (5.0, false);
        seen[21] = (5.25, false);
        for i in 40..52 {
            seen.push((i as f64 * 0.25, false));
        }
        for i in 52..72 {
            seen.push((i as f64 * 0.25, true));
        }
        let segs = build_segments(&seen, 18.0);
        // 0.5s dropout absorbed; 3s gap survives as one Wide.
        assert_eq!(segs.len(), 3, "{segs:?}");
        assert_eq!(segs[0].kind, SegKind::Track);
        assert_eq!(segs[1].kind, SegKind::Wide);
        assert!((segs[1].t1 - segs[1].t0 - 3.0).abs() < 0.6, "{segs:?}");
        assert_eq!(segs[2].kind, SegKind::Track);
    }

    #[test]
    fn ahash_sees_cuts_not_motion() {
        // 64x36 gray frame vs same frame with a bright half (a cut).
        let (w, h) = (64usize, 36usize);
        let flat = vec![128u8; w * h * 3];
        let mut cut = flat.clone();
        for y in 0..h / 2 {
            for x in 0..w {
                for ch in 0..3 {
                    cut[(y * w + x) * 3 + ch] = 255;
                }
            }
        }
        assert_eq!(hamdist(ahash(&flat, w, h), ahash(&flat, w, h)), 0);
        let d = hamdist(ahash(&flat, w, h), ahash(&cut, w, h));
        assert!(
            d > SHOT_CUT_BITS,
            "hard scene change must trip the cut: {d}"
        );
        // Small local change (a face shifting): same shot.
        let mut shift = flat.clone();
        for y in 10..20 {
            for x in 10..20 {
                for ch in 0..3 {
                    shift[(y * w + x) * 3 + ch] = 200;
                }
            }
        }
        let d2 = hamdist(ahash(&flat, w, h), ahash(&shift, w, h));
        assert!(d2 < SHOT_CUT_BITS, "local motion must not cut: {d2}");
        assert_eq!(hamdist(0, 0), 0);
        assert_eq!(hamdist(u64::MAX, 0), 64);
    }

    #[test]
    fn face_veto_tells_pans_from_cuts() {
        let a = Face {
            x: 100.0,
            y: 100.0,
            w: 60.0,
            h: 80.0,
            score: 0.9,
        };
        // Same face, handheld-shifted: overlap → veto (glide, don't snap).
        let b = Face {
            x: 115.0,
            y: 105.0,
            w: 60.0,
            h: 80.0,
            score: 0.9,
        };
        assert!(faces_overlap(&[b.clone()], &[a.clone()]));
        // Shot/reverse-shot: nobody overlaps → cut confirms.
        let c = Face {
            x: 400.0,
            y: 100.0,
            w: 60.0,
            h: 80.0,
            score: 0.9,
        };
        assert!(!faces_overlap(&[c.clone()], &[a.clone()]));
        // Empty either side: no continuity to protect.
        assert!(!faces_overlap(&[], &[a.clone()]));
        assert!(!faces_overlap(&[c], &[]));
    }

    #[test]
    fn hard_cut_snaps_instead_of_gliding() {
        // Static left, HARD cut, static right: no eased samples between —
        // the path jumps (times stay monotonic, values snap).
        let mut tg = raw_run(100.0, 100.0, 6, 1.0 / 15.0, None);
        let mut right = raw_run(450.0, 450.0, 14, 1.0 / 15.0, None);
        for r in &mut right {
            r.t += 6.0 / 15.0;
        }
        right[0].cut = true;
        right[0].hard = true;
        tg.extend(right);
        let path = smooth_path(&tg, 640.0, 360.0);
        let mut snapped = false;
        for wn in path.windows(2) {
            assert!(wn[1].t > wn[0].t, "times must stay monotonic");
            if (wn[1].x - wn[0].x).abs() > 100.0 {
                snapped = true; // the cut itself, unbridged
            }
        }
        assert!(snapped, "hard cut must snap, not glide");
        assert!((path.last().unwrap().x - 450.0).abs() < 3.0);
        // Same setup with a soft cut glides instead (no big single step).
        let mut tg2 = raw_run(100.0, 100.0, 6, 1.0 / 15.0, None);
        let mut right2 = raw_run(450.0, 450.0, 18, 1.0 / 15.0, Some(0));
        for r in &mut right2 {
            r.t += 6.0 / 15.0;
        }
        tg2.extend(right2);
        let path2 = smooth_path(&tg2, 640.0, 360.0);
        for wn in path2.windows(2) {
            assert!(
                (wn[1].x - wn[0].x).abs() < 45.0,
                "soft cut must glide: {} -> {}",
                wn[0].x,
                wn[1].x
            );
        }
    }
}
