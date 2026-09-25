//! End-to-end pipeline: provision -> ingest -> audio -> transcribe ->
//! pick -> render.
//!
//! The exe is portable: heavy bits (ffmpeg, YuNet, whisper weights) live
//! in the provision dir and download themselves once. Later runs are
//! fully offline.
//!
//! Outputs (all next to --out-dir):
//! - `audio.wav` (16kHz mono)
//! - `transcript.json` ({words, segments, language, model})
//! - `transcript.srt`
//! - clips mode: `clip-01-9x16.mp4` + `clip-01.ass/srt`, `clips.json`
//! - full mode: `full-9x16.mp4` + `full.ass/srt`

use std::path::PathBuf;

use crate::cli::{Args, Framing, Mode};
use crate::progress::{CancelFlag, ClipArtifact, Emitter, JobEvent, Stage};
use crate::render::{Chunk, ChunkKind};
use crate::track::RawTarget;

/// Per-run context: serve passes a live emitter + cancel flag, the CLI
/// passes [`JobCtx::cli`] (both no-ops) so CLI output never changes.
pub struct JobCtx<'a> {
    pub emit: &'a Emitter,
    pub cancel: &'a CancelFlag,
}

impl<'a> JobCtx<'a> {
    pub fn cli(emit: &'a Emitter, cancel: &'a CancelFlag) -> Self {
        Self { emit, cancel }
    }
}

/// Build the UI-facing artifact for a finished clip. Names are the exact
/// files `exec_clip` writes per rank (mp4, kit, `.ass`/`.srt` sidecars);
/// the shared `clip.ass`/`clip.srt` are still written too — last clip
/// wins, exactly as before.
fn artifact_for(clip: &crate::validator::Clip, mp4: &str, kit: Option<String>) -> ClipArtifact {
    let rank = clip.rank;
    let stem = mp4.strip_suffix(".mp4").unwrap_or(mp4);
    ClipArtifact {
        rank,
        title: clip.title.clone().unwrap_or_else(|| clip.hook_line.clone()),
        hook: clip.hook_line.clone(),
        start_s: clip.start_s,
        end_s: clip.end_s,
        tight_dur: (clip.end_s - clip.start_s).max(0.0),
        style: clip.caption_style.clone(),
        source: clip.source.clone(),
        score: clip.score_total,
        mp4: mp4.to_string(),
        poster: Some(format!("{stem}-poster.jpg")),
        ass: Some(format!("{stem}.ass")),
        srt: Some(format!("{stem}.srt")),
        kit,
    }
}

fn kit_name(rank: usize) -> String {
    format!("clip-{rank:02}-upload.txt")
}

/// Tracked timeline: raw per-sample framing targets + Track/Wide segments.
/// Rendering smooths the whole clip in ONE camera path (bezier dollies
/// between framings — never dissolves), so targets stay raw until render.
struct Timeline {
    raw: Vec<RawTarget>,
    segments: Vec<crate::track::Seg>,
}

fn out_dir_for(input: &std::path::Path, out_dir: &Option<PathBuf>) -> PathBuf {
    if let Some(d) = out_dir {
        return d.clone();
    }
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "video".into());
    let parent = input
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or(PathBuf::from("."));
    parent.join(format!("{stem}-digiclip"))
}

/// Resolve framing to a tracked timeline over `[a, b)`. Smart runs the
/// YuNet tracker (session created once, reused); any failure degrades to
/// center (None) with a warning (never fatal). Tracking only picked ranges
/// keeps it fast on long sources.
#[allow(clippy::too_many_arguments)]
/// Smart candidates: LLM scoring with heuristic fallback.
async fn pick_smart(
    ocfg: &crate::openrouter::Config,
    tr: &crate::whisper::Transcription,
    dur: f64,
    ask: usize,
    min_len: f64,
    max_len: f64,
) -> anyhow::Result<(Vec<crate::openrouter::RawClip>, String)> {
    if ocfg.has_key() {
        let sys = crate::prompt::system(ask, min_len.round() as u64, max_len.round() as u64);
        let usr = crate::prompt::user(&tr.words, &tr.segments, dur, 8000);
        match crate::openrouter::analyze(ocfg, &sys, &usr).await {
            Ok(r) => {
                tracing::info!("LLM picked {} candidates ({})", r.clips.len(), r.model);
                Ok((r.clips, format!("llm:{}", r.model)))
            }
            Err(e) => {
                let msg = e.to_string();
                // Auth errors are real config bugs — fail loudly.
                if msg.contains("refused the key") {
                    return Err(e);
                }
                tracing::warn!("LLM scoring failed ({msg}) — heuristic fallback");
                Ok((
                    crate::scorer::propose(&tr.words, ask, &crate::scorer::Heuristic::default()),
                    "heuristic".to_string(),
                ))
            }
        }
    } else {
        tracing::info!("no OpenRouter key — heuristic scorer");
        Ok((
            crate::scorer::propose(&tr.words, ask, &crate::scorer::Heuristic::default()),
            "heuristic".to_string(),
        ))
    }
}

/// Parse "A-B,C-D" seconds into ranges, clamped to dur, min 3s each.
fn parse_ranges(s: &str, dur: f64) -> Vec<(f64, f64)> {
    s.split(',')
        .filter_map(|p| {
            let mut it = p.split('-');
            let (a, b) = (
                it.next()?.trim().parse::<f64>().ok()?,
                it.next()?.trim().parse::<f64>().ok()?,
            );
            let (lo, hi) = (a.min(b).max(0.0), a.max(b).min(dur));
            (hi - lo >= 3.0).then_some((lo, hi))
        })
        .collect()
}

/// Drop later clips whose transcript overlaps >60% (word-set Jaccard)
/// with an earlier-kept one — compilations never repeat themselves.
/// The first (earliest-picked) always survives, so this never empties.
fn dedupe_similar(
    clips: Vec<crate::validator::Clip>,
    words: &[crate::whisper::Word],
) -> Vec<crate::validator::Clip> {
    let mut kept: Vec<crate::validator::Clip> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    for c in clips {
        let t: String = words
            .iter()
            .filter(|w| w.s >= c.start_s && w.e <= c.end_s)
            .map(|w| w.w.clone())
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        if texts.iter().any(|k| crate::validator::jaccard(k, &t) > 0.6) {
            tracing::info!(
                "merge: dropping repetition ({:.0}s-{:.0}s)",
                c.start_s,
                c.end_s
            );
            continue;
        }
        texts.push(t);
        kept.push(c);
    }
    kept
}

/// Chunk-kind names for the run log.
fn kind_names(chunks: &[Chunk]) -> Vec<String> {
    chunks
        .iter()
        .map(|c| match &c.kind {
            ChunkKind::Track(s) if s.is_empty() => "center".to_string(),
            ChunkKind::Track(_) => "track".to_string(),
            ChunkKind::Wide => "wide".to_string(),
            ChunkKind::Punch(_) => "punch".to_string(),
        })
        .collect()
}

/// Group-shot intervals (clip-relative) for the run log.
fn log_group_shots(chunks: &[Chunk], clip: &crate::validator::Clip) {
    let mut duo_start: Option<(f64, usize)> = None;
    for chunk in chunks {
        if let ChunkKind::Track(raws) = &chunk.kind {
            for r in raws {
                if r.n_faces >= 2 && duo_start.is_none() {
                    duo_start = Some((r.t, r.n_faces));
                } else if r.n_faces < 2 {
                    if let Some((s, n)) = duo_start.take() {
                        if r.t - s >= 0.8 {
                            tracing::info!(
                                "clip #{} group shot ({} faces) {:.1}s-{:.1}s",
                                clip.rank,
                                n,
                                s - clip.start_s,
                                r.t - clip.start_s
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Emphasis punch windows for one tracked group (best-effort: any failure
/// — or the toggle — yields no punches, never an error).
fn scan_punches(
    ffmpeg: &PathBuf,
    input: &PathBuf,
    words: &[crate::whisper::Word],
    keeps: &[crate::timeline::Keep],
    ga: f64,
    gb: f64,
    args: &Args,
) -> Vec<(f64, f64)> {
    if !args.punch || args.punch_max == 0 {
        return vec![];
    }
    match crate::punch::scan(ffmpeg, input, ga, gb) {
        Ok(m) => {
            let p = crate::punch::peaks(&m, args.punch_db);
            let mut w = crate::punch::windows(&p, words, Some(keeps), args.punch_max);
            // Clamp to the tracked span (word tails overshoot it) so every
            // window covers real samples; drop slivers.
            for (s, e) in w.iter_mut() {
                *s = (*s).max(ga);
                *e = (*e).min(gb);
            }
            w.retain(|(s, e)| e - s > 0.2);
            for (s, e) in &w {
                let txt: String = words
                    .iter()
                    .filter(|x| x.s >= *s - 0.25 && x.e <= *e + 0.25)
                    .take(6)
                    .map(|x| x.w.clone())
                    .collect::<Vec<_>>()
                    .join(" ");
                tracing::info!("emphasis punch {s:.1}s-{e:.1}s: {txt}");
            }
            w
        }
        Err(e) => {
            tracing::warn!("punch scan failed ({e}): continuing flat");
            vec![]
        }
    }
}

async fn resolve_timeline(
    args: &Args,
    ffmpeg: &PathBuf,
    tracker: &mut Option<crate::track::Tracker>,
    gpu_on: bool,
    src_w: u32,
    src_h: u32,
    a: f64,
    b: f64,
    words: &[crate::whisper::Word],
    forced: &[f64],
    punches: &[(f64, f64)],
    progress: Option<crate::progress::SharedPct>,
    cancel: &CancelFlag,
) -> Option<Timeline> {
    let max_x = (src_w as f64 - src_h as f64 * 9.0 / 16.0).max(0.0);
    match args.framing {
        Framing::Center => None,
        Framing::Plan => {
            let Some(p) = args.crop_plan.clone() else {
                tracing::warn!("--framing plan without --crop-plan: center fallback");
                return None;
            };
            match crate::framing::CropPlan::load(&p) {
                Ok(plan) => {
                    // External plans carry x-only tracks; expand to base raw targets.
                    let base_w = src_h as f64 * 9.0 / 16.0;
                    let raw: Vec<RawTarget> = plan
                        .tracks
                        .into_iter()
                        .map(|tp| RawTarget {
                            t: tp.t,
                            x: tp.x.clamp(0.0, max_x),
                            y: 0.0,
                            w: base_w,
                            h: src_h as f64,
                            cut: false,
                            hard: false,
                            n_faces: 1,
                            pick_cx: tp.x + base_w / 2.0,
                        })
                        .collect();
                    let end = raw.last().map(|t| t.t).unwrap_or(b);
                    let tl = Timeline {
                        raw,
                        segments: vec![crate::track::Seg {
                            t0: 0.0,
                            t1: end.max(b),
                            kind: crate::track::SegKind::Track,
                        }],
                    };
                    Some(tl)
                }
                Err(e) => {
                    tracing::warn!("crop plan failed ({e}): center fallback");
                    None
                }
            }
        }
        Framing::Smart => {
            if tracker.is_none() {
                let model = match crate::provision::ensure_yunet().await {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("face model unavailable ({e}): center fallback");
                        return None;
                    }
                };
                match crate::track::Tracker::load(&model, gpu_on) {
                    Ok(t) => *tracker = Some(t),
                    Err(e) => {
                        tracing::warn!("tracker init failed ({e}): center fallback");
                        return None;
                    }
                }
            }
            let source = args.input.clone().unwrap();
            let t = std::time::Instant::now();
            let res = crate::track::plan_tracks(
                tracker.as_mut().unwrap(),
                ffmpeg,
                &source,
                src_w,
                src_h,
                b,
                Some((a, b)),
                words,
                forced,
                punches,
                progress,
                cancel,
            );
            tracing::info!(
                "tracking [{a:.0}s-{b:.0}s] took {:.1}s",
                t.elapsed().as_secs_f64()
            );
            match res {
                Ok(tracked) => {
                    if !tracked.ever_seen {
                        tracing::warn!("no faces detected: output equals center-crop");
                    } else {
                        let wides = tracked
                            .segments
                            .iter()
                            .filter(|s| s.kind == crate::track::SegKind::Wide)
                            .count();
                        tracing::info!(
                            "tracked {} points ({} wide stretches{})",
                            tracked.raw.len(),
                            wides,
                            if tracked.group_secs >= 0.5 {
                                format!(", {:.1}s group", tracked.group_secs)
                            } else {
                                String::new()
                            }
                        );
                    }
                    Some(Timeline {
                        raw: tracked.raw,
                        segments: tracked.segments,
                    })
                }
                Err(e) => {
                    tracing::warn!("tracking failed ({e}): center fallback");
                    None
                }
            }
        }
    }
}

/// Intersect the timeline with `[a, b)` into render chunks. Slivers <0.5s
/// join the previous chunk; Track chunks carry their raw-target subset
/// (render smooths the whole clip as ONE camera path, so framing changes
/// become bezier dollies, never dissolves).
fn chunks_for(timeline: &Option<Timeline>, a: f64, b: f64) -> Vec<Chunk> {
    let Some(tl) = timeline else {
        return vec![Chunk {
            t0: a,
            t1: b,
            kind: ChunkKind::Track(vec![]),
        }];
    };
    let mut chunks: Vec<Chunk> = Vec::new();
    for seg in &tl.segments {
        let (t0, t1) = (seg.t0.max(a), seg.t1.min(b));
        if t1 - t0 < 0.5 {
            // Sliver: extend the previous chunk instead of strobing.
            if let Some(prev) = chunks.last_mut() {
                prev.t1 = prev.t1.max(t1);
            }
            continue;
        }
        let kind = match seg.kind {
            crate::track::SegKind::Wide => ChunkKind::Wide,
            crate::track::SegKind::Track => ChunkKind::Track(
                tl.raw
                    .iter()
                    .filter(|s| s.t >= t0 - 1e-6 && s.t <= t1 + 1e-6)
                    .cloned()
                    .collect(),
            ),
        };
        // Merge same-kind neighbors (Track schedules concatenate).
        if let Some(prev) = chunks.last_mut() {
            let same = matches!(
                (&prev.kind, &kind),
                (ChunkKind::Track(_), ChunkKind::Track(_)) | (ChunkKind::Wide, ChunkKind::Wide)
            );
            if same {
                prev.t1 = t1;
                if let (ChunkKind::Track(ps), ChunkKind::Track(ss)) = (&mut prev.kind, &kind) {
                    ps.extend(ss.iter().cloned());
                }
                continue;
            }
        }
        chunks.push(Chunk { t0, t1, kind });
    }
    if chunks.is_empty() {
        chunks.push(Chunk {
            t0: a,
            t1: b,
            kind: ChunkKind::Track(vec![]),
        });
    }
    chunks
}

/// Vision-guided punch-ins: Wide chunks >=4s get a VLM focus suggestion
/// (frame + transcript slice). Zoom >=0.6 becomes a static Punch chunk.
/// No key or any failure → stays Wide.
async fn resolve_punches(
    chunks: Vec<Chunk>,
    words: &[crate::whisper::Word],
    ffmpeg: &std::path::Path,
    source: &std::path::Path,
    ocfg: &crate::openrouter::Config,
    vision_model: &str,
) -> Vec<Chunk> {
    if !ocfg.has_key() {
        return chunks;
    }
    let key = ocfg.key.clone().unwrap();
    let mut out = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let wide = matches!(chunk.kind, ChunkKind::Wide);
        if !wide || chunk.t1 - chunk.t0 < 4.0 {
            out.push(chunk);
            continue;
        }
        let slice: Vec<String> = words
            .iter()
            .filter(|w| w.e > chunk.t0 && w.s < chunk.t1)
            .map(|w| w.w.clone())
            .collect();
        let text: String = slice.join(" ").chars().take(800).collect();
        let jpg = crate::vision::grab_frame(ffmpeg, source, (chunk.t0 + chunk.t1) / 2.0).ok();
        let focus = match jpg {
            Some(j) => {
                crate::vision::suggest_focus(&ocfg.base_url, &key, vision_model, &j, &text, 60)
                    .await
            }
            None => None,
        };
        match focus {
            Some(f) if f.zoom >= 0.6 => {
                tracing::info!(
                    "vlm punch-in x={:.2} zoom={:.2}: {}",
                    f.x01,
                    f.zoom,
                    f.reason
                );
                out.push(Chunk {
                    kind: ChunkKind::Punch(f.x01),
                    ..chunk
                });
            }
            Some(f) => {
                tracing::info!("vlm stays wide: {}", f.reason);
                out.push(chunk);
            }
            None => out.push(chunk),
        }
    }
    out
}

pub async fn run(args: Args) -> anyhow::Result<()> {
    let emit = Emitter::null();
    let cancel = CancelFlag::never();
    run_inner(&args, &JobCtx::cli(&emit, &cancel))
        .await
        .map(|_| ())
}

/// Full pipeline with serve hooks. Returns one [`ClipArtifact`] per
/// finished clip (full mode: one for the whole video). The CLI ignores
/// the report; serve streams it clip-by-clip via [`JobEvent`] and serves
/// the files over `/art`.
pub async fn run_inner(args: &Args, ctx: &JobCtx<'_>) -> anyhow::Result<Vec<ClipArtifact>> {
    let emit = ctx.emit;
    let cancel = ctx.cancel;
    // --- explicit prefetch ----------------------------------------------
    if args.provision {
        crate::provision::provision_all(&args.model).await?;
        return Ok(vec![]);
    }
    if !args.input.as_ref().is_some_and(|p| p.is_file()) {
        anyhow::bail!(
            "input not found: {}",
            args.input
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(none)".into())
        );
    }
    let input = args.input.clone().unwrap();
    let count = args.count.min(10); // 0 = auto: keep whatever clears the merit bar
    let threads = args.threads.unwrap_or_else(crate::render::threads).max(1);
    let out = out_dir_for(&input, &args.out_dir);
    std::fs::create_dir_all(&out)?;
    let t_total = std::time::Instant::now();

    // --- provision (cached after first run) -------------------------------
    let t = std::time::Instant::now();
    emit.stage(Stage::Provision, None);
    crate::provision::ensure_fonts()?;
    // Prefer an existing ffmpeg (system or bundled); download only on gap.
    if crate::binaries::resolve("ffmpeg").is_none() {
        let hook = emit.byte_hook("ffmpeg");
        crate::provision::ensure_ffmpeg_with(hook.as_deref()).await?;
    }
    let ffmpeg = crate::binaries::require("ffmpeg")?;
    tracing::info!("ffmpeg: {}", ffmpeg.display());
    if !crate::binaries::ffmpeg_has_libass(&ffmpeg) {
        anyhow::bail!(
            "ffmpeg at {} has no libass (ass/subtitles filters missing) — caption burn-in would fail. \
             Install a full build: https://www.gyan.dev/ffmpeg/builds (Windows: ffmpeg-release-essentials).",
            ffmpeg.display()
        );
    }
    if !crate::models::is_downloaded(&args.model) {
        tracing::info!("STT model '{}' missing — downloading (once)…", args.model);
        let hook = emit.byte_hook(&args.model);
        crate::models::download_with(&args.model, hook.as_deref()).await?;
    }
    // Master GPU switch: one policy drives STT (Vulkan sidecar vs embedded),
    // tracking (DirectML vs CPU) and renders (NVENC vs libx264).
    let (gpu_on, gpu_why) = crate::gpu::resolve_mode(args.gpu);
    tracing::info!("{gpu_why}");
    tracing::info!("provision took {:.1}s", t.elapsed().as_secs_f64());
    emit.stage(Stage::Provision, Some(100));
    cancel.check()?;

    // --- ingest / probe ---------------------------------------------------
    let probe = crate::ffmpeg::probe(&input);
    let duration_s = probe.duration_s.unwrap_or(0.0);
    let (src_w, src_h) = (probe.width.unwrap_or(1280), probe.height.unwrap_or(720));
    tracing::info!(
        "input: {} ({:.0}s, {src_w}x{src_h}), out: {}",
        input.display(),
        duration_s,
        out.display()
    );

    // (dry-run exits per-mode, after transcribe+picking, so it can print
    // the real picks and cut plan.)

    // --- extract audio ----------------------------------------------------
    let t = std::time::Instant::now();
    emit.stage(Stage::Audio, None);
    let wav = out.join("audio.wav");
    tracing::info!("extract audio -> {}", wav.display());
    crate::ffmpeg::extract_wav(&input, &wav, 16000)?;
    let _ = crate::ffmpeg::poster(&input, &out.join("poster.jpg"));
    tracing::info!("audio extract took {:.1}s", t.elapsed().as_secs_f64());
    emit.stage(Stage::Audio, Some(100));
    cancel.check()?;

    // --- transcribe: Vulkan sidecar on GPU, embedded CPU otherwise --------
    let t = std::time::Instant::now();
    emit.stage(Stage::Transcribe, None);
    let tr = if gpu_on && crate::binaries::resolve("whisper-cli-vulkan").is_some() {
        tracing::info!(
            "transcribe via whisper Vulkan sidecar (model={})…",
            args.model
        );
        let topts = crate::whisper::TranscribeOptions {
            model: args.model.clone(),
            lang: args.lang.clone(),
            gpu: true,
            threads,
            timeout_s: crate::whisper::timeout_for_model(&args.model),
        };
        crate::whisper::transcribe(&wav, &out.join("transcript"), &topts, cancel).await?
    } else {
        if gpu_on {
            tracing::warn!(
                "whisper-cli-vulkan not provisioned: CPU transcription (build it once, see README)"
            );
        }
        let sopts = crate::stt::SttOptions {
            model: args.model.clone(),
            lang: args.lang.clone(),
            threads,
        };
        tracing::info!(
            "transcribe embedded CPU (model={}, threads={})…",
            sopts.model,
            threads
        );
        crate::stt::transcribe_embedded(&wav, &sopts)?
    };
    tracing::info!(
        "transcript: {} words, {} segments",
        tr.words.len(),
        tr.segments.len()
    );
    std::fs::write(
        out.join("transcript.json"),
        serde_json::to_string_pretty(&tr)?,
    )?;
    std::fs::write(
        out.join("transcript.srt"),
        crate::captions::srt::from_words(&tr.words),
    )?;
    tracing::info!("transcribe took {:.1}s", t.elapsed().as_secs_f64());
    emit.stage(Stage::Transcribe, Some(100));
    cancel.check()?;

    let dur = if duration_s > 0.0 {
        duration_s
    } else {
        tr.words.last().map(|w| w.e).unwrap_or(0.0)
    };

    // --- framing: tracker session is lazy, reused across clips ------------
    // Tracking runs AFTER clip picking, scoped to picked ranges only.
    let mut tracker: Option<crate::track::Tracker> = None;
    // OpenRouter config doubles for clip scoring and VLM punch-ins.
    let ocfg = crate::openrouter::Config::from_env(
        args.openrouter_model.clone(),
        args.openrouter_key.clone(),
    );
    let vision_model = args.vision_model.clone().unwrap_or_else(|| {
        std::env::var("OPENROUTER_VISION_MODEL")
            .unwrap_or_else(|_| "google/gemini-2.5-flash".into())
    });

    match args.mode {
        Mode::Full => {
            // --- full-video subtitle mode -----------------------------------
            if args.dry_run {
                println!(
                    "dry-run: would render full video ({dur:.0}s) with '{}' captions.",
                    args.style.as_deref().unwrap_or("karaoke")
                );
                return Ok(vec![]);
            }
            let style =
                crate::captions::ass::valid_preset(args.style.as_deref().unwrap_or("karaoke"));
            // The full render is one pseudo-clip so the UI speaks a single
            // language (tiles, progress, player) for both modes.
            let stem = input
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "video".into());
            let pseudo = crate::validator::Clip {
                rank: 1,
                start_s: 0.0,
                end_s: dur,
                hook_line: stem.clone(),
                why_it_works: "Full video with burned-in subtitles.".into(),
                score_total: 100.0,
                scores: None,
                title: Some(stem),
                hashtags: vec![],
                caption_style: style.clone(),
                source: "full".into(),
            };
            let mp4 = out.join("full-9x16.mp4");
            let mut artifact = artifact_for(&pseudo, "full-9x16.mp4", None);
            artifact.tight_dur = dur;
            emit.stage(Stage::Pick, Some(100));
            emit.emit(JobEvent::ClipsPicked {
                clips: vec![artifact.clone()],
            });
            let track_hook = emit.shared_hook(|p| JobEvent::ClipTrack { rank: 1, pct: p });
            let timeline = resolve_timeline(
                args,
                &ffmpeg,
                &mut tracker,
                gpu_on,
                src_w,
                src_h,
                0.0,
                dur,
                &tr.words,
                &[],
                &[],
                track_hook.clone(),
                cancel,
            )
            .await;
            let chunks = chunks_for(&timeline, 0.0, dur);
            let chunks =
                resolve_punches(chunks, &tr.words, &ffmpeg, &input, &ocfg, &vision_model).await;
            let t = std::time::Instant::now();
            emit.stage(Stage::Render, None);
            let render_hook = emit.shared_hook(|p| JobEvent::ClipRender { rank: 1, pct: p });
            let enc = crate::render::render_full(
                &input,
                &tr.words,
                &style,
                &chunks,
                src_w,
                src_h,
                gpu_on,
                &mp4,
                threads,
                render_hook.clone(),
                cancel,
            )?;
            tracing::info!("render took {:.1}s", t.elapsed().as_secs_f64());
            emit.stage(Stage::Render, Some(100));
            if emit.active() {
                let _ = crate::ffmpeg::poster(&mp4, &out.join("full-poster.jpg"));
            }
            emit.emit(JobEvent::ClipDone {
                clip: artifact.clone(),
            });
            println!("done: {} (encoder {enc})", mp4.display());
            tracing::info!("TOTAL took {:.1}s", t_total.elapsed().as_secs_f64());
            return Ok(vec![artifact]);
        }
        Mode::Clips => {
            // --- clip picking FIRST (transcript only) -----------------------
            let t = std::time::Instant::now();
            emit.stage(Stage::Pick, None);
            // Parse --span START-END (timecut); defaults to the whole video.
            let parse_span = |span: Option<&str>, dur: f64| -> (f64, f64) {
                if let Some(sp) = span {
                    let mut it = sp.split('-');
                    if let (Some(a), Some(b)) = (it.next(), it.next()) {
                        if let (Ok(a), Ok(b)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
                            let (lo, hi) = (a.min(b).max(0.0), a.max(b).min(dur));
                            if hi - lo >= 5.0 {
                                return (lo, hi);
                            }
                        }
                    }
                    tracing::warn!("ignoring malformed --span '{sp}' (want START-END)");
                }
                (0.0, dur)
            };
            // --count 0 = auto (smart/complete only): ask for up to 10,
            // keep whatever clears the merit bar (best always survives).
            let auto = count == 0
                && matches!(
                    args.kind,
                    crate::cli::Kind::Smart | crate::cli::Kind::Complete
                );
            let ask = if auto { 10 } else { count.max(1) };
            // User duration window (--min-len/--max-len), needed by both
            // the LLM prompt and the validator below. Clamped so an
            // inverted or absurd window degrades to something sane.
            let min_len = args.min_len.clamp(1.0, 600.0);
            let max_len = args.max_len.clamp(min_len, 600.0);
            // Explicit merge ranges skip picking entirely (no LLM call):
            // overlapping/nearby ranges fuse so shared content never repeats.
            let explicit_ranges: Option<Vec<(f64, f64)>> = match args.merge.as_deref() {
                Some(ms) if !ms.trim().is_empty() => {
                    let ranges = crate::timeline::fuse_overlaps(parse_ranges(ms, dur));
                    if ranges.is_empty() {
                        anyhow::bail!(
                            "--merge parsed to no valid ranges (want A-B,C-D in seconds)"
                        );
                    }
                    Some(ranges)
                }
                _ => None,
            };
            let bare_merge = args.merge.is_some() && explicit_ranges.is_none();
            let (raw, source) = if explicit_ranges.is_some() {
                (vec![], "merge".to_string())
            } else {
                match args.kind {
                    crate::cli::Kind::Timecut => {
                        let (ts, te) = parse_span(args.span.as_deref(), dur);
                        let len = args.timecut_len.max(5.0);
                        let parts = crate::scorer::propose_timecut(&tr.words, ts, te, len);
                        tracing::info!(
                            "timecut: {} x {:.0}s parts over {:.0}s-{:.0}s",
                            parts.len(),
                            len,
                            ts,
                            te
                        );
                        (parts, "timecut".to_string())
                    }
                    crate::cli::Kind::Moments => {
                        let seed = args.seed.unwrap_or_else(|| {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_nanos() as u64)
                                .unwrap_or(0x9E3779B97F4A7C15)
                        });
                        tracing::info!(
                            "moments: {} seeded windows (seed {seed})",
                            if count == 0 { 3 } else { count }
                        );
                        (
                            crate::scorer::propose_moments(
                                &tr.words,
                                if count == 0 { 3 } else { count },
                                seed,
                            ),
                            format!("moments:{seed}"),
                        )
                    }
                    crate::cli::Kind::Complete => {
                        tracing::info!("complete-thought picking ({count} clips)");
                        (
                            crate::scorer::propose_complete(&tr.words, ask),
                            "heuristic-complete".to_string(),
                        )
                    }
                    crate::cli::Kind::Smart => {
                        pick_smart(&ocfg, &tr, dur, ask, min_len, max_len).await?
                    }
                }
            };
            // Equal min and max ("exact mode") additionally widens each
            // window below until the tightened render fills the length
            // (see exact_fit) — except timecut, which sizes its own parts.
            let exact_len = (max_len - min_len < 0.01
                && !matches!(args.kind, crate::cli::Kind::Timecut))
            .then_some(max_len);
            let validator = match args.kind {
                crate::cli::Kind::Timecut => {
                    let len = args.timecut_len.max(5.0);
                    crate::validator::Validator {
                        min_s: (len - 5.0).max(5.0),
                        max_s: len + 3.0,
                        gate_grow_s: 2.0,
                        complete_gate: args.complete_gate,
                        hook_guard: args.hook_guard,
                        ..Default::default()
                    }
                }
                _ => crate::validator::Validator {
                    min_s: min_len,
                    max_s: max_len,
                    complete_gate: args.complete_gate,
                    hook_guard: args.hook_guard,
                    ..Default::default()
                },
            };
            // Timecut renders every part unless --take caps it.
            let want = match args.kind {
                crate::cli::Kind::Timecut => args.take.unwrap_or(usize::MAX),
                _ => ask,
            };
            let mut clips = validator.normalize(raw, &tr.words, dur, want, &source);
            // Explicit --style overrides every picker's suggestion.
            if let Some(s) = args.style.as_deref() {
                let s = crate::captions::ass::valid_preset(s);
                for c in &mut clips {
                    c.caption_style.clone_from(&s);
                }
            }
            if auto && !clips.is_empty() {
                // Merit mode: normalize sorts desc, so clips[0] is the best.
                let best = clips[0].clone();
                clips.retain(|c| c.score_total >= crate::scorer::AUTO_KEEP_SCORE);
                if clips.is_empty() {
                    clips.push(best);
                }
                for (i, c) in clips.iter_mut().enumerate() {
                    c.rank = i + 1;
                }
                tracing::info!(
                    "auto: kept {} clip(s) above merit {:.0}",
                    clips.len(),
                    crate::scorer::AUTO_KEEP_SCORE
                );
            }
            // --- jobs: normal clips tighten in place; merge/flashback ----
            // assemble one multi-range job (both override --kind) -----------
            let tight_cfg = crate::timeline::TightenCfg {
                mode: match args.tighten {
                    crate::cli::TightenMode::Off => crate::timeline::TightenMode::Off,
                    crate::cli::TightenMode::Light => crate::timeline::TightenMode::Light,
                    crate::cli::TightenMode::Punchy => crate::timeline::TightenMode::Punchy,
                },
                pause_above: args.pause_above,
                pause_keep: args.pause_keep,
                fillers: args
                    .filler_words
                    .as_deref()
                    .map(|s| {
                        s.split(',')
                            .map(|x| x.trim().to_lowercase())
                            .filter(|x| !x.is_empty())
                            .collect()
                    })
                    .unwrap_or_else(|| {
                        crate::timeline::FILLERS
                            .iter()
                            .map(|s| s.to_string())
                            .collect()
                    }),
            };
            let caption_default = args
                .style
                .as_deref()
                .map(crate::captions::ass::valid_preset)
                .unwrap_or_else(|| "karaoke".into());
            let jobs: Vec<(crate::validator::Clip, Vec<crate::timeline::CutPlan>)> =
                if let Some(mut ranges) = explicit_ranges {
                    // Explicit ranges (pre-fused at parse): trim the tail past
                    // the cap, tighten each, one compilation.
                    while ranges.iter().map(|(a, b)| b - a).sum::<f64>() > args.merge_max
                        && ranges.len() > 1
                    {
                        ranges.pop();
                    }
                    tracing::info!("merge: {} ranges (explicit)", ranges.len());
                    let groups: Vec<crate::timeline::CutPlan> = ranges
                        .iter()
                        .map(|(a, b)| crate::timeline::tighten(*a, *b, &tr.words, &tight_cfg))
                        .collect();
                    let names = ranges
                        .iter()
                        .map(|(a, b)| format!("{a:.0}-{b:.0}s"))
                        .collect::<Vec<_>>()
                        .join(" + ");
                    let clip = crate::validator::Clip {
                        rank: 1,
                        start_s: ranges[0].0,
                        end_s: ranges.last().map(|r| r.1).unwrap_or(0.0),
                        hook_line: format!("Merged compilation ({names})"),
                        why_it_works: "User-merged ranges.".into(),
                        score_total: 70.0,
                        scores: None,
                        title: None,
                        hashtags: vec![],
                        caption_style: caption_default.clone(),
                        source: "merge".into(),
                    };
                    vec![(clip, groups)]
                } else if bare_merge {
                    // Bare merge: compile the agent's picks — repetitions
                    // out, chronological, capped, overlaps fused, each part
                    // tightened. This is the anti-fb3 path: no moment may
                    // repeat another, even partially.
                    if clips.is_empty() {
                        anyhow::bail!("no clips survived validation (transcript too short?)");
                    }
                    let mut picks = dedupe_similar(clips, &tr.words);
                    picks.sort_by(|a, b| a.start_s.partial_cmp(&b.start_s).unwrap());
                    while picks.iter().map(|c| c.end_s - c.start_s).sum::<f64>() > args.merge_max
                        && picks.len() > 1
                    {
                        let worst = picks
                            .iter()
                            .enumerate()
                            .min_by(|a, b| a.1.score_total.partial_cmp(&b.1.score_total).unwrap())
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        picks.remove(worst);
                    }
                    let n = picks.len();
                    let hook = picks
                        .iter()
                        .max_by(|a, b| a.score_total.partial_cmp(&b.score_total).unwrap())
                        .map(|c| c.hook_line.clone())
                        .unwrap_or_default();
                    let avg = picks.iter().map(|c| c.score_total).sum::<f64>() / n as f64;
                    let mut tags: Vec<String> = Vec::new();
                    for c in &picks {
                        for h in &c.hashtags {
                            if !tags.contains(h) {
                                tags.push(h.clone());
                            }
                        }
                    }
                    tags.truncate(8);
                    // Fuse anything still overlapping (validator tolerates
                    // <50% overlap between picks).
                    let ranges: Vec<(f64, f64)> = crate::timeline::fuse_overlaps(
                        picks.iter().map(|c| (c.start_s, c.end_s)).collect(),
                    );
                    tracing::info!("merge: compiled {n} picks into {} parts", ranges.len());
                    let groups: Vec<crate::timeline::CutPlan> = ranges
                        .iter()
                        .map(|(a, b)| crate::timeline::tighten(*a, *b, &tr.words, &tight_cfg))
                        .collect();
                    let clip = crate::validator::Clip {
                        rank: 1,
                        start_s: ranges[0].0,
                        end_s: ranges.last().map(|r| r.1).unwrap_or(0.0),
                        hook_line: hook,
                        why_it_works: format!(
                            "Compilation of the {n} picked moments, chronological."
                        ),
                        score_total: avg,
                        scores: None,
                        title: None,
                        hashtags: tags,
                        caption_style: caption_default.clone(),
                        source: format!("merge:{n}"),
                    };
                    vec![(clip, groups)]
                } else {
                    if clips.is_empty() {
                        anyhow::bail!("no clips survived validation (transcript too short?)");
                    }
                    clips
                        .into_iter()
                        .map(|mut c| {
                            let p = match exact_len {
                                Some(l) => exact_fit(&mut c, &tr.words, &tight_cfg, l, dur),
                                None => crate::timeline::tighten(
                                    c.start_s, c.end_s, &tr.words, &tight_cfg,
                                ),
                            };
                            (c, vec![p])
                        })
                        .collect()
                };
            let final_clips: Vec<crate::validator::Clip> =
                jobs.iter().map(|(c, _)| c.clone()).collect();
            std::fs::write(
                out.join("clips.json"),
                serde_json::to_string_pretty(&final_clips)?,
            )?;
            tracing::info!("clip picking took {:.1}s", t.elapsed().as_secs_f64());
            emit.stage(Stage::Pick, Some(100));
            // Picks land in the grid now (tiles show "making" until each
            // render finishes and its ClipDone arrives).
            emit.emit(JobEvent::ClipsPicked {
                clips: final_clips
                    .iter()
                    .map(|c| {
                        let kit = args.kit.then(|| kit_name(c.rank));
                        artifact_for(c, &format!("clip-{:02}-9x16.mp4", c.rank), kit)
                    })
                    .collect(),
            });
            cancel.check()?;
            // Auditable cut plan (every kept/removed span with reasons).
            if !matches!(args.tighten, crate::cli::TightenMode::Off) {
                #[derive(serde::Serialize)]
                struct PlanOut {
                    rank: usize,
                    start_s: f64,
                    end_s: f64,
                    tight_dur: f64,
                    keeps: Vec<crate::timeline::Keep>,
                    removed: Vec<crate::timeline::Removed>,
                }
                let plans: Vec<PlanOut> = jobs
                    .iter()
                    .map(|(c, groups)| {
                        let keeps: Vec<_> = groups.iter().flat_map(|g| g.keeps.clone()).collect();
                        let removed: Vec<_> =
                            groups.iter().flat_map(|g| g.removed.clone()).collect();
                        PlanOut {
                            rank: c.rank,
                            start_s: c.start_s,
                            end_s: c.end_s,
                            tight_dur: crate::timeline::plan_dur(&keeps),
                            keeps,
                            removed,
                        }
                    })
                    .collect();
                std::fs::write(
                    out.join("cut_plan.json"),
                    serde_json::to_string_pretty(&plans)?,
                )?;
            }
            if args.dry_run {
                for (clip, groups) in &jobs {
                    let tight: f64 = groups.iter().map(|g| g.tight_dur).sum();
                    let nkeep: usize = groups.iter().map(|g| g.keeps.len()).sum();
                    let ncut: usize = groups.iter().map(|g| g.removed.len()).sum();
                    println!(
                        "dry-run clip #{}: {:.1}s-{:.1}s -> {:.1}s tight ({} keeps, {} cuts) [{}]",
                        clip.rank, clip.start_s, clip.end_s, tight, nkeep, ncut, clip.source
                    );
                    for g in groups {
                        for k in &g.keeps {
                            println!("  keep {:.2}-{:.2}", k.a, k.b);
                        }
                        for r in &g.removed {
                            println!("  cut  {:.2}-{:.2} ({})", r.a, r.b, r.reason);
                        }
                    }
                }
                return Ok(vec![]);
            }

            emit.stage(Stage::Render, None);
            // Face tracking and punch look-ups plan one clip at a time (one
            // shared tracker); the ffmpeg passes then run side by side, so
            // clip N+1 plans while clip N renders.
            let par = render_parallelism(jobs.len());
            let per_threads = (threads / par).max(1);
            if par > 1 {
                tracing::info!(
                    "rendering up to {par} clips at once ({per_threads} ffmpeg threads each)"
                );
            }
            let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(par));
            let mut running: tokio::task::JoinSet<anyhow::Result<ClipArtifact>> =
                tokio::task::JoinSet::new();
            let mut report: Vec<ClipArtifact> = Vec::with_capacity(jobs.len());
            let mut first_err: Option<anyhow::Error> = None;
            for (clip, groups) in &jobs {
                while let Some(r) = running.try_join_next() {
                    settle_render(r, &mut report, &mut first_err);
                }
                if first_err.is_some() {
                    break;
                }
                if let Err(e) = cancel.check() {
                    first_err = Some(e);
                    break;
                }
                let plan = match plan_clip(
                    clip,
                    groups,
                    args.merge_flash && args.merge.is_some(),
                    args,
                    &ffmpeg,
                    &mut tracker,
                    gpu_on,
                    src_w,
                    src_h,
                    &tr.words,
                    &input,
                    &out,
                    &ocfg,
                    &vision_model,
                    ctx,
                )
                .await
                {
                    Ok(p) => p,
                    Err(e) => {
                        first_err = Some(e);
                        break;
                    }
                };
                let slot = slots
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("render slots never close");
                let (input, ffmpeg, out) = (input.clone(), ffmpeg.clone(), out.clone());
                let (emit, cancel, kit) = (emit.clone(), cancel.clone(), args.kit);
                running.spawn_blocking(move || {
                    let _slot = slot;
                    exec_clip(
                        plan,
                        &input,
                        &ffmpeg,
                        &out,
                        src_w,
                        src_h,
                        gpu_on,
                        per_threads,
                        kit,
                        &emit,
                        &cancel,
                    )
                });
            }
            // A failure stops new renders; those already running finish.
            while let Some(r) = running.join_next().await {
                settle_render(r, &mut report, &mut first_err);
            }
            if let Some(e) = first_err {
                return Err(e);
            }
            report.sort_by_key(|a| a.rank);
            // The shared `clip.ass`/`clip.srt` stay "last clip wins" (CLI).
            if let Some((last, _)) = jobs.last() {
                let stem = format!("clip-{:02}-9x16", last.rank);
                let _ = std::fs::copy(out.join(format!("{stem}.ass")), out.join("clip.ass"));
                let _ = std::fs::copy(out.join(format!("{stem}.srt")), out.join("clip.srt"));
            }
            emit.stage(Stage::Render, Some(100));
            println!("done: {} clip(s) in {}", jobs.len(), out.display());
            tracing::info!("TOTAL took {:.1}s", t_total.elapsed().as_secs_f64());
            return Ok(report);
        }
    }
}

/// How many clips render at once: roughly one per 4 hardware threads, at
/// most 3 (consumer NVENC session limits, memory), never more than there
/// are clips. `DIGICLIP_RENDER_JOBS` overrides.
fn render_parallelism(clips: usize) -> usize {
    let clips = clips.max(1);
    if let Some(n) = std::env::var("DIGICLIP_RENDER_JOBS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        return n.clamp(1, clips);
    }
    (crate::whisper::cpu_count() / 4).clamp(1, 3).min(clips)
}

/// Collect one finished render (keeps the first error).
fn settle_render(
    r: Result<anyhow::Result<ClipArtifact>, tokio::task::JoinError>,
    report: &mut Vec<ClipArtifact>,
    first_err: &mut Option<anyhow::Error>,
) {
    match r {
        Ok(Ok(a)) => report.push(a),
        Ok(Err(e)) => {
            first_err.get_or_insert(e);
        }
        Err(e) => {
            first_err.get_or_insert(anyhow::anyhow!("render task failed: {e}"));
        }
    }
}

/// One ffmpeg pass of a clip: the whole clip, or one keep of a
/// tightened/merged clip.
struct RenderUnit {
    words: Vec<crate::whisper::Word>,
    chunks: Vec<crate::render::Chunk>,
    out: PathBuf,
    flash: Option<(bool, bool)>,
    hook: Option<crate::progress::SharedPct>,
    /// Source range, for the timing log (segments only).
    seg: Option<(f64, f64)>,
}

/// A clip after its sequential phase (tracking, punch look-ups). Owns
/// everything the render needs, so it can run on a blocking thread next to
/// other clips' renders.
struct ClipPlan {
    clip: crate::validator::Clip,
    mp4: PathBuf,
    /// Tightened/merged clips render one segment per keep into here, then
    /// concatenate; `None` is the direct single pass.
    segdir: Option<PathBuf>,
    units: Vec<RenderUnit>,
    retimed: Vec<crate::whisper::Word>,
    tight_total: f64,
    kinds_all: Vec<String>,
}

/// Plan one clip's render: a direct single pass when nothing was cut, else
/// one segment per keep (each with its own local dolly schedule and
/// rebased captions) to be concatenated with stream copy. Joins are hard
/// jump cuts — the path snaps, never glides across deleted time.
#[allow(clippy::too_many_arguments)]
async fn plan_clip(
    clip: &crate::validator::Clip,
    groups: &[crate::timeline::CutPlan],
    merge_flash: bool,
    args: &Args,
    ffmpeg: &PathBuf,
    tracker: &mut Option<crate::track::Tracker>,
    gpu_on: bool,
    src_w: u32,
    src_h: u32,
    words: &[crate::whisper::Word],
    input: &PathBuf,
    out: &PathBuf,
    ocfg: &crate::openrouter::Config,
    vision_model: &str,
    ctx: &JobCtx<'_>,
) -> anyhow::Result<ClipPlan> {
    let emit = ctx.emit;
    let cancel = ctx.cancel;
    let rank = clip.rank;
    let mp4 = out.join(format!("clip-{rank:02}-9x16.mp4"));
    let all_keeps: Vec<crate::timeline::Keep> =
        groups.iter().flat_map(|g| g.keeps.clone()).collect();
    let retimed = crate::timeline::retime(words, &all_keeps);
    let tight_total = crate::timeline::plan_dur(&all_keeps);
    let fast = all_keeps.len() == 1
        && (all_keeps[0].a - clip.start_s).abs() < 0.05
        && (all_keeps[0].b - clip.end_s).abs() < 0.05;
    let mut kinds_all: Vec<String> = Vec::new();
    let mut units: Vec<RenderUnit> = Vec::new();
    if fast {
        // Untouched range: today's exact single pass (punch still applies).
        let slice = crate::render::words_in(words, clip.start_s, clip.end_s);
        if slice.is_empty() {
            anyhow::bail!("No transcript words inside clip range.");
        }
        let punches = scan_punches(
            ffmpeg,
            input,
            words,
            &all_keeps,
            clip.start_s,
            clip.end_s,
            args,
        );
        let track_hook = emit.shared_hook(move |p| JobEvent::ClipTrack { rank, pct: p });
        let timeline = resolve_timeline(
            args,
            ffmpeg,
            tracker,
            gpu_on,
            src_w,
            src_h,
            clip.start_s,
            clip.end_s,
            words,
            &[],
            &punches,
            track_hook.clone(),
            cancel,
        )
        .await;
        let chunks = chunks_for(&timeline, clip.start_s, clip.end_s);
        let chunks = resolve_punches(chunks, words, ffmpeg, input, ocfg, vision_model).await;
        kinds_all = kind_names(&chunks);
        log_group_shots(&chunks, clip);
        units.push(RenderUnit {
            words: slice,
            chunks,
            out: mp4.clone(),
            flash: None,
            hook: emit.shared_hook(move |p| JobEvent::ClipRender { rank, pct: p }),
            seg: None,
        });
        return Ok(ClipPlan {
            clip: clip.clone(),
            mp4,
            segdir: None,
            units,
            retimed,
            tight_total,
            kinds_all,
        });
    }
    // Tightened/merged: one render per keep, concatenated.
    let segdir = out.join(".segs").join(format!("{rank:02}"));
    std::fs::create_dir_all(&segdir)?;
    let mut o = 0.0;
    let n_groups = groups.len().max(1);
    let n_segs = all_keeps.len().max(1);
    for (gi, g) in groups.iter().enumerate() {
        cancel.check()?;
        let ga = g.keeps.first().map(|k| k.a).unwrap_or(clip.start_s);
        let gb = g.keeps.last().map(|k| k.b).unwrap_or(clip.end_s);
        let forced: Vec<f64> = g.keeps.iter().skip(1).map(|k| k.a).collect();
        let punches = scan_punches(ffmpeg, input, words, &g.keeps, ga, gb, args);
        // Group-local track pct maps onto the clip's overall track phase.
        let track_hook = emit.shared_hook(move |p| JobEvent::ClipTrack {
            rank,
            pct: ((gi as u32 * 100 + p as u32) / n_groups as u32).min(100) as u8,
        });
        let timeline = resolve_timeline(
            args,
            ffmpeg,
            tracker,
            gpu_on,
            src_w,
            src_h,
            ga,
            gb,
            words,
            &forced,
            &punches,
            track_hook.clone(),
            cancel,
        )
        .await;
        for k in &g.keeps {
            cancel.check()?;
            let len = (k.b - k.a).max(0.1);
            let chunks = chunks_for(&timeline, k.a, k.b);
            let chunks = resolve_punches(chunks, words, ffmpeg, input, ocfg, vision_model).await;
            kinds_all.extend(kind_names(&chunks));
            log_group_shots(&chunks, clip);
            // Caption slice: tight clock rebased to this keep's source
            // clock (render seeks + stamps from chunk times).
            let seg_cap: Vec<crate::whisper::Word> = retimed
                .iter()
                .filter(|w| w.s >= o - 1e-6 && w.s < o + len - 1e-6)
                .map(|w| {
                    let mut q = (*w).clone();
                    q.s += k.a - o;
                    q.e += k.a - o;
                    q
                })
                .collect();
            let si = units.len();
            // Merge flash joins: this segment carries its half of every
            // white dip (head dip unless first, tail dip unless last).
            let flash = merge_flash.then_some((si > 0, si + 1 < all_keeps.len()));
            // Segment-local render pct maps onto the clip overall.
            let hook = emit.shared_hook(move |p| JobEvent::ClipRender {
                rank,
                pct: ((si as u32 * 100 + p as u32) / n_segs as u32).min(100) as u8,
            });
            units.push(RenderUnit {
                words: seg_cap,
                chunks,
                out: segdir.join(format!("seg{si:03}.mp4")),
                flash,
                hook,
                seg: Some((k.a, k.b)),
            });
            o += len;
        }
    }
    Ok(ClipPlan {
        clip: clip.clone(),
        mp4,
        segdir: Some(segdir),
        units,
        retimed,
        tight_total,
        kinds_all,
    })
}

/// Render a planned clip (blocking: ffmpeg passes, concat, sidecars,
/// poster). Runs next to other clips' renders, so everything it writes is
/// named after its own rank.
#[allow(clippy::too_many_arguments)]
fn exec_clip(
    plan: ClipPlan,
    input: &std::path::Path,
    ffmpeg: &std::path::Path,
    out: &std::path::Path,
    src_w: u32,
    src_h: u32,
    gpu_on: bool,
    threads: usize,
    kit: bool,
    emit: &Emitter,
    cancel: &CancelFlag,
) -> anyhow::Result<ClipArtifact> {
    let ClipPlan {
        clip,
        mp4,
        segdir,
        units,
        retimed,
        tight_total,
        kinds_all,
    } = plan;
    let rank = clip.rank;
    let stem = format!("clip-{rank:02}-9x16");
    let mut enc = String::from("copy");
    for u in &units {
        cancel.check()?;
        let t = std::time::Instant::now();
        enc = crate::render::render_timeline(
            input,
            &u.words,
            &clip.caption_style,
            &u.chunks,
            src_w,
            src_h,
            gpu_on,
            &u.out,
            threads,
            "clip",
            u.flash,
            u.hook.clone(),
            cancel,
        )?;
        let took = t.elapsed().as_secs_f64();
        match u.seg {
            Some((a, b)) => tracing::info!("clip #{rank} seg {a:.0}s-{b:.0}s took {took:.1}s"),
            None => tracing::info!("clip #{rank} render took {took:.1}s"),
        }
    }
    if let Some(segdir) = &segdir {
        cancel.check()?;
        let seg_files: Vec<PathBuf> = units.iter().map(|u| u.out.clone()).collect();
        if seg_files.len() == 1 {
            std::fs::rename(&seg_files[0], &mp4)?;
        } else {
            crate::render::concat_copy(ffmpeg, &seg_files, &mp4)?;
        }
        let _ = std::fs::remove_dir_all(segdir);
        let _ = std::fs::remove_dir(out.join(".segs")); // drops the parent when empty
        tracing::info!(
            "clip #{} assembled {:.1}s tight from {} segs",
            rank,
            tight_total,
            seg_files.len()
        );
        // Whole-clip sidecars on the tight clock (segments wrote temps).
        std::fs::write(
            out.join(format!("{stem}.ass")),
            crate::captions::ass::build(&retimed, &clip.caption_style, 0.0),
        )?;
        std::fs::write(
            out.join(format!("{stem}.srt")),
            crate::captions::srt::from_words(&retimed),
        )?;
    }
    // Upload kit on the tight clock (cut fillers never become titles).
    if kit {
        let kit_clip = crate::validator::Clip {
            start_s: 0.0,
            end_s: tight_total,
            ..clip.clone()
        };
        let kit_path = out.join(kit_name(rank));
        if let Err(e) = crate::kit::write(&kit_path, &kit_clip, &retimed) {
            tracing::warn!("upload kit failed for clip #{}: {e}", rank);
        }
    }
    let mp4_name = format!("{stem}.mp4");
    let mut artifact = artifact_for(&clip, &mp4_name, kit.then(|| kit_name(rank)));
    artifact.tight_dur = tight_total;
    // Serve-only: tile poster + live events (skipped on the CLI).
    if emit.active() {
        let _ = crate::ffmpeg::poster(&mp4, &out.join(format!("{stem}-poster.jpg")));
        emit.emit(JobEvent::ClipRender { rank, pct: 100 });
        emit.emit(JobEvent::ClipDone {
            clip: artifact.clone(),
        });
    }
    println!(
        "clip #{}: {:.1}s-{:.1}s [{:.1}s tight|{}|{}] -> {} (encoder {enc}, {})",
        rank,
        clip.start_s,
        clip.end_s,
        tight_total,
        clip.caption_style,
        kinds_all.join("+"),
        mp4.display(),
        clip.source
    );
    Ok(artifact)
}

/// Exact-length fit for one picked clip: the validator sizes the source
/// window, but tightening shrinks the render — so widen the window (tail
/// only, the hook never moves) until the tight plan reaches `l` seconds,
/// then trim the tail back to `l`. Window edges always land on word
/// edges, so words never split; the dropped tail is audited as an
/// exact-cap removal. Short sources may still land under `l` (nothing
/// left to take).
fn exact_fit(
    clip: &mut crate::validator::Clip,
    words: &[crate::whisper::Word],
    cfg: &crate::timeline::TightenCfg,
    l: f64,
    duration: f64,
) -> crate::timeline::CutPlan {
    use crate::timeline::{plan_dur, tighten, Keep, Removed};
    let r2 = |t: f64| (t * 100.0).round() / 100.0;
    let mut plan = tighten(clip.start_s, clip.end_s, words, cfg);
    let mut guard = 0;
    while plan.tight_dur < l - 1e-9 && clip.end_s < duration - 1e-9 && guard < 40 {
        guard += 1;
        let t = (clip.end_s + 5.0).min(duration);
        // Next word end at/after the edge (past the last word: video end).
        clip.end_s = words
            .iter()
            .filter(|w| w.e >= t - 1e-9)
            .map(|w| w.e)
            .fold(f64::INFINITY, f64::min)
            .min(duration);
        if !(clip.end_s > clip.start_s) {
            clip.end_s = t;
        }
        plan = tighten(clip.start_s, clip.end_s, words, cfg);
    }
    if plan.tight_dur > l + 1e-9 {
        let mut acc = 0.0;
        let mut keeps: Vec<Keep> = Vec::new();
        let mut dropped: Vec<(f64, f64)> = Vec::new();
        let mut done = false;
        for k in &plan.keeps {
            if done {
                dropped.push((k.a, k.b));
                continue;
            }
            let d = k.b - k.a;
            if acc + d <= l + 1e-9 {
                keeps.push(*k);
                acc += d;
                continue;
            }
            // Overflow keep: cut at exactly l, backing up to the
            // containing word's start so the tail ends between words.
            let mut cut = k.a + (l - acc);
            if let Some(w) = words.iter().find(|w| w.s < cut - 1e-9 && w.e > cut + 1e-9) {
                cut = w.s.max(k.a);
            }
            if cut > k.a + 0.02 {
                keeps.push(Keep { a: k.a, b: cut });
                acc += cut - k.a;
                if k.b - cut > 1e-9 {
                    dropped.push((cut, k.b));
                }
            } else {
                dropped.push((k.a, k.b));
            }
            done = true;
        }
        for (a, b) in dropped {
            if b - a > 0.02 {
                plan.removed.push(Removed {
                    a: r2(a),
                    b: r2(b),
                    reason: format!("exact cap {l:.0}s"),
                });
            }
        }
        plan.keeps = keeps;
        plan.tight_dur = plan_dur(&plan.keeps);
    }
    // The tile range reports content actually rendered.
    if let Some(k) = plan.keeps.last() {
        clip.end_s = r2(k.b);
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_ranges_parse_and_clamp() {
        let r = parse_ranges("23.8-38.8,87.1-102.1", 631.0);
        assert_eq!(r.len(), 2);
        assert!((r[0].0 - 23.8).abs() < 1e-9 && (r[0].1 - 38.8).abs() < 1e-9);
        // Reversed, out-of-range and slivers.
        let r = parse_ranges("90-80,600-700,5-5.5,garbage", 631.0);
        assert_eq!(r.len(), 2, "got {r:?}");
        assert!((r[0].0 - 80.0).abs() < 1e-9 && (r[0].1 - 90.0).abs() < 1e-9);
        assert!((r[1].1 - 631.0).abs() < 1e-9);
    }

    #[test]
    fn similar_picks_dedupe_for_compilations() {
        // Two clips saying nearly the same thing: the later one drops.
        let words: Vec<crate::whisper::Word> = "the quick brown fox jumps over the fence today sir"
            .split_whitespace()
            .enumerate()
            .map(|(i, w)| crate::whisper::Word {
                w: w.into(),
                s: i as f64 * 0.5,
                e: i as f64 * 0.5 + 0.45,
                conf: Some(0.9),
            })
            .collect();
        let mk = |s: f64, e: f64| crate::validator::Clip {
            rank: 1,
            start_s: s,
            end_s: e,
            hook_line: "h".into(),
            why_it_works: String::new(),
            score_total: 70.0,
            scores: None,
            title: None,
            hashtags: vec![],
            caption_style: "karaoke".into(),
            source: "t".into(),
        };
        let out = dedupe_similar(vec![mk(0.0, 4.0), mk(0.5, 4.5), mk(20.0, 25.0)], &words);
        assert_eq!(out.len(), 2, "repetition must drop, far clip stays");
        assert!((out[0].start_s - 0.0).abs() < 1e-9);
    }

    fn ex_words(n: usize, skip: std::ops::Range<usize>) -> Vec<crate::whisper::Word> {
        (0..n)
            .filter(|i| !skip.contains(i))
            .map(|i| crate::whisper::Word {
                w: "word".into(),
                s: i as f64 * 0.5,
                e: i as f64 * 0.5 + 0.45,
                conf: Some(0.9),
            })
            .collect()
    }

    fn ex_clip(s: f64, e: f64) -> crate::validator::Clip {
        crate::validator::Clip {
            rank: 1,
            start_s: s,
            end_s: e,
            hook_line: "h".into(),
            why_it_works: String::new(),
            score_total: 70.0,
            scores: None,
            title: None,
            hashtags: vec![],
            caption_style: "karaoke".into(),
            source: "t".into(),
        }
    }

    #[test]
    fn exact_fit_extends_past_cuts_then_trims_to_length() {
        // 120 words over 0-60s with a 10s word gap (10-20s): light
        // tighten cuts ~9.75s, so a 30s window only holds ~20s tight.
        // The window must extend past 30s of source, then trim back.
        let words = ex_words(120, 20..40);
        let mut c = ex_clip(0.0, 30.0);
        let p = exact_fit(
            &mut c,
            &words,
            &crate::timeline::TightenCfg::default(),
            30.0,
            120.0,
        );
        assert!(
            (p.tight_dur - 30.0).abs() < 0.6,
            "tight must land on 30s, got {}",
            p.tight_dur
        );
        assert!(c.end_s > 30.0, "must take extra source, got {}", c.end_s);
        assert!((c.start_s - 0.0).abs() < 1e-9, "hook must not move");
        // Edges on word edges: no word straddles the window end.
        assert!(
            !words
                .iter()
                .any(|w| w.s < c.end_s - 1e-9 && w.e > c.end_s + 1e-9),
            "window end must not split a word"
        );
    }

    #[test]
    fn exact_fit_trims_dense_window_without_extension() {
        // Dense speech, no pauses: a 40s window holds 40s tight, so no
        // extension — just a tail trim to 30 on a word edge.
        let words = ex_words(120, 200..200);
        let mut c = ex_clip(0.0, 40.0);
        let p = exact_fit(
            &mut c,
            &words,
            &crate::timeline::TightenCfg::default(),
            30.0,
            120.0,
        );
        assert!(
            (p.tight_dur - 30.0).abs() < 0.6,
            "tight must land on 30s, got {}",
            p.tight_dur
        );
        assert!(c.end_s <= 30.01, "no extra source needed, got {}", c.end_s);
        assert!(
            p.removed.iter().any(|r| r.reason.starts_with("exact cap")),
            "trim must be audited"
        );
    }
}
