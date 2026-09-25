//! `digiclip serve`: localhost daemon for the desktop UI.
//!
//! One port, two surfaces, zero polling:
//! - `GET /ws?token=…` — a single JSON WebSocket. The client sends
//!   `{id, cmd, …params}` commands and gets `{type: "res", id, ok, …}`
//!   replies plus `{type: "ev", …}` push events. On `hello` the server
//!   answers with a full `snapshot` (jobs, settings, models, health),
//!   so reconnects resync without a single HTTP fetch.
//! - `GET /art/<job>/<file>?token=…` — job artifacts (mp4 with Range
//!   seeks for `<video>`, posters, ass/srt/kit) via [`ServeFile`].
//!
//! Single-user desktop assumptions: bind 127.0.0.1 only, one per-boot
//! token (Tauri spawns the sidecar with `--token`), one GPU worker
//! (jobs queue behind a semaphore). Pause/resume is intentionally
//! absent — the engine has no suspend points, so the UI offers
//! cancel/retry/remove instead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use clap::Parser;
use tokio::sync::{broadcast, Mutex, Semaphore};
use tower_http::services::ServeFile;

use crate::progress::{ClipArtifact, Emitter, JobEvent, Stage};

// ---------------------------------------------------------------------------
// Options / settings
// ---------------------------------------------------------------------------

/// Job knobs the UI sends. Every field is optional; unset means "server
/// default" (which itself falls back to the saved settings, then to the
/// CLI default). `merge`: absent = off, `""` = bare compile of the picks,
/// `"A-B,..."` = explicit ranges.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct JobOptions {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub count: Option<usize>,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub span: Option<String>,
    #[serde(default)]
    pub timecut_len: Option<f64>,
    #[serde(default)]
    pub take: Option<usize>,
    #[serde(default)]
    pub min_len: Option<f64>,
    #[serde(default)]
    pub max_len: Option<f64>,
    #[serde(default)]
    pub complete_gate: Option<bool>,
    #[serde(default)]
    pub hook_guard: Option<bool>,
    #[serde(default)]
    pub kit: Option<bool>,
    #[serde(default)]
    pub tighten: Option<String>,
    #[serde(default)]
    pub pause_above: Option<f64>,
    #[serde(default)]
    pub pause_keep: Option<f64>,
    #[serde(default)]
    pub filler_words: Option<String>,
    #[serde(default)]
    pub punch: Option<bool>,
    #[serde(default)]
    pub punch_max: Option<usize>,
    #[serde(default)]
    pub punch_db: Option<f64>,
    #[serde(default)]
    pub merge: Option<String>,
    #[serde(default)]
    pub merge_flash: Option<bool>,
    #[serde(default)]
    pub merge_max: Option<f64>,
    #[serde(default)]
    pub style: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub gpu: Option<bool>,
    #[serde(default)]
    pub framing: Option<String>,
    #[serde(default)]
    pub threads: Option<usize>,
}

/// Persisted server settings (secrets stay server-side; the UI only ever
/// sees `key_set`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Settings {
    openrouter_key: Option<String>,
    openrouter_model: Option<String>,
    stt_model: String,
    gpu: bool,
    clips_count: usize,
    caption_default: String,
    tighten: String,
    punch: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            openrouter_key: None,
            openrouter_model: None,
            stt_model: "base.en".into(),
            gpu: true,
            clips_count: 3,
            caption_default: "karaoke".into(),
            tighten: "light".into(),
            punch: true,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct SettingsPublic {
    key_set: bool,
    openrouter_model: Option<String>,
    stt_model: String,
    gpu: bool,
    clips_count: usize,
    caption_default: String,
    tighten: String,
    punch: bool,
}

impl Settings {
    fn public(&self) -> SettingsPublic {
        SettingsPublic {
            key_set: self.openrouter_key.as_ref().is_some_and(|k| !k.is_empty()),
            openrouter_model: self.openrouter_model.clone(),
            stt_model: self.stt_model.clone(),
            gpu: self.gpu,
            clips_count: self.clips_count,
            caption_default: self.caption_default.clone(),
            tighten: self.tighten.clone(),
            punch: self.punch,
        }
    }
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// Queue status. Mirrors the stepper the UI already speaks: queued →
/// extracting (audio) → transcribing → analyzing (picking) → clips_ready,
/// then per-clip renders run (clip rows carry their own state), then done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Extracting,
    Transcribing,
    Analyzing,
    ClipsReady,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClipState {
    pub rank: usize,
    pub title: String,
    pub hook: String,
    pub start_s: f64,
    pub end_s: f64,
    pub tight_dur: f64,
    pub style: String,
    pub source: String,
    pub score: f64,
    pub mp4: Option<String>,
    pub poster: Option<String>,
    pub ass: Option<String>,
    pub srt: Option<String>,
    pub kit: Option<String>,
    #[serde(default)]
    pub track_pct: u8,
    #[serde(default)]
    pub render_pct: u8,
    #[serde(default = "pending_state")]
    pub render_status: String,
}

fn pending_state() -> String {
    "pending".into()
}

impl ClipState {
    fn pending(a: &ClipArtifact) -> Self {
        Self {
            rank: a.rank,
            title: a.title.clone(),
            hook: a.hook.clone(),
            start_s: a.start_s,
            end_s: a.end_s,
            tight_dur: a.tight_dur,
            style: a.style.clone(),
            source: a.source.clone(),
            score: a.score,
            mp4: None,
            poster: None,
            ass: None,
            srt: None,
            kit: None,
            track_pct: 0,
            render_pct: 0,
            render_status: "pending".into(),
        }
    }

    fn done(a: &ClipArtifact) -> Self {
        let mut s = Self::pending(a);
        s.tight_dur = a.tight_dur;
        s.mp4 = Some(a.mp4.clone());
        s.poster = a.poster.clone();
        s.ass = a.ass.clone();
        s.srt = a.srt.clone();
        s.kit = a.kit.clone();
        s.track_pct = 100;
        s.render_pct = 100;
        s.render_status = "done".into();
        s
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub name: String,
    pub source: String,
    pub status: JobStatus,
    pub options: JobOptions,
    #[serde(default)]
    pub clips: Vec<ClipState>,
    #[serde(default)]
    pub error: Option<String>,
    pub created_ms: u64,
    pub out_dir: String,
    #[serde(default)]
    pub duration_s: f64,
}

struct LiveJob {
    record: JobRecord,
    cancel: crate::progress::CancelFlag,
    running: bool,
    removing: bool,
}

// ---------------------------------------------------------------------------
// Models / health
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
struct ModelState {
    size_mb: u64,
    downloaded: bool,
    downloading: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct Health {
    version: String,
    ffmpeg_ok: bool,
    ffmpeg_libass: bool,
    encoder: String,
    whisper_cli: bool,
    whisper_vulkan: bool,
    yunet_ok: bool,
    gpu_available: bool,
    gpu_reason: String,
    jobs_dir: String,
}

fn probe_health() -> Health {
    let ffmpeg = crate::binaries::resolve("ffmpeg");
    let whisper_cli = crate::binaries::resolve("whisper-cli").is_some();
    let whisper_vulkan = crate::binaries::resolve("whisper-cli-vulkan").is_some();
    let yunet_ok = crate::provision::yunet_path().is_file();
    let (gpu_available, gpu_reason) = crate::gpu::resolve_mode(true);
    Health {
        version: env!("CARGO_PKG_VERSION").into(),
        ffmpeg_ok: ffmpeg.is_some(),
        ffmpeg_libass: ffmpeg
            .as_ref()
            .is_some_and(|f| crate::binaries::ffmpeg_has_libass(f)),
        encoder: crate::render::pick_encoder(gpu_available),
        whisper_cli,
        whisper_vulkan,
        yunet_ok,
        gpu_available,
        gpu_reason,
        jobs_dir: jobs_root().display().to_string(),
    }
}

// ---------------------------------------------------------------------------
// Protocol
// ---------------------------------------------------------------------------

/// One client frame: `{ "id": 7, "cmd": "job_start", …params }`.
#[derive(Debug, serde::Deserialize)]
struct ClientMsg {
    id: u64,
    #[serde(flatten)]
    cmd: Cmd,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Cmd {
    Hello,
    JobStart {
        source: String,
        options: JobOptions,
    },
    JobCancel {
        job: String,
    },
    JobRemove {
        job: String,
    },
    JobRetry {
        job: String,
    },
    SettingsGet,
    SettingsSet {
        patch: serde_json::Value,
    },
    ModelsState,
    ModelsDownload {
        id: String,
    },
    ModelsDelete {
        id: String,
    },
    HealthGet,
    OrModels {
        #[serde(default)]
        refresh: bool,
    },
    ClipKit {
        job: String,
        rank: usize,
    },
}

/// One server frame: a correlated `res` or an async `ev`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg {
    Res {
        id: u64,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    Ev {
        #[serde(flatten)]
        ev: Event,
    },
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Event {
    JobCreated {
        job: JobRecord,
    },
    JobUpdated {
        job: JobRecord,
    },
    JobRemoved {
        id: String,
    },
    Stage {
        job: String,
        stage: Stage,
        pct: Option<u8>,
    },
    ClipsPicked {
        job: String,
        clips: Vec<ClipState>,
    },
    ClipTrack {
        job: String,
        rank: usize,
        pct: u8,
    },
    ClipRender {
        job: String,
        rank: usize,
        pct: u8,
    },
    ClipDone {
        job: String,
        clip: ClipState,
    },
    ModelsProgress {
        id: String,
        done: u64,
        total: u64,
    },
    ModelsState {
        models: HashMap<String, ModelState>,
    },
    Health {
        health: Health,
    },
    Toast {
        tone: String,
        title: String,
        body: String,
    },
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct AppState {
    token: String,
    data_dir: PathBuf,
    jobs: Mutex<HashMap<String, LiveJob>>,
    settings: Mutex<Settings>,
    model_runs: Mutex<HashMap<String, ModelRun>>,
    worker: Semaphore,
    bus: broadcast::Sender<ServerMsg>,
    id_counter: AtomicU64,
}

struct ModelRun {
    downloading: bool,
    progress: Option<u8>,
    error: Option<String>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn jobs_root() -> PathBuf {
    crate::provision::root().join("jobs")
}

impl AppState {
    fn settings_path(&self) -> PathBuf {
        self.data_dir.join("settings.json")
    }

    fn load_settings(&self) -> Settings {
        std::fs::read_to_string(self.settings_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save_settings(&self, s: &Settings) {
        let _ = std::fs::create_dir_all(&self.data_dir);
        if let Ok(txt) = serde_json::to_string_pretty(s) {
            let _ = std::fs::write(self.settings_path(), txt);
        }
    }

    fn job_path(&self, id: &str) -> PathBuf {
        jobs_root().join(id)
    }

    fn persist_job(&self, r: &JobRecord) {
        let dir = PathBuf::from(&r.out_dir);
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(txt) = serde_json::to_string_pretty(r) {
            let _ = std::fs::write(dir.join("job.json"), txt);
        }
    }

    fn models_state(&self, runs: &HashMap<String, ModelRun>) -> HashMap<String, ModelState> {
        let mut out = HashMap::new();
        for (id, meta) in crate::models::models() {
            let run = runs.get(id);
            out.insert(
                id.to_string(),
                ModelState {
                    size_mb: meta.size_mb,
                    downloaded: crate::models::is_downloaded(id),
                    downloading: run.is_some_and(|r| r.downloading),
                    progress: run.and_then(|r| r.progress),
                    error: run.and_then(|r| r.error.clone()),
                },
            );
        }
        out
    }

    fn next_id(&self, prefix: &str) -> String {
        let n = self.id_counter.fetch_add(1, Ordering::SeqCst);
        format!("{prefix}-{:x}-{:x}", now_ms(), n)
    }
}

// ---------------------------------------------------------------------------
// Args construction (serve options -> CLI args, one code path for runs)
// ---------------------------------------------------------------------------

/// Build the exact [`crate::cli::Args`] a CLI invocation with the same
/// knobs would parse, so serve runs can never drift from CLI semantics.
fn args_for(
    source: &Path,
    out_dir: &Path,
    o: &JobOptions,
    s: &Settings,
) -> anyhow::Result<crate::cli::Args> {
    use crate::cli::Args;
    let mut argv: Vec<String> = vec!["digiclip".into(), source.display().to_string()];
    fn flag(argv: &mut Vec<String>, k: &str, v: String) {
        argv.push(k.into());
        argv.push(v);
    }
    flag(
        &mut argv,
        "--mode",
        o.mode.clone().unwrap_or_else(|| "clips".into()),
    );
    flag(&mut argv, "--out-dir", out_dir.display().to_string());
    flag(
        &mut argv,
        "--kind",
        o.kind.clone().unwrap_or_else(|| "smart".into()),
    );
    flag(
        &mut argv,
        "--count",
        o.count.unwrap_or(s.clips_count).to_string(),
    );
    if let Some(v) = o.seed {
        flag(&mut argv, "--seed", v.to_string());
    }
    if let Some(v) = &o.span {
        flag(&mut argv, "--span", v.clone());
    }
    flag(
        &mut argv,
        "--timecut-len",
        o.timecut_len.unwrap_or(15.0).to_string(),
    );
    if let Some(v) = o.take {
        flag(&mut argv, "--take", v.to_string());
    }
    flag(
        &mut argv,
        "--min-len",
        o.min_len.unwrap_or(15.0).to_string(),
    );
    flag(
        &mut argv,
        "--max-len",
        o.max_len.unwrap_or(90.0).to_string(),
    );
    flag(
        &mut argv,
        "--complete-gate",
        o.complete_gate.unwrap_or(true).to_string(),
    );
    flag(
        &mut argv,
        "--hook-guard",
        o.hook_guard.unwrap_or(true).to_string(),
    );
    flag(&mut argv, "--kit", o.kit.unwrap_or(true).to_string());
    flag(
        &mut argv,
        "--tighten",
        o.tighten.clone().unwrap_or_else(|| s.tighten.clone()),
    );
    flag(
        &mut argv,
        "--pause-above",
        o.pause_above.unwrap_or(0.8).to_string(),
    );
    flag(
        &mut argv,
        "--pause-keep",
        o.pause_keep.unwrap_or(0.25).to_string(),
    );
    if let Some(v) = &o.filler_words {
        flag(&mut argv, "--filler-words", v.clone());
    }
    flag(&mut argv, "--punch", o.punch.unwrap_or(s.punch).to_string());
    flag(
        &mut argv,
        "--punch-max",
        o.punch_max.unwrap_or(2).to_string(),
    );
    flag(
        &mut argv,
        "--punch-db",
        o.punch_db.unwrap_or(4.0).to_string(),
    );
    if let Some(m) = &o.merge {
        // Bare `--merge ""` compiles the picks; non-empty joins ranges.
        argv.push("--merge".into());
        argv.push(m.clone());
    }
    if o.merge_flash.unwrap_or(false) {
        argv.push("--merge-flash".into());
    }
    flag(
        &mut argv,
        "--merge-max",
        o.merge_max.unwrap_or(60.0).to_string(),
    );
    let style = o.style.clone().unwrap_or_else(|| s.caption_default.clone());
    flag(&mut argv, "--style", style);
    flag(
        &mut argv,
        "--lang",
        o.lang.clone().unwrap_or_else(|| "en".into()),
    );
    flag(
        &mut argv,
        "--model",
        o.model.clone().unwrap_or_else(|| s.stt_model.clone()),
    );
    flag(&mut argv, "--gpu", o.gpu.unwrap_or(s.gpu).to_string());
    let key = s.openrouter_key.clone().filter(|k| !k.is_empty());
    if let Some(k) = key {
        flag(&mut argv, "--openrouter-key", k);
    }
    // The scoring model is account-level (owned by settings); jobs don't
    // override it, so the argv builder stays total over JobOptions.
    if let Some(m) = s.openrouter_model.clone().filter(|m| !m.is_empty()) {
        flag(&mut argv, "--openrouter-model", m);
    }
    flag(
        &mut argv,
        "--framing",
        o.framing.clone().unwrap_or_else(|| "smart".into()),
    );
    if let Some(v) = o.threads {
        flag(&mut argv, "--threads", v.to_string());
    }
    let args = Args::try_parse_from(&argv).map_err(|e| anyhow::anyhow!("bad options: {e}"))?;
    Ok(args)
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Broadcast + persist one record. Call with the jobs lock held.
fn publish(st: &AppState, r: &JobRecord) {
    st.persist_job(r);
    let _ = st.bus.send(ServerMsg::Ev {
        ev: Event::JobUpdated { job: r.clone() },
    });
}

async fn run_job(st: Arc<AppState>, id: String) {
    // Single GPU worker: queued jobs wait here (status stays Queued).
    let _permit = st.worker.acquire().await.unwrap();
    let (args, cancel) = {
        let mut jobs = st.jobs.lock().await;
        let Some(live) = jobs.get_mut(&id) else {
            return;
        };
        if live.removing {
            return;
        }
        live.running = true;
        live.record.status = JobStatus::Extracting;
        live.record.error = None;
        let r = live.record.clone();
        publish(&st, &r);
        let settings = st.settings.lock().await.clone();
        let source = PathBuf::from(&r.source);
        let out_dir = PathBuf::from(&r.out_dir);
        match args_for(&source, &out_dir, &r.options, &settings) {
            Ok(a) => (a, live.cancel.clone()),
            Err(e) => {
                live.record.status = JobStatus::Failed;
                live.record.error = Some(e.to_string());
                live.running = false;
                let r = live.record.clone();
                publish(&st, &r);
                let _ = st.bus.send(ServerMsg::Ev {
                    ev: Event::Toast {
                        tone: "error".into(),
                        title: "Job failed".into(),
                        body: e.to_string(),
                    },
                });
                return;
            }
        }
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JobEvent>();
    let emit = Emitter::new(tx);

    // Event forwarder: pipeline events -> record mutations + socket events.
    let stf = st.clone();
    let idf = id.clone();
    let fwd = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let mut jobs = stf.jobs.lock().await;
            let Some(live) = jobs.get_mut(&idf) else {
                break;
            };
            if live.removing {
                break;
            }
            let bus = &stf.bus;
            match ev {
                JobEvent::Stage { stage, pct } => {
                    let status = match stage {
                        Stage::Provision | Stage::Audio => JobStatus::Extracting,
                        Stage::Transcribe => JobStatus::Transcribing,
                        Stage::Pick | Stage::Track => JobStatus::Analyzing,
                        Stage::Render | Stage::Merge | Stage::Kit => {
                            if live.record.status == JobStatus::ClipsReady {
                                JobStatus::ClipsReady
                            } else {
                                JobStatus::Analyzing
                            }
                        }
                        Stage::Done => live.record.status,
                    };
                    if status != live.record.status {
                        live.record.status = status;
                        publish(&stf, &live.record.clone());
                    }
                    let _ = bus.send(ServerMsg::Ev {
                        ev: Event::Stage {
                            job: idf.clone(),
                            stage,
                            pct,
                        },
                    });
                }
                JobEvent::ClipsPicked { clips } => {
                    live.record.clips = clips.iter().map(ClipState::pending).collect();
                    live.record.status = JobStatus::ClipsReady;
                    let r = live.record.clone();
                    publish(&stf, &r);
                    let _ = bus.send(ServerMsg::Ev {
                        ev: Event::ClipsPicked {
                            job: idf.clone(),
                            clips: r.clips.clone(),
                        },
                    });
                }
                JobEvent::ClipTrack { rank, pct } => {
                    if let Some(c) = live.record.clips.iter_mut().find(|c| c.rank == rank) {
                        c.track_pct = pct;
                        if c.render_status == "pending" {
                            c.render_status = "rendering".into();
                        }
                    }
                    let _ = bus.send(ServerMsg::Ev {
                        ev: Event::ClipTrack {
                            job: idf.clone(),
                            rank,
                            pct,
                        },
                    });
                }
                JobEvent::ClipRender { rank, pct } => {
                    if let Some(c) = live.record.clips.iter_mut().find(|c| c.rank == rank) {
                        c.render_pct = pct;
                        if c.render_status == "pending" {
                            c.render_status = "rendering".into();
                        }
                    }
                    let _ = bus.send(ServerMsg::Ev {
                        ev: Event::ClipRender {
                            job: idf.clone(),
                            rank,
                            pct,
                        },
                    });
                }
                JobEvent::ClipDone { clip } => {
                    let state = ClipState::done(&clip);
                    if let Some(c) = live.record.clips.iter_mut().find(|c| c.rank == clip.rank) {
                        *c = state.clone();
                    } else {
                        live.record.clips.push(state.clone());
                    }
                    let r = live.record.clone();
                    publish(&stf, &r);
                    let _ = bus.send(ServerMsg::Ev {
                        ev: Event::ClipDone {
                            job: idf.clone(),
                            clip: state,
                        },
                    });
                }
                JobEvent::ModelsProgress {
                    id: mid,
                    done,
                    total,
                } => {
                    let _ = bus.send(ServerMsg::Ev {
                        ev: Event::ModelsProgress {
                            id: mid,
                            done,
                            total,
                        },
                    });
                }
                JobEvent::JobDone {} | JobEvent::JobFailed { .. } => {}
            }
        }
    });

    let joined = {
        // Quarantine: the pipeline blocks OS threads all over (child
        // waits, ONNX inference, ffmpeg progress reads). Running it on a
        // runtime worker would wedge the socket pump whenever the pool
        // runs dry (seen live: the forwarder went silent for a whole
        // job). A std thread with its own current-thread runtime keeps
        // the main pool fluid; the async bits inside (downloads,
        // OpenRouter) don't need Send workers, they need a driver.
        let args_o = args;
        let emit_o = emit;
        let cancel_o = cancel.clone();
        tokio::task::spawn_blocking(move || {
            let ctx_o = crate::pipeline::JobCtx {
                emit: &emit_o,
                cancel: &cancel_o,
            };
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(crate::pipeline::run_inner(&args_o, &ctx_o)),
                Err(e) => Err(anyhow::anyhow!("runtime: {e}")),
            }
        })
        .await
    };
    let res = match joined {
        Ok(inner) => inner,
        Err(e) => Err(anyhow::anyhow!("job thread: {e}")),
    };
    // Drain remaining events before finalizing (the join already
    // dropped the emitter, so the forwarder sees the closed channel).
    let _ = fwd.await;

    let mut jobs = st.jobs.lock().await;
    let Some(live) = jobs.get_mut(&id) else {
        return;
    };
    live.running = false;
    match res {
        Ok(_) => {
            if cancel.is_cancelled() {
                live.record.status = JobStatus::Cancelled;
            } else {
                live.record.status = JobStatus::Done;
                // Crash-proofing: a ClipDone may have been lost if the
                // process died between render and emit — here the run is
                // intact, so clips already streamed stand as-is.
            }
            let name = live.record.name.clone();
            let r = live.record.clone();
            let removing = live.removing;
            if removing {
                let dir = r.out_dir.clone();
                let jid = r.id.clone();
                jobs.remove(&id);
                drop(jobs);
                let _ = std::fs::remove_dir_all(&dir);
                let _ = st.bus.send(ServerMsg::Ev {
                    ev: Event::JobRemoved { id: jid },
                });
                return;
            }
            publish(&st, &r);
            if live.record.status == JobStatus::Done {
                let _ = st.bus.send(ServerMsg::Ev {
                    ev: Event::Toast {
                        tone: "success".into(),
                        title: "Clips ready".into(),
                        body: format!("{} — all done.", name),
                    },
                });
            }
        }
        Err(e) => {
            let msg = e.to_string();
            if cancel.is_cancelled() || msg.contains("cancelled by user") {
                live.record.status = JobStatus::Cancelled;
                live.record.error = None;
            } else {
                live.record.status = JobStatus::Failed;
                live.record.error = Some(msg.clone());
                let name = live.record.name.clone();
                let _ = st.bus.send(ServerMsg::Ev {
                    ev: Event::Toast {
                        tone: "error".into(),
                        title: "Job failed".into(),
                        body: format!("{} — {}", name, truncate(&msg, 160)),
                    },
                });
            }
            let removing = live.removing;
            let r = live.record.clone();
            if removing {
                let dir = r.out_dir.clone();
                let jid = r.id.clone();
                jobs.remove(&id);
                drop(jobs);
                let _ = std::fs::remove_dir_all(&dir);
                let _ = st.bus.send(ServerMsg::Ev {
                    ev: Event::JobRemoved { id: jid },
                });
                return;
            }
            publish(&st, &r);
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    format!("{}…", &s[..n])
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

fn ok(id: u64, data: Option<serde_json::Value>) -> ServerMsg {
    ServerMsg::Res {
        id,
        ok: true,
        error: None,
        data,
    }
}

fn err(id: u64, e: impl std::fmt::Display) -> ServerMsg {
    ServerMsg::Res {
        id,
        ok: false,
        error: Some(e.to_string()),
        data: None,
    }
}

async fn snapshot(st: &AppState) -> serde_json::Value {
    let jobs = st.jobs.lock().await;
    let records: Vec<JobRecord> = {
        let mut v: Vec<JobRecord> = jobs.values().map(|l| l.record.clone()).collect();
        v.sort_by_key(|r| r.created_ms);
        v
    };
    drop(jobs);
    let settings = st.settings.lock().await.clone().public();
    let runs = st.model_runs.lock().await;
    let models = st.models_state(&runs);
    drop(runs);
    serde_json::json!({
        "jobs": records,
        "settings": settings,
        "models": models,
        "health": probe_health(),
    })
}

async fn handle_cmd(st: Arc<AppState>, msg: ClientMsg) -> Vec<ServerMsg> {
    let id = msg.id;
    match msg.cmd {
        Cmd::Hello => {
            let data = snapshot(&st).await;
            vec![ok(id, Some(data))]
        }
        Cmd::JobStart { source, options } => {
            let src = PathBuf::from(&source);
            if !src.is_file() {
                return vec![err(id, format!("source not found: {source}"))];
            }
            let stem = src
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "video".into());
            let probe = crate::ffmpeg::probe(&src);
            let job_id = st.next_id("job");
            let out_dir = st.job_path(&job_id).display().to_string();
            let record = JobRecord {
                id: job_id.clone(),
                name: stem,
                source,
                status: JobStatus::Queued,
                options,
                clips: vec![],
                error: None,
                created_ms: now_ms(),
                out_dir,
                duration_s: probe.duration_s.unwrap_or(0.0),
            };
            st.persist_job(&record);
            {
                let mut jobs = st.jobs.lock().await;
                jobs.insert(
                    job_id.clone(),
                    LiveJob {
                        record: record.clone(),
                        cancel: crate::progress::CancelFlag::new(),
                        running: false,
                        removing: false,
                    },
                );
            }
            let _ = st.bus.send(ServerMsg::Ev {
                ev: Event::JobCreated {
                    job: record.clone(),
                },
            });
            let st2 = st.clone();
            tokio::spawn(async move {
                run_job(st2, job_id).await;
            });
            vec![ok(id, Some(serde_json::json!({ "job": record })))]
        }
        Cmd::JobCancel { job: jid } => {
            let jobs = st.jobs.lock().await;
            match jobs.get(&jid) {
                Some(live) => {
                    live.cancel.cancel();
                    vec![ok(id, None)]
                }
                None => vec![err(id, "unknown job")],
            }
        }
        Cmd::JobRemove { job: jid } => {
            let mut jobs = st.jobs.lock().await;
            match jobs.get_mut(&jid) {
                Some(live) if live.running => {
                    live.removing = true;
                    live.cancel.cancel();
                    vec![ok(id, None)]
                }
                Some(_) => {
                    let live = jobs.remove(&jid).unwrap();
                    drop(jobs);
                    let _ = std::fs::remove_dir_all(&live.record.out_dir);
                    let _ = st.bus.send(ServerMsg::Ev {
                        ev: Event::JobRemoved { id: jid },
                    });
                    vec![ok(id, None)]
                }
                None => vec![err(id, "unknown job")],
            }
        }
        Cmd::JobRetry { job: jid } => {
            let exists_running = {
                let jobs = st.jobs.lock().await;
                jobs.get(&jid).map(|l| (l.running, l.record.clone()))
            };
            match exists_running {
                Some((true, _)) => vec![err(id, "job is running")],
                Some((false, mut record)) => {
                    // Fresh outputs, same knobs. Old clips stay visible
                    // until the new picks land (same language as retry in
                    // the queue: rows melt, they never blank).
                    record.status = JobStatus::Queued;
                    record.error = None;
                    st.persist_job(&record);
                    {
                        let mut jobs = st.jobs.lock().await;
                        if let Some(live) = jobs.get_mut(&jid) {
                            live.record = record.clone();
                            live.cancel = crate::progress::CancelFlag::new();
                            live.removing = false;
                        }
                    }
                    let _ = st.bus.send(ServerMsg::Ev {
                        ev: Event::JobUpdated { job: record },
                    });
                    let st2 = st.clone();
                    let jid2 = jid.clone();
                    tokio::spawn(async move {
                        run_job(st2, jid2).await;
                    });
                    vec![ok(id, None)]
                }
                None => vec![err(id, "unknown job")],
            }
        }
        Cmd::SettingsGet => {
            let s = st.settings.lock().await.clone().public();
            vec![ok(id, Some(serde_json::json!(s)))]
        }
        Cmd::SettingsSet { patch } => {
            let mut s = st.settings.lock().await;
            if let Some(v) = patch.get("openrouter_key").and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    s.openrouter_key = Some(v.to_string());
                }
            }
            if patch.get("openrouter_key").is_some_and(|v| v.is_null()) {
                s.openrouter_key = None;
            }
            if let Some(v) = patch.get("openrouter_model").and_then(|v| v.as_str()) {
                s.openrouter_model = Some(v.to_string());
            }
            if let Some(v) = patch.get("stt_model").and_then(|v| v.as_str()) {
                if crate::models::meta(v).is_ok() {
                    s.stt_model = v.to_string();
                }
            }
            if let Some(v) = patch.get("gpu").and_then(|v| v.as_bool()) {
                s.gpu = v;
            }
            if let Some(v) = patch.get("clips_count").and_then(|v| v.as_u64()) {
                s.clips_count = (v as usize).clamp(1, 10);
            }
            if let Some(v) = patch.get("caption_default").and_then(|v| v.as_str()) {
                s.caption_default = crate::captions::ass::valid_preset(v);
            }
            if let Some(v) = patch.get("tighten").and_then(|v| v.as_str()) {
                if matches!(v, "off" | "light" | "punchy") {
                    s.tighten = v.to_string();
                }
            }
            if let Some(v) = patch.get("punch").and_then(|v| v.as_bool()) {
                s.punch = v;
            }
            let pub_ = s.public();
            st.save_settings(&s);
            drop(s);
            vec![ok(id, Some(serde_json::json!(pub_)))]
        }
        Cmd::ModelsState => {
            let runs = st.model_runs.lock().await;
            let models = st.models_state(&runs);
            vec![ok(id, Some(serde_json::json!({ "models": models })))]
        }
        Cmd::ModelsDownload { id: mid } => {
            if crate::models::meta(&mid).is_err() {
                return vec![err(id, format!("unknown model [{mid}]"))];
            }
            {
                let mut runs = st.model_runs.lock().await;
                if runs.get(&mid).is_some_and(|r| r.downloading) {
                    return vec![err(id, "already downloading")];
                }
                runs.insert(
                    mid.clone(),
                    ModelRun {
                        downloading: true,
                        progress: None,
                        error: None,
                    },
                );
            }
            let models = {
                let runs = st.model_runs.lock().await;
                st.models_state(&runs)
            };
            let _ = st.bus.send(ServerMsg::Ev {
                ev: Event::ModelsState { models },
            });
            let st2 = st.clone();
            tokio::spawn(async move {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JobEvent>();
                let emit = Emitter::new(tx);
                let hook = emit.byte_hook(&mid);
                let dl = crate::models::download_with(&mid, hook.as_deref());
                // Forward byte progress while downloading.
                let st3 = st2.clone();
                let mid3 = mid.clone();
                let fwd = tokio::spawn(async move {
                    while let Some(ev) = rx.recv().await {
                        if let JobEvent::ModelsProgress { done, total, .. } = ev {
                            let pct = if total > 0 {
                                (done.saturating_mul(100) / total) as u8
                            } else {
                                0
                            };
                            {
                                let mut runs = st3.model_runs.lock().await;
                                if let Some(r) = runs.get_mut(&mid3) {
                                    r.progress = Some(pct);
                                }
                            }
                            let _ = st3.bus.send(ServerMsg::Ev {
                                ev: Event::ModelsProgress {
                                    id: mid3.clone(),
                                    done,
                                    total,
                                },
                            });
                        }
                    }
                });
                let res = dl.await;
                drop(emit);
                let _ = fwd.await;
                {
                    let mut runs = st2.model_runs.lock().await;
                    match &res {
                        Ok(_) => {
                            runs.remove(&mid);
                        }
                        Err(e) => {
                            runs.insert(
                                mid.clone(),
                                ModelRun {
                                    downloading: false,
                                    progress: None,
                                    error: Some(truncate(&e.to_string(), 200)),
                                },
                            );
                        }
                    }
                }
                let models = {
                    let runs = st2.model_runs.lock().await;
                    st2.models_state(&runs)
                };
                let _ = st2.bus.send(ServerMsg::Ev {
                    ev: Event::ModelsState { models },
                });
                let _ = st2.bus.send(ServerMsg::Ev {
                    ev: Event::Toast {
                        tone: if res.is_ok() { "success" } else { "error" }.into(),
                        title: if res.is_ok() {
                            "Model ready"
                        } else {
                            "Download failed"
                        }
                        .into(),
                        body: mid.clone(),
                    },
                });
                let _ = st2.bus.send(ServerMsg::Ev {
                    ev: Event::Health {
                        health: probe_health(),
                    },
                });
            });
            vec![ok(id, None)]
        }
        Cmd::ModelsDelete { id: mid } => {
            if crate::models::meta(&mid).is_err() {
                return vec![err(id, format!("unknown model [{mid}]"))];
            }
            {
                let runs = st.model_runs.lock().await;
                if runs.get(&mid).is_some_and(|r| r.downloading) {
                    return vec![err(id, "download in progress")];
                }
            }
            match crate::models::file_for(&mid) {
                Ok(p) => {
                    let _ = std::fs::remove_file(&p);
                    let runs = st.model_runs.lock().await;
                    let models = st.models_state(&runs);
                    let _ = st.bus.send(ServerMsg::Ev {
                        ev: Event::ModelsState { models },
                    });
                    vec![ok(id, None)]
                }
                Err(e) => vec![err(id, e)],
            }
        }
        Cmd::HealthGet => vec![ok(id, Some(serde_json::json!(probe_health())))],
        Cmd::OrModels { refresh } => match or_models(&st, refresh).await {
            Ok(v) => vec![ok(id, Some(v))],
            Err(e) => vec![err(id, e)],
        },
        Cmd::ClipKit { job, rank } => {
            let jobs = st.jobs.lock().await;
            match jobs.get(&job) {
                Some(live) => match live.record.clips.iter().find(|c| c.rank == rank) {
                    Some(c) => match &c.kit {
                        Some(kit) => {
                            let p = PathBuf::from(&live.record.out_dir).join(kit);
                            match std::fs::read_to_string(&p) {
                                Ok(text) => vec![ok(id, Some(serde_json::json!({ "text": text })))],
                                Err(_) => vec![err(id, "kit not written yet")],
                            }
                        }
                        None => vec![err(id, "no kit for this clip")],
                    },
                    None => vec![err(id, "unknown clip")],
                },
                None => vec![err(id, "unknown job")],
            }
        }
    }
}

/// OpenRouter model catalog (for the model picker), cached 24h.
async fn or_models(st: &AppState, refresh: bool) -> anyhow::Result<serde_json::Value> {
    let cache = st.data_dir.join("or_models.json");
    if !refresh {
        if let Ok(txt) = std::fs::read_to_string(&cache) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                let age_ok = v
                    .get("fetched_ms")
                    .and_then(|m| m.as_u64())
                    .is_some_and(|m| now_ms().saturating_sub(m) < 24 * 3600 * 1000);
                if age_ok {
                    return Ok(v);
                }
            }
        }
    }
    let settings = st.settings.lock().await.clone();
    let key = settings
        .openrouter_key
        .clone()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no OpenRouter key saved"))?;
    let base = std::env::var("OPENROUTER_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into());
    let resp = reqwest::Client::new()
        .get(format!("{base}/models"))
        .bearer_auth(key)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("OpenRouter models: HTTP {}", resp.status());
    }
    let data: serde_json::Value = resp.json().await?;
    let models: Vec<serde_json::Value> = data
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|m| {
            let mid = m.get("id")?.as_str()?;
            Some(serde_json::json!({ "id": mid, "name": m.get("name").and_then(|n| n.as_str()).unwrap_or(mid) }))
        })
        .collect();
    let v = serde_json::json!({ "models": models, "fetched_ms": now_ms() });
    let _ = std::fs::create_dir_all(&st.data_dir);
    let _ = std::fs::write(&cache, serde_json::to_string_pretty(&v).unwrap_or_default());
    Ok(v)
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct TokenQ {
    token: String,
}

/// Stream the job's SOURCE file (range seeks, same as artifacts). The
/// webview can't read user disks directly, so source playback goes
/// through here instead of a file:// URL that would never load.
async fn src_handler(
    AxPath(job): AxPath<String>,
    Query(q): Query<TokenQ>,
    headers: axum::http::HeaderMap,
    State(st): State<Arc<AppState>>,
) -> impl IntoResponse {
    if q.token != st.token {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let jobs = st.jobs.lock().await;
    let Some(live) = jobs.get(&job) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = PathBuf::from(&live.record.source);
    drop(jobs);
    if !path.is_file() {
        return StatusCode::NOT_FOUND.into_response();
    }
    use tower::ServiceExt;
    let mut builder = axum::http::Request::builder();
    for key in [axum::http::header::RANGE, axum::http::header::IF_RANGE] {
        if let Some(v) = headers.get(key.clone()) {
            builder = builder.header(key, v);
        }
    }
    let req = builder.body(axum::body::Body::empty()).unwrap();
    match ServeFile::new(path).oneshot(req).await {
        Ok(res) => res.into_response(),
        Err(never) => match never {},
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(q): Query<TokenQ>,
    State(st): State<Arc<AppState>>,
) -> impl IntoResponse {
    if q.token != st.token {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |socket| socket_loop(st, socket))
        .into_response()
}

async fn socket_loop(st: Arc<AppState>, socket: WebSocket) {
    let (mut send, mut recv) = socket.split();
    use futures_util::{SinkExt, StreamExt};
    let mut bus = st.bus.subscribe();
    // Flush loop: bus events + command replies share one sender.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMsg>();
    let write = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let txt = match serde_json::to_string(&msg) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if send.send(Message::Text(txt.into())).await.is_err() {
                break;
            }
        }
    });
    // Bus -> socket.
    let out_tx2 = out_tx.clone();
    let pump = tokio::spawn(async move {
        loop {
            match bus.recv().await {
                Ok(msg) => {
                    if out_tx2.send(msg).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
    // Socket -> commands.
    while let Some(frame) = recv.next().await {
        let msg = match frame {
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };
        let req: ClientMsg = match serde_json::from_str(&msg) {
            Ok(r) => r,
            Err(e) => {
                let _ = out_tx.send(ServerMsg::Res {
                    id: 0,
                    ok: false,
                    error: Some(format!("bad frame: {e}")),
                    data: None,
                });
                continue;
            }
        };
        for reply in handle_cmd(st.clone(), req).await {
            if out_tx.send(reply).is_err() {
                break;
            }
        }
    }
    pump.abort();
    write.abort();
}

async fn art_handler(
    AxPath((job, file)): AxPath<(String, String)>,
    Query(q): Query<TokenQ>,
    headers: axum::http::HeaderMap,
    State(st): State<Arc<AppState>>,
) -> impl IntoResponse {
    if q.token != st.token {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Flat job dir: no separators, no traversal, ever.
    if file.contains('/') || file.contains('\\') || file.contains("..") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let jobs = st.jobs.lock().await;
    let Some(live) = jobs.get(&job) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = PathBuf::from(&live.record.out_dir).join(&file);
    drop(jobs);
    if !path.is_file() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // ServeFile answers Range requests, which is what makes <video>
    // seeking (and poster loads) instant instead of full downloads.
    // The client's conditional headers must be forwarded — a bare
    // request would always answer 200 with the whole file.
    use tower::ServiceExt;
    let mut builder = axum::http::Request::builder();
    for key in [axum::http::header::RANGE, axum::http::header::IF_RANGE] {
        if let Some(v) = headers.get(key.clone()) {
            builder = builder.header(key, v);
        }
    }
    let req = builder.body(axum::body::Body::empty()).unwrap();
    match ServeFile::new(path).oneshot(req).await {
        Ok(res) => res.into_response(),
        Err(never) => match never {},
    }
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

/// Reload persisted jobs. Anything non-terminal died with the last process
/// (retry is one click); a fully-rendered set recomputes to done.
fn reload_jobs() -> HashMap<String, LiveJob> {
    let mut out = HashMap::new();
    let root = jobs_root();
    let _ = std::fs::create_dir_all(&root);
    let entries = std::fs::read_dir(&root)
        .map(|r| r.collect::<Vec<_>>())
        .unwrap_or_default();
    for e in entries {
        let okr = e.as_ref().ok().and_then(|e| {
            std::fs::read_to_string(e.path().join("job.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<JobRecord>(&t).ok())
        });
        let Some(mut r) = okr else { continue };
        r.out_dir = root.join(&r.id).display().to_string();
        match r.status {
            JobStatus::Done | JobStatus::Failed | JobStatus::Cancelled => {}
            _ => {
                let all_done =
                    !r.clips.is_empty() && r.clips.iter().all(|c| c.render_status == "done");
                if all_done {
                    r.status = JobStatus::Done;
                } else {
                    r.status = JobStatus::Failed;
                    r.error = Some("interrupted by server restart".into());
                }
            }
        }
        out.insert(
            r.id.clone(),
            LiveJob {
                record: r,
                cancel: crate::progress::CancelFlag::new(),
                running: false,
                removing: false,
            },
        );
    }
    out
}

/// Serve forever: `digiclip --serve [--port N] [--token T]`.
///
/// Runs on a dedicated 8-worker runtime: the pipeline blocks workers
/// with child-process waits and ONNX inference, and the default runtime
/// sizes itself to the machine (a 1-CPU sandbox would starve the socket
/// pump for the whole job). Eight workers time-slice anywhere.
pub fn run_serve_blocking(
    port: u16,
    token: Option<String>,
    data_dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .thread_name("serve")
        .enable_all()
        .build()?
        .block_on(run_serve(port, token, data_dir))
}

/// Serve forever: `digiclip --serve [--port N] [--token T]`.
pub async fn run_serve(
    port: u16,
    token: Option<String>,
    data_dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    let data_dir = data_dir.unwrap_or_else(|| crate::provision::root());
    std::fs::create_dir_all(&data_dir)?;
    std::fs::create_dir_all(jobs_root())?;
    let token = token.filter(|t| !t.is_empty()).unwrap_or_else(|| {
        // Per-boot token: local-only auth without stored secrets.
        format!("{:x}{:x}", now_ms(), std::process::id())
    });
    let (bus, _) = broadcast::channel::<ServerMsg>(512);
    let mut st = AppState {
        token: token.clone(),
        data_dir,
        jobs: Mutex::new(HashMap::new()),
        settings: Mutex::new(Settings::default()),
        model_runs: Mutex::new(HashMap::new()),
        worker: Semaphore::new(1),
        bus,
        id_counter: AtomicU64::new(1),
    };
    st.settings = Mutex::new(st.load_settings());
    st.jobs = Mutex::new(reload_jobs());
    let st = Arc::new(st);

    let app = axum::Router::new()
        .route("/ws", get(ws_handler))
        .route("/art/{job}/{file}", get(art_handler))
        .route("/src/{job}", get(src_handler))
        // Loopback-only daemon, token-gated URLs: permissive CORS lets the
        // desktop webview fetch blobs (downloads) without a proxy.
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(st);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!("DIGICLIP_SERVE port={port} token={token}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> (PathBuf, PathBuf, JobOptions, Settings) {
        (
            PathBuf::from("C:\\vids\\in.mp4"),
            PathBuf::from("C:\\vids\\job-1"),
            JobOptions::default(),
            Settings::default(),
        )
    }

    #[test]
    fn serve_defaults_match_cli() {
        let (src, out, o, s) = opts();
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert!(matches!(a.mode, crate::cli::Mode::Clips));
        assert_eq!(a.count, 3);
        assert!(matches!(a.kind, crate::cli::Kind::Smart));
        assert!(matches!(a.framing, crate::cli::Framing::Smart));
        assert_eq!(a.style.as_deref(), Some("karaoke"));
        assert!(a.merge.is_none());
        assert!(a.kit && a.punch && a.gpu);
        assert!(a.openrouter_key.is_none());
    }

    #[test]
    fn bare_merge_compiles_picks() {
        let (src, out, mut o, s) = opts();
        o.merge = Some(String::new());
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!(a.merge.as_deref(), Some(""));
    }

    #[test]
    fn explicit_merge_ranges_survive() {
        let (src, out, mut o, s) = opts();
        o.merge = Some("23.8-38.8,87.1-102.1".into());
        o.merge_flash = Some(true);
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!(a.merge.as_deref(), Some("23.8-38.8,87.1-102.1"));
        assert!(a.merge_flash);
    }

    #[test]
    fn knobs_override_settings() {
        let (src, out, mut o, mut s) = opts();
        s.clips_count = 5;
        o.count = Some(2);
        o.kind = Some("timecut".into());
        o.tighten = Some("punchy".into());
        o.gpu = Some(false);
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!(a.count, 2);
        assert!(matches!(a.kind, crate::cli::Kind::Timecut));
        assert!(matches!(a.tighten, crate::cli::TightenMode::Punchy));
        assert!(!a.gpu);
        // Untouched knobs fall back to settings, then CLI defaults.
        let (_, _, o2, _) = opts();
        let b = args_for(&src, &out, &o2, &s).unwrap();
        assert_eq!(b.count, 5);
    }

    #[test]
    fn duration_knobs_reach_args() {
        let (src, out, mut o, s) = opts();
        // Untouched: engine default window.
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!((a.min_len, a.max_len), (15.0, 90.0));
        // Exact mode arrives as min == max.
        o.min_len = Some(30.0);
        o.max_len = Some(30.0);
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!((a.min_len, a.max_len), (30.0, 30.0));
    }

    #[test]
    fn settings_key_reaches_args() {
        let (src, out, o, mut s) = opts();
        s.openrouter_key = Some("sk-test".into());
        s.openrouter_model = Some("x/y".into());
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!(a.openrouter_key.as_deref(), Some("sk-test"));
        assert_eq!(a.openrouter_model.as_deref(), Some("x/y"));
    }
}
