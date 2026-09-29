//! Where each AI app keeps its MCP servers, and how to put DigiClip
//! there (or take it out) without disturbing anything else in the file.
//!
//! - JSON and JSONC configs go through a concrete syntax tree, so the
//!   user's comments, order and layout survive the edit.
//! - Codex's TOML goes through `toml_edit` for the same reason.
//! - YAML (Hermes, Goose) is edited line by line: only the `digiclip`
//!   block under the servers key is touched.
//! - Claude Code keeps its config in a big file it rewrites itself, so it
//!   is driven through its own `claude mcp` command instead.
//!
//! An app counts as found when its config folder exists (or its command
//! is on PATH); DigiClip never creates an app's home folder.

use std::path::{Path, PathBuf};

use jsonc_parser::cst::{CstInputValue, CstRootNode};
use jsonc_parser::ParseOptions;
use serde_json::{json, Value};

/// How the `digiclip` entry looks in a JSON config.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    /// `{command, args}`: the Claude Desktop layout most apps copy.
    Std,
    /// `{type: "stdio", command, args}`.
    Stdio,
    /// `{type: "local", command: [exe, ...args], enabled}` (OpenCode, Kilo).
    OpenCode,
    /// `{type: "local", command, args, tools: ["*"]}` (Copilot CLI).
    Copilot,
    /// `{command, args, env}` (Zed).
    Zed,
    /// `{type: "stdio", active, command, args, env}` (Jan).
    Jan,
    /// `{command, args, anythingllm.autoStart}`.
    AnythingLlm,
    /// `{command, args, disabled, autoApprove}` (Cline, Roo Code).
    Cline,
    /// `{command, args, disabled}` (Kiro).
    Kiro,
    /// `{type: "stdio", command, args, disabled}` (Factory Droid).
    Factory,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    /// JSON or JSONC; servers live under the top-level `key`.
    Json { key: &'static str, shape: Shape },
    /// `[mcp_servers.digiclip]` in Codex's config.toml.
    CodexToml,
    /// A YAML map under the top-level `key`.
    Yaml { key: &'static str, goose: bool },
    /// A file of its own that holds only DigiClip (Continue's drop-in
    /// folder): written whole, deleted on remove.
    OwnFile,
    /// Claude Code: through the `claude` command.
    ClaudeCli,
}

pub struct App {
    pub id: &'static str,
    pub format: Format,
}

pub const APPS: &[App] = &[
    App {
        id: "claude_desktop",
        format: Format::Json {
            key: "mcpServers",
            shape: Shape::Std,
        },
    },
    App {
        id: "claude_code",
        format: Format::ClaudeCli,
    },
    App {
        id: "codex",
        format: Format::CodexToml,
    },
    App {
        id: "cursor",
        format: Format::Json {
            key: "mcpServers",
            shape: Shape::Std,
        },
    },
    App {
        id: "vscode",
        format: Format::Json {
            key: "servers",
            shape: Shape::Stdio,
        },
    },
    App {
        id: "opencode",
        format: Format::Json {
            key: "mcp",
            shape: Shape::OpenCode,
        },
    },
    App {
        id: "hermes",
        format: Format::Yaml {
            key: "mcp_servers",
            goose: false,
        },
    },
    App {
        id: "gemini",
        format: Format::Json {
            key: "mcpServers",
            shape: Shape::Std,
        },
    },
    App {
        id: "windsurf",
        format: Format::Json {
            key: "mcpServers",
            shape: Shape::Std,
        },
    },
    App {
        id: "zed",
        format: Format::Json {
            key: "context_servers",
            shape: Shape::Zed,
        },
    },
    App {
        id: "copilot",
        format: Format::Json {
            key: "mcpServers",
            shape: Shape::Copilot,
        },
    },
    App {
        id: "cline",
        format: Format::Json {
            key: "mcpServers",
            shape: Shape::Cline,
        },
    },
    App {
        id: "roo",
        format: Format::Json {
            key: "mcpServers",
            shape: Shape::Cline,
        },
    },
    App {
        id: "kilo",
        format: Format::Json {
            key: "mcp",
            shape: Shape::OpenCode,
        },
    },
    App {
        id: "continue",
        format: Format::OwnFile,
    },
    App {
        id: "goose",
        format: Format::Yaml {
            key: "extensions",
            goose: true,
        },
    },
    App {
        id: "kiro",
        format: Format::Json {
            key: "mcpServers",
            shape: Shape::Kiro,
        },
    },
    App {
        id: "amp",
        format: Format::Json {
            key: "amp.mcpServers",
            shape: Shape::Std,
        },
    },
    App {
        id: "qwen",
        format: Format::Json {
            key: "mcpServers",
            shape: Shape::Std,
        },
    },
    App {
        id: "lm_studio",
        format: Format::Json {
            key: "mcpServers",
            shape: Shape::Std,
        },
    },
    App {
        id: "factory",
        format: Format::Json {
            key: "mcpServers",
            shape: Shape::Factory,
        },
    },
    App {
        id: "augment",
        format: Format::Json {
            key: "mcpServers",
            shape: Shape::Std,
        },
    },
    App {
        id: "jan",
        format: Format::Json {
            key: "mcpServers",
            shape: Shape::Jan,
        },
    },
    App {
        id: "anythingllm",
        format: Format::Json {
            key: "mcpServers",
            shape: Shape::AnythingLlm,
        },
    },
];

pub fn app(id: &str) -> Option<&'static App> {
    APPS.iter().find(|a| a.id == id)
}

// ---------------------------------------------------------------------------
// Where the configs are
// ---------------------------------------------------------------------------

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// `$VAR` as a folder, else `~/<rel>`.
fn env_or_home(var: &str, rel: &[&str]) -> Option<PathBuf> {
    if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(v));
    }
    let mut p = home()?;
    for r in rel {
        p.push(r);
    }
    Some(p)
}

/// `$XDG_CONFIG_HOME` or `~/.config`, on every OS: the CLI tools that use
/// it (OpenCode, Kilo, Amp) do so on Windows too.
fn xdg_config() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(home()?.join(".config")))
}

/// A command on PATH (npm installs `.cmd` shims on Windows).
pub fn on_path(name: &str) -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), format!("{name}.cmd")]
    } else {
        vec![name.to_string()]
    };
    let mut dirs_: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(h) = home() {
        dirs_.push(h.join(".local").join("bin"));
        if name == "claude" {
            dirs_.push(h.join(".claude").join("local"));
        }
    }
    dirs_
        .iter()
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

fn vscode_user_dirs() -> Vec<PathBuf> {
    let Some(c) = dirs::config_dir() else {
        return Vec::new();
    };
    ["Code", "Code - Insiders"]
        .iter()
        .map(|n| c.join(n).join("User"))
        .filter(|d| d.is_dir())
        .collect()
}

/// The first existing file of `names` in `dir`, else the first name.
fn first_existing(dir: &Path, names: &[&str]) -> PathBuf {
    names
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
        .unwrap_or_else(|| dir.join(names[0]))
}

pub fn claude_desktop_configs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(c) = dirs::config_dir() {
        out.push(c.join("Claude"));
    }
    // The Microsoft Store build keeps its own copy under the package.
    #[cfg(windows)]
    if let Some(local) = dirs::data_local_dir() {
        if let Ok(rd) = std::fs::read_dir(local.join("Packages")) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with("Claude_") {
                    out.push(e.path().join("LocalCache").join("Roaming").join("Claude"));
                }
            }
        }
    }
    out.into_iter()
        .filter(|d| d.is_dir())
        .map(|d| d.join("claude_desktop_config.json"))
        .collect()
}

/// Config files to edit for an app that is here, plus whether it was
/// found at all. A CLI found on PATH with no config folder yet gets its
/// default file (its folder is created on write).
pub fn locate(id: &str) -> (bool, Vec<PathBuf>) {
    let dir_file = |dir: Option<PathBuf>, file: &str| -> Vec<PathBuf> {
        dir.filter(|d| d.is_dir())
            .map(|d| d.join(file))
            .into_iter()
            .collect()
    };
    // Folder, or a CLI on PATH (then the default file even without it).
    let dir_or_cli = |dir: Option<PathBuf>, file: &str, cli: &str| -> (bool, Vec<PathBuf>) {
        match dir {
            Some(d) if d.is_dir() => (true, vec![d.join(file)]),
            Some(d) if on_path(cli).is_some() => (true, vec![d.join(file)]),
            _ => (false, Vec::new()),
        }
    };
    let found = |v: Vec<PathBuf>| (!v.is_empty(), v);
    match id {
        "claude_desktop" => found(claude_desktop_configs()),
        "claude_code" => {
            let cli = on_path("claude");
            (cli.is_some(), cli.into_iter().collect())
        }
        "codex" => dir_or_cli(
            env_or_home("CODEX_HOME", &[".codex"]),
            "config.toml",
            "codex",
        ),
        "cursor" => found(dir_file(home().map(|h| h.join(".cursor")), "mcp.json")),
        "vscode" => found(
            vscode_user_dirs()
                .into_iter()
                .map(|d| d.join("mcp.json"))
                .collect(),
        ),
        "opencode" => {
            let d = xdg_config().map(|c| c.join("opencode"));
            match d {
                Some(d) if d.is_dir() || on_path("opencode").is_some() => (
                    true,
                    vec![first_existing(
                        &d,
                        &["opencode.json", "opencode.jsonc", "config.json"],
                    )],
                ),
                _ => (false, Vec::new()),
            }
        }
        "hermes" => {
            let mut dirs_: Vec<PathBuf> = Vec::new();
            if let Some(v) = std::env::var_os("HERMES_HOME").filter(|v| !v.is_empty()) {
                dirs_.push(PathBuf::from(v));
            }
            if let Some(h) = home() {
                dirs_.push(h.join(".hermes"));
            }
            if cfg!(windows) {
                if let Some(l) = dirs::data_local_dir() {
                    dirs_.push(l.join("hermes"));
                }
            }
            dirs_.dedup();
            found(
                dirs_
                    .into_iter()
                    .filter(|d| d.is_dir())
                    .map(|d| d.join("config.yaml"))
                    .collect(),
            )
        }
        "gemini" => dir_or_cli(
            env_or_home("GEMINI_CLI_HOME", &[]).map(|h| h.join(".gemini")),
            "settings.json",
            "gemini",
        ),
        "qwen" => dir_or_cli(home().map(|h| h.join(".qwen")), "settings.json", "qwen"),
        "windsurf" => {
            let mut v = Vec::new();
            let devin = if cfg!(windows) {
                dirs::config_dir().map(|c| c.join("devin"))
            } else {
                xdg_config().map(|c| c.join("devin"))
            };
            v.extend(dir_file(devin, "mcp_config.json"));
            v.extend(dir_file(
                home().map(|h| h.join(".codeium").join("windsurf")),
                "mcp_config.json",
            ));
            found(v)
        }
        "zed" => {
            let d = if cfg!(windows) {
                dirs::config_dir().map(|c| c.join("Zed"))
            } else {
                xdg_config().map(|c| c.join("zed"))
            };
            found(dir_file(d, "settings.json"))
        }
        "copilot" => dir_or_cli(
            env_or_home("COPILOT_HOME", &[".copilot"]),
            "mcp-config.json",
            "copilot",
        ),
        "cline" => {
            let mut v = Vec::new();
            if let Some(h) = home().map(|h| h.join(".cline")).filter(|d| d.is_dir()) {
                v.push(
                    h.join("data")
                        .join("settings")
                        .join("cline_mcp_settings.json"),
                );
            }
            // The VS Code extension's old spot, while it still has a file.
            for u in vscode_user_dirs() {
                let p = u
                    .join("globalStorage")
                    .join("saoudrizwan.claude-dev")
                    .join("settings")
                    .join("cline_mcp_settings.json");
                if p.is_file() {
                    v.push(p);
                }
            }
            found(v)
        }
        "roo" => found(
            vscode_user_dirs()
                .into_iter()
                .map(|u| u.join("globalStorage").join("rooveterinaryinc.roo-cline"))
                .filter(|d| d.is_dir())
                .map(|d| d.join("settings").join("mcp_settings.json"))
                .collect(),
        ),
        "kilo" => {
            let d = xdg_config().map(|c| c.join("kilo"));
            match d {
                Some(d) if d.is_dir() => {
                    (true, vec![first_existing(&d, &["kilo.jsonc", "kilo.json"])])
                }
                _ => (false, Vec::new()),
            }
        }
        "continue" => found(dir_file(
            env_or_home("CONTINUE_GLOBAL_DIR", &[".continue"]),
            "mcpServers",
        ))
        .pipe(|(f, v)| (f, v.into_iter().map(|d| d.join("digiclip.json")).collect())),
        "goose" => {
            let d = if cfg!(windows) {
                dirs::config_dir().map(|c| c.join("Block").join("goose").join("config"))
            } else {
                xdg_config().map(|c| c.join("goose"))
            };
            found(dir_file(d, "config.yaml"))
        }
        "kiro" => match home().map(|h| h.join(".kiro")) {
            Some(d) if d.is_dir() => (true, vec![d.join("settings").join("mcp.json")]),
            _ => (false, Vec::new()),
        },
        "amp" => {
            if let Some(f) = std::env::var_os("AMP_SETTINGS_FILE").filter(|v| !v.is_empty()) {
                return (true, vec![PathBuf::from(f)]);
            }
            dir_or_cli(xdg_config().map(|c| c.join("amp")), "settings.json", "amp")
        }
        "lm_studio" => {
            let mut v = dir_file(home().map(|h| h.join(".lmstudio")), "mcp.json");
            if v.is_empty() {
                v = dir_file(
                    home().map(|h| h.join(".cache").join("lm-studio")),
                    "mcp.json",
                );
            }
            found(v)
        }
        "factory" => found(dir_file(home().map(|h| h.join(".factory")), "mcp.json")),
        "augment" => dir_or_cli(
            home().map(|h| h.join(".augment")),
            "settings.json",
            "auggie",
        ),
        "jan" => found(dir_file(
            dirs::config_dir().map(|c| c.join("Jan").join("data")),
            "mcp_config.json",
        )),
        "anythingllm" => {
            match dirs::config_dir().map(|c| c.join("anythingllm-desktop").join("storage")) {
                Some(d) if d.is_dir() => (
                    true,
                    vec![d.join("plugins").join("anythingllm_mcp_servers.json")],
                ),
                _ => (false, Vec::new()),
            }
        }
        _ => (false, Vec::new()),
    }
}

/// Tiny helper so a match arm can post-process its tuple inline.
trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

pub fn json_entry(shape: Shape, exe: &str) -> Value {
    let args = json!(["--mcp"]);
    match shape {
        Shape::Std => json!({ "command": exe, "args": args }),
        Shape::Stdio => json!({ "type": "stdio", "command": exe, "args": args }),
        Shape::OpenCode => json!({ "type": "local", "command": [exe, "--mcp"], "enabled": true }),
        Shape::Copilot => json!({ "type": "local", "command": exe, "args": args, "tools": ["*"] }),
        Shape::Zed => json!({ "command": exe, "args": args, "env": {} }),
        Shape::Jan => {
            json!({ "type": "stdio", "active": true, "command": exe, "args": args, "env": {} })
        }
        Shape::AnythingLlm => {
            json!({ "command": exe, "args": args, "anythingllm.autoStart": true })
        }
        Shape::Cline => {
            json!({ "command": exe, "args": args, "disabled": false, "autoApprove": [] })
        }
        Shape::Kiro => json!({ "command": exe, "args": args, "disabled": false }),
        Shape::Factory => {
            json!({ "type": "stdio", "command": exe, "args": args, "disabled": false })
        }
    }
}

/// YAML body lines (unindented) of the `digiclip:` block. Paths go in
/// single quotes: backslashes stay literal there.
fn yaml_body(goose: bool, exe: &str) -> Vec<String> {
    let q = format!("'{}'", exe.replace('\'', "''"));
    if goose {
        vec![
            "type: stdio".into(),
            "name: digiclip".into(),
            "description: Turn long videos into short captioned clips".into(),
            "enabled: true".into(),
            format!("cmd: {q}"),
            "args: ['--mcp']".into(),
            "envs: {}".into(),
            "env_keys: []".into(),
            "timeout: 300".into(),
        ]
    } else {
        vec![
            format!("command: {q}"),
            "args: ['--mcp']".into(),
            "enabled: true".into(),
        ]
    }
}

/// Whether a found entry runs this bridge (or points at the HTTP server,
/// which never goes stale).
fn is_current(entry: &Value, exe: Option<&str>) -> bool {
    if entry["url"].is_string() || entry["serverUrl"].is_string() {
        return true;
    }
    let Some(exe) = exe else { return false };
    fn walk(v: &Value, exe: &str) -> bool {
        match v {
            Value::String(s) => s == exe,
            Value::Array(a) => a.iter().any(|x| walk(x, exe)),
            Value::Object(o) => o.values().any(|x| walk(x, exe)),
            _ => false,
        }
    }
    walk(entry, exe)
}

// ---------------------------------------------------------------------------
// JSON / JSONC
// ---------------------------------------------------------------------------

fn read_text(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Write via a temp file and a rename, so a crash never leaves half a
/// config behind.
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".digiclip-tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

fn to_input(v: &Value) -> CstInputValue {
    match v {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(a) => CstInputValue::Array(a.iter().map(to_input).collect()),
        Value::Object(o) => {
            CstInputValue::Object(o.iter().map(|(k, v)| (k.clone(), to_input(v))).collect())
        }
    }
}

fn json_read(path: &Path, key: &str) -> Option<Value> {
    let text = read_text(path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    let v: Value = jsonc_parser::parse_to_serde_value(&text, &ParseOptions::default()).ok()?;
    v.get(key)?.get("digiclip").cloned()
}

/// Put `<key>.digiclip` into (or take it out of) a JSON/JSONC file,
/// keeping comments and everything else as the user left it. A file that
/// doesn't parse is never touched.
pub fn json_edit(path: &Path, key: &str, entry: Option<&Value>) -> Result<(), String> {
    let text = read_text(path)?;
    if entry.is_none() && text.trim().is_empty() {
        return Ok(());
    }
    let src = if text.trim().is_empty() {
        "{}\n".to_string()
    } else {
        text.clone()
    };
    let bad = |why: String| {
        format!(
            "{} isn't valid JSON ({why}); fix it or add DigiClip by hand",
            path.display()
        )
    };
    let root =
        CstRootNode::parse(&src, &ParseOptions::default()).map_err(|e| bad(e.to_string()))?;
    let obj = root
        .object_value()
        .ok_or_else(|| bad("not an object".into()))?;
    if let Some(p) = obj.get(key) {
        if p.object_value().is_none() {
            return Err(format!(
                "{key} in {} isn't an object; add DigiClip by hand",
                path.display()
            ));
        }
    }
    match entry {
        Some(e) => {
            let servers = obj.object_value_or_set(key);
            match servers.get("digiclip") {
                Some(p) => p.set_value(to_input(e)),
                None => {
                    servers.append("digiclip", to_input(e));
                }
            }
        }
        None => {
            let Some(p) = obj.object_value(key).and_then(|s| s.get("digiclip")) else {
                return Ok(());
            };
            p.remove();
        }
    }
    let mut out = root.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    write_atomic(path, &out)
}

// ---------------------------------------------------------------------------
// TOML (Codex)
// ---------------------------------------------------------------------------

fn toml_read(path: &Path) -> Option<Value> {
    let text = read_text(path).ok()?;
    let doc: toml_edit::DocumentMut = text.parse().ok()?;
    let t = doc.get("mcp_servers")?.get("digiclip")?;
    let command = t
        .get("command")
        .and_then(|c| c.as_str())
        .unwrap_or_default();
    Some(json!({ "command": command }))
}

pub fn toml_edit(path: &Path, exe: Option<&str>) -> Result<(), String> {
    use toml_edit::{value, Array, DocumentMut, Item, Table};
    let text = read_text(path)?;
    let mut doc: DocumentMut = text.parse().map_err(|e| {
        format!(
            "{} isn't valid TOML ({e}); fix it or add DigiClip by hand",
            path.display()
        )
    })?;
    match exe {
        Some(exe) => {
            if !doc.contains_key("mcp_servers") {
                let mut t = Table::new();
                t.set_implicit(true);
                doc.insert("mcp_servers", Item::Table(t));
            }
            let servers = doc["mcp_servers"].as_table_mut().ok_or_else(|| {
                format!(
                    "mcp_servers in {} isn't a table; add DigiClip by hand",
                    path.display()
                )
            })?;
            let mut t = Table::new();
            t.insert("command", value(exe));
            let mut args = Array::new();
            args.push("--mcp");
            t.insert("args", value(args));
            servers.insert("digiclip", Item::Table(t));
        }
        None => {
            let removed = doc
                .get_mut("mcp_servers")
                .and_then(|s| s.as_table_like_mut())
                .and_then(|s| s.remove("digiclip"))
                .is_some();
            if !removed {
                return Ok(());
            }
        }
    }
    write_atomic(path, &doc.to_string())
}

// ---------------------------------------------------------------------------
// YAML (line by line)
// ---------------------------------------------------------------------------

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Blank or comment-only.
fn is_quiet(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

/// The `key:` line (top level) and whether its value is written inline.
/// `Err` when the value is an inline map/list we won't rewrite.
fn yaml_key_line(lines: &[String], key: &str) -> Result<Option<(usize, bool)>, String> {
    for (i, l) in lines.iter().enumerate() {
        if indent_of(l) != 0 {
            continue;
        }
        let Some(rest) = l.strip_prefix(key).and_then(|r| r.strip_prefix(':')) else {
            continue;
        };
        let v = rest.split(" #").next().unwrap_or("").trim();
        return match v {
            "" => Ok(Some((i, false))),
            "{}" | "null" | "~" => Ok(Some((i, true))),
            _ => Err(format!("{key} is written inline; add DigiClip by hand")),
        };
    }
    Ok(None)
}

/// The section under `key:` (exclusive end) and its children's indent.
fn yaml_section(lines: &[String], at: usize) -> (usize, usize) {
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(at + 1) {
        if !is_quiet(l) && indent_of(l) == 0 {
            end = i;
            break;
        }
    }
    let child = lines[at + 1..end]
        .iter()
        .find(|l| !is_quiet(l))
        .map(|l| indent_of(l))
        .unwrap_or(2);
    (end, child)
}

/// `digiclip:` block range inside a section.
fn yaml_block(lines: &[String], from: usize, end: usize, child: usize) -> Option<(usize, usize)> {
    let start = (from..end).find(|&i| {
        let l = &lines[i];
        indent_of(l) == child && l.trim_start().strip_prefix("digiclip:").is_some()
    })?;
    let mut stop = end;
    for (i, l) in lines.iter().enumerate().take(end).skip(start + 1) {
        if !is_quiet(l) && indent_of(l) <= child {
            stop = i;
            break;
        }
    }
    // Trailing blank lines belong to whatever comes next.
    while stop > start + 1 && lines[stop - 1].trim().is_empty() {
        stop -= 1;
    }
    Some((start, stop))
}

fn yaml_read(path: &Path, key: &str) -> Option<Value> {
    let text = read_text(path).ok()?;
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let (at, _) = yaml_key_line(&lines, key).ok()??;
    let (end, child) = yaml_section(&lines, at);
    let (s, e) = yaml_block(&lines, at + 1, end, child)?;
    // Enough for `is_current`: the block's quoted scalars.
    let strings: Vec<Value> = lines[s..e]
        .iter()
        .filter_map(|l| {
            let v = l.split_once(':')?.1.trim();
            let v = v
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .map(|v| v.replace("''", "'"))
                .or_else(|| {
                    v.strip_prefix('"')
                        .and_then(|v| v.strip_suffix('"'))
                        .map(|v| v.replace("\\\\", "\\"))
                })
                .unwrap_or_else(|| v.to_string());
            Some(Value::String(v))
        })
        .collect();
    Some(Value::Array(strings))
}

pub fn yaml_edit(path: &Path, key: &str, body: Option<Vec<String>>) -> Result<(), String> {
    let text = read_text(path)?;
    let crlf = text.contains("\r\n");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    match yaml_key_line(&lines, key).map_err(|e| format!("{}: {e}", path.display()))? {
        None => {
            let Some(body) = body else { return Ok(()) };
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(format!("{key}:"));
            lines.push("  digiclip:".into());
            lines.extend(body.iter().map(|b| format!("    {b}")));
        }
        Some((at, inline)) => {
            if inline {
                lines[at] = format!("{key}:");
            }
            let (end, child) = yaml_section(&lines, at);
            let step = child.max(2);
            let mut insert_at = match yaml_block(&lines, at + 1, end, child) {
                Some((s, e)) => {
                    lines.drain(s..e);
                    s
                }
                None => {
                    if body.is_none() {
                        return Ok(());
                    }
                    // After the section's last real line.
                    (at + 1..end)
                        .rev()
                        .find(|&i| !is_quiet(&lines[i]))
                        .map_or(at + 1, |i| i + 1)
                }
            };
            match body {
                Some(body) => {
                    lines.insert(insert_at, format!("{}digiclip:", " ".repeat(child)));
                    for b in body {
                        insert_at += 1;
                        lines.insert(insert_at, format!("{}{b}", " ".repeat(child + step)));
                    }
                }
                None => {
                    // Last server gone: leave an empty map, not a null.
                    let (end, _) = yaml_section(&lines, at);
                    if lines[at + 1..end].iter().all(|l| is_quiet(l)) {
                        lines[at] = format!("{key}: {{}}");
                    }
                }
            }
        }
    }
    let nl = if crlf { "\r\n" } else { "\n" };
    let mut out = lines.join(nl);
    out.push_str(nl);
    write_atomic(path, &out)
}

// ---------------------------------------------------------------------------
// One app
// ---------------------------------------------------------------------------

/// `(added, current)` over an app's files.
pub fn state(app: &App, paths: &[PathBuf], exe: Option<&str>) -> (bool, bool) {
    let entries: Vec<Value> = paths
        .iter()
        .filter_map(|p| match app.format {
            Format::Json { key, .. } => json_read(p, key),
            Format::CodexToml => toml_read(p),
            Format::Yaml { key, .. } => yaml_read(p, key),
            Format::OwnFile => json_read(p, "mcpServers"),
            Format::ClaudeCli => None,
        })
        .collect();
    (
        !entries.is_empty(),
        entries.iter().any(|e| is_current(e, exe)),
    )
}

/// Write (or with `exe: None`, remove) DigiClip in every file of a
/// file-based app. Stops at the first file that fails.
pub fn apply(app: &App, paths: &[PathBuf], exe: Option<&str>) -> Result<(), String> {
    for p in paths {
        match app.format {
            Format::Json { key, shape } => {
                json_edit(p, key, exe.map(|e| json_entry(shape, e)).as_ref())?
            }
            Format::CodexToml => toml_edit(p, exe)?,
            Format::Yaml { key, goose } => yaml_edit(p, key, exe.map(|e| yaml_body(goose, e)))?,
            Format::OwnFile => match exe {
                Some(e) => {
                    let body = json!({ "mcpServers": { "digiclip": json_entry(Shape::Std, e) } });
                    let txt = serde_json::to_string_pretty(&body).map_err(|e| e.to_string())?;
                    write_atomic(p, &(txt + "\n"))?;
                }
                None => match std::fs::remove_file(p) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(format!("{}: {e}", p.display())),
                },
            },
            Format::ClaudeCli => return Err("Claude Code is set up through its own command".into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("digiclip-apps-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const EXE: &str = r"C:\b\digiclip-mcp.exe";

    #[test]
    fn jsonc_keeps_comments_and_neighbours() {
        let d = scratch("jsonc");
        let p = d.join("settings.json");
        let src = "// Zed settings\n{\n  // theme first\n  \"theme\": \"One Dark\",\n  \"context_servers\": {\n    \"other\": { \"command\": \"o\" },\n  },\n}\n";
        std::fs::write(&p, src).unwrap();
        json_edit(&p, "context_servers", Some(&json_entry(Shape::Zed, EXE))).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(
            out.contains("// Zed settings") && out.contains("// theme first"),
            "{out}"
        );
        let app = app("zed").unwrap();
        assert_eq!(
            state(app, std::slice::from_ref(&p), Some(EXE)),
            (true, true)
        );
        assert_eq!(
            state(app, std::slice::from_ref(&p), Some("C:/other.exe")),
            (true, false)
        );
        // Idempotent: a second add replaces, never duplicates.
        json_edit(&p, "context_servers", Some(&json_entry(Shape::Zed, EXE))).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p)
                .unwrap()
                .matches("\"digiclip\"")
                .count(),
            1
        );
        json_edit(&p, "context_servers", None).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(
            !out.contains("digiclip")
                && out.contains("\"other\"")
                && out.contains("// theme first"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn json_new_file_and_broken_file() {
        let d = scratch("json");
        let p = d.join("sub").join("mcp.json");
        json_edit(&p, "servers", Some(&json_entry(Shape::Stdio, EXE))).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["servers"]["digiclip"]["type"], "stdio");
        assert_eq!(v["servers"]["digiclip"]["command"], EXE);
        // Removing from a missing file is a no-op, not a new file.
        let q = d.join("none.json");
        json_edit(&q, "servers", None).unwrap();
        assert!(!q.exists());
        std::fs::write(&p, "{ nope").unwrap();
        assert!(json_edit(&p, "servers", Some(&json_entry(Shape::Std, EXE))).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ nope");
        std::fs::write(&p, r#"{"servers": 3}"#).unwrap();
        assert!(json_edit(&p, "servers", Some(&json_entry(Shape::Std, EXE))).is_err());
        // A dotted key is one key (Amp).
        let a = d.join("amp.json");
        std::fs::write(&a, r#"{"amp.url": "x"}"#).unwrap();
        json_edit(&a, "amp.mcpServers", Some(&json_entry(Shape::Std, EXE))).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&a).unwrap()).unwrap();
        assert_eq!(v["amp.mcpServers"]["digiclip"]["args"][0], "--mcp");
        assert_eq!(v["amp.url"], "x");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn codex_toml_round_trip() {
        let d = scratch("toml");
        let p = d.join("config.toml");
        std::fs::write(
            &p,
            "# my codex\nmodel = \"gpt-5\"\n\n[mcp_servers.other]\ncommand = \"o\"\n",
        )
        .unwrap();
        toml_edit(&p, Some(EXE)).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(
            out.contains("# my codex") && out.contains("[mcp_servers.other]"),
            "{out}"
        );
        assert!(out.contains("[mcp_servers.digiclip]"), "{out}");
        let doc: toml_edit::DocumentMut = out.parse().unwrap();
        assert_eq!(
            doc["mcp_servers"]["digiclip"]["command"].as_str(),
            Some(EXE)
        );
        assert_eq!(
            state(app("codex").unwrap(), std::slice::from_ref(&p), Some(EXE)),
            (true, true)
        );
        toml_edit(&p, None).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(
            !out.contains("digiclip") && out.contains("[mcp_servers.other]"),
            "{out}"
        );
        // Fresh file: no stray empty [mcp_servers] header.
        let q = d.join("fresh.toml");
        toml_edit(&q, Some(EXE)).unwrap();
        let out = std::fs::read_to_string(&q).unwrap();
        assert!(out.starts_with("[mcp_servers.digiclip]"), "{out}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn yaml_blocks() {
        let d = scratch("yaml");
        let p = d.join("config.yaml");
        let src = "# Hermes\nmodel: x\nmcp_servers:\n    github:\n        command: gh\n    # keep me\n\nterminal:\n  backend: local\n";
        std::fs::write(&p, src).unwrap();
        yaml_edit(&p, "mcp_servers", Some(yaml_body(false, EXE))).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(
            out.contains("    digiclip:\n        command: 'C:\\b\\digiclip-mcp.exe'"),
            "{out}"
        );
        assert!(
            out.contains("# keep me") && out.contains("terminal:\n  backend: local"),
            "{out}"
        );
        let hermes = app("hermes").unwrap();
        assert_eq!(
            state(hermes, std::slice::from_ref(&p), Some(EXE)),
            (true, true)
        );
        // Re-adding replaces the block in place.
        yaml_edit(&p, "mcp_servers", Some(yaml_body(false, EXE))).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p)
                .unwrap()
                .matches("digiclip:")
                .count(),
            1
        );
        yaml_edit(&p, "mcp_servers", None).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(
            !out.contains("digiclip") && out.contains("github:") && out.contains("terminal:"),
            "{out}"
        );

        // Missing key, then the only server removed again.
        let q = d.join("goose.yaml");
        std::fs::write(&q, "GOOSE_PROVIDER: openai\n").unwrap();
        yaml_edit(&q, "extensions", Some(yaml_body(true, EXE))).unwrap();
        let out = std::fs::read_to_string(&q).unwrap();
        assert!(
            out.contains("extensions:\n  digiclip:\n    type: stdio"),
            "{out}"
        );
        yaml_edit(&q, "extensions", None).unwrap();
        assert!(std::fs::read_to_string(&q)
            .unwrap()
            .contains("extensions: {}"));
        // And back from `{}`.
        yaml_edit(&q, "extensions", Some(yaml_body(true, EXE))).unwrap();
        assert!(std::fs::read_to_string(&q)
            .unwrap()
            .contains("extensions:\n  digiclip:"));
        // Inline maps are left alone.
        std::fs::write(&q, "extensions: {a: 1}\n").unwrap();
        assert!(yaml_edit(&q, "extensions", Some(yaml_body(true, EXE))).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn own_file() {
        let d = scratch("own");
        let p = d.join("mcpServers").join("digiclip.json");
        let a = app("continue").unwrap();
        apply(a, std::slice::from_ref(&p), Some(EXE)).unwrap();
        assert_eq!(state(a, std::slice::from_ref(&p), Some(EXE)), (true, true));
        apply(a, std::slice::from_ref(&p), None).unwrap();
        assert!(!p.exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
