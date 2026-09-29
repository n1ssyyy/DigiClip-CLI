//! LLM clip-scoring client. The module keeps its OpenRouter name, but it
//! now serves every OpenAI-compatible provider in `providers.rs`
//! (OpenAI, Anthropic, Gemini, Ollama, Groq, custom...).
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
//! to the caller instead of hard-failing. Providers that reject
//! `tool_choice` (or tools) are retried without them.

use serde::{Deserialize, Serialize};

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Registry id (`openrouter`, `openai`, `ollama`...).
    pub provider: String,
    /// Name for messages.
    pub label: String,
    pub base_url: String,
    pub key: Option<String>,
    pub model: String,
    pub timeout_s: u64,
    /// The provider refuses requests without a key.
    pub needs_key: bool,
    #[allow(dead_code)]
    pub token_cap: usize,
}

impl Config {
    pub fn from_env(model_override: Option<String>, key_override: Option<String>) -> Self {
        Self::for_provider("openrouter", None, model_override, key_override)
    }

    /// Settings for one provider. Overrides win, then the provider's key
    /// env var, then (OpenRouter only) the legacy `OPENROUTER_*` vars,
    /// then the registry defaults. An unknown id falls back to OpenRouter.
    pub fn for_provider(
        provider_id: &str,
        base_override: Option<String>,
        model_override: Option<String>,
        key_override: Option<String>,
    ) -> Self {
        let p = crate::providers::provider(provider_id)
            .or_else(|| crate::providers::provider("openrouter"))
            .expect("openrouter is in the registry");
        let or = p.id == "openrouter";
        let nonempty =
            |v: Option<String>| v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let base_url = nonempty(base_override)
            .or_else(|| {
                if or {
                    env_nonempty("OPENROUTER_BASE_URL")
                } else {
                    None
                }
            })
            .unwrap_or_else(|| p.base_url.to_string());
        let key = nonempty(key_override).or_else(|| p.key_env.and_then(env_nonempty));
        let model = nonempty(model_override)
            .or_else(|| {
                if or {
                    env_nonempty("OPENROUTER_MODEL")
                } else {
                    None
                }
            })
            .or_else(|| p.default_model.map(str::to_string))
            .unwrap_or_default();
        let timeout_s = std::env::var("OPENROUTER_TIMEOUT_S")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        Self {
            provider: p.id.to_string(),
            label: p.label.to_string(),
            base_url,
            key,
            model,
            timeout_s,
            needs_key: p.needs_key(),
            token_cap: 120_000,
        }
    }

    /// Ready to call the model: a key when the provider wants one, and a
    /// model to ask for.
    pub fn has_key(&self) -> bool {
        let key = self.key.as_ref().is_some_and(|k| !k.trim().is_empty());
        (key || !self.needs_key) && !self.model.is_empty()
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
                                "title": {"type": "string", "description": "On-screen headline shown over the clip: 3-7 words, sentence case, states the payoff or tension so a scroller instantly gets why to watch (e.g. \"Why most startups die in year one\"). Not a transcript quote, no hashtags, no emojis, no trailing period."},
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

/// A non-success HTTP status, so callers can tell "this provider doesn't
/// take that parameter" (4xx) from auth, rate limits and outages.
#[derive(Debug)]
struct HttpError {
    status: u16,
    msg: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for HttpError {}

/// A 4xx that a different request shape might fix. Auth and rate limits
/// are not that.
fn is_shape_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HttpError>()
        .is_some_and(|h| (400..500).contains(&h.status) && ![401, 403, 429].contains(&h.status))
}

/// POST one chat completion: 3 attempts on transport errors, 429 and 5xx;
/// auth and other 4xx fail at once. Returns the parsed response.
async fn post(cfg: &Config, body: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let label = &cfg.label;
    let key = cfg.key.clone().filter(|k| !k.trim().is_empty());
    if key.is_none() && cfg.needs_key {
        anyhow::bail!("no key saved for {label} — add one in Settings → Clip AI.");
    }
    if cfg.base_url.trim().is_empty() {
        anyhow::bail!("no address set for {label} — add one in Settings → Clip AI.");
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(cfg.timeout_s))
        .build()?;
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let mut last_err = String::new();
    for attempt in 1..=3 {
        let mut req = client.post(&url).json(body);
        if let Some(k) = &key {
            req = req.header("Authorization", format!("Bearer {k}"));
        }
        if cfg.provider == "openrouter" {
            req = req
                .header("HTTP-Referer", "https://digiclip.app")
                .header("X-Title", "DigiClip");
        }
        let resp = match req.send().await {
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
            return Err(HttpError {
                status: status.as_u16(),
                msg: format!(
                    "{label} refused the key (HTTP {status}). Check the key in Settings → Clip AI."
                ),
            }
            .into());
        }
        if status.as_u16() == 429 {
            last_err = format!("{label} rate-limited (HTTP 429, attempt {attempt}/3)");
            tracing::warn!("{last_err}");
            tokio::time::sleep(std::time::Duration::from_secs(5 * attempt as u64)).await;
            continue;
        }
        if !status.is_success() {
            last_err = format!(
                "{label} HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            );
            tracing::warn!("{last_err}");
            if status.is_server_error() {
                continue;
            }
            return Err(HttpError {
                status: status.as_u16(),
                msg: last_err,
            }
            .into());
        }
        return serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{label} returned non-JSON: {e}"));
    }
    anyhow::bail!("{label} failed after retries: {last_err}")
}

/// Appended to the system prompt when the provider takes no tools at all.
const JSON_ONLY: &str =
    r#"Reply with only a JSON object {"clips":[...]} matching the submit_clips schema."#;

/// The clip-picking request at one of three shapes: 0 forces the
/// `submit_clips` tool, 1 offers it without forcing, 2 has no tools and
/// asks for plain JSON.
fn analyze_body(cfg: &Config, system: &str, user: &str, shape: u8) -> serde_json::Value {
    let system = if shape >= 2 {
        format!("{system}\n\n{JSON_ONLY}")
    } else {
        system.to_string()
    };
    let mut body = serde_json::json!({
        "model": cfg.model,
        "temperature": 0.3,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    });
    if shape < 2 {
        body["tools"] = serde_json::json!([tool_schema()]);
    }
    if shape == 0 {
        body["tool_choice"] =
            serde_json::json!({"type": "function", "function": {"name": "submit_clips"}});
    }
    body
}

/// Analyze via the configured provider. Returns Err on transport/4xx/5xx —
/// caller falls back to the heuristic scorer (except auth errors, which
/// are reported).
pub async fn analyze(cfg: &Config, system: &str, user: &str) -> anyhow::Result<AnalyzeResult> {
    let mut shape = 0u8;
    // A reply with neither a tool call nor content is retried.
    for _ in 0..2 {
        // Some providers 4xx on `tool_choice`, or on tools altogether:
        // step down a shape and ask again.
        let v = loop {
            match post(cfg, &analyze_body(cfg, system, user, shape)).await {
                Ok(v) => break v,
                Err(e) if shape < 2 && is_shape_error(&e) => {
                    tracing::warn!("{e} — retrying with a simpler request");
                    shape += 1;
                }
                Err(e) => return Err(e),
            }
        };
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
        tracing::warn!("{} response had no tool call and no content", cfg.label);
    }
    anyhow::bail!("{} response had no tool call and no content", cfg.label)
}

/// First JSON object in a model reply: bare, fenced, or embedded in prose.
pub fn json_object(text: &str) -> Option<serde_json::Value> {
    let text = text.trim();
    if let Ok(v @ serde_json::Value::Object(_)) = serde_json::from_str(text) {
        return Some(v);
    }
    for block in text.split("```").skip(1).step_by(2) {
        let block = block.trim().trim_start_matches("json").trim();
        if let Ok(v @ serde_json::Value::Object(_)) = serde_json::from_str(block) {
            return Some(v);
        }
    }
    let (a, b) = (text.find('{')?, text.rfind('}')?);
    if a < b {
        if let Ok(v @ serde_json::Value::Object(_)) = serde_json::from_str(&text[a..=b]) {
            return Some(v);
        }
    }
    None
}

/// One plain chat turn whose reply must be a JSON object (translation,
/// chapters). Errors when the reply has none.
pub async fn chat_json(
    cfg: &Config,
    system: &str,
    user: &str,
) -> anyhow::Result<serde_json::Value> {
    let body = serde_json::json!({
        "model": cfg.model,
        "temperature": 0.2,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    });
    let v = post(cfg, &body).await?;
    let content = v
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .unwrap_or_default();
    json_object(content).ok_or_else(|| {
        anyhow::anyhow!(
            "LLM reply had no JSON object: {}",
            content.chars().take(200).collect::<String>()
        )
    })
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
    fn json_objects_are_found_in_replies() {
        assert_eq!(json_object(r#"{"a":1}"#).unwrap()["a"], 1);
        let fenced = "Sure!
```json
{\"lines\": [\"x\"]}
```
Done.";
        assert_eq!(json_object(fenced).unwrap()["lines"][0], "x");
        assert_eq!(
            json_object(r#"Here: {"a": {"b": 2}} ok"#).unwrap()["a"]["b"],
            2
        );
        assert!(json_object("no json here").is_none());
        assert!(json_object("[1, 2]").is_none());
    }

    #[test]
    fn missing_scores_survive_normalization() {
        let mut c = scored(0, 0, 0, 0);
        c.scores = None;
        let out = normalize_scores(vec![c]);
        assert!(out[0].scores.is_none());
    }

    #[test]
    fn keyless_provider_is_ready_with_a_model() {
        let c = Config::for_provider("ollama", None, Some("llama3:8b".into()), None);
        assert!(!c.needs_key && c.key.is_none());
        assert!(c.has_key());
        // Nothing chosen yet: not ready.
        let c = Config::for_provider("ollama", None, None, None);
        assert!(!c.has_key());
        // Key-only providers need both.
        let c = Config::for_provider("openai", None, None, Some(" sk-x ".into()));
        assert_eq!(c.key.as_deref(), Some("sk-x"));
        assert_eq!(c.model, "gpt-5-mini");
        assert!(c.has_key());
        let c = Config::for_provider("deepseek", None, Some("m".into()), Some("  ".into()));
        assert!(c.needs_key && c.key.is_none() && !c.has_key());
    }

    #[test]
    fn base_override_and_custom_provider() {
        let c = Config::for_provider(
            "ollama",
            Some("http://10.0.0.5:11434/v1".into()),
            None,
            None,
        );
        assert_eq!(c.base_url, "http://10.0.0.5:11434/v1");
        let c = Config::for_provider("ollama", None, None, None);
        assert_eq!(c.base_url, "http://localhost:11434/v1");
        let c = Config::for_provider("custom", None, Some("m".into()), None);
        assert!(c.base_url.is_empty() && !c.needs_key);
        let c = Config::for_provider("bogus", None, None, Some("k".into()));
        assert_eq!(c.provider, "openrouter");
    }

    #[test]
    fn analyze_shapes_step_down() {
        let c = Config::for_provider("openai", None, Some("m".into()), Some("k".into()));
        let b0 = analyze_body(&c, "sys", "usr", 0);
        assert!(b0["tools"].is_array() && b0["tool_choice"].is_object());
        let b1 = analyze_body(&c, "sys", "usr", 1);
        assert!(b1["tools"].is_array() && b1.get("tool_choice").is_none());
        let b2 = analyze_body(&c, "sys", "usr", 2);
        assert!(b2.get("tools").is_none() && b2.get("tool_choice").is_none());
        let sys = b2["messages"][0]["content"].as_str().unwrap();
        assert!(sys.starts_with("sys") && sys.ends_with(JSON_ONLY));
        let soft = |s: u16| {
            is_shape_error(&anyhow::Error::new(HttpError {
                status: s,
                msg: String::new(),
            }))
        };
        assert!(soft(400) && soft(404) && soft(422));
        assert!(!soft(401) && !soft(403) && !soft(429) && !soft(500));
        assert!(!is_shape_error(&anyhow::anyhow!("boom")));
    }
}
