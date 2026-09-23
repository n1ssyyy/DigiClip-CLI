//! ffmpeg wrapper. Tolerant: ffprobe preferred, falls back to
//! parsing `ffmpeg -i` stderr. All meta fields optional, never block ingest.

use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub duration_s: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
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
    let streams = v.get("streams")?.as_array()?;
    let video = streams
        .iter()
        .find(|s| s.get("codec_type").and_then(|c| c.as_str()) == Some("video"));
    let dur = v
        .pointer("/format/duration")
        .and_then(|d| {
            d.as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .or_else(|| d.as_f64())
        })
        .or_else(|| {
            video?
                .get("duration")
                .and_then(|d| d.as_str()?.parse::<f64>().ok())
        });
    Some(Probe {
        duration_s: dur.map(|d| (d * 100.0).round() / 100.0),
        width: video?
            .get("width")?
            .as_u64()
            .map(|w| w as u32)
            .or_else(|| video?.get("width").and_then(|w| w.as_str()?.parse().ok())),
        height: video?
            .get("height")?
            .as_u64()
            .map(|h| h as u32)
            .or_else(|| video?.get("height").and_then(|w| w.as_str()?.parse().ok())),
    })
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
    Probe {
        duration_s,
        width,
        height,
    }
}
