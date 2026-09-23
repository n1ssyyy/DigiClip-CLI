//! Progress + cancellation plumbing for serve mode (and any future UI).
//!
//! The CLI passes [`Emitter::null()`] + [`CancelFlag::never()`], so every
//! hook below is a no-op there — CLI behavior is byte-identical. Serve mode
//! passes a live emitter; pipeline stages report through it and honor
//! cancellation. [`JobEvent`] is the exact JSON shape the `/ws` socket
//! forwards to the frontend (see [`crate::serve`]).

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use tokio::sync::mpsc;

/// Percent callback for long loops (track frames, ffmpeg renders).
/// Values are 0-100; callers dedupe so receivers only see changes.
pub type PctFn = dyn Fn(u8) + Send + Sync;
/// Shareable handle: ffmpeg progress is parsed on two threads (stdout
/// `-progress` lines on the caller, stderr `time=` stats on a drain
/// thread) feeding one deduping closure.
pub type SharedPct = std::sync::Arc<PctFn>;
/// Byte callback for downloads: `(done_bytes, total_bytes)`.
pub type ByteFn = dyn Fn(u64, u64) + Send + Sync;

/// Cooperative cancellation. Checked between stages, every track frame,
/// and in every ffmpeg/sidecar wait loop (children are killed on cancel).
/// The embedded CPU transcriber can't be interrupted mid-pass, so cancel
/// there takes effect at the next stage boundary instead.
#[derive(Clone, Debug)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Permanently uncancelled (CLI path).
    pub fn never() -> Self {
        Self::new()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// `Err("cancelled by user")` when the flag is set.
    pub fn check(&self) -> anyhow::Result<()> {
        if self.is_cancelled() {
            anyhow::bail!("cancelled by user");
        }
        Ok(())
    }
}

impl Default for CancelFlag {
    fn default() -> Self {
        Self::new()
    }
}

/// Pipeline stage. Names mirror the queue stepper the UI already speaks
/// (upload/audio/script/clips), plus the serve-only phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Provision,
    Audio,
    Transcribe,
    Pick,
    Track,
    Render,
    Merge,
    Kit,
    Done,
}

/// One finished clip (or compilation, or full render): everything the UI
/// needs for a tile + player + downloads. Paths are job-relative file
/// names; serve maps them to `/art/<job>/<file>` URLs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClipArtifact {
    pub rank: usize,
    pub title: String,
    pub hook: String,
    pub start_s: f64,
    pub end_s: f64,
    pub tight_dur: f64,
    pub style: String,
    pub source: String,
    pub score: f64,
    pub mp4: String,
    pub poster: Option<String>,
    pub ass: Option<String>,
    pub srt: Option<String>,
    pub kit: Option<String>,
}

/// Events the pipeline emits. Serve forwards these over `/ws` (throttling
/// percent repeats); the CLI ignores them.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobEvent {
    /// A stage started (`pct: None`) or reported progress.
    Stage {
        stage: Stage,
        pct: Option<u8>,
    },
    /// Picking finished: clips land in the grid with "making" pills.
    ClipsPicked {
        clips: Vec<ClipArtifact>,
    },
    /// Per-clip tracking progress (phase within the clip's turn).
    ClipTrack {
        rank: usize,
        pct: u8,
    },
    /// Per-clip render progress (0-100 across all its segments).
    ClipRender {
        rank: usize,
        pct: u8,
    },
    /// A clip file landed: tile flips making -> done.
    ClipDone {
        clip: ClipArtifact,
    },
    /// A download (ffmpeg zip, YuNet, STT weights) reported bytes.
    ModelsProgress {
        id: String,
        done: u64,
        total: u64,
    },
    /// Whole job finished (all artifacts in the report the serve layer
    /// already streamed via ClipDone; this is the status flip).
    JobDone {},
    JobFailed {
        error: String,
    },
}

/// Sink for [`JobEvent`]. `null()` drops everything (CLI).
#[derive(Clone)]
pub struct Emitter {
    tx: Option<mpsc::UnboundedSender<JobEvent>>,
}

impl Emitter {
    pub fn null() -> Self {
        Self { tx: None }
    }

    pub fn new(tx: mpsc::UnboundedSender<JobEvent>) -> Self {
        Self { tx: Some(tx) }
    }

    /// True when something is listening (serve uses it to skip serve-only
    /// side work like clip posters).
    pub fn active(&self) -> bool {
        self.tx.is_some()
    }

    pub fn emit(&self, ev: JobEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ev);
        }
    }

    pub fn stage(&self, stage: Stage, pct: Option<u8>) {
        self.emit(JobEvent::Stage { stage, pct });
    }

    /// Build an owned percent hook that forwards deduped percents.
    /// Call sites keep the box alive and pass `hook.as_deref()`.
    pub fn pct_hook(
        &self,
        mk: impl Fn(u8) -> JobEvent + Send + Sync + 'static,
    ) -> Option<Box<PctFn>> {
        let tx = self.tx.clone()?;
        let last = std::sync::Mutex::new(None::<u8>);
        Some(Box::new(move |p: u8| {
            let mut l = match last.lock() {
                Ok(l) => l,
                Err(_) => return,
            };
            if *l == Some(p) {
                return;
            }
            *l = Some(p);
            let _ = tx.send(mk(p));
        }))
    }

    /// Same, shareable across threads (ffmpeg parses stdout on the
    /// caller and stderr stats on a drain thread into one closure).
    pub fn shared_hook(
        &self,
        mk: impl Fn(u8) -> JobEvent + Send + Sync + 'static,
    ) -> Option<SharedPct> {
        self.pct_hook(mk).map(std::sync::Arc::from)
    }

    /// Build an owned byte hook for downloads (`id` labels the download:
    /// `ffmpeg`, `yunet`, or the STT model id). Forwards only when the
    /// whole percent changes, so chunk streams stay quiet.
    pub fn byte_hook(&self, id: &str) -> Option<Box<ByteFn>> {
        let tx = self.tx.clone()?;
        let id = id.to_string();
        let last = std::sync::Mutex::new(None::<u64>);
        Some(Box::new(move |done: u64, total: u64| {
            let pct = if total > 0 {
                done.saturating_mul(100) / total
            } else {
                0
            };
            let mut l = match last.lock() {
                Ok(l) => l,
                Err(_) => return,
            };
            if *l == Some(pct) {
                return;
            }
            *l = Some(pct);
            let _ = tx.send(JobEvent::ModelsProgress {
                id: id.clone(),
                done,
                total,
            });
        }))
    }
}

impl Default for Emitter {
    fn default() -> Self {
        Self::null()
    }
}
