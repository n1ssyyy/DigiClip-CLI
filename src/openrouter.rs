//! OpenRouter clip-scoring client.
//!
//! Port of `OpenRouterClient.php` with the Windows failure fixed:
//! the old PHP build often failed clip-picking because (a) no key was
//! configured yet scoring was attempted, (b) the model returned plain
//! prose instead of the `submit_clips` tool call and the fallback was
//! too strict, (c) timeouts were too short for long transcripts.
//!
//! This client: explicit `has_key()`, `submit_clips` tool call first,
//! ```json content fallback, markdown-extracted-JSON fallback, 300s
//! default timeout, 2 retries, offline heuristic fallback signalled
//! to the caller instead of hard-failing.

use serde::{Deserialize, Serialize};

const DEFAULT_MODEL: &str = "nvidia/nemotron-3-ultra-550b-a55b:free";

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub key: Option<String>,
    pub model: String,
    pub timeout_s: u64,
    #[allow(dead_code)]
    pub token_cap: usize,
}

impl Config {
    pub fn from_env(model_override: Option<String>, key_override: Option<String>) -> Self {
        let key = key_override.filter(|k| !k.trim().is_empty()).or_else(|| {
            std::env::var("OPENROUTER_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty())
        });
        let model = model_override
            .filter(|m| !m.trim().is_empty())
            .or_else(|| {
                std::env::var("OPENROUTER_MODEL")
                    .ok()
                    .filter(|m| !m.trim().is_empty())
            })
            .unwrap_or_else(|| DEFAULT_MODEL.into());
        let timeout_s = std::env::var("OPENROUTER_TIMEOUT_S")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        Self {
            base_url: std::env::var("OPENROUTER_BASE_URL")
                .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into()),
            key,
            model,
            timeout_s,
            token_cap: 120_000,
        }
    }

    pub fn has_key(&self) -> bool {
        self.key.as_ref().is_some_and(|k| !k.trim().is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scores {
    pub hook: i64,
    pub retention: i64,
    pub value: i64,
    pub share: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawClip {
    pub start_s: f64,
    pub end_s: f64,
    pub hook_line: Option<String>,
    pub why_it_works: Option<String>,
    pub scores: Option<Scores>,
    pub title: Option<String>,
    pub hashtags: Option<Vec<String>>,
    pub caption_style: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AnalyzeResult {
    pub clips: Vec<RawClip>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub model: String,
}

fn tool_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "submit_clips",
            "description": "Return the ranked clip candidates",
            "parameters": {
                "type": "object",
                "properties": {
                    "clips": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "start_s": {"type": "number"},
                                "end_s": {"type": "number"},
                                "hook_line": {"type": "string"},
                                "why_it_works": {"type": "string"},
                                "scores": {
                                    "type": "object",
                                    "description": "1-10 scale per dimension",
                                    "properties": {
                                        "hook": {"type": "integer"},
                                        "retention": {"type": "integer"},
                                        "value": {"type": "integer"},
                                        "share": {"type": "integer"}
                                    }
                                },
                                "title": {"type": "string"},
                                "hashtags": {"type": "array", "items": {"type": "string"}},
                                "caption_style": {"type": "string", "enum": ["tiktok","karaoke","hormozi","minimal"]}
                            },
                            "required": ["start_s","end_s"]
                        }
                    }
                },
                "required": ["clips"]
            }
        }
    })
}

/// Score scale unification: the tool schema says 1-10, but models are
/// only loosely obedient — some return 0-100. The engine (merit bar,
/// scorecards) speaks 0-100, so per-clip 1-10 scales up by 10 on
/// ingest; already-0-100 clips pass through untouched.
fn normalize_scores(clips: Vec<RawClip>) -> Vec<RawClip> {
    clips
        .into_iter()
        .map(|mut c| {
            if let Some(s) = c.scores.as_mut() {
                let dims = [s.hook, s.retention, s.value, s.share];
                if dims.iter().all(|&d| (0..=10).contains(&d)) {
                    let up = |d: i64| (d * 10).clamp(0, 100);
                    s.hook = up(s.hook);
                    s.retention = up(s.retention);
                    s.value = up(s.value);
                    s.share = up(s.share);
                }
            }
            c
        })
        .collect()
}

fn extract_json_candidates(text: &str) -> Option<Vec<RawClip>> {
    // 1. Direct parse: {"clips":[...]}
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        if let Some(clips) = v.get("clips").and_then(|c| c.as_array()) {
            if let Ok(parsed) = serde_json::from_value::<Vec<RawClip>>(clips.clone().into()) {
                if !parsed.is_empty() {
                    return Some(parsed);
                }
            }
        }
        // Bare array.
        if let Ok(parsed) = serde_json::from_value::<Vec<RawClip>>(v.clone()) {
            if !parsed.is_empty() {
                return Some(parsed);
            }
        }
    }
    // 2. Fenced ```json ... ``` blocks.
    for block in text.split("```") {
        let block = block.trim().trim_start_matches("json").trim();
        if block.starts_with('{') || block.starts_with('[') {
            if let Some(found) = extract_json_candidates(block) {
                return Some(found);
            }
        }
    }
    // 3. Greedy first {...} window.
    if let (Some(a), Some(b)) = (text.find('{'), text.rfind('}')) {
        if a < b {
            if let Some(found) = extract_json_candidates(&text[a..=b]) {
                return Some(found);
            }
        }
    }
    None
}

/// Analyze via OpenRouter. Returns Err on transport/4xx/5xx — caller falls
/// back to the heuristic scorer (except auth errors, which are reported).
pub async fn analyze(cfg: &Config, system: &str, user: &str) -> anyhow::Result<AnalyzeResult> {
    let key = cfg.key.clone().ok_or_else(|| {
        anyhow::anyhow!("OPENROUTER_API_KEY is not set — use the heuristic scorer.")
    })?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(cfg.timeout_s))
        .build()?;
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": cfg.model,
        "temperature": 0.3,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "tools": [tool_schema()],
        "tool_choice": {"type": "function", "function": {"name": "submit_clips"}},
    });

    let mut last_err = String::new();
    for attempt in 1..=3 {
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {key}"))
            .header("HTTP-Referer", "https://digiclip.app")
            .header("X-Title", "DigiClip")
            .json(&body)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("request failed (attempt {attempt}/3): {e}");
                tracing::warn!("{last_err}");
                continue;
            }
        };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            anyhow::bail!("OpenRouter refused the key (HTTP {status}). Check OPENROUTER_API_KEY.");
        }
        if status.as_u16() == 429 {
            last_err = format!("OpenRouter rate-limited (HTTP 429, attempt {attempt}/3)");
            tracing::warn!("{last_err}");
            tokio::time::sleep(std::time::Duration::from_secs(5 * attempt as u64)).await;
            continue;
        }
        if !status.is_success() {
            last_err = format!(
                "OpenRouter HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            );
            tracing::warn!("{last_err}");
            if status.is_server_error() {
                continue;
            }
            anyhow::bail!("{last_err}");
        }
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("OpenRouter returned non-JSON: {e}"))?;
        // Tool call first.
        if let Some(args_s) = v
            .pointer("/choices/0/message/tool_calls/0/function/arguments")
            .and_then(|a| a.as_str())
        {
            if let Ok(args_v) = serde_json::from_str::<serde_json::Value>(args_s) {
                if let Some(clips_v) = args_v.get("clips") {
                    if let Ok(clips) = serde_json::from_value::<Vec<RawClip>>(clips_v.clone()) {
                        if !clips.is_empty() {
                            return Ok(AnalyzeResult {
                                clips: normalize_scores(clips),
                                prompt_tokens: v
                                    .pointer("/usage/prompt_tokens")
                                    .and_then(|n| n.as_u64())
                                    .unwrap_or(0),
                                completion_tokens: v
                                    .pointer("/usage/completion_tokens")
                                    .and_then(|n| n.as_u64())
                                    .unwrap_or(0),
                                model: cfg.model.clone(),
                            });
                        }
                    }
                }
            }
        }
        // Content fallbacks.
        if let Some(content) = v
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
        {
            if let Some(clips) = extract_json_candidates(content) {
                return Ok(AnalyzeResult {
                    clips: normalize_scores(clips),
                    prompt_tokens: v
                        .pointer("/usage/prompt_tokens")
                        .and_then(|n| n.as_u64())
                        .unwrap_or(0),
                    completion_tokens: v
                        .pointer("/usage/completion_tokens")
                        .and_then(|n| n.as_u64())
                        .unwrap_or(0),
                    model: cfg.model.clone(),
                });
            }
            anyhow::bail!(
                "LLM returned prose without parseable clips: {}",
                content.chars().take(300).collect::<String>()
            );
        }
        last_err = "OpenRouter response had no tool call and no content".into();
    }
    anyhow::bail!("OpenRouter failed after retries: {last_err}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scored(h: i64, r: i64, v: i64, s: i64) -> RawClip {
        RawClip {
            start_s: 0.0,
            end_s: 10.0,
            hook_line: None,
            why_it_works: None,
            scores: Some(Scores {
                hook: h,
                retention: r,
                value: v,
                share: s,
            }),
            title: None,
            hashtags: None,
            caption_style: None,
        }
    }

    #[test]
    fn ten_scale_scores_unify_to_hundred() {
        let out = normalize_scores(vec![scored(10, 9, 9, 10)]);
        let s = out[0].scores.clone().unwrap();
        assert_eq!((s.hook, s.retention, s.value, s.share), (100, 90, 90, 100));
    }

    #[test]
    fn hundred_scale_scores_pass_through() {
        let out = normalize_scores(vec![scored(85, 70, 64, 91)]);
        let s = out[0].scores.clone().unwrap();
        assert_eq!((s.hook, s.retention, s.value, s.share), (85, 70, 64, 91));
    }

    #[test]
    fn missing_scores_survive_normalization() {
        let mut c = scored(0, 0, 0, 0);
        c.scores = None;
        let out = normalize_scores(vec![c]);
        assert!(out[0].scores.is_none());
    }
}
