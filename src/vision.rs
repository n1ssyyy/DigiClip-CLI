//! Vision-guided punch-ins: when nobody is on camera, ask a vision
//! model (plus the transcript slice) where the eye should go.
//!
//! Optional and best-effort: no key, no network, or any parse failure
//! simply keeps the wide fill. Used only for Wide stretches >=4s.

use base64::Engine;

#[derive(Debug, Clone)]
pub struct Focus {
    /// 0..1 horizontal center of interest in frame.
    pub x01: f64,
    /// 0..1 punch-in amount (0 = stay wide).
    pub zoom: f64,
    pub reason: String,
}

fn extract_json_object(text: &str) -> Option<serde_json::Value> {
    // Fenced block first, then greedy first-{...}-last-} window.
    for block in text.split("```") {
        let b = block.trim().trim_start_matches("json").trim();
        if b.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(b) {
                return Some(v);
            }
        }
    }
    if let (Some(a), Some(b)) = (text.find('{'), text.rfind('}')) {
        if a < b {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text[a..=b]) {
                return Some(v);
            }
        }
    }
    None
}

pub fn parse_focus(text: &str) -> Option<Focus> {
    let v = extract_json_object(text)?;
    Some(Focus {
        x01: v.get("focus_x")?.as_f64()?.clamp(0.0, 1.0),
        zoom: v.get("zoom")?.as_f64()?.clamp(0.0, 1.0),
        reason: v
            .get("reason")
            .and_then(|r| r.as_str())
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect(),
    })
}

/// Grab one JPEG frame for the VLM (640px wide).
pub fn grab_frame(
    ffmpeg: &std::path::Path,
    source: &std::path::Path,
    t: f64,
) -> anyhow::Result<Vec<u8>> {
    let out = std::env::temp_dir().join(format!("digiclip-vlm-{}.jpg", std::process::id()));
    let status = crate::process::command(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-ss",
            &format!("{t:.2}"),
            "-i",
            &source.display().to_string(),
            "-frames:v",
            "1",
            "-vf",
            "scale=640:-2",
            "-q:v",
            "4",
            &out.display().to_string(),
        ])
        .status()?;
    if !status.success() || !out.is_file() {
        anyhow::bail!("frame grab failed");
    }
    let bytes = std::fs::read(&out)?;
    let _ = std::fs::remove_file(&out);
    Ok(bytes)
}

/// Ask the vision model where to punch in. `transcript` is the words
/// spoken during the stretch. Returns None on any failure (stay wide).
pub async fn suggest_focus(
    base_url: &str,
    key: &str,
    model: &str,
    jpg: &[u8],
    transcript: &str,
    timeout_s: u64,
) -> Option<Focus> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(jpg);
    let prompt = format!(
        "You are a short-form video editor. The camera currently shows the FULL frame below \
         (nobody's face is visible). Spoken during this stretch: \"{transcript}\". \
         Should we punch in on something, or stay wide? Reply with ONLY JSON: \
         {{\"focus_x\": 0.0-1.0 horizontal center of interest, \"zoom\": 0.0-1.0 punch-in amount \
         (0 means stay wide), \"reason\": \"short why\"}}. Prefer zoom 0 unless the transcript \
         names a specific visible thing worth framing (an object, a person entering, text on screen)."
    );
    let body = serde_json::json!({
        "model": model,
        "temperature": 0.2,
        "max_tokens": 200,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": prompt},
                {"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{b64}")}},
            ],
        }],
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_s))
        .build()
        .ok()?;
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {key}"))
        .header("HTTP-Referer", "https://digiclip.app")
        .header("X-Title", "DigiClip")
        .json(&body)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    let content = v.pointer("/choices/0/message/content")?.as_str()?;
    parse_focus(content)
}
