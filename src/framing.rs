//! Speaker-aware framing plan (face-tracking hook).
//!
//! RESEARCH summary (Sept 2026, Windows-first, offline, Rust-friendly):
//! - Production pattern (ClipSpeedAI/AutoCrop-vertical/VerticalX): detect
//!   faces per scene (YOLOv8-seg or YuNet ~228KB ONNX), track with
//!   Kalman/ByteTrack, smooth with EMA (alpha 0.1-0.2) or Kalman Q=0.1/R=25,
//!   clamp crop window to frame bounds, face at ~40% from top, cap crop_y
//!   at 60% of vertical slack to avoid head cutoff. Render via a single
//!   ffmpeg pass with per-second crop (concat segments or sendcmd).
//! - Rust options: `ort` crate (static ONNX Runtime, DirectML EP on Windows
//!   for NVIDIA, CPU fallback) + YuNet face detection + Kalman in-house;
//!   or `rust-onnx-infer` (YuNet+SFace prebuilt); YSCV is CPU-focused and
//!   heavier than needed for v1. MediaPipe has no official Rust boxes.
//! - Mouth-motion speaker bias (roadmap V2): correlate word timestamps from
//!   whisper with per-face mouth openness (106-landmark model) to pick the
//!   active speaker; two-shot fallback keeps both faces in frame.
//!
//! V1 ships `Center` (matches the PHP app) + `Plan` (JSON import) so a
//! future detector can plug in without CLI changes:
//! `{"tracks":[{"t":0.0,"x":320.0}]}` — x = crop-left in source pixels.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackPoint {
    pub t: f64,
    pub x: f64,
}

/// Full camera pose: 9:16 window in source pixels (all rounded even at emit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CamPose {
    pub t: f64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CropPlan {
    pub tracks: Vec<TrackPoint>,
}

impl CropPlan {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let plan: CropPlan = serde_json::from_str(&text)?;
        if plan.tracks.is_empty() {
            anyhow::bail!("crop plan has no tracks");
        }
        Ok(plan)
    }

    /// EMA smoothing (alpha 0.15): kills jitter without lagging speech.
    pub fn smoothed(&self, alpha: f64) -> Vec<TrackPoint> {
        let mut out = Vec::with_capacity(self.tracks.len());
        let mut acc: Option<f64> = None;
        for p in &self.tracks {
            acc = Some(match acc {
                None => p.x,
                Some(a) => alpha * p.x + (1.0 - alpha) * a,
            });
            out.push(TrackPoint {
                t: p.t,
                x: acc.unwrap(),
            });
        }
        out
    }

    /// Crop-left at time t (hold-last, clamped to [0, max_x]).
    /// V2 hook: used by per-segment plan rendering.
    #[allow(dead_code)]
    pub fn x_at(&self, t: f64, max_x: f64) -> f64 {
        let mut x = self.tracks[0].x;
        for p in &self.tracks {
            if p.t <= t {
                x = p.x;
            } else {
                break;
            }
        }
        x.clamp(0.0, max_x.max(0.0))
    }
}

/// V2 hook: the detector-side framing abstraction. CLI v1 uses
/// [`CropPlan`] directly; this enum stays for the ONNX/YuNet plug-in.
#[allow(dead_code)]
pub enum Framing {
    Center,
    Plan(CropPlan),
}

#[allow(dead_code)]
impl Framing {
    /// Static ffmpeg crop expression for the center mode (v1 default).
    /// Plan mode is applied per-segment by the caller (see render.rs).
    pub fn crop_expr(source_w: u32, source_h: u32) -> String {
        let _ = source_w;
        let _ = source_h;
        // Full-height 9:16 window, horizontally centered.
        "crop=ih*9/16:ih".into()
    }
}
