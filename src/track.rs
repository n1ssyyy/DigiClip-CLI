//! Speaker-aware face tracking (YuNet via ONNX Runtime).
//!
//! Offline, in-process: frames stream from the provisioned ffmpeg (one
//! frame in memory at a time), faces are detected with YuNet
//! (`yunet_2026may.onnx`, MIT, auto-downloaded once to the provision dir)
//! and the primary speaker is tracked across samples. The output is raw
//! framing *evidence* per sample (float windows, never rounded, never
//! lag-filtered) plus frame-exact shot cuts; [`crate::camera`] turns it
//! into the camera path with the whole clip in view.
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
    x.div_ceil(div) * div
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
        let mut input = ndarray::Array4::<f32>::zeros((1, 3, pad_h, pad_w));
        {
            // Planar fill straight into the contiguous buffer (per-element
            // 4-D indexing was a measurable slice of CPU tracking time).
            let plane = pad_w * pad_h;
            let buf = input
                .as_slice_mut()
                .ok_or_else(|| anyhow::anyhow!("non-contiguous input tensor"))?;
            for y in 0..h {
                let row = &rgb[y * w * 3..(y + 1) * w * 3];
                let o = y * pad_w;
                for (x, px) in row.as_chunks::<3>().0.iter().enumerate() {
                    buf[o + x] = px[0] as f32;
                    buf[plane + o + x] = px[1] as f32;
                    buf[2 * plane + o + x] = px[2] as f32;
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

/// Streaming frame sampler: ffmpeg decodes `fps` frames/sec as `width` x
/// `height` RGB24 and this yields them one at a time (absolute timestamps
/// when `seek = Some((start, dur))`). Memory stays at one frame no matter
/// how long the span is: collecting a 10-minute range at 15 Hz used to
/// hold ~3.5 GB of RGB.
pub struct FrameReader {
    child: std::process::Child,
    out: std::process::ChildStdout,
    frame_len: usize,
    t0: f64,
    fps: f64,
    i: usize,
    /// Expected frame count (progress denominators).
    pub expected: usize,
}

impl FrameReader {
    pub fn open(
        ffmpeg: &Path,
        source: &Path,
        fps: u32,
        width: u32,
        height: u32,
        seek: Option<(f64, f64)>,
    ) -> anyhow::Result<Self> {
        use std::process::Stdio;
        let frame_len = width as usize * height as usize * 3;
        if frame_len == 0 || fps == 0 {
            anyhow::bail!("bad sample dims");
        }
        let mut cmd = crate::process::command(ffmpeg);
        cmd.args(["-nostdin", "-hide_banner", "-v", "error"]);
        let t0 = seek.map(|(s, _)| s).unwrap_or(0.0);
        let mut expected = 0;
        if let Some((s, d)) = seek {
            let d = d.max(0.5);
            cmd.arg("-ss")
                .arg(format!("{s:.6}"))
                .arg("-t")
                .arg(format!("{d:.6}"));
            expected = (d * fps as f64).ceil() as usize;
        }
        cmd.arg("-i").arg(source).args([
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-vf",
            &format!("fps={fps},scale={width}:{height}:flags=bilinear"),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "pipe:1",
        ]);
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        crate::process::gentle(&child);
        let out = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("sampler stdout missing"))?;
        Ok(Self {
            child,
            out,
            frame_len,
            t0,
            fps: fps as f64,
            i: 0,
            expected,
        })
    }
}

impl Iterator for FrameReader {
    type Item = anyhow::Result<(f64, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        use std::io::Read;
        let mut buf = vec![0u8; self.frame_len];
        let mut got = 0;
        while got < self.frame_len {
            match self.out.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Some(Err(e.into())),
            }
        }
        if got < self.frame_len {
            // End of stream (a partial tail frame is dropped).
            return match self.child.wait() {
                Ok(s) if !s.success() && self.i == 0 => {
                    Some(Err(anyhow::anyhow!("frame sampling failed ({s})")))
                }
                _ => None,
            };
        }
        let t = self.t0 + self.i as f64 / self.fps;
        self.i += 1;
        Some(Ok((t, buf)))
    }
}

impl Drop for FrameReader {
    fn drop(&mut self) {
        // Early exits (cancel, errors) must not leave ffmpeg running.
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// Collect sampled frames (small spans, benches and smoke tests; the
/// tracker itself streams through [`FrameReader`]).
pub fn sample_frames(
    ffmpeg: &Path,
    source: &Path,
    fps: u32,
    width: u32,
    sample_h: u32,
    seek: Option<(f64, f64)>,
) -> anyhow::Result<Vec<(f64, Vec<u8>)>> {
    FrameReader::open(ffmpeg, source, fps, width, sample_h, seek)?.collect()
}

/// Detector sample size: ~768x432 worth of pixels at the source aspect,
/// upscaling small sources by up to 1.25x. Measured on real podcasts
/// (`examples/track_recall.rs`): 480px-wide samples missed ~20% of frames
/// with a clearly visible face (each miss froze the framing), 640px
/// missed <1% and 768px none, all at the same ~5 ms/frame.
pub fn sample_dims(src_w: u32, src_h: u32) -> (u32, u32) {
    let (w, h) = (src_w.max(2) as f64, src_h.max(2) as f64);
    let s = ((768.0 * 432.0) / (w * h)).sqrt().min(1.25);
    let even = |v: f64| ((v.round() as u32).max(2) + 1) & !1;
    (even(w * s), even(h * s))
}

/// Verify and pin a candidate shot cut. The tracker samples at 8–15 Hz, so
/// a hash jump only says "something changed between two samples" — a cut,
/// or a whip pan / fast motion across a faceless shot. Decoding the sliver
/// (plus ~0.25 s of context each side) at 60 Hz in 64x36 gray settles both:
///
/// - **Verify.** A cut is a one-frame spike in frame-to-frame change
///   (the classic shot-boundary test, as in ffmpeg's `scene` score): its
///   peak must stand well above the surrounding motion level, or the
///   luma histogram must turn over. Sustained change (a pan) is not a cut.
/// - **Pin.** The spike's frame lands the cut within ~1/60 s instead of up
///   to 125 ms late.
///
/// Returns `None` when the candidate is motion, `Some(hi)` when the
/// decode fails (trust the detector).
pub fn refine_cut(ffmpeg: &Path, source: &Path, lo: f64, hi: f64) -> Option<f64> {
    const R: f64 = 60.0;
    const W: usize = 64;
    const H: usize = 36;
    const CTX: f64 = 0.25;
    let start = (lo - CTX).max(0.0);
    let dur = (hi - start + CTX).max(0.05);
    let out = crate::process::command(ffmpeg)
        .args(["-nostdin", "-hide_banner", "-v", "error"])
        .arg("-ss")
        .arg(format!("{start:.6}"))
        .arg("-t")
        .arg(format!("{dur:.6}"))
        .arg("-i")
        .arg(source)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-vf",
            &format!("fps={R},scale={W}:{H}:flags=area,format=gray"),
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(out) = out else { return Some(hi) };
    let frames: Vec<&[u8]> = out
        .stdout
        .as_chunks::<{ W * H }>()
        .0
        .iter()
        .map(|f| &f[..])
        .collect();
    if frames.len() < 2 {
        return Some(hi);
    }
    let diffs: Vec<f64> = (1..frames.len())
        .map(|i| mean_abs_diff(frames[i], frames[i - 1]))
        .collect();
    classify_cut(
        &diffs,
        |i| hist_turnover(frames[i], frames[i + 1]),
        start,
        R,
        lo,
        hi,
    )
}

/// Mean absolute luma difference of two equal-size gray frames.
fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    let s: u64 = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u64).sum();
    s as f64 / a.len().max(1) as f64
}

/// L1 distance of 16-bin luma histograms (0 = same tonal content, 2 =
/// disjoint). Motion within a shot keeps it low; a new scene turns it over.
fn hist_turnover(a: &[u8], b: &[u8]) -> f64 {
    let mut ha = [0f64; 16];
    let mut hb = [0f64; 16];
    for &v in a {
        ha[(v >> 4) as usize] += 1.0;
    }
    for &v in b {
        hb[(v >> 4) as usize] += 1.0;
    }
    let (na, nb) = (a.len().max(1) as f64, b.len().max(1) as f64);
    ha.iter()
        .zip(hb.iter())
        .map(|(x, y)| (x / na - y / nb).abs())
        .sum()
}

/// Cut test over a frame-difference profile (pure, unit-tested).
/// `diffs[i]` is the change from frame i to i+1 of a decode starting at
/// `start` at `r` fps; `hist(i)` is the histogram turnover of that pair.
/// The peak inside [lo, hi] must be a real change (>= 6 levels) AND either
/// a spike (>= 2.5x the median motion around it, duplicate frames from
/// the 60 Hz resample ignored) or a histogram turnover (>= 0.6).
pub fn classify_cut(
    diffs: &[f64],
    hist: impl Fn(usize) -> f64,
    start: f64,
    r: f64,
    lo: f64,
    hi: f64,
) -> Option<f64> {
    let t_of = |i: usize| start + (i + 1) as f64 / r;
    let (mut best, mut bi) = (0.0f64, None);
    for (i, &d) in diffs.iter().enumerate() {
        let t = t_of(i);
        if t >= lo - 0.5 / r && t <= hi + 0.5 / r && d > best {
            best = d;
            bi = Some(i);
        }
    }
    let bi = bi?;
    if best < 6.0 {
        return None;
    }
    let mut around: Vec<f64> = diffs
        .iter()
        .enumerate()
        .filter(|(i, &d)| i.abs_diff(bi) > 1 && d > 0.5)
        .map(|(_, &d)| d)
        .collect();
    around.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let motion = around.get(around.len() / 2).copied().unwrap_or(0.0);
    let spike = best >= 2.5 * motion;
    if !(spike || hist(bi) >= 0.6) {
        return None;
    }
    Some(t_of(bi).clamp(lo, hi))
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
    /// Raw per-sample framing evidence; the camera planner turns it into a
    /// path (dead zones, holds, eased moves) once the whole clip is known.
    pub raw: Vec<RawTarget>,
    /// Shot cuts pinned to the frame (source clock, sorted).
    pub shots: Vec<f64>,
    /// Seconds held in two-shot (both talkers framed).
    pub group_secs: f64,
}

/// Post-cut settle (s): after a shot cut the challenger gate needs a beat
/// to confirm the new shot's speaker. Those samples are placeholders
/// (`weak`), so the planner opens the new shot directly on the settled
/// speaker instead of on a guess.
pub const SETTLE_S: f64 = 0.4;
/// A primary-target center jump beyond this fraction of the frame width
/// between consecutive samples is a discrete framing change (handoff or
/// reacquire), not motion.
const CUT_FRAC: f64 = 0.10;
/// Detection gaps shorter than this hold the framing instead of going
/// wide: a wide shot must stay on screen long enough to read as a shot
/// (a head turn or a hand over the face is not a reason to zoom out).
pub const WIDE_MIN_S: f64 = 2.5;
/// A faceless stretch with a source shot cut (or the clip's edge) at both
/// ends is a cutaway — b-roll, a screen, the room — not a lost face: it
/// goes wide after this long, and the cuts hide the layout change.
pub const WIDE_CUT_MIN_S: f64 = 1.0;
/// A face/no-face flip this close to a shot cut belongs to the cut (the
/// detector confirms a new face a sample or two late).
const CUT_SNAP_S: f64 = 0.35;
/// How far (fraction of frame width) the lone face may be from the lost
/// primary and still count as the same person: jitter radius plus a
/// walking pace for the time unseen, capped well below the spacing of two
/// seated people so a real handoff is never mistaken for a reacquire.
pub fn reacquire_frac(unseen_s: f64) -> f64 {
    (0.08 + 0.25 * unseen_s.max(0.0)).min(0.2)
}
/// Pending-challenger match radius (fraction of frame width): box centers
/// are stable under size jitter where IoU is not.
const PENDING_FRAC: f64 = 0.08;
/// Unzoomed window for the output canvas, in source px: the largest
/// canvas-aspect rect in the source (`x`, `y` centered), plus the fewest
/// source rows a zoom may shrink to (bounded upscaling — see
/// [`crate::compose::Canvas::min_crop_h`]; a 360p source on a 9:16 canvas
/// never zooms at all).
#[derive(Debug, Clone, Copy)]
pub struct Base {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub min_h: f64,
}

impl Base {
    pub fn new(src_w: f64, src_h: f64, canvas: crate::compose::Canvas) -> Self {
        let r = canvas.base_rect(src_w, src_h);
        Base {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
            min_h: canvas.min_crop_h(),
        }
    }
}

/// One raw per-sample framing target (source px, f64 — never rounded).
/// `cut` marks a discrete change (speaker handoff, group edge, reacquire);
/// `hard` marks the first sample of a new shot.
#[derive(Debug, Clone)]
pub struct RawTarget {
    pub t: f64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub cut: bool,
    pub hard: bool,
    /// Faces framed by this target (2+ = group shot).
    pub n_faces: usize,
    /// Picked primary center-x (NaN when no face).
    pub pick_cx: f64,
    /// Subject anchor (face / group center, source px): punch-ins zoom
    /// around it so the face keeps its place on screen.
    pub ax: f64,
    pub ay: f64,
    /// Placeholder, not evidence (post-cut settle, nothing seen yet): the
    /// planner fills it from real samples around it.
    pub weak: bool,
}

/// Transcript gate: is anyone audibly speaking at `t` (word spans padded)?
/// Reframes freeze during silence — the camera never chases quiet faces.
/// Pure (unit-tested).
pub fn speech_active(words: &[Word], t: f64) -> bool {
    words.iter().any(|w| t > w.s - 0.4 && t < w.e + 0.4)
}

/// Desired canvas-aspect window for a face — (center-x, center-y,
/// height-fraction) in source px → window geometry. Zoom stays generous
/// (≤1.3x), weak detections never earn more than 1.15x, and no zoom ever
/// crops below the base's `min_h` source rows. Face rides ~40% from the
/// window top.
pub fn window_for_face(
    cx: f64,
    cy: f64,
    frac: f64,
    src_w: f64,
    src_h: f64,
    base: Base,
    score: f64,
) -> (f64, f64, f64, f64) {
    // Continuous in the score: a threshold here made the zoom pump
    // whenever the detector's confidence flickered across it.
    let u = ((score - 0.5) / 0.3).clamp(0.0, 1.0);
    let zmax: f64 = 1.15 + 0.15 * u * u * (3.0 - 2.0 * u);
    let zmax = zmax.min((base.h / base.min_h).max(1.0));
    let z = (0.38 / frac.max(0.05)).clamp(1.0, zmax);
    let w = base.w / z;
    let h = base.h / z;
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
/// People in one conversation sit at about the same distance from the
/// camera: a member's face must be at least this fraction of the biggest
/// face's height. Tiny faces (a video playing on a TV behind the speaker,
/// the back row) never form a group of their own and steal the frame.
const GROUP_MIN_REL: f64 = 0.5;

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
    let biggest = scored.iter().map(|(f, _)| f.h).fold(0.0, f64::max);
    for &(f, m) in scored {
        if f.score < 0.6 || f.h < GROUP_MIN_REL * biggest {
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

/// Group window for 2+ simultaneous talkers: the base (widest) canvas
/// window over the whole span — or `None` when they don't fit, in which
/// case the camera stays on the primary speaker. Never half-frames anyone:
/// if edge-clamping would cut a member out, `None`. Pure (unit-tested).
pub fn group_window(
    faces: &[&Face],
    src_w: f64,
    src_h: f64,
    base: Base,
) -> Option<(f64, f64, f64, f64)> {
    let base_w = base.w;
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
    let h = base.h.min(src_h);
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

/// Track the primary speaker and segment the timeline.
/// Returns raw per-sample framing evidence, frame-exact shot cuts and
/// Track/Wide segments (short wides <WIDE_MIN_S are absorbed so the framing
/// never strobes). Smoothing is the camera planner's job, offline, with
/// the whole clip in view — nothing here filters with lag.
#[allow(clippy::too_many_arguments)]
pub fn plan_tracks(
    tracker: &mut Tracker,
    ffmpeg: &Path,
    source: &Path,
    src_w: u32,
    src_h: u32,
    duration: f64,
    range: Option<(f64, f64)>,
    words: &[Word],
    canvas: crate::compose::Canvas,
    // Serve hooks (`None` on the CLI): per-frame progress + cancel.
    progress: Option<crate::progress::SharedPct>,
    cancel: &crate::progress::CancelFlag,
) -> anyhow::Result<Tracked> {
    // 15 Hz on either EP: YuNet at ~768x432 costs ~5 ms/frame on CPU and
    // DirectML alike. See [`sample_dims`] for why that size.
    let fps: u32 = 15;
    let (sample_w, sample_h) = sample_dims(src_w, src_h);
    // Track only the requested span (picked clip ranges, not the source).
    let (r0, r1) = range.unwrap_or((0.0, duration));
    let frames = FrameReader::open(
        ffmpeg,
        source,
        fps,
        sample_w,
        sample_h,
        Some((r0, (r1 - r0).max(0.5))),
    )?;
    let sx = src_w as f64 / sample_w as f64;

    let (src_wf, src_hf) = (src_w as f64, src_h as f64);
    let base = Base::new(src_wf, src_hf, canvas);
    let crop_w = base.w;
    let cut_dist = CUT_FRAC * src_wf;

    let mut primary: Option<Face> = None;
    // Challenger waiting to dethrone the primary (face + consecutive wins).
    let mut pending: Option<(Face, u32)> = None;
    // No ping-pong: a fresh switch holds the floor ~2.5s (a full turn of
    // banter) unless the challenger holds ~0.75s of consecutive wins (a
    // real turn-take, not alternation). Isolated handoffs are instant
    // regardless.
    let mut last_switch_t = f64::NEG_INFINITY;
    // Last hard shot cut (drives the post-cut settle window).
    let mut last_shot_t = f64::NEG_INFINITY;
    // Frame-exact shot cuts (source clock).
    let mut shots: Vec<f64> = Vec::new();
    let mut prev_t = r0;
    // Previous sample (pixels + kept boxes) for temporal confirm + mouth.
    let mut prev_rgb: Option<Vec<u8>> = None;
    let mut prev_kept: Vec<Face> = Vec::new();
    // Previous sample's raw detections: a consistent low-confidence run
    // confirms itself (a dropout must not break the chain for good).
    let mut prev_raw_faces: Vec<Face> = Vec::new();
    // When the primary identity was last actually matched.
    let mut primary_t = f64::NEG_INFINITY;
    // Previous frame hash for shot-cut detection.
    let mut prev_hash: Option<u64> = None;
    let mut raw: Vec<RawTarget> = Vec::new();
    let mut seen: Vec<(f64, bool)> = Vec::new();
    let mut ever_seen = false;
    let mut group_samples = 0u32;
    // Last pushed target + anchor (silence and dropouts hold them).
    let mut prev_raw: Option<(f64, f64, f64, f64)> = None;
    let mut prev_anchor: Option<(f64, f64)> = None;
    // Previous members (incumbency for span hysteresis).
    let mut prev_members: Vec<Face> = Vec::new();
    // Group state (shared framing while a talking crew holds together).
    let mut group_state = GroupState::default();
    // Speech memory (samples): refreshed while either top face talks.
    let mut speech_live = 0u32;

    let n_frames = frames.expected.max(1);
    let every = (n_frames / 25).max(1);
    for (fi, item) in frames.enumerate() {
        let (t_now, rgb_now) = item?;
        let (t, rgb) = (&t_now, &rgb_now);
        cancel.check()?;
        // Serve progress (throttled: ~25 updates per range).
        if let Some(p) = progress.as_deref() {
            if fi % every == 0 || fi + 1 == n_frames {
                p((((fi + 1) as f64 / n_frames as f64 * 100.0) as u8).min(99));
            }
        }
        // Stale identity after 2s unseen: re-pick fresh (avoids latching
        // onto the wrong person when the speaker changes off-screen).
        if *t - primary_t > 2.0 {
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
        let kept: Vec<Face> = confirm(sample_faces.clone(), &prev_raw_faces);
        prev_raw_faces = sample_faces;
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
        // A hash jump is only a cut when the faces turn over too, and the
        // frame-exact check sees a spike (not a pan across a faceless shot).
        let cut_at = if hash_cut && !faces_overlap(&kept, &prev_kept) {
            refine_cut(ffmpeg, source, prev_t, *t)
        } else {
            None
        };
        let shot = cut_at.is_some();
        if let Some(c) = cut_at {
            primary = None;
            pending = None;
            group_state = GroupState::default();
            speech_live = 0;
            last_switch_t = f64::NEG_INFINITY;
            last_shot_t = *t;
            shots.push(c);
        }
        prev_t = *t;
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
        } else {
            speech_live = speech_live.saturating_sub(1);
        }
        let group_candidate: Option<(f64, f64, f64, f64)> = if members.len() >= 2 && speech_live > 0
        {
            let fs: Vec<&Face> = members.iter().map(|(f, _)| *f).collect();
            let w = group_window(&fs, src_wf, src_hf, base);
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
        let (group_now, group_edge) = if frozen {
            (group_state.on, false)
        } else {
            group_state.step(group_candidate.is_some())
        };
        if group_now {
            group_samples += 1;
        }
        // Where is the primary now? Its best-overlapping box, or — when it
        // is the only face, or a head-width away at most — the face within
        // walking distance of where it was last matched (a presenter who
        // walked through a detector dropout is not a new speaker). Following
        // the primary is independent of the handoff gate below: a pending
        // challenger or a silent stretch never freezes the camera on a
        // stale box.
        let n_det = faces.len();
        let matched: Option<usize> = primary.as_ref().and_then(|prev| {
            let (bi, bo) = faces
                .iter()
                .enumerate()
                .map(|(i, f)| (i, iou(f, prev)))
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;
            if bo >= 0.2 {
                return Some(bi);
            }
            let reach = reacquire_frac(*t - primary_t) * src_wf;
            let (ni, nd) = faces
                .iter()
                .enumerate()
                .map(|(i, f)| (i, (f.cx() - prev.cx()).abs()))
                .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;
            let close = nd <= 0.5 * prev.w.max(faces[ni].w);
            ((n_det == 1 && nd <= reach) || close).then_some(ni)
        });
        // Contender = persistence + mouth + detector score (a fresh pick
        // takes the biggest, most confident talker).
        let contender: Option<usize> = if let Some(prev) = &primary {
            (0..n_det).max_by(|&a, &b| {
                let s = |i: usize| faces[i].score * 1.5 + iou(&faces[i], prev) + motions[i] * 2.5;
                s(a).partial_cmp(&s(b)).unwrap_or(std::cmp::Ordering::Equal)
            })
        } else {
            (0..n_det).max_by(|&a, &b| {
                let s = |i: usize| faces[i].score * (1.0 + motions[i]) * faces[i].w * faces[i].h;
                s(a).partial_cmp(&s(b)).unwrap_or(std::cmp::Ordering::Equal)
            })
        };
        // Handoff gate. A different face must win twice running before it
        // dethrones the primary, so a single-sample teleport detection can
        // never take over the camera. Breaking a fresh 2.5s lock takes
        // SUSTAINED winning (~0.75s of consecutive wins): overlapping banter
        // doesn't whip A→B→C in under a second. A primary that is off screen
        // holds no lock. Nobody takes the floor in silence.
        // `follow`: the pick is the primary itself (not a handoff).
        let mut follow = false;
        let pick: Option<Face> = if primary.is_none() {
            pending = None;
            contender.map(|i| faces[i].clone())
        } else {
            // A face far smaller than the speaker (the TV behind them, the
            // back row) is someone else's shot, never a handoff.
            let big_enough = |c: usize| {
                primary
                    .as_ref()
                    .is_none_or(|p| faces[c].h >= GROUP_MIN_REL * p.h)
            };
            let challenger = contender.filter(|&c| Some(c) != matched && !frozen && big_enough(c));
            let won = match challenger {
                None => {
                    pending = None;
                    None
                }
                Some(ci) => {
                    let c = &faces[ci];
                    let hits = match &pending {
                        Some((p, h))
                            if iou(c, p) > 0.25
                                || (c.cx() - p.cx()).abs() < PENDING_FRAC * src_wf =>
                        {
                            h + 1
                        }
                        _ => 1,
                    };
                    // ~0.75s of consecutive wins steals inside the lock.
                    let steal_hits: u32 = ((0.75 * fps as f64).ceil() as u32).max(4);
                    let unlocked = matched.is_none() || *t - last_switch_t >= 2.5;
                    if hits >= 2 && (unlocked || hits >= steal_hits) {
                        pending = None;
                        last_switch_t = *t;
                        Some(ci)
                    } else {
                        pending = Some((c.clone(), hits));
                        None
                    }
                }
            };
            match (won, matched) {
                (Some(ci), _) => Some(faces[ci].clone()),
                (None, Some(mi)) => {
                    follow = true;
                    Some(faces[mi].clone())
                }
                (None, None) => None,
            }
        };

        let face = pick
            .as_ref()
            .map(|f| (f.cx(), f.y + f.h / 2.0, f.h / src_hf));
        let pick_cx = face.map(|(cx, _, _)| cx).unwrap_or(f64::NAN);
        let pick_score = pick.as_ref().map(|f| f.score).unwrap_or(0.0);
        // Anyone on screen keeps the close-up layout (Wide means nobody).
        let has_face = n_det > 0 || group_now;
        if has_face {
            ever_seen = true;
        }
        if pick.is_some() {
            primary_t = *t;
            primary = pick;
        }
        // Raw window target (float, unrounded). A talking group shares one
        // window; otherwise the primary's face window. No evidence (primary
        // off screen, group lost, post-cut settle, nothing seen yet) is a
        // placeholder (`weak`) carrying the last real framing: the planner
        // bridges it from the real samples around it. (Copied boxes used to
        // pass as sightings, which stair-stepped the camera path.)
        let settling = *t - last_shot_t < SETTLE_S;
        let hold = || {
            let (hx, hy, hw, hh) = prev_raw.unwrap_or((base.x, base.y, base.w, base.h));
            (hx, hy, hw, hh, false, true)
        };
        let (gx, gy, gw, gh, grouped, weak) = if settling {
            (base.x, base.y, base.w, base.h, false, true)
        } else if group_now {
            match group_candidate {
                Some((dx, dy, dw, dh)) => (dx, dy, dw, dh, true, false),
                None => hold(),
            }
        } else {
            match face {
                Some((cx, cy, frac)) => {
                    let (wx, wy, ww, wh) =
                        window_for_face(cx, cy, frac, src_wf, src_hf, base, pick_score);
                    (wx, wy, ww, wh, false, false)
                }
                None => hold(),
            }
        };
        // Punch anchor: the face (or the group's eye line) this frames.
        let (ax, ay) = if grouped || settling {
            (gx + gw / 2.0, gy + gh * 0.4)
        } else {
            match face {
                Some((cx, cy, _)) => (cx, cy),
                None => prev_anchor.unwrap_or((gx + gw / 2.0, gy + gh * 0.4)),
            }
        };
        // Speaker handoff (or group enter/exit): the desired window jumps.
        let ncx = gx + gw / 2.0;
        let cut = shot
            || group_edge
            || !weak
                && !follow
                && match prev_raw {
                    Some((px, _, pw, _)) => (ncx - (px + pw / 2.0)).abs() > cut_dist,
                    None => false,
                };
        if !weak {
            prev_raw = Some((gx, gy, gw, gh));
            prev_anchor = Some((ax, ay));
        }
        raw.push(RawTarget {
            t: *t,
            x: gx,
            y: gy,
            w: gw,
            h: gh,
            cut,
            hard: shot,
            n_faces: if grouped { n_group } else { 1 },
            pick_cx,
            ax,
            ay,
            weak,
        });
        seen.push((*t, has_face));
    }
    if raw.is_empty() {
        seen.push((r0, false));
    }

    let end = r1.max(raw.last().map(|s| s.t).unwrap_or(r1));
    let merged = build_segments(&seen, end, &shots);
    let group_secs = group_samples as f64 / fps as f64;

    Ok(Tracked {
        segments: merged,
        ever_seen,
        raw,
        shots,
        group_secs,
    })
}

/// Build Track/Wide segments from per-sample face flags and the source
/// shot cuts. Pure (unit-tested): hysteresis via majority vote, flips
/// moved onto nearby cuts, short wides absorbed (cutaways between cuts
/// may be shorter than mid-shot dropouts).
pub fn build_segments(seen: &[(f64, bool)], end: f64, shots: &[f64]) -> Vec<Seg> {
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
    // Flips near a shot cut land on it: the layout changes with the
    // picture. The range's own edges are edits too.
    let n = segments.len();
    let mut on_cut = vec![false; n + 1];
    on_cut[0] = true;
    on_cut[n] = true;
    for i in 1..n {
        let t = segments[i].t0;
        let near = shots
            .iter()
            .copied()
            .filter(|&c| (c - t).abs() <= CUT_SNAP_S)
            .filter(|&c| c > segments[i - 1].t0 && c < segments[i].t1)
            .min_by(|a, b| (a - t).abs().total_cmp(&(b - t).abs()));
        if let Some(c) = near {
            segments[i - 1].t1 = c;
            segments[i].t0 = c;
            on_cut[i] = true;
        }
    }
    // Segment times are absolute; the first one opens at 0 for coverage,
    // so its length counts from the first sample.
    let first_t = seen[0].0;
    let keep: Vec<bool> = segments
        .iter()
        .enumerate()
        .map(|(i, seg)| {
            let min = if on_cut[i] && on_cut[i + 1] {
                WIDE_CUT_MIN_S
            } else {
                WIDE_MIN_S
            };
            seg.kind == SegKind::Wide && seg.t1 - seg.t0.max(first_t) >= min
        })
        .collect();
    // Absorb short wides.
    let mut merged: Vec<Seg> = Vec::new();
    let mut kept: Vec<bool> = Vec::new();
    for (seg, k) in segments.into_iter().zip(keep) {
        let short_wide = seg.kind == SegKind::Wide && !k && !merged.is_empty();
        if short_wide {
            if let Some(prev) = merged.last_mut() {
                prev.t1 = seg.t1;
                continue;
            }
        }
        // Merge same-kind neighbors.
        if let (Some(prev), Some(pk)) = (merged.last_mut(), kept.last_mut()) {
            if prev.kind == seg.kind {
                prev.t1 = seg.t1;
                *pk |= k;
                continue;
            }
        }
        merged.push(seg);
        kept.push(k);
    }
    // A leading short wide has no Track to join — drop it into the next.
    if merged.len() > 1 && merged[0].kind == SegKind::Wide && !kept[0] {
        let t1 = merged[0].t1;
        merged.remove(0);
        merged[0].t0 = merged[0].t0.min(t1);
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::Canvas;

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
        // Two tiny faces on a TV behind a speaker: close together and
        // "talking" (it's a video), but a quarter of the speaker's size.
        let speaker = Face {
            x: 100.0,
            y: 60.0,
            w: 60.0,
            h: 80.0,
            score: 0.9,
        };
        let tv = |x: f64| Face {
            x,
            y: 40.0,
            w: 14.0,
            h: 20.0,
            score: 0.66,
        };
        let (t1, t2) = (tv(560.0), tv(600.0));
        assert!(
            cluster_members(&[(&t1, 0.9), (&t2, 0.9), (&speaker, 0.3)], base_w, &[]).is_empty(),
            "screen faces never form a group"
        );
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
        let base = Base::new(src_w, src_h, Canvas::TALL);
        let base_w = base.w;
        let f = |x: f64, w: f64| Face {
            x,
            y: 100.0,
            w,
            h: 80.0,
            score: 0.9,
        };
        // Pair: shared base window, both inside.
        let (a, b) = (f(220.0, 60.0), f(300.0, 60.0));
        let (x, _y, w, h) = group_window(&[&a, &b], src_w, src_h, base).expect("must fit");
        assert!((w - base_w).abs() < 1e-9 && (h - src_h).abs() < 1e-9);
        assert!(x <= 220.0 && x + w >= 360.0, "both faces inside: x={x}");
        // Trio: same deal.
        let (g, h2, i) = (f(230.0, 40.0), f(280.0, 40.0), f(330.0, 40.0));
        let (x3, _, _, _) = group_window(&[&g, &h2, &i], src_w, src_h, base).expect("trio fits");
        assert!(x3 <= 230.0 && x3 + base_w >= 370.0, "trio inside: x={x3}");
        // Spread crew: no shared window (camera stays on primary).
        let far = f(500.0, 60.0);
        assert!(group_window(&[&a, &far], src_w, src_h, base).is_none());
        // Pair at the right edge: the window may overhang the frame (the
        // renderer clamps), but both members must survive inside it.
        let (c, d) = (f(520.0, 50.0), f(580.0, 40.0));
        // NOTE: c faces have different h here; containment is x-only.
        if let Some((x2, _, w2, _)) = group_window(&[&c, &d], src_w, src_h, base) {
            assert!(c.x >= x2 - 1.0 && d.x + d.w <= x2 + w2 + 1.0);
        }
        // Single face: never a group.
        assert!(group_window(&[&a], src_w, src_h, base).is_none());
    }

    #[test]
    fn group_state_has_slow_hysteresis() {
        let mut s = GroupState::default();
        assert_eq!(s.step(false), (false, false));
        for _ in 0..3 {
            assert!(!s.step(true).0, "blips must not engage");
        }
        // Fourth straight yes: on + edge (arrival dollies in).
        assert_eq!(s.step(true), (true, true));
        assert_eq!(s.step(true), (true, false));
        // Brief misses don't drop it...
        for _ in 0..7 {
            assert!(s.step(false).0);
        }
        // ...but eight straight misses exit with an edge.
        assert_eq!(s.step(false), (false, true));
        assert_eq!(s.step(false), (false, false));
    }

    #[test]
    fn face_zoom_stays_generous() {
        // 1080p source, small confident face: bounded zoom, never a
        // postage stamp.
        let b = Base::new(1920.0, 1080.0, Canvas::TALL);
        let (.., w, h) = window_for_face(960.0, 540.0, 0.15, 1920.0, 1080.0, b, 0.9);
        assert!(b.w / w <= 1.3 + 1e-9, "z={}", b.w / w);
        assert!(h >= b.min_h, "resolution floor: h={h}");
        // Same face, weak detection: stays wider (no zoom on doubt).
        let (.., weak_w, _) = window_for_face(960.0, 540.0, 0.15, 1920.0, 1080.0, b, 0.5);
        assert!(weak_w > w, "doubt stays wide: {weak_w} vs {w}");
        // Big face: no zoom at all.
        let (.., w2, _) = window_for_face(960.0, 540.0, 0.6, 1920.0, 1080.0, b, 0.9);
        assert!((w2 - b.w).abs() < 1e-9);
        // Low-res source: zooming would only magnify mush, so it never does.
        let lo = Base::new(640.0, 360.0, Canvas::TALL);
        let (.., w3, h3) = window_for_face(320.0, 180.0, 0.15, 640.0, 360.0, lo, 0.9);
        assert!((w3 - 202.5).abs() < 1e-9 && (h3 - 360.0).abs() < 1e-9);
        // 720p: some zoom, capped by the floor.
        let mid = Base::new(1280.0, 720.0, Canvas::TALL);
        let (.., h4) = window_for_face(640.0, 360.0, 0.15, 1280.0, 720.0, mid, 0.9);
        assert!(h4 >= mid.min_h - 1e-9 && h4 < 720.0, "h={h4}");
    }

    #[test]
    fn windows_follow_the_canvas_aspect() {
        // Square and 16:9 canvases on a 1080p source: every face window
        // keeps the canvas aspect (no letterbox), and the floor scales
        // with the smaller output (1080 rows -> 304 source rows).
        for canvas in [Canvas::SQUARE, Canvas::WIDE, Canvas::PORTRAIT] {
            let b = Base::new(1920.0, 1080.0, canvas);
            assert!((b.w / b.h - canvas.aspect()).abs() < 1e-9);
            let (_, _, w, h) = window_for_face(700.0, 400.0, 0.12, 1920.0, 1080.0, b, 0.9);
            assert!((w / h - canvas.aspect()).abs() < 1e-9, "{canvas:?}");
            assert!(h >= b.min_h - 1e-9);
        }
        assert!((Canvas::SQUARE.min_crop_h() - 303.75).abs() < 1e-9);
        // Vertical phone source on a 16:9 canvas: width-bound, centered.
        let v = Base::new(1080.0, 1920.0, Canvas::WIDE);
        assert!((v.w - 1080.0).abs() < 1e-9 && (v.y - (1920.0 - 607.5) / 2.0).abs() < 1e-9);
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
        let segs = build_segments(&seen, 18.0, &[]);
        // 0.5s dropout absorbed; 3s gap survives as one Wide.
        assert_eq!(segs.len(), 3, "{segs:?}");
        assert_eq!(segs[0].kind, SegKind::Track);
        assert_eq!(segs[1].kind, SegKind::Wide);
        assert!((segs[1].t1 - segs[1].t0 - 3.0).abs() < 0.6, "{segs:?}");
        assert_eq!(segs[2].kind, SegKind::Track);
    }

    #[test]
    fn cutaways_between_shot_cuts_go_wide_sooner() {
        // 15 Hz samples from 60s: speaker, a 1.4s faceless cutaway that
        // the detector notices a sample late (cut at 63.0), speaker again
        // (cut at 64.4).
        let at = |i: usize| 60.0 + i as f64 / 15.0;
        let seen: Vec<(f64, bool)> = (0..90)
            .map(|i| {
                let t = at(i);
                (t, !(63.07..64.45).contains(&t))
            })
            .collect();
        let segs = build_segments(&seen, 66.0, &[63.0, 64.4]);
        assert_eq!(segs.len(), 3, "{segs:?}");
        assert_eq!(segs[1].kind, SegKind::Wide);
        // Both flips moved onto the cuts.
        assert!((segs[1].t0 - 63.0).abs() < 1e-9 && (segs[1].t1 - 64.4).abs() < 1e-9);
        // The same gap mid-shot (no cuts) is a lost face: it holds.
        let segs = build_segments(&seen, 66.0, &[]);
        assert_eq!(segs.len(), 1, "{segs:?}");
        assert_eq!(segs[0].kind, SegKind::Track);
        // A clip opening on 1.2s of b-roll (cut at 61.2) opens wide, even
        // though segment times are absolute.
        let seen: Vec<(f64, bool)> = (0..60).map(|i| (at(i), at(i) >= 61.2)).collect();
        let segs = build_segments(&seen, 64.0, &[61.2]);
        assert_eq!(segs[0].kind, SegKind::Wide, "{segs:?}");
        assert!((segs[0].t1 - 61.2).abs() < 1e-9);
        // …but a slow first detection with no cut is not b-roll.
        let segs = build_segments(&seen, 64.0, &[]);
        assert_eq!(segs.len(), 1, "{segs:?}");
    }

    #[test]
    fn cut_check_separates_cuts_from_pans() {
        // 60 Hz resample of 30 fps: every other diff is a duplicate (0).
        let r = 60.0;
        let profile = |motion: f64, peak_at: usize, peak: f64| -> Vec<f64> {
            (0..40)
                .map(|i| {
                    if i == peak_at {
                        peak
                    } else if i % 2 == 0 {
                        motion
                    } else {
                        0.0
                    }
                })
                .collect()
        };
        // Candidate window: frames 15..25 of a decode starting at t=0.
        let (lo, hi) = (15.0 / r, 25.0 / r);
        // A cut in a calm shot: pinned to the spike frame.
        let d = profile(2.0, 20, 40.0);
        let t = classify_cut(&d, |_| 0.1, 0.0, r, lo, hi).expect("cut");
        assert!((t - 21.0 / r).abs() < 1e-9, "{t}");
        // A whip pan: big but sustained change, same tonal content.
        let d = profile(28.0, 20, 32.0);
        assert!(classify_cut(&d, |_| 0.2, 0.0, r, lo, hi).is_none());
        // A cut between two busy shots: no spike, but the scene turns over.
        assert!(classify_cut(&d, |i| if i == 20 { 0.9 } else { 0.2 }, 0.0, r, lo, hi).is_some());
        // Too small to be anything.
        let d = profile(0.5, 20, 4.0);
        assert!(classify_cut(&d, |_| 0.0, 0.0, r, lo, hi).is_none());
    }

    #[test]
    fn reacquire_radius_is_bounded() {
        assert!((reacquire_frac(0.0) - 0.08).abs() < 1e-9);
        assert!(reacquire_frac(0.3) > 0.12);
        // Never wide enough to mistake the other side of a table.
        assert!((reacquire_frac(5.0) - 0.2).abs() < 1e-9);
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
        assert!(faces_overlap(
            std::slice::from_ref(&b),
            std::slice::from_ref(&a)
        ));
        // Shot/reverse-shot: nobody overlaps → cut confirms.
        let c = Face {
            x: 400.0,
            y: 100.0,
            w: 60.0,
            h: 80.0,
            score: 0.9,
        };
        assert!(!faces_overlap(
            std::slice::from_ref(&c),
            std::slice::from_ref(&a)
        ));
        // Empty either side: no continuity to protect.
        assert!(!faces_overlap(&[], std::slice::from_ref(&a)));
        assert!(!faces_overlap(&[c], &[]));
    }
}
