use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// DigiClip CLI — drop a video, get TikTok-ready clips.
///
/// Offline-first: transcription runs locally via a whisper.cpp sidecar,
/// renders run locally via ffmpeg. Only clip *scoring* optionally calls
/// OpenRouter (bring-your-own-key); without a key an offline heuristic
/// scorer is used instead.
#[derive(Parser, Debug, Clone)]
#[command(name = "digiclip", version, about)]
pub struct Args {
    /// Input video (.mp4, .mov, .mkv, .webm, .m4a). Not needed with --provision.
    pub input: Option<PathBuf>,

    /// Output mode: `clips` cuts captioned 9:16 highlights (default 3),
    /// `full` keeps the whole video and burns in subtitles for it.
    #[arg(long, value_enum, default_value = "clips")]
    pub mode: Mode,

    /// Output directory (default: <input-stem>-digiclip/ next to input)
    #[arg(long)]
    pub out_dir: Option<PathBuf>,

    /// Whisper model id: tiny.en, base.en, large-v3-turbo-q5_0,
    /// large-v3-turbo, large-v3 (default: base.en)
    #[arg(long, default_value = "base.en")]
    pub model: String,

    /// Number of clips to pick in `clips` mode (1-10); 0 = auto, the agent
    /// keeps however many clear the merit bar (smart/complete kinds).
    /// The picker proposes 3x ranked candidates and the validator keeps
    /// the best distinct N (overlap/near-dupe picks drop).
    #[arg(long, default_value_t = 3)]
    pub count: usize,

    /// Clip picking kind: `smart` (LLM/heuristic highlights, default),
    /// `complete` (finished thoughts, flexible length), `moments`
    /// (seeded random windows), `timecut` (uniform parts over --span,
    /// reports N x len and renders them).
    #[arg(long, value_enum, default_value = "smart")]
    pub kind: Kind,

    /// Seed for --kind moments (default: time-based random)
    #[arg(long)]
    pub seed: Option<u64>,

    /// Span for --kind timecut as START-END seconds (default: whole video)
    #[arg(long)]
    pub span: Option<String>,

    /// Part length in seconds for --kind timecut (default: 15)
    #[arg(long, default_value_t = 15.0)]
    pub timecut_len: f64,

    /// Max parts to render in timecut (default: all parts)
    #[arg(long)]
    pub take: Option<usize>,

    /// Min clip length in seconds: short picks grow to fit (default:
    /// 15). Equal min and max ("exact mode") widens each window until
    /// the tightened render fills the length, then trims the tail —
    /// final videos land exactly there.
    #[arg(long, default_value_t = 15.0)]
    pub min_len: f64,

    /// Max clip length in seconds: long picks trim to fit (default: 90).
    #[arg(long, default_value_t = 90.0)]
    pub max_len: f64,

    /// Completeness gate (default on): snap clip boundaries to finished
    /// sentences instead of cutting mid-thought. --complete-gate=false off.
    #[arg(long = "complete-gate", default_value_t = true, action = clap::ArgAction::Set)]
    pub complete_gate: bool,

    /// Hook guard (default on): never open a clip on a filler word.
    /// --hook-guard=false off.
    #[arg(long = "hook-guard", default_value_t = true, action = clap::ArgAction::Set)]
    pub hook_guard: bool,

    /// Upload kit (default on): write clip-XX-upload.txt per clip
    /// (title, description, hashtags). --kit=false off.
    #[arg(long = "kit", default_value_t = true, action = clap::ArgAction::Set)]
    pub kit: bool,

    /// Tightening (default light): `light` shrinks long pauses, `punchy`
    /// also cuts filler words, `off` keeps every frame. Plans audit in
    /// cut_plan.json; joins snap like jump cuts.
    #[arg(long, value_enum, default_value = "light")]
    pub tighten: TightenMode,

    /// Pauses longer than this (s) shrink to --pause-keep (default: 0.8)
    #[arg(long, default_value_t = 0.8)]
    pub pause_above: f64,

    /// Pauses shrink to this (s) (default: 0.25)
    #[arg(long, default_value_t = 0.25)]
    pub pause_keep: f64,

    /// Comma-separated filler words for punchy tightening
    /// (default: um,uh,umm,uhh,hmm,er,ah,mm,mhm)
    #[arg(long)]
    pub filler_words: Option<String>,

    /// Emphasis punch-ins (default on): brief zoom on loud words
    /// (max --punch-max per clip). --punch=false off.
    #[arg(long = "punch", default_value_t = true, action = clap::ArgAction::Set)]
    pub punch: bool,

    /// Max emphasis punches per clip (default: 2)
    #[arg(long, default_value_t = 2)]
    pub punch_max: usize,

    /// Emphasis bar: peak dB above the local median (default: 4.0 —
    /// mastered audio is compressed, 6+ rarely fires)
    #[arg(long, default_value_t = 4.0)]
    pub punch_db: f64,

    /// Merge into ONE compilation clip. Bare `--merge` compiles the picked
    /// clips (chronological, repetitions dropped); `--merge "12.4-28.1,..."`
    /// joins explicit ranges (overlaps fuse). Overrides per-clip renders.
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    pub merge: Option<String>,

    /// White-flash joins between merged parts (default off = hard cuts)
    #[arg(long = "merge-flash", default_value_t = false)]
    pub merge_flash: bool,

    /// Cap merged total source length in seconds (default: 60)
    #[arg(long, default_value_t = 60.0)]
    pub merge_max: f64,

    /// Caption preset override: tiktok, karaoke (default), hormozi,
    /// minimal, beast, neon, highlight, ghost. Applies to every render;
    /// unset -> karaoke captions (pickers may still suggest their own).
    #[arg(long)]
    pub style: Option<String>,

    /// Language code passed to whisper-cli (default: en)
    #[arg(long, default_value = "en")]
    pub lang: String,

    /// Master GPU switch (default on): NVENC renders, Vulkan STT sidecar,
    /// DirectML face tracking. Use --no-gpu to force everything CPU
    /// (embedded STT, CPU ort, libx264). Missing pieces degrade loudly
    /// but never fatally.
    #[arg(long = "gpu", default_value_t = true, action = clap::ArgAction::Set)]
    pub gpu: bool,

    /// OpenRouter API key (or set OPENROUTER_API_KEY). Unset ->
    /// offline heuristic scorer, no network call for scoring.
    #[arg(long)]
    pub openrouter_key: Option<String>,

    /// OpenRouter scoring model (or set OPENROUTER_MODEL).
    #[arg(long)]
    pub openrouter_model: Option<String>,

    /// Framing for 9:16 renders: `center` (static center-crop),
    /// `plan` (load a per-second crop plan JSON via --crop-plan), or
    /// `smart` (YuNet face tracker, auto-downloaded once, offline after).
    /// Face-tracking detectors plug in via --crop-plan without CLI changes.
    #[arg(long, value_enum, default_value = "center")]
    pub framing: Framing,

    /// Path to a crop-plan JSON file (required with --framing plan).
    /// Format: {"tracks":[{"t":0.0,"x":320.0}]} — x = crop-left in source
    /// pixels at time t (seconds). Y stays 0 (full-height crop).
    #[arg(long)]
    pub crop_plan: Option<PathBuf>,

    /// Worker threads for whisper/ffmpeg (default: cpus-2, min 1)
    #[arg(long)]
    pub threads: Option<usize>,

    /// Vision model for no-face punch-in suggestions in smart mode
    /// (or set OPENROUTER_VISION_MODEL). Only used when an OpenRouter
    /// key is set; any failure keeps the wide fill.
    #[arg(long)]
    pub vision_model: Option<String>,

    /// Print picks + cut plan without tracking/rendering (still
    /// transcribes first, so the plan is real)
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Download everything the exe needs (ffmpeg, face model, STT model,
    /// fonts) into the provision dir, then exit.
    #[arg(long, default_value_t = false)]
    pub provision: bool,

    /// Serve mode: run a localhost daemon for the desktop UI (JSON
    /// WebSocket at /ws + artifacts at /art), instead of one CLI run.
    /// Prints `DIGICLIP_SERVE port=… token=…` on stdout for the spawner.
    #[arg(long, default_value_t = false)]
    pub serve: bool,

    /// Serve port (localhost only, default: 4317).
    #[arg(long, default_value_t = 4317)]
    pub port: u16,

    /// Serve auth token (default: random per boot). The spawner (Tauri)
    /// passes one so it knows the secret without parsing stdout.
    #[arg(long)]
    pub token: Option<String>,

    /// Serve data dir: jobs + settings.json (default: the provision
    /// root, i.e. %LOCALAPPDATA%/digiclip on Windows).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Clips,
    Full,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Smart,
    Complete,
    Moments,
    Timecut,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TightenMode {
    Off,
    Light,
    Punchy,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    Center,
    Plan,
    Smart,
}
