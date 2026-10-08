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

use std::collections::{BTreeMap, HashMap};
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

mod diag;
pub mod mcp;
mod mcp_apps;
mod watch;

use diag::diagnostics;

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
    /// Caption language (translated); absent/"off" = the spoken language.
    #[serde(default)]
    pub subs_lang: Option<String>,
    /// Two-person layout: "auto" | "single" | "split".
    #[serde(default)]
    pub layout: Option<String>,
    /// System One judge: "auto" | "jev" | "laya" | "off" (absent = the
    /// settings default).
    #[serde(default)]
    pub decider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub gpu: Option<bool>,
    #[serde(default)]
    pub framing: Option<String>,
    #[serde(default)]
    pub threads: Option<usize>,
    /// `9:16` (default), `4:5`, `1:1`, `16:9`.
    #[serde(default)]
    pub aspect: Option<String>,
    /// Caption motion: `pop` (default), `words`, `none`.
    #[serde(default)]
    pub caption_anim: Option<String>,
    /// The Look (see [`crate::look`]): kept as raw JSON so a field this
    /// build does not know never fails the frame, and so a saved preset
    /// round-trips it untouched.
    #[serde(default)]
    pub look: Option<serde_json::Value>,
    /// Absent = off, `""` = each clip's title, text = that text.
    #[serde(default)]
    pub headline: Option<String>,
    /// Progress bar color `#RRGGBB` (absent = off).
    #[serde(default)]
    pub progress_bar: Option<String>,
    /// Logo image path + corner (`tl`/`tr`/`bl`/`br`).
    #[serde(default)]
    pub logo: Option<String>,
    #[serde(default)]
    pub logo_pos: Option<String>,
    /// Background music path + level (dB relative to speech).
    #[serde(default)]
    pub music: Option<String>,
    #[serde(default)]
    pub music_db: Option<f64>,
    /// Topic to steer clip picking toward.
    #[serde(default)]
    pub focus: Option<String>,
}

/// Persisted server settings (secrets stay server-side; the UI only ever
/// sees `key_set` / `ai_keys_set`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Settings {
    openrouter_key: Option<String>,
    openrouter_model: Option<String>,
    /// Clip AI provider (an id from `providers::PROVIDERS`).
    ai_provider: String,
    /// Keys, models and addresses of the providers other than OpenRouter
    /// (which keeps `openrouter_key` / `openrouter_model`). Keys never
    /// leave the server; addresses only exist for `base_editable` ones.
    ai_keys: BTreeMap<String, String>,
    ai_models: BTreeMap<String, String>,
    ai_base_urls: BTreeMap<String, String>,
    stt_model: String,
    /// Spoken language for transcription: a whisper code or `auto`.
    stt_lang: String,
    gpu: bool,
    clips_count: usize,
    caption_default: String,
    tighten: String,
    punch: bool,
    /// TypeSafe Jev key (System One judging); never leaves the server.
    jev_key: Option<String>,
    /// Default System One judge: "auto" | "jev" | "laya" | "off".
    decider: String,
    /// Watch folder: new videos dropped here start jobs with
    /// `watch_options`.
    watch_dir: Option<String>,
    watch_on: bool,
    watch_options: JobOptions,
    /// Saved job-option presets (named), in the order the UI lists them.
    presets: Vec<Preset>,
    /// MCP server: on/off and its fixed loopback port.
    mcp_on: bool,
    mcp_port: u16,
    /// MCP bearer token (made on first start, kept across restarts);
    /// never part of [`SettingsPublic`].
    mcp_token: Option<String>,
}

/// A named set of job options.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Preset {
    name: String,
    options: JobOptions,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            openrouter_key: None,
            openrouter_model: None,
            ai_provider: "openrouter".into(),
            ai_keys: BTreeMap::new(),
            ai_models: BTreeMap::new(),
            ai_base_urls: BTreeMap::new(),
            stt_model: "base.en".into(),
            stt_lang: "en".into(),
            gpu: true,
            clips_count: 3,
            caption_default: "karaoke".into(),
            tighten: "light".into(),
            punch: true,
            jev_key: None,
            decider: "auto".into(),
            watch_dir: None,
            watch_on: false,
            watch_options: JobOptions::default(),
            presets: vec![],
            mcp_on: true,
            mcp_port: mcp::DEFAULT_PORT,
            mcp_token: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct SettingsPublic {
    key_set: bool,
    openrouter_model: Option<String>,
    ai_provider: String,
    /// Provider ids that have a key saved (OpenRouter included).
    ai_keys_set: Vec<String>,
    /// Chosen model per provider (OpenRouter included).
    ai_models: BTreeMap<String, String>,
    ai_base_urls: BTreeMap<String, String>,
    ai_providers: Vec<serde_json::Value>,
    stt_model: String,
    stt_lang: String,
    gpu: bool,
    clips_count: usize,
    caption_default: String,
    tighten: String,
    punch: bool,
    jev_key_set: bool,
    decider: String,
    watch_dir: Option<String>,
    watch_on: bool,
    watch_options: JobOptions,
    presets: Vec<Preset>,
    mcp_on: bool,
    mcp_port: u16,
}

fn nonempty(v: Option<&String>) -> Option<String> {
    v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl Settings {
    /// The saved key for a provider, if any.
    fn ai_key(&self, id: &str) -> Option<String> {
        if id == "openrouter" {
            nonempty(self.openrouter_key.as_ref())
        } else {
            nonempty(self.ai_keys.get(id))
        }
    }

    /// The chosen model for a provider, if any.
    fn ai_model(&self, id: &str) -> Option<String> {
        if id == "openrouter" {
            nonempty(self.openrouter_model.as_ref())
        } else {
            nonempty(self.ai_models.get(id))
        }
    }

    /// The address override for a provider (only editable ones have one).
    fn ai_base(&self, id: &str) -> Option<String> {
        if crate::providers::provider(id).is_some_and(|p| p.base_editable) {
            nonempty(self.ai_base_urls.get(id))
        } else {
            None
        }
    }

    fn set_ai_model(&mut self, id: &str, model: Option<String>) {
        if id == "openrouter" {
            self.openrouter_model = model;
        } else if let Some(m) = model {
            self.ai_models.insert(id.to_string(), m);
        } else {
            self.ai_models.remove(id);
        }
    }

    /// The provider jobs run with (a stale or unknown id means OpenRouter).
    fn ai_active(&self) -> &'static crate::providers::Provider {
        crate::providers::provider(&self.ai_provider)
            .or_else(|| crate::providers::provider("openrouter"))
            .expect("openrouter is in the registry")
    }

    /// Apply the `ai_*` parts of a settings patch. Returns the providers
    /// whose model list is worth fetching now (a key was set, the active
    /// provider changed, or an address changed).
    fn apply_ai_patch(&mut self, patch: &serde_json::Value) -> Vec<String> {
        use serde_json::Value;
        let mut refresh: Vec<String> = Vec::new();
        let mut want = |id: &str| {
            if !refresh.iter().any(|r| r == id) {
                refresh.push(id.to_string());
            }
        };
        // Legacy OpenRouter key: a new one deserves a fresh list too.
        if matches!(patch.get("openrouter_key"), Some(Value::String(v)) if !v.trim().is_empty()) {
            want("openrouter");
        }
        if let Some(Value::Object(keys)) = patch.get("ai_keys") {
            for (id, v) in keys {
                if crate::providers::provider(id).is_none() {
                    continue;
                }
                match v {
                    Value::String(k) if !k.trim().is_empty() => {
                        if id == "openrouter" {
                            self.openrouter_key = Some(k.trim().to_string());
                        } else {
                            self.ai_keys.insert(id.clone(), k.trim().to_string());
                        }
                        want(id);
                    }
                    Value::Null => {
                        if id == "openrouter" {
                            self.openrouter_key = None;
                        } else {
                            self.ai_keys.remove(id);
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(Value::Object(models)) = patch.get("ai_models") {
            for (id, v) in models {
                if crate::providers::provider(id).is_none() {
                    continue;
                }
                if let Value::String(m) = v {
                    let m = m.trim();
                    self.set_ai_model(id, (!m.is_empty()).then(|| m.to_string()));
                }
            }
        }
        if let Some(Value::Object(bases)) = patch.get("ai_base_urls") {
            for (id, v) in bases {
                if !crate::providers::provider(id).is_some_and(|p| p.base_editable) {
                    continue;
                }
                let before = self.ai_base_urls.get(id).cloned();
                match v {
                    Value::String(u) => {
                        let u = u.trim().trim_end_matches('/');
                        if u.is_empty() {
                            self.ai_base_urls.remove(id);
                        } else if u.starts_with("http://") || u.starts_with("https://") {
                            self.ai_base_urls.insert(id.clone(), u.to_string());
                        }
                    }
                    Value::Null => {
                        self.ai_base_urls.remove(id);
                    }
                    _ => {}
                }
                if self.ai_base_urls.get(id) != before.as_ref() {
                    want(id);
                }
            }
        }
        if let Some(id) = patch.get("ai_provider").and_then(Value::as_str) {
            if crate::providers::provider(id).is_some() && id != self.ai_provider {
                self.ai_provider = id.to_string();
                want(id);
            }
        }
        refresh
    }

    fn public(&self) -> SettingsPublic {
        let providers = crate::providers::PROVIDERS;
        SettingsPublic {
            key_set: self.openrouter_key.as_ref().is_some_and(|k| !k.is_empty()),
            openrouter_model: self.openrouter_model.clone(),
            ai_provider: self.ai_provider.clone(),
            ai_keys_set: providers
                .iter()
                .filter(|p| self.ai_key(p.id).is_some())
                .map(|p| p.id.to_string())
                .collect(),
            ai_models: providers
                .iter()
                .filter_map(|p| Some((p.id.to_string(), self.ai_model(p.id)?)))
                .collect(),
            ai_base_urls: providers
                .iter()
                .filter_map(|p| Some((p.id.to_string(), self.ai_base(p.id)?)))
                .collect(),
            ai_providers: providers
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "id": p.id,
                        "label": p.label,
                        "needs_key": p.key.as_str(),
                        "key_url": p.key_url,
                        "default_base": p.base_url,
                        "base_editable": p.base_editable,
                        "local": p.local,
                        "default_model": p.default_model,
                    })
                })
                .collect(),
            stt_model: self.stt_model.clone(),
            stt_lang: self.stt_lang.clone(),
            gpu: self.gpu,
            clips_count: self.clips_count,
            caption_default: self.caption_default.clone(),
            tighten: self.tighten.clone(),
            punch: self.punch,
            jev_key_set: self.jev_key.as_ref().is_some_and(|k| !k.is_empty()),
            decider: self.decider.clone(),
            watch_dir: self.watch_dir.clone(),
            watch_on: self.watch_on,
            watch_options: self.watch_options.clone(),
            presets: self.presets.clone(),
            mcp_on: self.mcp_on,
            mcp_port: self.mcp_port,
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
    /// Fetching the source from a link (before it queues).
    Downloading,
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
    #[serde(default)]
    pub why: String,
    #[serde(default)]
    pub scores: Option<crate::openrouter::Scores>,
    #[serde(default)]
    pub hashtags: Vec<String>,
    /// Bumps every time this clip's files are (re)written, so the UI can
    /// cache-bust the video and poster after an edit.
    #[serde(default)]
    pub rev: u32,
    /// Extra-aspect files of this clip.
    #[serde(default)]
    pub variants: Vec<crate::progress::Variant>,
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
            why: a.why.clone(),
            scores: a.scores.clone(),
            hashtags: a.hashtags.clone(),
            rev: 0,
            variants: a.variants.clone(),
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
    /// The link a downloaded source came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Who started the job when it wasn't the UI: `mcp:<client name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

struct LiveJob {
    record: JobRecord,
    cancel: crate::progress::CancelFlag,
    running: bool,
    removing: bool,
    /// A run is spawned and waiting for the worker.
    queued: bool,
    /// Clips the next run re-renders (edits, clips from the transcript)
    /// instead of a full pick.
    redo: Option<Vec<crate::redo::Spec>>,
}

impl LiveJob {
    fn new(record: JobRecord) -> Self {
        Self {
            record,
            cancel: crate::progress::CancelFlag::new(),
            running: false,
            removing: false,
            queued: false,
            redo: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Models / health
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
struct ModelState {
    /// "stt" (whisper) or "decider" (Laya).
    kind: &'static str,
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

/// `probe_health` shells out (ffmpeg, nvidia-smi and a WMI query that can
/// take seconds), so async callers run it on the blocking pool instead of
/// stalling a runtime worker and, with it, the socket.
async fn health() -> Health {
    tokio::task::spawn_blocking(probe_health)
        .await
        .expect("health probe panicked")
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
#[allow(clippy::large_enum_variant)]
enum Cmd {
    Hello,
    JobStart {
        source: String,
        #[serde(default)]
        options: JobOptions,
        /// Set by the MCP server (`mcp:<client>`), never by the UI.
        #[serde(default, skip)]
        origin: Option<String>,
    },
    /// A job from a link: the source downloads first (yt-dlp).
    JobStartUrl {
        url: String,
        #[serde(default)]
        options: JobOptions,
        #[serde(default, skip)]
        origin: Option<String>,
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
    /// `model`, not `id`: a param named `id` would overwrite the frame id
    /// when the client spreads params into the frame.
    ModelsDownload {
        #[serde(rename = "model")]
        id: String,
    },
    ModelsDelete {
        #[serde(rename = "model")]
        id: String,
    },
    HealthGet,
    /// Model list of a Clip AI provider (default: the active one).
    AiModels {
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        refresh: bool,
    },
    /// Older spelling of `ai_models` for OpenRouter.
    OrModels {
        #[serde(default)]
        refresh: bool,
    },
    ClipKit {
        job: String,
        rank: usize,
    },
    /// Re-render one clip with a new range / title / caption style /
    /// fixed caption words.
    ClipEdit {
        job: String,
        rank: usize,
        #[serde(default)]
        start_s: Option<f64>,
        #[serde(default)]
        end_s: Option<f64>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        style: Option<String>,
        #[serde(default)]
        fixes: Vec<crate::redo::Fix>,
    },
    /// A new clip over an exact range (picked from the transcript).
    ClipAdd {
        job: String,
        start_s: f64,
        end_s: f64,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        style: Option<String>,
    },
    /// The job's transcript words (editor + transcript clip maker).
    TranscriptGet {
        job: String,
    },
    /// Write a redacted diagnostics bundle; replies with its path.
    Diagnostics,
    /// MCP server state (port, token, clients, activity, tools).
    McpState,
    /// New MCP token: every connected client has to be set up again.
    McpRotateToken,
    /// Add (or with `remove`, take out) DigiClip in an AI app's MCP config:
    /// any id in `mcp_apps::APPS` (`claude_desktop`, `codex`, `vscode`…).
    McpInstall {
        client: String,
        #[serde(default)]
        remove: bool,
    },
}

/// One server frame: a correlated `res` or an async `ev`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
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
    /// Settings changed outside a `settings_set` (a model was picked
    /// after a fetch).
    Settings {
        settings: SettingsPublic,
    },
    /// MCP server state changed (see [`mcp::public`]).
    Mcp {
        mcp: serde_json::Value,
    },
    /// An MCP client asked the app to show something.
    McpFocus {
        #[serde(skip_serializing_if = "Option::is_none")]
        job: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        page: Option<String>,
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
    mcp: mcp::Mcp,
}

struct ModelRun {
    downloading: bool,
    progress: Option<u8>,
    error: Option<String>,
}

/// Model id of the Laya bundle in the model list.
const LAYA: &str = "laya";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A transcription language whisper takes: `auto` or a 2-3 letter code
/// (`en`, `es`, `haw`…). Anything else is dropped.
fn valid_lang(v: &str) -> Option<String> {
    let v = v.trim().to_ascii_lowercase();
    (v == "auto" || (2..=3).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_lowercase()))
        .then_some(v)
}

/// A `--decider` value, or none.
fn valid_decider(v: &str) -> Option<String> {
    let v = v.trim().to_ascii_lowercase();
    matches!(v.as_str(), "auto" | "jev" | "laya" | "off").then_some(v)
}

/// `<data dir>/jobs`, set once at boot. A `--data-dir` override moves the
/// jobs too, so a test daemon never sees (or touches) the real queue.
static JOBS_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

fn jobs_root() -> PathBuf {
    JOBS_ROOT
        .get()
        .cloned()
        .unwrap_or_else(|| crate::provision::root().join("jobs"))
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
                    kind: "stt",
                    size_mb: meta.size_mb,
                    downloaded: crate::models::is_downloaded(id),
                    downloading: run.is_some_and(|r| r.downloading),
                    progress: run.and_then(|r| r.progress),
                    error: run.and_then(|r| r.error.clone()),
                },
            );
        }
        let run = runs.get(LAYA);
        out.insert(
            LAYA.to_string(),
            ModelState {
                kind: "decider",
                size_mb: crate::decide::laya::SIZE_MB,
                downloaded: crate::decide::laya::is_ready(),
                downloading: run.is_some_and(|r| r.downloading),
                progress: run.and_then(|r| r.progress),
                error: run.and_then(|r| r.error.clone()),
            },
        );
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
        o.lang
            .as_deref()
            .and_then(valid_lang)
            .unwrap_or_else(|| s.stt_lang.clone()),
    );
    flag(
        &mut argv,
        "--model",
        o.model.clone().unwrap_or_else(|| s.stt_model.clone()),
    );
    flag(&mut argv, "--gpu", o.gpu.unwrap_or(s.gpu).to_string());
    // Clip AI: the active provider's address, key and model. The scoring
    // model is account-level (owned by settings); jobs don't override it,
    // so the argv builder stays total over JobOptions.
    let ai = s.ai_active().id;
    flag(&mut argv, "--ai-provider", ai.to_string());
    if let Some(b) = s.ai_base(ai) {
        flag(&mut argv, "--ai-base-url", b);
    }
    if let Some(k) = s.ai_key(ai) {
        flag(&mut argv, "--openrouter-key", k);
    }
    if let Some(m) = s.ai_model(ai) {
        flag(&mut argv, "--openrouter-model", m);
    }
    if let Some(k) = s.jev_key.clone().filter(|k| !k.is_empty()) {
        flag(&mut argv, "--jev-key", k);
    }
    flag(
        &mut argv,
        "--decider",
        o.decider
            .as_deref()
            .and_then(valid_decider)
            .or_else(|| valid_decider(&s.decider))
            .unwrap_or_else(|| "auto".into()),
    );
    flag(
        &mut argv,
        "--framing",
        o.framing.clone().unwrap_or_else(|| "smart".into()),
    );
    if let Some(v) = o.threads {
        flag(&mut argv, "--threads", v.to_string());
    }
    if let Some(v) = o.aspect.as_ref().filter(|v| !v.trim().is_empty()) {
        flag(&mut argv, "--aspect", v.clone());
    }
    if let Some(v) = crate::translate::target_code(o.subs_lang.as_deref()) {
        flag(&mut argv, "--subs-lang", v);
    }
    if let Some(v) = o
        .layout
        .as_ref()
        .filter(|v| matches!(v.as_str(), "auto" | "single" | "split"))
    {
        flag(&mut argv, "--layout", v.clone());
    }
    if let Some(v) = o
        .caption_anim
        .as_ref()
        .filter(|v| matches!(v.as_str(), "pop" | "words" | "none"))
    {
        flag(&mut argv, "--caption-anim", v.clone());
    }
    // Only a non-empty object travels: no look is the default argv.
    if let Some(v) = o
        .look
        .as_ref()
        .filter(|v| v.as_object().is_some_and(|m| !m.is_empty()))
    {
        flag(&mut argv, "--look", v.to_string());
    }
    // `""` = on with the default (clip title / brand yellow).
    if let Some(v) = &o.headline {
        if v.trim().is_empty() {
            argv.push("--headline".into());
        } else {
            flag(&mut argv, "--headline", v.clone());
        }
    }
    if let Some(v) = &o.progress_bar {
        if v.trim().is_empty() {
            argv.push("--progress-bar".into());
        } else {
            flag(&mut argv, "--progress-bar", v.clone());
        }
    }
    if let Some(v) = o.logo.as_ref().filter(|v| !v.is_empty()) {
        flag(&mut argv, "--logo", v.clone());
        if let Some(p) = o.logo_pos.as_ref().filter(|p| !p.trim().is_empty()) {
            flag(&mut argv, "--logo-pos", p.trim().to_ascii_lowercase());
        }
    }
    if let Some(v) = o.music.as_ref().filter(|v| !v.is_empty()) {
        flag(&mut argv, "--music", v.clone());
        if let Some(db) = o.music_db {
            flag(&mut argv, "--music-db", db.to_string());
        }
    }
    if let Some(v) = o.focus.as_ref().filter(|v| !v.trim().is_empty()) {
        flag(&mut argv, "--focus", v.clone());
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
    let (args, cancel, redo_ranks) = {
        let mut jobs = st.jobs.lock().await;
        let Some(live) = jobs.get_mut(&id) else {
            return;
        };
        live.queued = false;
        if live.removing {
            return;
        }
        live.running = true;
        live.record.status = JobStatus::Extracting;
        live.record.error = None;
        let redo = live.redo.take();
        let r = live.record.clone();
        publish(&st, &r);
        let settings = st.settings.lock().await.clone();
        let source = PathBuf::from(&r.source);
        let out_dir = PathBuf::from(&r.out_dir);
        let built = match &redo {
            // Redo: exact clips on the same out dir (the transcript cache
            // makes it a straight re-render), whatever the job's mode.
            Some(specs) => {
                let options = JobOptions {
                    mode: Some("clips".into()),
                    merge: None,
                    ..r.options.clone()
                };
                args_for(&source, &out_dir, &options, &settings).and_then(|mut a| {
                    let p = out_dir.join("redo.json");
                    std::fs::create_dir_all(&out_dir)?;
                    std::fs::write(&p, serde_json::to_string_pretty(specs)?)?;
                    a.redo = Some(p);
                    Ok(a)
                })
            }
            None => args_for(&source, &out_dir, &r.options, &settings),
        };
        let redo_ranks: Option<Vec<usize>> =
            redo.map(|specs| specs.iter().map(|s| s.rank).collect());
        match built {
            Ok(a) => (a, live.cancel.clone(), redo_ranks),
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
    let redo_run = redo_ranks.is_some();
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
                        Stage::Download => JobStatus::Downloading,
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
                    if redo_run {
                        // Only the redone ranks go back to "making"; the
                        // rest of the grid stays as it is.
                        for a in &clips {
                            let mut fresh = ClipState::pending(a);
                            match live.record.clips.iter_mut().find(|c| c.rank == a.rank) {
                                Some(c) => {
                                    fresh.rev = c.rev;
                                    *c = fresh;
                                }
                                None => live.record.clips.push(fresh),
                            }
                        }
                        live.record.clips.sort_by_key(|c| c.rank);
                    } else {
                        let prev = std::mem::take(&mut live.record.clips);
                        live.record.clips = clips
                            .iter()
                            .map(|a| {
                                let mut c = ClipState::pending(a);
                                c.rev = prev.iter().find(|p| p.rank == a.rank).map_or(0, |p| p.rev);
                                c
                            })
                            .collect();
                    }
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
                    let mut state = ClipState::done(&clip);
                    if let Some(c) = live.record.clips.iter_mut().find(|c| c.rank == clip.rank) {
                        state.rev = c.rev + 1;
                        // Extra aspects can finish before the main file.
                        state.variants = std::mem::take(&mut c.variants);
                        *c = state.clone();
                    } else {
                        state.rev = 1;
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
                JobEvent::ClipVariant { rank, variant } => {
                    if let Some(c) = live.record.clips.iter_mut().find(|c| c.rank == rank) {
                        c.variants.retain(|v| v.aspect != variant.aspect);
                        c.variants.push(variant);
                        let r = live.record.clone();
                        publish(&stf, &r);
                    }
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
                let (title, body) = if redo_run {
                    ("Clip updated", format!("{name} — re-rendered."))
                } else {
                    ("Clips ready", format!("{name} — all done."))
                };
                let _ = st.bus.send(ServerMsg::Ev {
                    ev: Event::Toast {
                        tone: "success".into(),
                        title: title.into(),
                        body,
                    },
                });
            }
        }
        Err(e) => {
            let msg = e.to_string();
            if let Some(ranks) = &redo_ranks {
                // A failed edit never fails the job: the other clips are
                // fine. The redone ranks show as failed (their files may be
                // half-written) and the error goes to a toast.
                for c in live.record.clips.iter_mut() {
                    if ranks.contains(&c.rank) && c.render_status != "done" {
                        c.render_status = "failed".into();
                    }
                }
                live.record.status = JobStatus::Done;
                if !(cancel.is_cancelled() || msg.contains("cancelled by user")) {
                    let _ = st.bus.send(ServerMsg::Ev {
                        ev: Event::Toast {
                            tone: "error".into(),
                            title: "Edit failed".into(),
                            body: truncate(&msg, 160),
                        },
                    });
                }
            } else if cancel.is_cancelled() || msg.contains("cancelled by user") {
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
        "health": health().await,
        "mcp": mcp::public(st).await,
        // What this engine can do, so the app only enables live controls.
        "caps": crate::look::CAPS,
    })
}

async fn handle_cmd(st: Arc<AppState>, msg: ClientMsg) -> Vec<ServerMsg> {
    let id = msg.id;
    match msg.cmd {
        Cmd::Hello => {
            let data = snapshot(&st).await;
            vec![ok(id, Some(data))]
        }
        Cmd::JobStart {
            source,
            options,
            origin,
        } => {
            if crate::fetch::is_url(&source) {
                let record = start_url_job(&st, source.trim().to_string(), options, origin).await;
                return vec![ok(id, Some(serde_json::json!({ "job": record })))];
            }
            match start_job(&st, source, options, origin).await {
                Ok(record) => vec![ok(id, Some(serde_json::json!({ "job": record })))],
                Err(e) => vec![err(id, e)],
            }
        }
        Cmd::JobStartUrl {
            url,
            options,
            origin,
        } => {
            if !crate::fetch::is_url(&url) {
                return vec![err(id, "not a link (http:// or https://)")];
            }
            let record = start_url_job(&st, url.trim().to_string(), options, origin).await;
            vec![ok(id, Some(serde_json::json!({ "job": record })))]
        }
        Cmd::McpState => vec![ok(id, Some(mcp::public(&st).await))],
        Cmd::McpRotateToken => {
            mcp::rotate_token(&st).await;
            vec![ok(id, Some(mcp::public(&st).await))]
        }
        Cmd::McpInstall { client, remove } => match mcp::install(&st, &client, remove).await {
            Ok(path) => {
                mcp::broadcast(&st).await;
                vec![ok(id, Some(serde_json::json!({ "path": path })))]
            }
            Err(e) => vec![err(id, e)],
        },
        Cmd::Diagnostics => {
            let text = diagnostics(&st).await;
            let dir = st.data_dir.join("logs");
            let path = dir.join(format!("diagnostics-{}.txt", now_ms()));
            match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &text)) {
                Ok(()) => vec![ok(
                    id,
                    Some(serde_json::json!({
                        "path": path.display().to_string(),
                        "bytes": text.len(),
                    })),
                )],
                Err(e) => vec![err(id, format!("could not write diagnostics: {e}"))],
            }
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
                Some(live) if live.running || live.record.status == JobStatus::Downloading => {
                    // Running (or downloading): the run cleans up once it
                    // stops.
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
                jobs.get(&jid)
                    .map(|l| (l.running || l.queued, l.record.clone()))
            };
            match exists_running {
                Some((true, _)) => vec![err(id, "job is running")],
                Some((false, record))
                    if record.url.is_some() && !Path::new(&record.source).is_file() =>
                {
                    // The download never finished: fetch again.
                    {
                        let mut jobs = st.jobs.lock().await;
                        if let Some(live) = jobs.get_mut(&jid) {
                            live.cancel = crate::progress::CancelFlag::new();
                            live.removing = false;
                            live.queued = true;
                            live.redo = None;
                        }
                    }
                    let st2 = st.clone();
                    tokio::spawn(async move { fetch_then_run(st2, jid).await });
                    vec![ok(id, None)]
                }
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
                            live.queued = true;
                            live.redo = None;
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
            let refresh = s.apply_ai_patch(&patch);
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
            match patch.get("jev_key") {
                Some(serde_json::Value::String(v)) if !v.trim().is_empty() => {
                    s.jev_key = Some(v.trim().to_string())
                }
                Some(serde_json::Value::Null) => s.jev_key = None,
                _ => {}
            }
            if let Some(v) = patch
                .get("decider")
                .and_then(|v| v.as_str())
                .and_then(valid_decider)
            {
                s.decider = v;
            }
            if let Some(v) = patch.get("stt_model").and_then(|v| v.as_str()) {
                if crate::models::meta(v).is_ok() {
                    s.stt_model = v.to_string();
                }
            }
            if let Some(v) = patch.get("stt_lang").and_then(|v| v.as_str()) {
                if let Some(l) = valid_lang(v) {
                    s.stt_lang = l;
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
            match patch.get("watch_dir") {
                Some(serde_json::Value::String(v)) if !v.trim().is_empty() => {
                    s.watch_dir = Some(v.trim().to_string())
                }
                Some(serde_json::Value::Null) => s.watch_dir = None,
                _ => {}
            }
            if let Some(v) = patch.get("watch_on").and_then(|v| v.as_bool()) {
                s.watch_on = v;
            }
            if let Some(v) = patch
                .get("watch_options")
                .and_then(|v| serde_json::from_value::<JobOptions>(v.clone()).ok())
            {
                s.watch_options = v;
            }
            if let Some(v) = patch
                .get("presets")
                .and_then(|v| serde_json::from_value::<Vec<Preset>>(v.clone()).ok())
            {
                s.presets = v
                    .into_iter()
                    .filter(|p| !p.name.trim().is_empty())
                    .take(50)
                    .collect();
            }
            let mcp_before = (s.mcp_on, s.mcp_port);
            if let Some(v) = patch.get("mcp_on").and_then(|v| v.as_bool()) {
                s.mcp_on = v;
            }
            if let Some(v) = patch.get("mcp_port").and_then(|v| v.as_u64()) {
                if (1024..=65535).contains(&v) {
                    s.mcp_port = v as u16;
                }
            }
            let mcp_changed = mcp_before != (s.mcp_on, s.mcp_port);
            let pub_ = s.public();
            st.save_settings(&s);
            drop(s);
            // A new key, provider or address: fetch that provider's models
            // now (this also picks one when none is set yet).
            for pid in refresh {
                let st2 = st.clone();
                tokio::spawn(async move {
                    if let Err(e) = ai_models(&st2, Some(pid.clone()), true).await {
                        tracing::info!("model list for {pid}: {e}");
                    }
                });
            }
            if mcp_changed {
                mcp::restart(&st).await;
            }
            vec![ok(id, Some(serde_json::json!(pub_)))]
        }
        Cmd::ModelsState => {
            let runs = st.model_runs.lock().await;
            let models = st.models_state(&runs);
            vec![ok(id, Some(serde_json::json!({ "models": models })))]
        }
        Cmd::ModelsDownload { id: mid } => {
            if mid != LAYA && crate::models::meta(&mid).is_err() {
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
                let dl = async {
                    if mid == LAYA {
                        crate::decide::laya::download(hook.map(Arc::from))
                            .await
                            .map(|_| PathBuf::new())
                    } else {
                        crate::models::download_with(&mid, hook.as_deref()).await
                    }
                };
                // Forward byte progress while downloading.
                let st3 = st2.clone();
                let mid3 = mid.clone();
                let fwd = tokio::spawn(async move {
                    while let Some(ev) = rx.recv().await {
                        if let JobEvent::ModelsProgress { done, total, .. } = ev {
                            let pct =
                                done.saturating_mul(100).checked_div(total).unwrap_or(0) as u8;
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
                        health: health().await,
                    },
                });
            });
            vec![ok(id, None)]
        }
        Cmd::ModelsDelete { id: mid } => {
            if mid != LAYA && crate::models::meta(&mid).is_err() {
                return vec![err(id, format!("unknown model [{mid}]"))];
            }
            {
                let runs = st.model_runs.lock().await;
                if runs.get(&mid).is_some_and(|r| r.downloading) {
                    return vec![err(id, "download in progress")];
                }
            }
            let gone = if mid == LAYA {
                crate::decide::laya::remove().map_err(anyhow::Error::from)
            } else {
                crate::models::file_for(&mid).map(|p| {
                    let _ = std::fs::remove_file(p);
                })
            };
            match gone {
                Ok(()) => {
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
        Cmd::HealthGet => vec![ok(id, Some(serde_json::json!(health().await)))],
        Cmd::AiModels { provider, refresh } => match ai_models(&st, provider, refresh).await {
            Ok(v) => vec![ok(id, Some(v))],
            Err(e) => vec![err(id, e)],
        },
        Cmd::OrModels { refresh } => {
            match ai_models(&st, Some("openrouter".into()), refresh).await {
                Ok(v) => vec![ok(id, Some(v))],
                Err(e) => vec![err(id, e)],
            }
        }
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
        Cmd::ClipEdit {
            job,
            rank,
            start_s,
            end_s,
            title,
            style,
            fixes,
        } => {
            let res = queue_redo(&st, &job, |live| {
                let c = live
                    .record
                    .clips
                    .iter()
                    .find(|c| c.rank == rank)
                    .ok_or("unknown clip")?;
                let (a, b) = (start_s.unwrap_or(c.start_s), end_s.unwrap_or(c.end_s));
                // The hook is the clip's opening words: a new range
                // gets new ones.
                let same_range = (a - c.start_s).abs() < 1e-3 && (b - c.end_s).abs() < 1e-3;
                Ok(crate::redo::Spec {
                    rank,
                    start_s: a,
                    end_s: b,
                    title: Some(title.unwrap_or_else(|| c.title.clone())),
                    hook: same_range.then(|| c.hook.clone()),
                    style: Some(style.unwrap_or_else(|| c.style.clone())),
                    why: Some(c.why.clone()).filter(|w| !w.is_empty()),
                    score: Some(c.score),
                    scores: c.scores.clone(),
                    hashtags: c.hashtags.clone(),
                    source: Some(c.source.clone()),
                    fixes,
                })
            })
            .await;
            match res {
                Ok(()) => vec![ok(id, None)],
                Err(e) => vec![err(id, e)],
            }
        }
        Cmd::ClipAdd {
            job,
            start_s,
            end_s,
            title,
            style,
        } => {
            let mut new_rank = 0;
            let res = queue_redo(&st, &job, |live| {
                let pending = live.redo.iter().flatten().map(|s| s.rank);
                let rank = live
                    .record
                    .clips
                    .iter()
                    .map(|c| c.rank)
                    .chain(pending)
                    .max()
                    .unwrap_or(0)
                    + 1;
                new_rank = rank;
                Ok(crate::redo::Spec {
                    rank,
                    start_s,
                    end_s,
                    title,
                    style,
                    ..Default::default()
                })
            })
            .await;
            match res {
                Ok(()) => vec![ok(id, Some(serde_json::json!({ "rank": new_rank })))],
                Err(e) => vec![err(id, e)],
            }
        }
        Cmd::TranscriptGet { job } => {
            let out_dir = {
                let jobs = st.jobs.lock().await;
                match jobs.get(&job) {
                    Some(live) => PathBuf::from(&live.record.out_dir),
                    None => return vec![err(id, "unknown job")],
                }
            };
            let tr = std::fs::read(out_dir.join("transcript.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<crate::whisper::Transcription>(&b).ok());
            match tr {
                Some(tr) => {
                    let words: Vec<serde_json::Value> = tr
                        .words
                        .iter()
                        .map(|w| serde_json::json!({ "w": w.w, "s": w.s, "e": w.e }))
                        .collect();
                    vec![ok(
                        id,
                        Some(serde_json::json!({ "words": words, "language": tr.language })),
                    )]
                }
                None => vec![err(id, "no transcript yet")],
            }
        }
    }
}

/// Queue a redo (clip edit or new clip) on a finished clips job. The spec
/// is built under the jobs lock; when a redo is already waiting for the
/// worker the new spec joins it (same rank replaces) instead of queueing
/// another run.
/// Queue a job on a local file.
async fn start_job(
    st: &Arc<AppState>,
    source: String,
    options: JobOptions,
    origin: Option<String>,
) -> Result<JobRecord, String> {
    let src = PathBuf::from(&source);
    if !src.is_file() {
        return Err(format!("source not found: {source}"));
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
        url: None,
        origin,
    };
    st.persist_job(&record);
    {
        let mut jobs = st.jobs.lock().await;
        let mut live = LiveJob::new(record.clone());
        live.queued = true;
        jobs.insert(job_id.clone(), live);
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
    Ok(record)
}

/// A job on a link: it shows at once (downloading), then queues like any
/// other once the video is on disk.
async fn start_url_job(
    st: &Arc<AppState>,
    url: String,
    options: JobOptions,
    origin: Option<String>,
) -> JobRecord {
    let job_id = st.next_id("job");
    let out_dir = st.job_path(&job_id).display().to_string();
    let record = JobRecord {
        id: job_id.clone(),
        name: crate::fetch::host(&url),
        source: url.clone(),
        status: JobStatus::Downloading,
        options,
        clips: vec![],
        error: None,
        created_ms: now_ms(),
        out_dir,
        duration_s: 0.0,
        url: Some(url),
        origin,
    };
    st.persist_job(&record);
    {
        let mut jobs = st.jobs.lock().await;
        let mut live = LiveJob::new(record.clone());
        live.queued = true;
        jobs.insert(job_id.clone(), live);
    }
    let _ = st.bus.send(ServerMsg::Ev {
        ev: Event::JobCreated {
            job: record.clone(),
        },
    });
    let st2 = st.clone();
    tokio::spawn(async move { fetch_then_run(st2, job_id).await });
    record
}

/// Download a link job's source (outside the render worker: downloads
/// overlap renders), then run it.
async fn fetch_then_run(st: Arc<AppState>, jid: String) {
    let (url, dir, cancel) = {
        let mut jobs = st.jobs.lock().await;
        let Some(live) = jobs.get_mut(&jid) else {
            return;
        };
        let Some(url) = live.record.url.clone() else {
            return;
        };
        live.record.status = JobStatus::Downloading;
        live.record.error = None;
        let r = live.record.clone();
        publish(&st, &r);
        (
            url,
            PathBuf::from(&r.out_dir).join("source"),
            live.cancel.clone(),
        )
    };
    let stage = |pct: Option<u8>| {
        let _ = st.bus.send(ServerMsg::Ev {
            ev: Event::Stage {
                job: jid.clone(),
                stage: Stage::Download,
                pct,
            },
        });
    };
    stage(Some(0));
    let got = match crate::fetch::ensure_tool(None).await {
        Ok(tool) => {
            let (bus, j) = (st.bus.clone(), jid.clone());
            let c = cancel.clone();
            tokio::task::spawn_blocking(move || {
                let last = std::sync::atomic::AtomicU8::new(0);
                let on_pct = |p: u8| {
                    if last.swap(p, Ordering::Relaxed) != p {
                        let _ = bus.send(ServerMsg::Ev {
                            ev: Event::Stage {
                                job: j.clone(),
                                stage: Stage::Download,
                                pct: Some(p),
                            },
                        });
                    }
                };
                crate::fetch::download(&tool, &url, &dir, &on_pct, &c)
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
        }
        Err(e) => Err(anyhow::anyhow!("yt-dlp unavailable: {e}")),
    };
    let mut jobs = st.jobs.lock().await;
    let Some(live) = jobs.get_mut(&jid) else {
        return;
    };
    if live.removing {
        let dir = live.record.out_dir.clone();
        jobs.remove(&jid);
        drop(jobs);
        let _ = std::fs::remove_dir_all(dir);
        let _ = st.bus.send(ServerMsg::Ev {
            ev: Event::JobRemoved { id: jid },
        });
        return;
    }
    match got {
        Ok(path) => {
            live.record.name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| live.record.name.clone());
            live.record.duration_s = crate::ffmpeg::probe(&path).duration_s.unwrap_or(0.0);
            live.record.source = path.display().to_string();
            live.record.status = JobStatus::Queued;
            let r = live.record.clone();
            drop(jobs);
            publish(&st, &r);
            run_job(st, jid).await;
        }
        Err(e) => {
            live.queued = false;
            let cancelled = cancel.is_cancelled();
            live.record.status = if cancelled {
                JobStatus::Cancelled
            } else {
                JobStatus::Failed
            };
            live.record.error = (!cancelled).then(|| e.to_string());
            let r = live.record.clone();
            drop(jobs);
            publish(&st, &r);
            if !cancelled {
                let _ = st.bus.send(ServerMsg::Ev {
                    ev: Event::Toast {
                        tone: "error".into(),
                        title: "Download failed".into(),
                        body: truncate(&e.to_string(), 200),
                    },
                });
            }
        }
    }
}

async fn queue_redo(
    st: &Arc<AppState>,
    jid: &str,
    build: impl FnOnce(&LiveJob) -> Result<crate::redo::Spec, &'static str>,
) -> Result<(), String> {
    let record = {
        let mut jobs = st.jobs.lock().await;
        let Some(live) = jobs.get_mut(jid) else {
            return Err("unknown job".into());
        };
        if live.running {
            return Err("job is busy — wait for it to finish".into());
        }
        if live.record.options.mode.as_deref() == Some("full") {
            return Err("clip edits need a clips job".into());
        }
        if !PathBuf::from(&live.record.out_dir)
            .join("transcript.json")
            .is_file()
        {
            return Err("no transcript yet — run the job first".into());
        }
        let spec = build(live).map_err(String::from)?;
        if spec.end_s - spec.start_s < crate::redo::MIN_LEN_S {
            return Err(format!("a clip needs at least {}s", crate::redo::MIN_LEN_S));
        }
        if live.queued {
            return match &mut live.redo {
                Some(specs) => {
                    specs.retain(|s| s.rank != spec.rank);
                    specs.push(spec);
                    Ok(())
                }
                None => Err("job is queued — wait for it to finish".into()),
            };
        }
        live.redo = Some(vec![spec]);
        live.queued = true;
        live.cancel = crate::progress::CancelFlag::new();
        live.removing = false;
        live.record.status = JobStatus::Queued;
        live.record.error = None;
        live.record.clone()
    };
    publish(st, &record);
    let st2 = st.clone();
    let jid2 = jid.to_string();
    tokio::spawn(async move {
        run_job(st2, jid2).await;
    });
    Ok(())
}

/// Model list of a Clip AI provider, for the model picker. Cached per
/// provider for 24h (local and custom ones are always asked live). When
/// the chosen model isn't offered (or none is chosen yet) one is picked
/// and saved. Reply: `{provider, models:[{id,name}], fetched_ms, picked}`;
/// `picked` is the model that was just saved, or null.
async fn ai_models(
    st: &AppState,
    provider: Option<String>,
    refresh: bool,
) -> anyhow::Result<serde_json::Value> {
    let settings = st.settings.lock().await.clone();
    let pid = provider
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| settings.ai_provider.clone());
    let p = crate::providers::provider(&pid)
        .ok_or_else(|| anyhow::anyhow!("unknown provider [{pid}]"))?;
    let base = settings
        .ai_base(p.id)
        .unwrap_or_else(|| p.base_url.to_string());
    let base = base.trim_end_matches('/').to_string();
    if base.is_empty() {
        anyhow::bail!(
            "set a base URL for {}",
            p.label.split(" (").next().unwrap_or(p.label)
        );
    }
    let key = settings.ai_key(p.id);
    if p.needs_key() && key.is_none() {
        anyhow::bail!("no key saved for {}", p.label);
    }

    let live = p.local || p.id == "custom";
    let cache = st.data_dir.join("ai_models").join(format!("{}.json", p.id));
    let cached = (!refresh && !live)
        .then(|| std::fs::read_to_string(&cache).ok())
        .flatten()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter(|v| {
            v.get("base").and_then(|b| b.as_str()) == Some(base.as_str())
                && v.get("fetched_ms")
                    .and_then(|m| m.as_u64())
                    .is_some_and(|m| now_ms().saturating_sub(m) < 24 * 3600 * 1000)
        });
    let (models, fetched_ms) = match cached {
        Some(v) => (
            serde_json::from_value::<Vec<serde_json::Value>>(v["models"].clone())
                .unwrap_or_default()
                .iter()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?.to_string();
                    let name = m.get("name").and_then(|n| n.as_str()).unwrap_or(&id);
                    Some(crate::providers::Model {
                        name: name.to_string(),
                        id,
                    })
                })
                .collect::<Vec<_>>(),
            v["fetched_ms"].as_u64().unwrap_or(0),
        ),
        None => {
            let models = fetch_models(p, &base, key.as_deref()).await?;
            let fetched = now_ms();
            if !live {
                let v = serde_json::json!({
                    "provider": p.id,
                    "base": base,
                    "fetched_ms": fetched,
                    "models": models_json(&models),
                });
                let _ = std::fs::create_dir_all(st.data_dir.join("ai_models"));
                let _ =
                    std::fs::write(&cache, serde_json::to_string_pretty(&v).unwrap_or_default());
            }
            (models, fetched)
        }
    };

    // Keep the chosen model valid; the settings are re-read under the
    // lock, since the user may have changed them during the fetch.
    let mut picked = None;
    {
        let mut s = st.settings.lock().await;
        let current = s.ai_model(p.id);
        if let Some(m) = crate::providers::pick_model(p, current.as_deref(), &models) {
            if current.as_deref() != Some(m.as_str()) {
                s.set_ai_model(p.id, Some(m.clone()));
                st.save_settings(&s);
                let _ = st.bus.send(ServerMsg::Ev {
                    ev: Event::Settings {
                        settings: s.public(),
                    },
                });
                picked = Some(m);
            }
        }
    }
    Ok(serde_json::json!({
        "provider": p.id,
        "models": models_json(&models),
        "fetched_ms": fetched_ms,
        "picked": picked,
    }))
}

fn models_json(models: &[crate::providers::Model]) -> Vec<serde_json::Value> {
    models
        .iter()
        .map(|m| serde_json::json!({ "id": m.id, "name": m.name }))
        .collect()
}

/// `GET {base}/models` with the provider's auth, in plain-words errors.
async fn fetch_models(
    p: &crate::providers::Provider,
    base: &str,
    key: Option<&str>,
) -> anyhow::Result<Vec<crate::providers::Model>> {
    use crate::providers::ModelsAuth;
    let timeout = if p.local { 4 } else { 10 };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout))
        .build()?;
    let mut req = client.get(format!("{base}/models"));
    if let Some(k) = key {
        req = match p.models_auth {
            ModelsAuth::Bearer => req.bearer_auth(k),
            ModelsAuth::Anthropic => req
                .header("x-api-key", k)
                .header("anthropic-version", "2023-06-01"),
        };
    }
    let resp = req.send().await.map_err(|e| {
        if p.local || p.id == "custom" {
            anyhow::anyhow!("couldn't reach {} at {base} — is it running?", p.label)
        } else if e.is_timeout() {
            anyhow::anyhow!("{} took too long to answer", p.label)
        } else {
            anyhow::anyhow!("couldn't reach {} at {base}", p.label)
        }
    })?;
    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        anyhow::bail!("{} refused the key (HTTP {})", p.label, status.as_u16());
    }
    if !status.is_success() {
        anyhow::bail!("{} model list: HTTP {}", p.label, status.as_u16());
    }
    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("{} sent a model list DigiClip can't read", p.label))?;
    Ok(crate::providers::parse_models(p, &data))
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
        out.insert(r.id.clone(), LiveJob::new(r));
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
    let data_dir = data_dir.unwrap_or_else(crate::provision::root);
    std::fs::create_dir_all(&data_dir)?;
    let _ = JOBS_ROOT.set(data_dir.join("jobs"));
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
        mcp: mcp::Mcp::default(),
    };
    st.settings = Mutex::new(st.load_settings());
    st.jobs = Mutex::new(reload_jobs());
    let st = Arc::new(st);
    tokio::spawn(watch::run(st.clone()));
    mcp::restart(&st).await;

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
    fn caption_motion_reaches_args() {
        let (src, out, mut o, s) = opts();
        assert_eq!(args_for(&src, &out, &o, &s).unwrap().caption_anim, "pop");
        o.caption_anim = Some("words".into());
        assert_eq!(args_for(&src, &out, &o, &s).unwrap().caption_anim, "words");
        o.caption_anim = Some("bogus".into());
        assert_eq!(args_for(&src, &out, &o, &s).unwrap().caption_anim, "pop");
    }

    #[test]
    fn look_reaches_args_and_only_when_set() {
        let (src, out, mut o, s) = opts();
        let plain = args_for(&src, &out, &o, &s).unwrap();
        assert!(plain.look.is_none());
        // No look, `{}`, null and junk leave the argv alone.
        for v in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::Value::Null),
            Some(serde_json::json!("nope")),
        ] {
            o.look = v;
            let a = args_for(&src, &out, &o, &s).unwrap();
            assert!(a.look.is_none(), "{:?}", o.look);
        }
        // A look travels as compact JSON and parses back to the same thing.
        o.look =
            Some(serde_json::json!({"v": 1, "captions": {"x": 0.5, "y": 0.25, "anim": "bounce"}}));
        let a = args_for(&src, &out, &o, &s).unwrap();
        let raw = a.look.clone().unwrap();
        assert!(!raw.contains('\n') && !raw.contains(": "), "{raw}");
        let c = crate::look::Look::from_arg(&raw).captions.unwrap();
        assert_eq!((c.x, c.y), (Some(0.5), Some(0.25)));
        assert_eq!(c.anim, Some(crate::captions::ass::Anim::Bounce));
        // Everything else in the argv is unchanged.
        let b = crate::cli::Args { look: None, ..a };
        assert_eq!(format!("{b:?}"), format!("{plain:?}"));
    }

    #[test]
    fn job_options_with_an_odd_look_still_deserialise() {
        let o: JobOptions = serde_json::from_str(
            r#"{"style":"hormozi","look":{"v":9,"future":[1,2],"captions":{"x":0.2,"sparkle":true,"font":42},"hologram":{"on":1}},"also_new":1}"#,
        )
        .unwrap();
        assert_eq!(o.style.as_deref(), Some("hormozi"));
        let look = o.look.clone().unwrap();
        assert_eq!(look["captions"]["sparkle"], true);
        // A look of the wrong JSON type does not break the frame either.
        let o2: JobOptions = serde_json::from_str(r#"{"look":"not an object"}"#).unwrap();
        let (src, out, _, s) = opts();
        assert!(args_for(&src, &out, &o2, &s).unwrap().look.is_none());
        // Saved presets carry it through a round trip.
        let p = Preset {
            name: "x".into(),
            options: o,
        };
        let back: Preset = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back.options.look, Some(look));
        // The engine side reads what it understands and ignores the rest.
        let a = args_for(&src, &out, &back.options, &s).unwrap();
        let l = crate::look::Look::from_arg(a.look.as_deref().unwrap());
        assert_eq!(l.captions.unwrap().x, Some(0.2));
    }

    #[test]
    fn transcription_language_reaches_args() {
        let (src, out, mut o, mut s) = opts();
        assert_eq!(args_for(&src, &out, &o, &s).unwrap().lang, "en");
        s.stt_lang = "auto".into();
        assert_eq!(args_for(&src, &out, &o, &s).unwrap().lang, "auto");
        o.lang = Some("ES".into());
        assert_eq!(args_for(&src, &out, &o, &s).unwrap().lang, "es");
        o.lang = Some("-m evil".into());
        assert_eq!(args_for(&src, &out, &o, &s).unwrap().lang, "auto");
    }

    #[test]
    fn old_settings_files_keep_their_values() {
        // A settings.json written before a field existed still loads.
        let s: Settings =
            serde_json::from_str(r#"{"stt_model":"large-v3-turbo","gpu":false}"#).unwrap();
        assert_eq!(s.stt_model, "large-v3-turbo");
        assert!(!s.gpu);
        assert_eq!(s.stt_lang, "en");
        assert_eq!(s.clips_count, 3);
    }

    #[test]
    fn creator_options_reach_args() {
        let (src, out, o, s) = opts();
        // Off by default: 9:16, no overlays.
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!(a.canvas(), crate::compose::Canvas::TALL);
        assert!(a.headline.is_none() && a.progress_bar.is_none());
        assert!(a.logo.is_none() && a.music.is_none() && a.focus.is_none());
        // Empty strings mean "on, default".
        let (_, _, mut o, _) = opts();
        o.aspect = Some("1:1".into());
        o.headline = Some(String::new());
        o.progress_bar = Some(String::new());
        o.logo = Some("C:/brand/logo.png".into());
        o.logo_pos = Some("bl".into());
        o.music = Some("C:/brand/bed.mp3".into());
        o.music_db = Some(-12.5);
        o.focus = Some("pricing".into());
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!(a.canvas(), crate::compose::Canvas::SQUARE);
        assert_eq!(a.headline.as_deref(), Some(""));
        assert_eq!(a.progress_bar.as_deref(), Some("#FFD400"));
        assert_eq!(a.logo_pos, "bl");
        assert_eq!(a.music_db, -12.5);
        assert_eq!(a.focus.as_deref(), Some("pricing"));
        // Explicit values pass through.
        o.headline = Some("Big news".into());
        o.progress_bar = Some("00e5ff".into());
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!(a.headline.as_deref(), Some("Big news"));
        assert_eq!(a.progress_bar.as_deref(), Some("#00E5FF"));
        // Empty = default; a bad value is a clear error, not a silent default.
        o.aspect = Some(String::new());
        o.logo_pos = Some(" ".into());
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!(a.canvas(), crate::compose::Canvas::TALL);
        assert_eq!(a.logo_pos, "tr");
        o.aspect = Some("3:1".into());
        assert!(args_for(&src, &out, &o, &s).is_err());
    }

    #[test]
    fn aspect_lists_render_every_canvas_main_first() {
        use crate::compose::Canvas;
        let (src, out, mut o, s) = opts();
        o.aspect = Some("1:1, 9:16,1:1,,16:9".into());
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!(a.aspect, "1:1,9:16,16:9");
        assert_eq!(a.canvas(), Canvas::SQUARE);
        assert_eq!(a.canvases(), [Canvas::SQUARE, Canvas::TALL, Canvas::WIDE]);
        o.aspect = Some("9:16,3:1".into());
        assert!(args_for(&src, &out, &o, &s).is_err());
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

    #[test]
    fn active_provider_reaches_args() {
        let (src, out, o, mut s) = opts();
        // Untouched: OpenRouter, nothing extra.
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!(a.ai_provider, "openrouter");
        assert!(a.ai_base_url.is_none() && a.openrouter_key.is_none());
        // Another provider carries its own key, model and address, not
        // OpenRouter's.
        s.openrouter_key = Some("sk-or-test".into());
        s.openrouter_model = Some("or/model".into());
        s.apply_ai_patch(&serde_json::json!({
            "ai_provider": "ollama",
            "ai_models": { "ollama": "llama3:8b", "openai": "gpt-5-mini" },
            "ai_base_urls": { "ollama": "http://10.0.0.5:11434/v1/" },
            "ai_keys": { "openai": "sk-openai-test" },
        }));
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!(a.ai_provider, "ollama");
        assert_eq!(a.ai_base_url.as_deref(), Some("http://10.0.0.5:11434/v1"));
        assert_eq!(a.openrouter_model.as_deref(), Some("llama3:8b"));
        assert!(a.openrouter_key.is_none());
        // A keyed provider passes its key.
        s.apply_ai_patch(&serde_json::json!({ "ai_provider": "openai" }));
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!(a.ai_provider, "openai");
        assert_eq!(a.openrouter_key.as_deref(), Some("sk-openai-test"));
        assert_eq!(a.openrouter_model.as_deref(), Some("gpt-5-mini"));
        assert!(a.ai_base_url.is_none());
    }

    #[test]
    fn ai_patch_round_trip() {
        let mut s = Settings::default();
        let refresh = s.apply_ai_patch(&serde_json::json!({
            "ai_provider": "anthropic",
            "ai_keys": { "anthropic": "  sk-ant-secret  ", "openrouter": "sk-or-secret", "bogus": "x" },
            "ai_models": { "anthropic": "claude-sonnet-5", "openrouter": "a/b", "bogus": "m" },
        }));
        assert_eq!(s.ai_provider, "anthropic");
        assert_eq!(s.ai_keys.get("anthropic").unwrap(), "sk-ant-secret");
        assert!(!s.ai_keys.contains_key("bogus") && !s.ai_keys.contains_key("openrouter"));
        assert_eq!(s.openrouter_key.as_deref(), Some("sk-or-secret"));
        assert_eq!(s.openrouter_model.as_deref(), Some("a/b"));
        assert!(!s.ai_models.contains_key("bogus") && !s.ai_models.contains_key("openrouter"));
        // Every changed provider gets a model refresh, once each.
        let mut r = refresh.clone();
        r.sort();
        assert_eq!(r, ["anthropic", "openrouter"]);

        // Public output: no secret anywhere, but the facts the UI needs.
        let p = s.public();
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("secret"));
        assert_eq!(p.ai_provider, "anthropic");
        assert_eq!(p.ai_keys_set, ["openrouter", "anthropic"]);
        assert_eq!(p.ai_models["openrouter"], "a/b");
        assert_eq!(p.ai_models["anthropic"], "claude-sonnet-5");
        assert_eq!(p.ai_providers.len(), crate::providers::PROVIDERS.len());
        assert_eq!(p.ai_providers[0]["id"], "openrouter");
        assert_eq!(p.ai_providers[0]["needs_key"], "required");
        assert_eq!(p.ai_providers[5]["needs_key"], "none");
        assert_eq!(p.ai_providers[5]["base_editable"], true);
        assert!(p.key_set);

        // null clears keys; an empty model clears the model; blank keys
        // and unknown providers are ignored.
        let refresh = s.apply_ai_patch(&serde_json::json!({
            "ai_keys": { "anthropic": null, "openrouter": null },
            "ai_models": { "anthropic": "", "openrouter": "" },
        }));
        assert!(refresh.is_empty());
        assert!(s.ai_keys.is_empty() && s.openrouter_key.is_none());
        assert!(s.ai_models.is_empty() && s.openrouter_model.is_none());
        s.apply_ai_patch(&serde_json::json!({
            "ai_keys": { "openai": "   " },
            "ai_provider": "nope",
        }));
        assert!(s.ai_keys.is_empty());
        assert_eq!(s.ai_provider, "anthropic");
    }

    #[test]
    fn ai_base_urls_are_checked() {
        let mut s = Settings::default();
        // Only editable providers, only http(s), no trailing slash.
        let r = s.apply_ai_patch(&serde_json::json!({
            "ai_base_urls": {
                "ollama": "http://localhost:9999/v1/",
                "custom": "ftp://nope",
                "lm_studio": "not a url",
                "openai": "https://evil.example/v1",
            },
        }));
        assert_eq!(r, ["ollama"]);
        assert_eq!(s.ai_base_urls.len(), 1);
        assert_eq!(s.ai_base("ollama").unwrap(), "http://localhost:9999/v1");
        assert!(s.ai_base("openai").is_none());
        // Unchanged: no refresh. Null clears.
        assert!(s
            .apply_ai_patch(
                &serde_json::json!({ "ai_base_urls": { "ollama": "http://localhost:9999/v1" } })
            )
            .is_empty());
        let r = s.apply_ai_patch(&serde_json::json!({ "ai_base_urls": { "ollama": null } }));
        assert_eq!(r, ["ollama"]);
        assert!(s.ai_base_urls.is_empty());
        assert!(s.public().ai_base_urls.is_empty());
    }

    #[test]
    fn old_settings_files_still_load() {
        let s: Settings = serde_json::from_str(
            r#"{"openrouter_key":"sk-old","openrouter_model":"x/y","clips_count":5}"#,
        )
        .unwrap();
        assert_eq!(s.ai_provider, "openrouter");
        assert!(s.ai_keys.is_empty() && s.ai_models.is_empty() && s.ai_base_urls.is_empty());
        assert_eq!(s.ai_key("openrouter").as_deref(), Some("sk-old"));
        assert_eq!(s.ai_model("openrouter").as_deref(), Some("x/y"));
        assert_eq!(s.clips_count, 5);
        assert_eq!(s.public().ai_keys_set, ["openrouter"]);
    }

    /// One-shot loopback HTTP server: answers once, hands back the request.
    async fn mock_http(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", l.local_addr().unwrap());
        let h = tokio::spawn(async move {
            let (mut c, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = c.read(&mut buf).await.unwrap();
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            c.write_all(resp.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_lowercase()
        });
        (base, h)
    }

    #[tokio::test]
    async fn model_fetch_auth_and_errors() {
        use crate::providers::provider;
        // Anthropic: its own headers, display names, no Bearer.
        let (base, h) = mock_http(
            "200 OK",
            r#"{"data":[{"id":"claude-sonnet-5","display_name":"Claude Sonnet 5"}]}"#,
        )
        .await;
        let m = fetch_models(provider("anthropic").unwrap(), &base, Some("sk-ant-x"))
            .await
            .unwrap();
        assert_eq!(
            (m[0].id.as_str(), m[0].name.as_str()),
            ("claude-sonnet-5", "Claude Sonnet 5")
        );
        let req = h.await.unwrap();
        assert!(req.starts_with("get /v1/models "));
        assert!(
            req.contains("x-api-key: sk-ant-x") && req.contains("anthropic-version: 2023-06-01")
        );
        assert!(!req.contains("authorization"));
        // Everyone else: Bearer; a keyless local provider sends nothing.
        let (base, h) = mock_http("200 OK", r#"[{"id":"m1"}]"#).await;
        let m = fetch_models(provider("together").unwrap(), &base, Some("k1"))
            .await
            .unwrap();
        assert_eq!(m[0].id, "m1");
        assert!(h.await.unwrap().contains("authorization: bearer k1"));
        let (base, h) = mock_http("200 OK", r#"{"models":[{"name":"llama3:8b"}]}"#).await;
        let m = fetch_models(provider("ollama").unwrap(), &base, None)
            .await
            .unwrap();
        assert_eq!(m[0].id, "llama3:8b");
        let req = h.await.unwrap();
        assert!(!req.contains("authorization") && !req.contains("x-api-key"));
        // Plain-words errors.
        let (base, _h) = mock_http("401 Unauthorized", "{}").await;
        let e = fetch_models(provider("openai").unwrap(), &base, Some("bad"))
            .await
            .unwrap_err();
        assert_eq!(e.to_string(), "OpenAI refused the key (HTTP 401)");
        let (base, _h) = mock_http("500 Internal Server Error", "{}").await;
        let e = fetch_models(provider("groq").unwrap(), &base, Some("k"))
            .await
            .unwrap_err();
        assert_eq!(e.to_string(), "Groq model list: HTTP 500");
        // Nothing listening: the local wording.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = format!("http://{}/v1", l.local_addr().unwrap());
        drop(l);
        let e = fetch_models(provider("ollama").unwrap(), &dead, None)
            .await
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            format!("couldn't reach Ollama (this PC) at {dead} — is it running?")
        );
    }

    #[tokio::test]
    async fn hello_announces_what_the_engine_can_do() {
        let (bus, _) = broadcast::channel::<ServerMsg>(8);
        let st = Arc::new(AppState {
            token: "t".into(),
            data_dir: std::env::temp_dir().join(format!("digiclip-hello-{}", std::process::id())),
            jobs: Mutex::new(HashMap::new()),
            settings: Mutex::new(Settings::default()),
            model_runs: Mutex::new(HashMap::new()),
            worker: Semaphore::new(1),
            bus,
            id_counter: AtomicU64::new(1),
            mcp: mcp::Mcp::default(),
        });
        let msg: ClientMsg = serde_json::from_str(r#"{"id":1,"cmd":"hello"}"#).unwrap();
        let replies = handle_cmd(st, msg).await;
        let ServerMsg::Res {
            ok: true,
            data: Some(data),
            ..
        } = &replies[0]
        else {
            panic!("hello did not answer ok: {replies:?}");
        };
        let caps: Vec<&str> = data["caps"]
            .as_array()
            .expect("caps is an array")
            .iter()
            .filter_map(|c| c.as_str())
            .collect();
        assert!(caps.contains(&"look"), "{caps:?}");
        for s in ["captions", "headline", "bar", "logo"] {
            assert!(caps.contains(&format!("look.{s}").as_str()), "{caps:?}");
        }
        assert_eq!(caps, crate::look::CAPS);
    }

    #[test]
    fn ai_models_wire_names() {
        let m: ClientMsg =
            serde_json::from_str(r#"{"id":1,"cmd":"ai_models","provider":"groq","refresh":true}"#)
                .unwrap();
        assert!(matches!(
            m.cmd,
            Cmd::AiModels { provider: Some(ref p), refresh: true } if p == "groq"
        ));
        let m: ClientMsg = serde_json::from_str(r#"{"id":2,"cmd":"ai_models"}"#).unwrap();
        assert!(matches!(
            m.cmd,
            Cmd::AiModels {
                provider: None,
                refresh: false
            }
        ));
        let m: ClientMsg = serde_json::from_str(r#"{"id":3,"cmd":"or_models"}"#).unwrap();
        assert!(matches!(m.cmd, Cmd::OrModels { refresh: false }));
    }

    #[test]
    fn decider_follows_settings_then_job() {
        use crate::decide::Pick;
        let (src, out, mut o, mut s) = opts();
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!(a.decider, Pick::Auto);
        assert!(a.jev_key.is_none());
        s.jev_key = Some("jev-test-key".into());
        s.decider = "laya".into();
        let a = args_for(&src, &out, &o, &s).unwrap();
        assert_eq!(a.decider, Pick::Laya);
        assert_eq!(a.jev_key.as_deref(), Some("jev-test-key"));
        o.decider = Some("off".into());
        assert_eq!(args_for(&src, &out, &o, &s).unwrap().decider, Pick::Off);
        // Junk falls back to the settings default.
        o.decider = Some("gpt".into());
        assert_eq!(args_for(&src, &out, &o, &s).unwrap().decider, Pick::Laya);
        assert!(s.public().jev_key_set);
    }

    #[test]
    fn clip_edit_frames_parse() {
        let m: ClientMsg = serde_json::from_str(
            r#"{"id":3,"cmd":"clip_edit","job":"job-1","rank":2,"end_s":41.5,
                "fixes":[{"s":12.3,"w":"Hormozi"}]}"#,
        )
        .unwrap();
        match m.cmd {
            Cmd::ClipEdit {
                rank,
                start_s,
                end_s,
                title,
                fixes,
                ..
            } => {
                assert_eq!(rank, 2);
                assert_eq!((start_s, end_s), (None, Some(41.5)));
                assert!(title.is_none());
                assert_eq!(fixes[0].w, "Hormozi");
            }
            other => panic!("parsed as {other:?}"),
        }
        let m: ClientMsg = serde_json::from_str(
            r#"{"id":4,"cmd":"clip_add","job":"job-1","start_s":10,"end_s":40}"#,
        )
        .unwrap();
        assert!(matches!(m.cmd, Cmd::ClipAdd { .. }));
    }

    #[test]
    fn model_frames_keep_their_frame_id() {
        let m: ClientMsg =
            serde_json::from_str(r#"{"id":5,"cmd":"models_download","model":"laya"}"#).unwrap();
        assert_eq!(m.id, 5);
        assert!(matches!(m.cmd, Cmd::ModelsDownload { ref id } if id == "laya"));
        let m: ClientMsg =
            serde_json::from_str(r#"{"id":6,"cmd":"models_delete","model":"base.en"}"#).unwrap();
        assert!(matches!(m.cmd, Cmd::ModelsDelete { ref id } if id == "base.en"));
    }

    #[test]
    fn old_job_files_load_without_insights() {
        // A clip saved before why/scores/rev existed still loads.
        let c: ClipState = serde_json::from_str(
            r#"{"rank":1,"title":"t","hook":"h","start_s":0,"end_s":30,"tight_dur":28,
                "style":"karaoke","source":"llm","score":80,"mp4":"clip_01.mp4",
                "poster":null,"ass":null,"srt":null,"kit":null}"#,
        )
        .unwrap();
        assert_eq!(c.rev, 0);
        assert!(c.why.is_empty() && c.scores.is_none() && c.hashtags.is_empty());
        assert_eq!(c.render_status, "pending");
    }
}
