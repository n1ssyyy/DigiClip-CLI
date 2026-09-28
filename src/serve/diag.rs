//! Diagnostics bundle for bug reports: versions, health, settings (never
//! the key), recent jobs and the tail of `logs/serve.log`, with anything
//! key-shaped redacted.

use super::{probe_health, AppState, JobRecord};

/// Log lines kept from the end of `serve.log`.
const LOG_TAIL: usize = 1500;
/// Most recent jobs listed.
const JOBS: usize = 20;

/// Blank out API keys: the saved key itself, `sk-…` tokens and bearer
/// headers.
pub(super) fn redact(text: &str, key: Option<&str>) -> String {
    let mut s = text.to_string();
    if let Some(k) = key.map(str::trim).filter(|k| k.len() >= 8) {
        s = s.replace(k, "[REDACTED]");
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    let is_tok = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let next = |r: &str| match (r.find("sk-"), r.find("Bearer ")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    while let Some(i) = next(rest) {
        let (head, tail) = rest.split_at(i);
        let prefix = if tail.starts_with("sk-") {
            "sk-"
        } else {
            "Bearer "
        };
        let body = &tail[prefix.len()..];
        let n = body.find(|c: char| !is_tok(c)).unwrap_or(body.len());
        out.push_str(head);
        out.push_str(prefix);
        if n >= 8 {
            out.push_str("[REDACTED]");
        } else {
            out.push_str(&body[..n]);
        }
        rest = &body[n..];
    }
    out.push_str(rest);
    out
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn job_line(r: &JobRecord) -> String {
    let clips: Vec<String> = r
        .clips
        .iter()
        .map(|c| format!("#{}:{}", c.rank, c.render_status))
        .collect();
    format!(
        "- {} [{:?}] {:.0}s \"{}\"{}\n  options: {}\n  clips: {}\n",
        r.id,
        r.status,
        r.duration_s,
        r.name,
        r.error
            .as_deref()
            .map(|e| format!("\n  error: {e}"))
            .unwrap_or_default(),
        serde_json::to_string(&r.options).unwrap_or_default(),
        if clips.is_empty() {
            "-".into()
        } else {
            clips.join(" ")
        },
    )
}

pub(super) async fn diagnostics(st: &AppState) -> String {
    let settings = st.settings.lock().await.clone();
    let mut jobs: Vec<JobRecord> = st
        .jobs
        .lock()
        .await
        .values()
        .map(|l| l.record.clone())
        .collect();
    jobs.sort_by_key(|r| std::cmp::Reverse(r.created_ms));
    let models = {
        let runs = st.model_runs.lock().await;
        st.models_state(&runs)
    };
    let pretty = |v: serde_json::Value| serde_json::to_string_pretty(&v).unwrap_or_default();
    let log =
        std::fs::read_to_string(st.data_dir.join("logs").join("serve.log")).unwrap_or_default();
    let mut s = format!(
        "DigiClip diagnostics\nengine {} on {}/{}\nwritten at {} ms since epoch\n\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        super::now_ms()
    );
    s += &format!(
        "== health\n{}\n\n",
        pretty(serde_json::json!(probe_health()))
    );
    s += &format!(
        "== settings\n{}\n\n",
        pretty(serde_json::json!(settings.public()))
    );
    s += &format!("== models\n{}\n\n", pretty(serde_json::json!(models)));
    s += &format!("== jobs (latest {JOBS} of {})\n", jobs.len());
    for r in jobs.iter().take(JOBS) {
        s += &job_line(r);
    }
    s += &format!(
        "\n== serve.log (last {LOG_TAIL} lines)\n{}\n",
        tail(&log, LOG_TAIL)
    );
    redact(&s, settings.openrouter_key.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_never_leave() {
        let t = "key=sk-or-v1-abcdef0123456789 and Authorization: Bearer abcdefghijkl, \
                 my-secret-key-value, sk-short";
        let r = redact(t, Some("my-secret-key-value"));
        assert!(!r.contains("abcdef0123456789"));
        assert!(!r.contains("abcdefghijkl"));
        assert!(!r.contains("my-secret-key-value"));
        assert!(r.contains("sk-[REDACTED] and"));
        assert!(r.contains("Bearer [REDACTED],"));
        // Short look-alikes are not keys.
        assert!(r.ends_with("sk-short"));
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
    }
}
