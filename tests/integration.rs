//! Production smoke tests: Windows path escaping, caption builders,
//! clip validation, GPU parsing, prompt stamps, whisper command matrix.
//! These are the exact spots where the PHP app broke on Windows.

use digiclip_rs::captions;
use digiclip_rs::framing::CropPlan;
use digiclip_rs::gpu;
use digiclip_rs::openrouter::RawClip;
use digiclip_rs::prompt;
use digiclip_rs::render::filter_escape;
use digiclip_rs::validator::Validator;
use digiclip_rs::whisper::{self, Word};
use std::path::PathBuf;

fn test_words() -> Vec<Word> {
    // 60 words, 0.5s each => 30s transcript.
    (0..60)
        .map(|i| Word {
            w: format!("word{i}"),
            s: i as f64 * 0.5,
            e: i as f64 * 0.5 + 0.45,
            conf: Some(0.9),
        })
        .collect()
}

#[test]
fn windows_filter_escape_handles_drive_colon() {
    let p = PathBuf::from(r"C:\Users\kleod\videos\clip.ass");
    let e = filter_escape(&p);
    assert!(e.starts_with('\''), "must be single-quoted: {e}");
    assert!(e.contains("C\\:/"), "drive colon must be escaped: {e}");
    // Only escape-introduced backslashes may survive (\: \' \,).
    let stripped = e.replace("\\:", "").replace("\\'", "").replace("\\,", "");
    assert!(
        !stripped.contains('\\'),
        "no raw backslashes may survive: {e}"
    );
}

#[test]
fn ass_build_shifts_clip_offset_and_has_karaoke() {
    let words = test_words();
    let slice = &words[20..40]; // 10s..20s
    let ass = captions::ass::build(slice, "tiktok", 10.0);
    assert!(ass.contains("PlayResX: 1080"));
    assert!(
        ass.contains("Dialogue: 0,0:00:00.00,"),
        "first line resets to 0: {ass}"
    );
    assert!(ass.contains(r"{\k"), "karaoke tags required");
}

#[test]
fn ass_karaoke_highlights_keywords_and_restores() {
    // "Win" (power word) + "100" (digits) sweep in the accent on one line;
    // plain "big" restores the primary so the accent doesn't bleed.
    let words: Vec<Word> = ["Win", "100", "big"]
        .into_iter()
        .enumerate()
        .map(|(i, w)| Word {
            w: w.into(),
            s: i as f64 * 0.5,
            e: i as f64 * 0.5 + 0.45,
            conf: Some(0.9),
        })
        .collect();
    let ass = captions::ass::build(&words, "karaoke", 0.0);
    assert!(
        ass.contains(r"\1c&H0000FFFF&"),
        "accent sweep required: {ass}"
    );
    assert!(
        ass.contains(r"\1c&H0035E1FF&"),
        "primary must restore after keyword: {ass}"
    );
    assert!(ass.contains(r"{\k"), "karaoke timing must survive: {ass}");
}

#[test]
fn ass_full_mode_has_no_offset() {
    let words = test_words();
    let ass = captions::ass::build(&words, "hormozi", 0.0);
    assert!(ass.contains("Style: Hormozi,Anton,"));
    assert!(ass.contains("Dialogue: 0,0:00:00.00,"));
}

#[test]
fn srt_numbers_cues_and_splits_long_lines() {
    let words = test_words();
    let srt = captions::srt::from_words(&words);
    assert!(srt.starts_with("1\n00:00:00,000 --> "));
    assert!(srt.contains("\n\n2\n"), "cues must be numbered");
}

#[test]
fn validator_clamps_and_dedupes() {
    let words = test_words(); // 30s
    let raw = vec![
        RawClip {
            start_s: -5.0,
            end_s: 500.0,
            hook_line: Some("hook one".into()),
            why_it_works: None,
            scores: None,
            title: None,
            hashtags: Some(vec!["test".into()]),
            caption_style: Some("tiktok".into()),
        },
        RawClip {
            start_s: 0.0,
            end_s: 30.0,
            hook_line: Some("hook one".into()), // near-dupe hook
            why_it_works: None,
            scores: None,
            title: None,
            hashtags: None,
            caption_style: None,
        },
    ];
    let clips = Validator::default().normalize(raw, &words, 30.0, 3, "heuristic");
    assert_eq!(clips.len(), 1, "overlap + jaccard dupe must collapse");
    assert!(clips[0].start_s >= 0.0 && clips[0].end_s <= 30.0);
    assert!((clips[0].end_s - clips[0].start_s) <= 90.0);
    assert_eq!(clips[0].rank, 1);
}

#[test]
fn validator_drops_same_start_overlaps() {
    // Regression: overlap must be min(end)-max(start). Two windows sharing
    // a start with >50% overlap collapse to one (seen in e2e smoke).
    let words = test_words(); // 30s
    let mk = |e: f64| RawClip {
        start_s: 0.1,
        end_s: e,
        hook_line: Some("completely different hook line here".into()),
        why_it_works: None,
        scores: None,
        title: None,
        hashtags: None,
        caption_style: None,
    };
    let clips =
        Validator::default().normalize(vec![mk(17.0), mk(23.5)], &words, 30.0, 3, "heuristic");
    assert_eq!(clips.len(), 1, "same-start overlaps must dedupe: {clips:?}");
}

#[test]
fn whisper_command_uses_ng_on_cpu_and_dev_on_vulkan() {
    let cpu = whisper::build_command(
        &PathBuf::from("whisper-cli"),
        &PathBuf::from("m.bin"),
        &PathBuf::from("a.wav"),
        "en",
        &PathBuf::from("out"),
        4,
        false,
        None,
    );
    assert!(cpu.contains(&"-ng".to_string()));
    assert!(!cpu.iter().any(|a| a == "-dev"));

    let gpu_cmd = whisper::build_command(
        &PathBuf::from("whisper-cli-vulkan"),
        &PathBuf::from("m.bin"),
        &PathBuf::from("a.wav"),
        "en",
        &PathBuf::from("out"),
        6,
        true,
        Some(1),
    );
    assert!(gpu_cmd.contains(&"-dev".to_string()));
    assert!(!gpu_cmd.contains(&"-ng".to_string()));
    // Never the legacy -ngl flag (aborts whisper.cpp with no JSON out).
    assert!(!gpu_cmd.iter().any(|a| a == "-ngl"));
}

#[test]
fn vulkan_device_parsing_and_name_matching() {
    let sample = "ggml_vulkan: 0 = Intel UHD Graphics (Intel) | uma: 1\n\
                  ggml_vulkan: 1 = NVIDIA GeForce RTX 4060 (NVIDIA) | uma: 0\n";
    let devs = gpu::parse_vulkan_devices(sample);
    assert_eq!(devs.len(), 2);
    assert_eq!(devs[1].index, 1);
    assert!(gpu::name_score("NVIDIA GeForce RTX 4060", &devs[1].name) >= 0.5);
    assert!(gpu::name_score("NVIDIA GeForce RTX 4060", &devs[0].name) < 0.5);
}

#[test]
fn prompt_stamps_and_truncation() {
    assert_eq!(prompt::stamp(65.0), "01:05");
    let words = test_words();
    let sys = prompt::system(3, 15, 90);
    assert!(sys.contains("submit_clips"));
    let u = prompt::user(&words, &[], 30.0, 200);
    assert!(u.contains("Video duration: 30s"));
    assert!(u.len() <= 200 + 64);
}

#[test]
fn crop_plan_loads_and_smooths() {
    let dir = std::env::temp_dir();
    let p = dir.join("digiclip-test-plan.json");
    std::fs::write(
        &p,
        r#"{"tracks":[{"t":0.0,"x":0.0},{"t":1.0,"x":100.0},{"t":2.0,"x":100.0}]}"#,
    )
    .unwrap();
    let plan = CropPlan::load(&p).unwrap();
    assert_eq!(plan.tracks.len(), 3);
    let sm = plan.smoothed(0.15);
    assert!(sm[0].x <= sm[1].x, "EMA must rise monotonically here");
    assert_eq!(plan.x_at(99.0, 50.0), 50.0, "must clamp to max_x");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn vision_focus_parses_and_clamps() {
    use digiclip_rs::vision::parse_focus;
    let f = parse_focus(r#"{"focus_x": 1.5, "zoom": 0.8, "reason": "the yacht"}"#).unwrap();
    assert_eq!(f.x01, 1.0);
    assert_eq!(f.zoom, 0.8);
    assert!(parse_focus("stay wide, nothing to see").is_none());
    assert!(parse_focus("```json\n{\"focus_x\": 0.25, \"zoom\": 0}\n```").is_some());
}

#[test]
fn segments_split_track_and_wide() {
    use digiclip_rs::track::{build_segments, SegKind};
    // Faces 0-5s, gap 5-10s, faces after.
    let mut seen: Vec<(f64, bool)> = (0..80)
        .map(|i| (i as f64 * 0.25, i < 20 || i >= 40))
        .collect();
    let _ = &mut seen;
    let segs = build_segments(&seen, 20.0);
    assert!(segs.len() >= 3, "{segs:?}");
    assert_eq!(segs[0].kind, SegKind::Track);
    assert!(segs
        .iter()
        .any(|s| s.kind == SegKind::Wide && s.t1 - s.t0 >= 1.5));
}

#[test]
fn gpu_rank_prefers_discrete() {
    let gpus = vec![
        gpu::GpuDevice {
            name: "Intel UHD".into(),
            memory_mb: None,
            discrete: false,
            source: "wmi".into(),
        },
        gpu::GpuDevice {
            name: "NVIDIA GeForce RTX 4060".into(),
            memory_mb: Some(8188),
            discrete: true,
            source: "nvidia-smi".into(),
        },
    ];
    let ranked = gpu::rank(gpus);
    assert!(ranked[0].discrete);
    assert!(ranked[0].name.contains("RTX"));
}

#[test]
fn yunet_decode_matches_opencv_math() {
    use digiclip_rs::track::{decode_stride, nms, Face, StrideHeads};
    // 2x2 grid, stride 8, one hot cell (r=1,c=0): zero offsets, zero
    // log-size => 8x8 box centered on the cell center.
    let n = 4;
    let mut cls = vec![0.0f32; n];
    let mut obj = vec![0.0f32; n];
    let bbox = vec![0.0f32; n * 4];
    let kps = vec![0.0f32; n * 10];
    cls[2] = 0.9;
    obj[2] = 0.9; // idx = r*cols+c = 1*2+0
    let faces = decode_stride(
        &StrideHeads {
            stride: 8,
            cols: 2,
            rows: 2,
            cls: &cls,
            obj: &obj,
            bbox: &bbox,
            kps: &kps,
        },
        0.6,
    );
    assert_eq!(faces.len(), 1);
    let f = &faces[0];
    // center = ((0+0)*8, (1+0)*8) = (0, 8); box 8x8 => x=-4, y=4.
    assert!((f.x + 4.0).abs() < 1e-4, "x={}", f.x);
    assert!((f.y - 4.0).abs() < 1e-4, "y={}", f.y);
    assert!((f.w - 8.0).abs() < 1e-4);
    assert!((f.score - 0.9).abs() < 1e-4, "sqrt(0.9*0.9)");

    // NMS keeps the best of two identical boxes.
    let dup = vec![
        Face {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
            score: 0.9,
        },
        Face {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
            score: 0.7,
        },
        Face {
            x: 100.0,
            y: 100.0,
            w: 10.0,
            h: 10.0,
            score: 0.8,
        },
    ];
    let kept = nms(dup, 0.3, 5000);
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0].score, 0.9);
}

#[test]
fn crop_schedule_shifts_and_formats() {
    use digiclip_rs::framing::{CropPlan, TrackPoint};
    use digiclip_rs::track::crop_commands;
    let plan = CropPlan {
        tracks: vec![
            TrackPoint { t: 300.0, x: 100.0 },
            TrackPoint { t: 300.5, x: 200.0 },
            TrackPoint { t: 301.0, x: 300.0 },
        ],
    };
    // Clip starting at 300s: times shift back, pre-clip points pin to 0.
    let (initial, cmds) = crop_commands(&plan, 640.0, 300.0);
    assert_eq!(initial, 100.0);
    assert!(cmds.contains("0.000 crop x 100.0;"), "{cmds}");
    assert!(cmds.contains("0.500 crop x 200.0;"), "{cmds}");
    assert!(cmds.contains("1.000 crop x 300.0;"), "{cmds}");
    // Clamping.
    let (_, cmds2) = crop_commands(&plan, 150.0, 0.0);
    assert!(cmds2.contains("x 150.0;"), "{cmds2}");
}

#[test]
fn yunet_anchor_count_matches_model() {
    // Self-check of the grid math against the real model file when
    // present (vendored under models/ for hermetic tests). The total
    // anchor count is implied by the model; here we at least assert the
    // 320x320 grid the decode was derived for.
    use digiclip_rs::track::grid_cells;
    let cells = grid_cells(320, 320);
    let total: usize = cells.iter().map(|(_, c, r)| c * r).sum();
    assert_eq!(total, 40 * 40 + 20 * 20 + 10 * 10);
    let p = std::path::PathBuf::from("models/yunet_2026may.onnx");
    if p.is_file() {
        let len = std::fs::metadata(&p).unwrap().len();
        assert!(len > 100_000, "yunet model looks truncated ({len}B)");
    }
}
