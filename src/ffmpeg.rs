//! ffmpeg wrapper. Tolerant: ffprobe preferred, falls back to
//! parsing `ffmpeg -i` stderr. All meta fields optional, never block ingest.

use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub duration_s: Option<f64>,
    /// DISPLAY width/height: rotation metadata is already applied (ffmpeg
    /// autorotates decoded frames, so a portrait phone clip stored as
    /// 1920x1080 + rotate=90 decodes as 1080x1920 and reports that here).
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Average frame rate as an exact rational (e.g. 30000/1001).
    pub fps: Option<(u32, u32)>,
    /// Whether the input carries an audio stream at all.
    pub has_audio: bool,
    /// Tagged YCbCr matrix (`bt709`, `bt470bg`, `smpte170m`, …), if any.
    pub color_space: Option<String>,
}

impl Probe {
    /// Output frame rate for renders: the source's own cadence snapped to
    /// the nearest broadcast rate (29.97 stays 29.97 — forcing 30 would
    /// judder), capped at 60. Unknown/odd rates fall back to 30.
    pub fn render_fps(&self) -> (u32, u32) {
        pick_fps(self.fps)
    }

    /// True when the source is BT.601 (tagged, or untagged SD — what
    /// players assume). Renders convert it to BT.709 so colors survive the
    /// jump to 1080x1920 (players read untagged HD as 709).
    pub fn is_bt601(&self) -> bool {
        match self.color_space.as_deref() {
            Some("bt470bg") | Some("smpte170m") => true,
            Some(_) => false,
            None => match (self.width, self.height) {
                (Some(w), Some(h)) => w.min(h) < 720,
                _ => false,
            },
        }
    }
}

/// Broadcast rates a source may carry; anything else snaps to the nearest.
const STD_RATES: [(u32, u32); 8] = [
    (24000, 1001),
    (24, 1),
    (25, 1),
    (30000, 1001),
    (30, 1),
    (50, 1),
    (60000, 1001),
    (60, 1),
];

/// Pure (unit-tested): see [`Probe::render_fps`]. High-rate captures
/// (90/120/240) render at 60; VFR phone footage lands on its nominal rate.
pub fn pick_fps(fps: Option<(u32, u32)>) -> (u32, u32) {
    let Some((n, d)) = fps.filter(|(n, d)| *n > 0 && *d > 0) else {
        return (30, 1);
    };
    let f = n as f64 / d as f64;
    if !(9.0..=1000.0).contains(&f) {
        return (30, 1);
    }
    if f > 61.0 {
        return (60, 1);
    }
    let (best, err) = STD_RATES
        .iter()
        .map(|&(sn, sd)| ((sn, sd), ((sn as f64 / sd as f64) - f).abs() / f))
        .fold(
            ((30, 1), f64::INFINITY),
            |acc, x| if x.1 < acc.1 { x } else { acc },
        );
    if err < 0.03 {
        best
    } else {
        (30, 1)
    }
}

/// Parse an ffprobe rational (`30000/1001`, `25/1`, `0/0`).
fn parse_rational(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.split_once('/')?;
    let (a, b) = (a.trim().parse::<u32>().ok()?, b.trim().parse::<u32>().ok()?);
    (a > 0 && b > 0).then_some((a, b))
}

pub fn extract_wav(source: &Path, dest: &Path, rate: u32) -> anyhow::Result<()> {
    let ffmpeg = crate::binaries::require("ffmpeg")?;
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p)?;
    }
    let status = crate::process::command(&ffmpeg)
        .args([
            "-y",
            "-i",
            &source.display().to_string(),
            "-ar",
            &rate.to_string(),
            "-ac",
            "1",
            "-c:a",
            "pcm_s16le",
            &dest.display().to_string(),
        ])
        .status()?;
    if !status.success() || !dest.is_file() {
        anyhow::bail!("Audio extract failed (ffmpeg exit {status}).");
    }
    Ok(())
}

/// Grab a single poster frame (~1s in, 320px wide). Tolerant: false on
/// any failure, never blocks ingest for a thumbnail.
pub fn poster(source: &Path, dest: &Path) -> bool {
    (|| -> anyhow::Result<()> {
        let ffmpeg = crate::binaries::require("ffmpeg")?;
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p)?;
        }
        let status = crate::process::command(&ffmpeg)
            .args([
                "-y",
                "-ss",
                "1",
                "-i",
                &source.display().to_string(),
                "-frames:v",
                "1",
                "-vf",
                "scale=320:-1",
                "-q:v",
                "4",
                &dest.display().to_string(),
            ])
            .status()?;
        if !status.success() || !dest.is_file() {
            anyhow::bail!("poster failed");
        }
        Ok(())
    })()
    .is_ok()
}

pub fn probe(source: &Path) -> Probe {
    if let Some(p) = probe_via_ffprobe(source) {
        return p;
    }
    probe_via_ffmpeg_stderr(source)
}

fn probe_via_ffprobe(source: &Path) -> Option<Probe> {
    let ffprobe = crate::binaries::resolve("ffprobe")?;
    let out = crate::process::command(&ffprobe)
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            &source.display().to_string(),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("streams")?;
    Some(parse_ffprobe_json(&v))
}

/// Pure (unit-tested): ffprobe `-show_format -show_streams` JSON → Probe.
pub fn parse_ffprobe_json(v: &serde_json::Value) -> Probe {
    let empty = vec![];
    let streams = v
        .get("streams")
        .and_then(|s| s.as_array())
        .unwrap_or(&empty);
    let kind = |s: &serde_json::Value| {
        s.get("codec_type")
            .and_then(|c| c.as_str())
            .map(str::to_string)
    };
    // Cover art rides as a 1-frame "video" stream (attached_pic): skip it.
    let video = streams.iter().find(|s| {
        kind(s).as_deref() == Some("video")
            && s.pointer("/disposition/attached_pic")
                .and_then(|d| d.as_i64())
                .unwrap_or(0)
                == 0
    });
    let num = |x: Option<&serde_json::Value>| -> Option<f64> {
        let x = x?;
        x.as_f64().or_else(|| x.as_str()?.parse::<f64>().ok())
    };
    let dur =
        num(v.pointer("/format/duration")).or_else(|| num(video.and_then(|s| s.get("duration"))));
    let dim = |k: &str| -> Option<u32> {
        let x = video?.get(k)?;
        x.as_u64()
            .map(|w| w as u32)
            .or_else(|| x.as_str()?.parse().ok())
    };
    let (mut width, mut height) = (dim("width"), dim("height"));
    // Rotation: display-matrix side data (modern) or the legacy tag.
    let rotation = video
        .and_then(|s| s.get("side_data_list"))
        .and_then(|l| l.as_array())
        .and_then(|l| {
            l.iter()
                .find_map(|d| d.get("rotation").and_then(|r| r.as_f64()))
        })
        .or_else(|| num(video.and_then(|s| s.pointer("/tags/rotate"))))
        .unwrap_or(0.0);
    if (rotation.round() as i64).rem_euclid(180) == 90 {
        std::mem::swap(&mut width, &mut height);
    }
    let rate = |k: &str| {
        video
            .and_then(|s| s.get(k)?.as_str())
            .and_then(parse_rational)
    };
    Probe {
        duration_s: dur.map(|d| (d * 100.0).round() / 100.0),
        width,
        height,
        fps: rate("avg_frame_rate").or_else(|| rate("r_frame_rate")),
        has_audio: streams.iter().any(|s| kind(s).as_deref() == Some("audio")),
        color_space: video
            .and_then(|s| s.get("color_space")?.as_str())
            .filter(|c| *c != "unknown")
            .map(str::to_string),
    }
}

fn probe_via_ffmpeg_stderr(source: &Path) -> Probe {
    let ffmpeg = match crate::binaries::require("ffmpeg") {
        Ok(f) => f,
        Err(_) => return Probe::default(),
    };
    let out = crate::process::command(&ffmpeg)
        .args(["-hide_banner", "-i", &source.display().to_string()])
        .output();
    let Ok(out) = out else {
        return Probe::default();
    };
    let err = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Duration: 00:01:23.45
    let mut duration_s = None;
    if let Some(idx) = err.find("Duration:") {
        let rest = &err[idx + 9..];
        let mut parts = rest.trim().split([':', ',', ' ']);
        let h: f64 = parts.next().unwrap_or("0").trim().parse().unwrap_or(0.0);
        let m: f64 = parts.next().unwrap_or("0").trim().parse().unwrap_or(0.0);
        let s: f64 = parts.next().unwrap_or("0").trim().parse().unwrap_or(0.0);
        if h > 0.0 || m > 0.0 || s > 0.0 {
            duration_s = Some(((h * 3600.0 + m * 60.0 + s) * 100.0).round() / 100.0);
        }
    }
    // Video: ... 1920x1080
    let (mut width, mut height) = (None, None);
    for tok in err.split(|c: char| !c.is_ascii_alphanumeric() && c != 'x') {
        if let Some((a, b)) = tok.split_once('x') {
            if let (Ok(w), Ok(h)) = (a.parse::<u32>(), b.parse::<u32>()) {
                if (16..=8192).contains(&w) && (16..=8192).contains(&h) {
                    width = Some(w);
                    height = Some(h);
                    break;
                }
            }
        }
    }
    // "..., 29.97 fps, ..." on the video line.
    let fps = err
        .split(',')
        .find_map(|p| p.trim().strip_suffix(" fps")?.trim().parse::<f64>().ok())
        .map(|f| ((f * 1000.0).round() as u32, 1000));
    Probe {
        duration_s,
        width,
        height,
        fps,
        has_audio: err.contains("Audio:"),
        color_space: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_snaps_to_broadcast_rates() {
        assert_eq!(pick_fps(Some((30000, 1001))), (30000, 1001));
        assert_eq!(pick_fps(Some((2997, 100))), (30000, 1001));
        assert_eq!(pick_fps(Some((25, 1))), (25, 1));
        assert_eq!(pick_fps(Some((24000, 1001))), (24000, 1001));
        assert_eq!(pick_fps(Some((120, 1))), (60, 1));
        assert_eq!(pick_fps(Some((15, 1))), (30, 1));
        assert_eq!(pick_fps(None), (30, 1));
        assert_eq!(pick_fps(Some((0, 0))), (30, 1));
        // VFR phone capture averaging 29.83: nominal 29.97.
        assert_eq!(pick_fps(Some((2983, 100))), (30000, 1001));
    }

    #[test]
    fn ffprobe_json_applies_rotation_and_skips_cover_art() {
        let v = serde_json::json!({
            "format": {"duration": "12.345"},
            "streams": [
                {"codec_type": "video", "width": 600, "height": 600,
                 "disposition": {"attached_pic": 1}},
                {"codec_type": "video", "width": 1920, "height": 1080,
                 "avg_frame_rate": "30000/1001", "color_space": "bt709",
                 "disposition": {"attached_pic": 0},
                 "side_data_list": [{"rotation": -90}]},
                {"codec_type": "audio"}
            ]
        });
        let p = parse_ffprobe_json(&v);
        assert_eq!((p.width, p.height), (Some(1080), Some(1920)));
        assert_eq!(p.fps, Some((30000, 1001)));
        assert!(p.has_audio);
        assert!(!p.is_bt601());
        assert!((p.duration_s.unwrap() - 12.35).abs() < 1e-9);
        // Untagged SD reads as BT.601, like players assume.
        let sd = parse_ffprobe_json(&serde_json::json!({
            "streams": [{"codec_type": "video", "width": 640, "height": 360}]
        }));
        assert!(sd.is_bt601() && !sd.has_audio);
    }
}
