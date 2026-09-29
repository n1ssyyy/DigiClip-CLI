//! LLM providers for Clip AI: one static registry, and the model-list
//! parsing shared by every provider.
//!
//! All of them speak OpenAI chat/completions (see `openrouter.rs`), so a
//! provider is just a base URL, how it wants its key, and a few hints for
//! listing and picking models.

use serde_json::Value;

/// Does the provider need an API key?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyNeed {
    Required,
    Optional,
    None,
}

impl KeyNeed {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyNeed::Required => "required",
            KeyNeed::Optional => "optional",
            KeyNeed::None => "none",
        }
    }
}

/// How `GET /models` authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelsAuth {
    /// `Authorization: Bearer <key>` (when there is a key).
    Bearer,
    /// `x-api-key` plus `anthropic-version: 2023-06-01`.
    Anthropic,
}

#[derive(Debug, Clone, Copy)]
pub struct Provider {
    pub id: &'static str,
    pub label: &'static str,
    pub base_url: &'static str,
    pub key: KeyNeed,
    /// Environment variable the CLI falls back to for the key.
    pub key_env: Option<&'static str>,
    pub models_auth: ModelsAuth,
    pub default_model: Option<&'static str>,
    pub vision_model: Option<&'static str>,
    /// Where the user creates a key.
    pub key_url: Option<&'static str>,
    /// Runs on this PC: short timeouts, never cached.
    pub local: bool,
    pub base_editable: bool,
    /// The model list mixes in embeddings, speech, image models…
    pub filter_non_chat: bool,
}

const fn cloud(id: &'static str, label: &'static str, base_url: &'static str) -> Provider {
    Provider {
        id,
        label,
        base_url,
        key: KeyNeed::Required,
        key_env: None,
        models_auth: ModelsAuth::Bearer,
        default_model: None,
        vision_model: None,
        key_url: None,
        local: false,
        base_editable: false,
        filter_non_chat: false,
    }
}

pub static PROVIDERS: &[Provider] = &[
    Provider {
        default_model: Some("nvidia/nemotron-3-ultra-550b-a55b:free"),
        vision_model: Some("google/gemini-2.5-flash"),
        key_url: Some("https://openrouter.ai/keys"),
        key_env: Some("OPENROUTER_API_KEY"),
        ..cloud("openrouter", "OpenRouter", "https://openrouter.ai/api/v1")
    },
    Provider {
        default_model: Some("gpt-5-mini"),
        key_url: Some("https://platform.openai.com/api-keys"),
        key_env: Some("OPENAI_API_KEY"),
        filter_non_chat: true,
        ..cloud("openai", "OpenAI", "https://api.openai.com/v1")
    },
    Provider {
        default_model: Some("claude-sonnet-5"),
        key_url: Some("https://console.anthropic.com/settings/keys"),
        key_env: Some("ANTHROPIC_API_KEY"),
        models_auth: ModelsAuth::Anthropic,
        ..cloud("anthropic", "Anthropic", "https://api.anthropic.com/v1")
    },
    Provider {
        default_model: Some("gemini-2.5-flash"),
        key_url: Some("https://aistudio.google.com/apikey"),
        key_env: Some("GEMINI_API_KEY"),
        filter_non_chat: true,
        ..cloud(
            "gemini",
            "Google Gemini",
            "https://generativelanguage.googleapis.com/v1beta/openai",
        )
    },
    Provider {
        default_model: Some("gpt-oss:120b"),
        key_url: Some("https://ollama.com/settings/keys"),
        key_env: Some("OLLAMA_API_KEY"),
        ..cloud("ollama_cloud", "Ollama Cloud", "https://ollama.com/v1")
    },
    Provider {
        key: KeyNeed::None,
        local: true,
        base_editable: true,
        ..cloud("ollama", "Ollama (this PC)", "http://localhost:11434/v1")
    },
    Provider {
        key: KeyNeed::None,
        local: true,
        base_editable: true,
        ..cloud("lm_studio", "LM Studio", "http://localhost:1234/v1")
    },
    Provider {
        default_model: Some("llama-3.3-70b-versatile"),
        key_url: Some("https://console.groq.com/keys"),
        key_env: Some("GROQ_API_KEY"),
        ..cloud("groq", "Groq", "https://api.groq.com/openai/v1")
    },
    Provider {
        default_model: Some("mistral-large-latest"),
        key_url: Some("https://console.mistral.ai/api-keys"),
        key_env: Some("MISTRAL_API_KEY"),
        filter_non_chat: true,
        ..cloud("mistral", "Mistral", "https://api.mistral.ai/v1")
    },
    Provider {
        default_model: Some("deepseek-chat"),
        key_url: Some("https://platform.deepseek.com/api_keys"),
        key_env: Some("DEEPSEEK_API_KEY"),
        ..cloud("deepseek", "DeepSeek", "https://api.deepseek.com/v1")
    },
    Provider {
        key_url: Some("https://console.x.ai"),
        key_env: Some("XAI_API_KEY"),
        filter_non_chat: true,
        ..cloud("xai", "xAI (Grok)", "https://api.x.ai/v1")
    },
    Provider {
        key_url: Some("https://api.together.ai/settings/api-keys"),
        key_env: Some("TOGETHER_API_KEY"),
        ..cloud("together", "Together AI", "https://api.together.xyz/v1")
    },
    Provider {
        key_url: Some("https://fireworks.ai/account/api-keys"),
        key_env: Some("FIREWORKS_API_KEY"),
        ..cloud(
            "fireworks",
            "Fireworks",
            "https://api.fireworks.ai/inference/v1",
        )
    },
    Provider {
        key_url: Some("https://cloud.cerebras.ai"),
        key_env: Some("CEREBRAS_API_KEY"),
        ..cloud("cerebras", "Cerebras", "https://api.cerebras.ai/v1")
    },
    Provider {
        key: KeyNeed::Optional,
        base_editable: true,
        ..cloud("custom", "Custom (OpenAI-compatible)", "")
    },
];

pub fn provider(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

impl Provider {
    /// Does the provider need a key to be usable?
    pub fn needs_key(&self) -> bool {
        self.key == KeyNeed::Required
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub name: String,
}

/// Ids that are not chat models, for providers that list everything.
const NON_CHAT: &[&str] = &[
    "embed",
    "tts",
    "whisper",
    "dall-e",
    "moderation",
    "image",
    "audio",
    "realtime",
    "transcribe",
    "sora",
    "davinci",
    "babbage",
    "imagen",
    "veo",
    "aqa",
    "search",
    "computer-use",
];

/// Pull the model list out of a `/models` reply: `{data:[…]}`, a bare
/// array (Together), or Ollama's `{models:[{name|model}]}`.
pub fn parse_models(p: &Provider, v: &Value) -> Vec<Model> {
    let list = v
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| v.get("models").and_then(Value::as_array))
        .or_else(|| v.as_array());
    let mut out: Vec<Model> = Vec::new();
    for m in list.into_iter().flatten() {
        let field = |k: &str| m.get(k).and_then(Value::as_str).map(str::trim);
        let raw = match m.as_str() {
            Some(s) => s,
            None => match field("id")
                .or_else(|| field("name"))
                .or_else(|| field("model"))
            {
                Some(s) => s,
                None => continue,
            },
        };
        let id = raw.strip_prefix("models/").unwrap_or(raw).trim();
        if id.is_empty() || out.iter().any(|x| x.id == id) {
            continue;
        }
        if p.filter_non_chat {
            let l = id.to_ascii_lowercase();
            if NON_CHAT.iter().any(|w| l.contains(w)) {
                continue;
            }
        }
        // A display name only when it says more than the id does.
        let name = field("display_name")
            .or_else(|| field("name"))
            .map(|n| n.strip_prefix("models/").unwrap_or(n))
            .filter(|n| !n.is_empty())
            .unwrap_or(id);
        out.push(Model {
            id: id.to_string(),
            name: name.to_string(),
        });
    }
    out
}

/// The model to use after a fetch: the current one when it is still
/// offered, else the provider default, else the first listed.
pub fn pick_model(p: &Provider, current: Option<&str>, list: &[Model]) -> Option<String> {
    let has = |id: &str| list.iter().any(|m| m.id == id);
    let current = current.map(str::trim).filter(|c| !c.is_empty());
    if let Some(c) = current {
        if list.is_empty() || has(c) {
            return Some(c.to_string());
        }
    }
    if let Some(d) = p.default_model {
        if list.is_empty() || has(d) {
            return Some(d.to_string());
        }
    }
    list.first().map(|m| m.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(id: &str) -> &'static Provider {
        provider(id).unwrap()
    }

    fn ids(v: &[Model]) -> Vec<&str> {
        v.iter().map(|m| m.id.as_str()).collect()
    }

    fn list(names: &[&str]) -> Vec<Model> {
        names
            .iter()
            .map(|n| Model {
                id: n.to_string(),
                name: n.to_string(),
            })
            .collect()
    }

    #[test]
    fn registry_is_sane() {
        let mut seen = std::collections::HashSet::new();
        for pr in PROVIDERS {
            assert!(seen.insert(pr.id), "duplicate id {}", pr.id);
            if !pr.local && pr.id != "custom" {
                assert!(pr.base_url.starts_with("https://"), "{}", pr.id);
            }
            if let Some(m) = pr.default_model {
                assert!(!m.is_empty(), "{}", pr.id);
            }
            // Only the local ones and custom get to edit the address.
            assert_eq!(pr.base_editable, pr.local || pr.id == "custom", "{}", pr.id);
            if pr.key == KeyNeed::Required {
                assert!(pr.key_url.is_some() && pr.key_env.is_some(), "{}", pr.id);
            }
        }
        assert_eq!(PROVIDERS.len(), 15);
        assert!(provider("nope").is_none());
    }

    #[test]
    fn parses_data_shape() {
        let v = json!({ "data": [
            { "id": "a", "display_name": "Model A" },
            { "id": "b", "name": "Model B" },
            { "id": "c" },
            { "nope": 1 },
        ]});
        let m = parse_models(p("openrouter"), &v);
        assert_eq!(ids(&m), ["a", "b", "c"]);
        assert_eq!(m[0].name, "Model A");
        assert_eq!(m[1].name, "Model B");
        assert_eq!(m[2].name, "c");
    }

    #[test]
    fn parses_bare_array_and_ollama() {
        let bare = json!([{ "id": "meta/llama" }, { "id": "qwen/qwen" }]);
        assert_eq!(
            ids(&parse_models(p("together"), &bare)),
            ["meta/llama", "qwen/qwen"]
        );
        let ollama = json!({ "models": [{ "name": "llama3:8b" }, { "model": "phi4:latest" }] });
        assert_eq!(
            ids(&parse_models(p("ollama"), &ollama)),
            ["llama3:8b", "phi4:latest"]
        );
        assert!(parse_models(p("ollama"), &json!({ "error": "x" })).is_empty());
    }

    #[test]
    fn strips_models_prefix_and_dedups() {
        let v = json!({ "data": [
            { "id": "models/gemini-2.5-flash", "display_name": "Gemini 2.5 Flash" },
            { "id": "gemini-2.5-flash" },
            { "id": "models/gemini-2.5-pro" },
        ]});
        let m = parse_models(p("gemini"), &v);
        assert_eq!(ids(&m), ["gemini-2.5-flash", "gemini-2.5-pro"]);
        assert_eq!(m[0].name, "Gemini 2.5 Flash");
    }

    #[test]
    fn filters_non_chat_only_where_asked() {
        let v = json!({ "data": [
            { "id": "gpt-5-mini" }, { "id": "text-embedding-3-small" },
            { "id": "whisper-1" }, { "id": "dall-e-3" }, { "id": "gpt-4o-realtime-preview" },
            { "id": "gpt-4o-mini-tts" }, { "id": "omni-moderation-latest" },
        ]});
        assert_eq!(ids(&parse_models(p("openai"), &v)), ["gpt-5-mini"]);
        // Groq keeps everything it lists.
        assert_eq!(parse_models(p("groq"), &v).len(), 7);
    }

    #[test]
    fn picks_a_model() {
        let o = p("openai"); // default gpt-5-mini
        let l = list(&["gpt-4.1", "gpt-5-mini", "o3"]);
        // Current wins while offered.
        assert_eq!(pick_model(o, Some("o3"), &l).as_deref(), Some("o3"));
        // Gone: the default.
        assert_eq!(
            pick_model(o, Some("gpt-3"), &l).as_deref(),
            Some("gpt-5-mini")
        );
        assert_eq!(pick_model(o, None, &l).as_deref(), Some("gpt-5-mini"));
        assert_eq!(pick_model(o, Some("  "), &l).as_deref(), Some("gpt-5-mini"));
        // Default missing too: the first.
        let l2 = list(&["x", "y"]);
        assert_eq!(pick_model(o, Some("gpt-3"), &l2).as_deref(), Some("x"));
        // Providers with no default fall to the first.
        assert_eq!(pick_model(p("xai"), None, &l2).as_deref(), Some("x"));
        // Empty list: keep current, else the default, else nothing.
        assert_eq!(pick_model(o, Some("mine"), &[]).as_deref(), Some("mine"));
        assert_eq!(pick_model(o, None, &[]).as_deref(), Some("gpt-5-mini"));
        assert_eq!(pick_model(p("xai"), None, &[]), None);
    }
}
