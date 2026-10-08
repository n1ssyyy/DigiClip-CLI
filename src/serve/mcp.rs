//! MCP server: AI apps (Claude Desktop, Claude Code, Cursor…) drive
//! DigiClip with the same powers as the UI.
//!
//! - Streamable HTTP on `127.0.0.1:<mcp_port>/mcp` (default 47420): plain
//!   JSON-RPC answers (no SSE), `Authorization: Bearer <mcp_token>`, and
//!   browser origins other than localhost are refused.
//! - `digiclip --mcp`: a stdio bridge for apps that only launch commands.
//!   It reads `<data>/mcp/server.json` (port, token, app path), relays
//!   each JSON-RPC line and starts the app hidden when nothing answers.
//!   The engine copies itself to `<data>/mcp/digiclip-mcp(.exe)` for this,
//!   so an AI app never holds the installed engine open (updates stay
//!   possible).
//!
//! Tools mostly wrap the socket commands ([`handle_cmd`]), so the UI sees
//! every change live; each call also lands in an activity feed the app
//! shows (who is working, on what).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use super::mcp_apps as apps;
use super::{
    handle_cmd, health, now_ms, AppState, ClientMsg, Cmd, Event, JobOptions, JobRecord, JobStatus,
    ServerMsg,
};

pub const DEFAULT_PORT: u16 = 47420;
/// Newest first; the first one is what we answer when a client asks for
/// something we don't know.
const PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const ACTIVITY_MAX: usize = 50;
const SESSIONS_MAX: usize = 64;
/// Folder under the data dir: bridge copy + `server.json`.
const DIR: &str = "mcp";
const SERVER_FILE: &str = "server.json";
const SESSION_HEADER: &str = "mcp-session-id";
/// The bridge names its client here, so calls keep a name across engine
/// restarts (a stale session id is not an error).
const CLIENT_HEADER: &str = "x-mcp-client";

#[derive(Default)]
pub struct Mcp {
    inner: std::sync::Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, Session>,
    activity: VecDeque<Activity>,
    seq: u64,
    server: Option<tokio::task::JoinHandle<()>>,
    running_port: Option<u16>,
    error: Option<String>,
    bridge: Option<PathBuf>,
    /// Per AI app: found on this machine / DigiClip added (cached, reading
    /// the config files on every broadcast would be wasteful).
    installed: Value,
}

#[derive(Clone)]
struct Session {
    client: String,
    last_ms: u64,
}

#[derive(Clone, serde::Serialize)]
struct Activity {
    seq: u64,
    at_ms: u64,
    client: String,
    tool: String,
    /// `running`, `ok` or `error`.
    state: &'static str,
    detail: String,
    ms: u64,
}

impl Mcp {
    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g)
    }
}

// ---------------------------------------------------------------------------
// State the UI sees
// ---------------------------------------------------------------------------

pub(super) async fn public(st: &AppState) -> Value {
    let (on, port, token) = {
        let s = st.settings.lock().await;
        (
            s.mcp_on,
            s.mcp_port,
            s.mcp_token.clone().unwrap_or_default(),
        )
    };
    let url = format!("http://127.0.0.1:{port}/mcp");
    st.mcp.with(|m| {
        let mut clients: Vec<(&String, &Session)> = m.sessions.iter().collect();
        clients.sort_by_key(|(_, s)| std::cmp::Reverse(s.last_ms));
        let mut seen = Vec::new();
        let clients: Vec<Value> = clients
            .into_iter()
            .filter(|(_, s)| {
                let new = !seen.contains(&s.client);
                seen.push(s.client.clone());
                new
            })
            .take(8)
            .map(|(_, s)| json!({ "name": s.client, "last_ms": s.last_ms }))
            .collect();
        let bridge = m.bridge.as_ref().map(|b| b.display().to_string());
        json!({
            "on": on,
            "port": port,
            "running": m.running_port.is_some(),
            "error": m.error,
            "url": url,
            "token": token,
            "bridge": bridge,
            "clients": clients,
            "activity": m.activity.iter().rev().collect::<Vec<_>>(),
            "tools": tools()
                .iter()
                .map(|t| json!({ "name": t["name"], "title": t["title"], "description": t["description"] }))
                .collect::<Vec<_>>(),
            "installed": m.installed,
            "configs": {
                "stdio": bridge.as_ref().map(|b| json!({
                    "mcpServers": { "digiclip": { "command": b, "args": ["--mcp"] } }
                })),
                "http": {
                    "mcpServers": { "digiclip": {
                        "type": "http",
                        "url": url,
                        "headers": { "Authorization": format!("Bearer {token}") },
                    } }
                },
                "claude_code": bridge.as_ref().map(|b| claude_code_cmd(b)),
            },
        })
    })
}

fn claude_code_cmd(bridge: &str) -> String {
    format!("claude mcp add digiclip --scope user -- \"{bridge}\" --mcp")
}

pub(super) async fn broadcast(st: &AppState) {
    let mcp = public(st).await;
    let _ = st.bus.send(ServerMsg::Ev {
        ev: Event::Mcp { mcp },
    });
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// A 128-bit hex secret. `RandomState` is seeded from the OS, which is
/// all a loopback token needs (no rand crate).
fn secret() -> String {
    use std::hash::{BuildHasher, Hasher};
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut out = String::new();
    for _ in 0..2 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(now_ms());
        h.write_u32(std::process::id());
        h.write_u64(N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// (Re)start the listener from the saved settings: make the token on first
/// run, refresh the bridge copy, write `server.json`, tell the UI.
/// Boxed: `handle_cmd` restarts this server, whose handler runs
/// `handle_cmd`; the erased type breaks that cycle for the compiler.
pub(super) fn restart(st: &Arc<AppState>) -> BoxFut<'_> {
    Box::pin(restart_inner(st))
}

type BoxFut<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

async fn restart_inner(st: &Arc<AppState>) {
    let (on, port) = {
        let mut s = st.settings.lock().await;
        if s.mcp_token.as_deref().is_none_or(|t| t.len() < 16) {
            s.mcp_token = Some(secret());
            st.save_settings(&s);
        }
        (s.mcp_on, s.mcp_port)
    };
    let old = st.mcp.with(|m| {
        m.running_port = None;
        m.error = None;
        m.server.take()
    });
    if let Some(h) = old {
        h.abort();
        let _ = h.await;
    }
    let dir = st.data_dir.join(DIR);
    let bridge = tokio::task::spawn_blocking(move || copy_bridge(&dir))
        .await
        .ok()
        .flatten();
    st.mcp.with(|m| m.bridge = bridge);
    if on {
        match bind(port).await {
            Ok(listener) => {
                let app = axum::Router::new()
                    .route(
                        "/mcp",
                        axum::routing::post(mcp_post)
                            .get(mcp_get)
                            .delete(mcp_delete),
                    )
                    .with_state(st.clone());
                let h = tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, app).await {
                        tracing::warn!("mcp server stopped: {e}");
                    }
                });
                st.mcp.with(|m| {
                    m.server = Some(h);
                    m.running_port = Some(port);
                });
                tracing::info!("mcp server on http://127.0.0.1:{port}/mcp");
            }
            Err(e) => {
                tracing::warn!("mcp server: port {port}: {e}");
                st.mcp
                    .with(|m| m.error = Some(format!("port {port} is taken ({e})")));
            }
        }
    }
    write_server_file(st).await;
    refresh_installed(st).await;
    broadcast(st).await;
}

/// The old listener closes as its task unwinds: retry a moment.
async fn bind(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    let mut last = None;
    for _ in 0..15 {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => return Ok(l),
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("bind failed")))
}

pub(super) async fn rotate_token(st: &Arc<AppState>) {
    {
        let mut s = st.settings.lock().await;
        s.mcp_token = Some(secret());
        st.save_settings(&s);
    }
    st.mcp.with(|m| m.sessions.clear());
    write_server_file(st).await;
    broadcast(st).await;
}

/// What the bridge needs to find us (and to start the app when we're
/// not running).
async fn write_server_file(st: &AppState) {
    let (on, port, token) = {
        let s = st.settings.lock().await;
        (
            s.mcp_on,
            s.mcp_port,
            s.mcp_token.clone().unwrap_or_default(),
        )
    };
    let app = std::env::var("DIGICLIP_APP_EXE")
        .ok()
        .filter(|a| !a.trim().is_empty());
    let body = json!({
        "on": on,
        "port": port,
        "url": format!("http://127.0.0.1:{port}/mcp"),
        "token": token,
        "app": app,
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
    });
    let dir = st.data_dir.join(DIR);
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(txt) = serde_json::to_string_pretty(&body) {
        let _ = std::fs::write(dir.join(SERVER_FILE), txt);
    }
}

fn bridge_name() -> &'static str {
    if cfg!(windows) {
        "digiclip-mcp.exe"
    } else {
        "digiclip-mcp"
    }
}

/// Copy this engine (plus the VC runtime DLLs on Windows) to
/// `<data>/mcp/`, once per version. A copy an AI app is running stays as
/// it is until it quits.
fn copy_bridge(dir: &Path) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let target = dir.join(bridge_name());
    let stamp = dir.join("digiclip-mcp.version");
    let want = format!(
        "{} {}",
        env!("CARGO_PKG_VERSION"),
        std::fs::metadata(&exe).map(|m| m.len()).unwrap_or(0)
    );
    let fresh = target.is_file() && std::fs::read_to_string(&stamp).is_ok_and(|s| s == want);
    if fresh || exe == target {
        return Some(target);
    }
    std::fs::create_dir_all(dir).ok()?;
    let copied = std::fs::copy(&exe, &target).is_ok();
    if let Some(src_dir) = exe.parent() {
        if let Ok(rd) = std::fs::read_dir(src_dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                if name.ends_with(".dll")
                    && (name.starts_with("msvcp") || name.starts_with("vcruntime"))
                {
                    let _ = std::fs::copy(e.path(), dir.join(e.file_name()));
                }
            }
        }
    }
    if copied {
        let _ = std::fs::write(&stamp, want);
    } else if !target.is_file() {
        tracing::warn!("mcp: could not copy the bridge to {}", target.display());
        return None;
    }
    Some(target)
}

// ---------------------------------------------------------------------------
// AI app configs
// ---------------------------------------------------------------------------

fn claude_code_config() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude.json"))
}

/// Found / added / current for every app DigiClip knows (see `mcp_apps`).
async fn refresh_installed(st: &AppState) {
    let bridge = st.mcp.with(|m| m.bridge.clone());
    let v = tokio::task::spawn_blocking(move || {
        let exe = bridge.as_ref().map(|b| b.display().to_string());
        let mut out = serde_json::Map::new();
        for app in apps::APPS {
            let (found, paths) = apps::locate(app.id);
            // Claude Code: the CLI is what we run, its file is what we read.
            let read: Vec<PathBuf> = match app.format {
                apps::Format::ClaudeCli => claude_code_config().into_iter().collect(),
                _ => paths.clone(),
            };
            let probe = match app.format {
                apps::Format::ClaudeCli => apps::App {
                    id: app.id,
                    format: apps::Format::Json {
                        key: "mcpServers",
                        shape: apps::Shape::Std,
                    },
                },
                f => apps::App {
                    id: app.id,
                    format: f,
                },
            };
            let (added, current) = apps::state(&probe, &read, exe.as_deref());
            out.insert(
                app.id.into(),
                json!({
                    "found": found,
                    "added": added,
                    "current": current,
                    "paths": paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                }),
            );
        }
        Value::Object(out)
    })
    .await
    .unwrap_or(Value::Null);
    st.mcp.with(|m| m.installed = v);
}

async fn run_claude(cli: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::from(crate::process::command(cli));
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(40), cmd.output())
        .await
        .map_err(|_| "claude didn't answer in 40 s".to_string())?
        .map_err(|e| format!("couldn't run claude: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        Ok(text)
    } else {
        Err(text.trim().to_string())
    }
}

/// Add DigiClip to (or with `remove`, take it out of) an AI app.
/// Replies with the file(s) changed.
pub(super) async fn install(
    st: &Arc<AppState>,
    client: &str,
    remove: bool,
) -> Result<String, String> {
    let bridge = st.mcp.with(|m| m.bridge.clone());
    let exe = match (&bridge, remove) {
        (_, true) => None,
        (Some(b), false) => Some(b.display().to_string()),
        (None, false) => return Err("the MCP bridge couldn't be set up (see the log)".into()),
    };
    let Some(app) = apps::app(client) else {
        return Err(format!("unknown app [{client}]"));
    };
    let (found, paths) = apps::locate(client);
    let res = if !found || paths.is_empty() {
        Err("that app isn't installed for this user".to_string())
    } else if app.format == apps::Format::ClaudeCli {
        let cli = paths[0].clone();
        // `add` refuses an existing name: drop the old one first.
        let _ = run_claude(&cli, &["mcp", "remove", "--scope", "user", "digiclip"]).await;
        match &exe {
            Some(b) => run_claude(
                &cli,
                &[
                    "mcp", "add", "--scope", "user", "digiclip", "--", b, "--mcp",
                ],
            )
            .await
            .map(|_| "~/.claude.json".to_string()),
            None => Ok("~/.claude.json".to_string()),
        }
    } else {
        let shown = paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        tokio::task::spawn_blocking(move || apps::apply(app, &paths, exe.as_deref()))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r)
            .map(|_| shown)
    };
    refresh_installed(st).await;
    res
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// Browsers send an Origin; only pages on this machine may talk to us
/// (DNS-rebinding guard, as the MCP spec asks).
fn origin_ok(headers: &HeaderMap) -> bool {
    let Some(o) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let rest = o
        .strip_prefix("http://")
        .or_else(|| o.strip_prefix("https://"))
        .unwrap_or("");
    let host = if rest.starts_with('[') {
        rest.split(']')
            .next()
            .map(|h| format!("{h}]"))
            .unwrap_or_default()
    } else {
        rest.split(':').next().unwrap_or("").to_string()
    };
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_err(id: Value, code: i64, msg: impl std::fmt::Display) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg.to_string() } })
}

fn json_response(status: StatusCode, body: &Value) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

async fn authorized(st: &AppState, headers: &HeaderMap) -> bool {
    let token = st
        .settings
        .lock()
        .await
        .mcp_token
        .clone()
        .unwrap_or_default();
    let got = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .unwrap_or("");
    !token.is_empty() && got.trim() == token
}

/// Substrings of an MCP client's self-reported name, and what to call it.
const KNOWN_CLIENTS: &[(&str, &str)] = &[
    ("cursor", "Cursor"),
    ("windsurf", "Windsurf"),
    ("devin", "Windsurf"),
    ("vscode", "VS Code"),
    ("visual studio code", "VS Code"),
    ("codex", "Codex"),
    ("opencode", "OpenCode"),
    ("hermes", "Hermes"),
    ("gemini", "Gemini CLI"),
    ("qwen", "Qwen Code"),
    ("zed", "Zed"),
    ("copilot", "Copilot"),
    ("roo", "Roo Code"),
    ("cline", "Cline"),
    ("kilo", "Kilo Code"),
    ("continue", "Continue"),
    ("goose", "Goose"),
    ("kiro", "Kiro"),
    ("amp", "Amp"),
    ("lm studio", "LM Studio"),
    ("lmstudio", "LM Studio"),
    ("factory", "Factory"),
    ("droid", "Factory"),
    ("augment", "Augment"),
    ("auggie", "Augment"),
    ("jan", "Jan"),
    ("anythingllm", "AnythingLLM"),
];

/// A friendly name for the activity feed.
fn client_label(info: &Value) -> String {
    if let Some(t) = info["title"].as_str().filter(|t| !t.trim().is_empty()) {
        return t.trim().chars().take(40).collect();
    }
    let name = info["name"].as_str().unwrap_or("").trim();
    let l = name.to_ascii_lowercase();
    if l.contains("claude-code") || l.contains("claude code") {
        "Claude Code".into()
    } else if l.starts_with("claude") {
        "Claude".into()
    } else if let Some(&(_, label)) = KNOWN_CLIENTS.iter().find(|(k, _)| {
        // Short names only as whole words ("amp", not "example").
        if k.len() > 4 {
            l.contains(k)
        } else {
            l.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| w == *k)
        }
    }) {
        label.into()
    } else if name.is_empty() {
        "AI app".into()
    } else {
        name.chars().take(40).collect()
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// Who is calling: the session's client, else the bridge's name header.
/// A session id from before an engine restart is simply adopted.
fn caller(st: &AppState, headers: &HeaderMap) -> String {
    let sid = header(headers, SESSION_HEADER).map(str::to_string);
    let hinted = header(headers, CLIENT_HEADER).map(|n| client_label(&json!({ "name": n })));
    st.mcp.with(|m| {
        if let Some(sid) = &sid {
            if let Some(s) = m.sessions.get_mut(sid) {
                s.last_ms = now_ms();
                return s.client.clone();
            }
        }
        let client = hinted.unwrap_or_else(|| "AI app".into());
        if let Some(sid) = sid {
            add_session(m, sid, client.clone());
        }
        client
    })
}

fn add_session(m: &mut Inner, sid: String, client: String) {
    if m.sessions.len() >= SESSIONS_MAX {
        if let Some(oldest) = m
            .sessions
            .iter()
            .min_by_key(|(_, s)| s.last_ms)
            .map(|(k, _)| k.clone())
        {
            m.sessions.remove(&oldest);
        }
    }
    m.sessions.insert(
        sid,
        Session {
            client,
            last_ms: now_ms(),
        },
    );
}

async fn mcp_post(
    axum::extract::State(st): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !origin_ok(&headers) {
        return json_response(
            StatusCode::FORBIDDEN,
            &rpc_err(Value::Null, -32600, "origin not allowed"),
        );
    }
    if !authorized(&st, &headers).await {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &rpc_err(
                Value::Null,
                -32001,
                "missing or wrong token (see the MCP page in DigiClip)",
            ),
        );
    }
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &rpc_err(Value::Null, -32700, format!("parse error: {e}")),
            )
        }
    };
    let mut client = caller(&st, &headers);
    let mut new_session = None;
    let batch = msg.is_array();
    let msgs = match msg {
        Value::Array(v) => v,
        v => vec![v],
    };
    let mut replies = Vec::new();
    for m in msgs {
        if m["method"] == "initialize" {
            client = client_label(&m["params"]["clientInfo"]);
            let sid = secret();
            st.mcp.with(|x| add_session(x, sid.clone(), client.clone()));
            new_session = Some(sid);
            broadcast(&st).await;
        }
        if let Some(r) = rpc(&st, &client, m).await {
            replies.push(r);
        }
    }
    if replies.is_empty() {
        return StatusCode::ACCEPTED.into_response();
    }
    let body = if batch {
        Value::Array(replies)
    } else {
        replies.remove(0)
    };
    let mut resp = json_response(StatusCode::OK, &body);
    if let Some(sid) = new_session.and_then(|s| HeaderValue::from_str(&s).ok()) {
        resp.headers_mut().insert(SESSION_HEADER, sid);
    }
    resp
}

/// No server-to-client stream: every answer comes back on its POST.
async fn mcp_get() -> Response {
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

async fn mcp_delete(
    axum::extract::State(st): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&st, &headers).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(sid) = header(&headers, SESSION_HEADER) {
        st.mcp.with(|m| m.sessions.remove(sid));
    }
    StatusCode::OK.into_response()
}

const INSTRUCTIONS: &str = "DigiClip turns long videos into short captioned clips, all on this \
computer. Typical flow: start_job with a file path or link (optionally a saved preset or \
options), then wait_for_job until it is done, then get_job for the clips (titles, scores, \
file paths). Refine with edit_clip (range, title, caption style, caption word fixes) or \
add_clip over a range found with get_transcript. The DigiClip app shows everything live; \
show_in_app brings a job up there for the user.";

/// One JSON-RPC message → its reply (`None` for notifications and
/// responses).
async fn rpc(st: &Arc<AppState>, client: &str, m: Value) -> Option<Value> {
    let id = m.get("id").cloned();
    let method = m["method"].as_str().unwrap_or("").to_string();
    if method.is_empty() {
        // A response to a request we never send, or junk.
        return id.map(|id| rpc_err(id, -32600, "invalid request"));
    }
    let id = id?; // notifications (`notifications/initialized`…) need no answer
    let params = m.get("params").cloned().unwrap_or(Value::Null);
    Some(match method.as_str() {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = PROTOCOLS
                .iter()
                .find(|v| **v == asked)
                .unwrap_or(&PROTOCOLS[0]);
            rpc_ok(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "digiclip",
                        "title": "DigiClip",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "ping" => rpc_ok(id, json!({})),
        "tools/list" => rpc_ok(id, json!({ "tools": tools() })),
        "resources/list" => rpc_ok(id, json!({ "resources": [] })),
        "resources/templates/list" => rpc_ok(id, json!({ "resourceTemplates": [] })),
        "prompts/list" => rpc_ok(id, json!({ "prompts": [] })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("").to_string();
            if !tools().iter().any(|t| t["name"] == name.as_str()) {
                return Some(rpc_err(id, -32602, format!("unknown tool [{name}]")));
            }
            let args = match params.get("arguments") {
                Some(Value::Object(o)) => Value::Object(o.clone()),
                _ => json!({}),
            };
            rpc_ok(id, call_logged(st, client, &name, args).await)
        }
        _ => rpc_err(id, -32601, format!("method not found: {method}")),
    })
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

async fn call_logged(st: &Arc<AppState>, client: &str, tool: &str, args: Value) -> Value {
    let started = now_ms();
    let seq = st.mcp.with(|m| {
        m.seq += 1;
        let seq = m.seq;
        m.activity.push_back(Activity {
            seq,
            at_ms: started,
            client: client.to_string(),
            tool: tool.to_string(),
            state: "running",
            detail: brief(&args),
            ms: 0,
        });
        while m.activity.len() > ACTIVITY_MAX {
            m.activity.pop_front();
        }
        seq
    });
    broadcast(st).await;
    let res = call_tool(st, client, tool, &args).await;
    st.mcp.with(|m| {
        if let Some(a) = m.activity.iter_mut().find(|a| a.seq == seq) {
            a.ms = now_ms().saturating_sub(started);
            match &res {
                Ok(r) => {
                    a.state = "ok";
                    if let Some(d) = &r.detail {
                        a.detail = d.clone();
                    }
                }
                Err(e) => {
                    a.state = "error";
                    a.detail = e.chars().take(160).collect();
                }
            }
        }
    });
    broadcast(st).await;
    match res {
        Ok(r) => json!({ "content": r.content, "isError": false }),
        Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
    }
}

/// What the feed shows while a call runs.
fn brief(args: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(s) = args["source"].as_str() {
        let name = s.rsplit(['/', '\\']).next().unwrap_or(s);
        parts.push(name.chars().take(60).collect::<String>());
    }
    if let Some(j) = args["job"].as_str() {
        parts.push(j.to_string());
    }
    if let Some(r) = args["rank"].as_u64() {
        parts.push(format!("clip #{r}"));
    }
    if let Some(m) = args["model"].as_str() {
        parts.push(m.to_string());
    }
    parts.join(" · ")
}

struct ToolOut {
    content: Vec<Value>,
    /// Replaces the feed line when set.
    detail: Option<String>,
}

fn text(v: impl Into<String>) -> Value {
    json!({ "type": "text", "text": v.into() })
}

fn out_json(v: &Value) -> ToolOut {
    ToolOut {
        content: vec![text(serde_json::to_string_pretty(v).unwrap_or_default())],
        detail: None,
    }
}

async fn run(st: &Arc<AppState>, cmd: Cmd) -> Result<Value, String> {
    for m in handle_cmd(st.clone(), ClientMsg { id: 0, cmd }).await {
        if let ServerMsg::Res {
            ok, error, data, ..
        } = m
        {
            return if ok {
                Ok(data.unwrap_or(Value::Null))
            } else {
                Err(error.unwrap_or_else(|| "failed".into()))
            };
        }
    }
    Err("no reply".into())
}

fn arg_str(args: &Value, k: &str) -> Result<String, String> {
    args[k]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("`{k}` is required"))
}

fn arg_rank(args: &Value) -> Result<usize, String> {
    args["rank"]
        .as_u64()
        .map(|r| r as usize)
        .ok_or_else(|| "`rank` is required (the clip number, 1 = best)".to_string())
}

fn is_finished(s: JobStatus) -> bool {
    matches!(
        s,
        JobStatus::Done | JobStatus::Failed | JobStatus::Cancelled
    )
}

fn job_summary(r: &JobRecord) -> Value {
    json!({
        "job": r.id,
        "name": r.name,
        "status": r.status,
        "created_ms": r.created_ms,
        "duration_s": r.duration_s,
        "clips": r.clips.len(),
        "clips_rendered": r.clips.iter().filter(|c| c.render_status == "done").count(),
        "error": r.error,
        "source": r.source,
        "url": r.url,
        "origin": r.origin,
    })
}

fn job_detail(r: &JobRecord) -> Value {
    let dir = PathBuf::from(&r.out_dir);
    let abs = |f: &Option<String>| f.as_ref().map(|f| dir.join(f).display().to_string());
    let clips: Vec<Value> = r
        .clips
        .iter()
        .map(|c| {
            json!({
                "rank": c.rank,
                "title": c.title,
                "hook": c.hook,
                "start_s": c.start_s,
                "end_s": c.end_s,
                "length_s": c.tight_dur,
                "style": c.style,
                "score": c.score,
                "why": c.why,
                "scores": c.scores,
                "hashtags": c.hashtags,
                "render_status": c.render_status,
                "render_pct": c.render_pct,
                "files": {
                    "mp4": abs(&c.mp4),
                    "poster": abs(&c.poster),
                    "ass": abs(&c.ass),
                    "srt": abs(&c.srt),
                    "kit": abs(&c.kit),
                },
                "variants": c.variants.iter().map(|v| json!({
                    "aspect": v.aspect,
                    "mp4": dir.join(&v.mp4).display().to_string(),
                    "poster": v.poster.as_ref().map(|p| dir.join(p).display().to_string()),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut v = job_summary(r);
    v["out_dir"] = json!(r.out_dir);
    v["options"] = json!(r.options);
    v["clips"] = json!(clips);
    v
}

async fn record(st: &AppState, job: &str) -> Result<(JobRecord, bool), String> {
    let jobs = st.jobs.lock().await;
    jobs.get(job)
        .map(|l| (l.record.clone(), l.running || l.queued))
        .ok_or_else(|| format!("unknown job [{job}] (list_jobs shows the ids)"))
}

/// Options keys `start_job` takes (every [`JobOptions`] field).
fn option_keys() -> Vec<String> {
    serde_json::to_value(JobOptions::default())
        .ok()
        .and_then(|v| v.as_object().map(|o| o.keys().cloned().collect()))
        .unwrap_or_default()
}

/// A saved preset (by name, any case) with `options` laid over it.
fn build_options(
    presets: &[super::Preset],
    preset: Option<&str>,
    options: &Value,
) -> Result<JobOptions, String> {
    let mut base = match preset {
        Some(name) => {
            let p = presets
                .iter()
                .find(|p| p.name.trim().eq_ignore_ascii_case(name.trim()))
                .ok_or_else(|| {
                    let names: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
                    format!(
                        "no preset named [{name}] (saved: {})",
                        if names.is_empty() {
                            "none".into()
                        } else {
                            names.join(", ")
                        }
                    )
                })?;
            serde_json::to_value(&p.options).map_err(|e| e.to_string())?
        }
        None => json!({}),
    };
    if let Some(o) = options.as_object() {
        let keys = option_keys();
        for (k, v) in o {
            if !keys.contains(k) {
                return Err(format!("unknown option [{k}] (known: {})", keys.join(", ")));
            }
            if !v.is_null() {
                base[k] = v.clone();
            }
        }
    } else if !options.is_null() {
        return Err("`options` must be an object".into());
    }
    serde_json::from_value(base).map_err(|e| format!("bad options: {e}"))
}

/// Words → readable lines: a new line at sentence ends, pauses over a
/// second, or every ~12 s.
fn transcript_text(words: &[Value]) -> String {
    let mut out = String::new();
    let mut line = String::new();
    let mut line_start = 0.0;
    let mut last_end = 0.0;
    for w in words {
        let (t, s, e) = (
            w["w"].as_str().unwrap_or("").trim(),
            w["s"].as_f64().unwrap_or(0.0),
            w["e"].as_f64().unwrap_or(0.0),
        );
        if t.is_empty() {
            continue;
        }
        if !line.is_empty() && (s - last_end > 1.0 || s - line_start > 12.0) {
            out.push_str(&format!("[{line_start:.1}] {line}\n"));
            line.clear();
        }
        if line.is_empty() {
            line_start = s;
        } else {
            line.push(' ');
        }
        line.push_str(t);
        last_end = e;
        if t.ends_with(['.', '!', '?', '…']) && s - line_start > 3.0 {
            out.push_str(&format!("[{line_start:.1}] {line}\n"));
            line.clear();
        }
    }
    if !line.is_empty() {
        out.push_str(&format!("[{line_start:.1}] {line}\n"));
    }
    out
}

const TRANSCRIPT_MAX: usize = 80_000;
const PAGES: [&str; 4] = ["home", "settings", "health", "mcp"];

async fn call_tool(
    st: &Arc<AppState>,
    client: &str,
    tool: &str,
    args: &Value,
) -> Result<ToolOut, String> {
    match tool {
        "get_status" => {
            let jobs: Vec<(JobRecord, bool)> = {
                let jobs = st.jobs.lock().await;
                jobs.values()
                    .map(|l| (l.record.clone(), l.running || l.queued))
                    .collect()
            };
            let mut counts: HashMap<String, usize> = HashMap::new();
            for (r, _) in &jobs {
                let k = serde_json::to_value(r.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                *counts.entry(k).or_default() += 1;
            }
            let active: Vec<Value> = jobs
                .iter()
                .filter(|(r, busy)| *busy || r.status == JobStatus::Downloading)
                .map(|(r, _)| job_summary(r))
                .collect();
            let settings = st.settings.lock().await.clone().public();
            let models = {
                let runs = st.model_runs.lock().await;
                st.models_state(&runs)
            };
            let downloaded: Vec<&String> = models
                .iter()
                .filter(|(_, m)| m.downloaded)
                .map(|(k, _)| k)
                .collect();
            Ok(out_json(&json!({
                "engine_version": env!("CARGO_PKG_VERSION"),
                "health": health().await,
                "jobs_by_status": counts,
                "active_jobs": active,
                "models_downloaded": downloaded,
                "settings": settings,
            })))
        }
        "list_jobs" => {
            let want = args["status"].as_str().map(str::to_string);
            let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 500) as usize;
            let mut recs: Vec<(JobRecord, bool)> = {
                let jobs = st.jobs.lock().await;
                jobs.values()
                    .map(|l| (l.record.clone(), l.running || l.queued))
                    .collect()
            };
            recs.sort_by_key(|(r, _)| std::cmp::Reverse(r.created_ms));
            let total = recs.len();
            let list: Vec<Value> = recs
                .iter()
                .filter(|(r, busy)| match want.as_deref() {
                    None | Some("") | Some("all") => true,
                    Some("active") => *busy || !is_finished(r.status),
                    Some(s) => serde_json::to_value(r.status).is_ok_and(|v| v == s),
                })
                .take(limit)
                .map(|(r, _)| job_summary(r))
                .collect();
            Ok(out_json(&json!({ "total": total, "jobs": list })))
        }
        "get_job" => {
            let (r, busy) = record(st, &arg_str(args, "job")?).await?;
            let mut v = job_detail(&r);
            v["busy"] = json!(busy);
            Ok(out_json(&v))
        }
        "start_job" => {
            let source = arg_str(args, "source")?;
            let presets = st.settings.lock().await.presets.clone();
            let options = build_options(&presets, args["preset"].as_str(), &args["options"])?;
            let origin = Some(format!("mcp:{client}"));
            let data = if crate::fetch::is_url(&source) {
                run(
                    st,
                    Cmd::JobStartUrl {
                        url: source,
                        options,
                        origin,
                    },
                )
                .await?
            } else {
                let p = PathBuf::from(&source);
                if !p.is_file() {
                    return Err(format!(
                        "no such file: {source} (give an absolute path or an http(s) link)"
                    ));
                }
                run(
                    st,
                    Cmd::JobStart {
                        source,
                        options,
                        origin,
                    },
                )
                .await?
            };
            let r: JobRecord =
                serde_json::from_value(data["job"].clone()).map_err(|e| e.to_string())?;
            let mut v = job_summary(&r);
            v["next"] = json!("Call wait_for_job with this job id to follow it.");
            Ok(ToolOut {
                detail: Some(format!("{} · {}", r.name, r.id)),
                ..out_json(&v)
            })
        }
        "wait_for_job" => {
            let job = arg_str(args, "job")?;
            let timeout = args["timeout_s"].as_f64().unwrap_or(50.0).clamp(1.0, 300.0);
            let t0 = std::time::Instant::now();
            let mut rx = st.bus.subscribe();
            loop {
                let (r, busy) = record(st, &job).await?;
                let finished = !busy && is_finished(r.status);
                let waited = t0.elapsed().as_secs_f64();
                if finished || waited >= timeout {
                    let mut v = job_detail(&r);
                    v["finished"] = json!(finished);
                    v["waited_s"] = json!((waited * 10.0).round() / 10.0);
                    if !finished {
                        v["next"] = json!("Still working: call wait_for_job again.");
                    }
                    let status = v["status"].as_str().unwrap_or("").to_string();
                    return Ok(ToolOut {
                        detail: Some(format!("{job} · {status}")),
                        ..out_json(&v)
                    });
                }
                let left = Duration::from_secs_f64((timeout - waited).clamp(0.05, 1.0));
                // Wake on any bus event (or after a second).
                let _ = tokio::time::timeout(left, rx.recv()).await;
            }
        }
        "cancel_job" => {
            let job = arg_str(args, "job")?;
            run(st, Cmd::JobCancel { job }).await?;
            Ok(ToolOut {
                content: vec![text("Cancelling.")],
                detail: None,
            })
        }
        "retry_job" => {
            let job = arg_str(args, "job")?;
            run(st, Cmd::JobRetry { job }).await?;
            Ok(ToolOut {
                content: vec![text("Queued again. Call wait_for_job to follow it.")],
                detail: None,
            })
        }
        "remove_job" => {
            let job = arg_str(args, "job")?;
            run(st, Cmd::JobRemove { job }).await?;
            Ok(ToolOut {
                content: vec![text("Removed (its folder and clips are deleted).")],
                detail: None,
            })
        }
        "get_transcript" => {
            let job = arg_str(args, "job")?;
            let data = run(st, Cmd::TranscriptGet { job }).await?;
            let from = args["start_s"].as_f64().unwrap_or(0.0);
            let to = args["end_s"].as_f64().unwrap_or(f64::MAX);
            let words: Vec<Value> = data["words"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|w| {
                    let s = w["s"].as_f64().unwrap_or(0.0);
                    s >= from && s <= to
                })
                .collect();
            let mut body = if args["format"] == "words" {
                serde_json::to_string(&words).unwrap_or_default()
            } else {
                transcript_text(&words)
            };
            if body.len() > TRANSCRIPT_MAX {
                let mut cut = TRANSCRIPT_MAX;
                while !body.is_char_boundary(cut) {
                    cut -= 1;
                }
                body.truncate(cut);
                body.push_str("\n… (cut: ask for a narrower start_s/end_s window)");
            }
            let head = format!(
                "language: {} · {} words · times are seconds in the source\n",
                data["language"].as_str().unwrap_or("?"),
                words.len()
            );
            Ok(ToolOut {
                content: vec![text(head + &body)],
                detail: None,
            })
        }
        "edit_clip" => {
            let fixes: Vec<crate::redo::Fix> = match args.get("fixes") {
                Some(v) if !v.is_null() => serde_json::from_value(v.clone()).map_err(|e| {
                    format!("bad fixes: {e} (use [{{\"s\": 12.3, \"w\": \"word\"}}])")
                })?,
                _ => vec![],
            };
            run(
                st,
                Cmd::ClipEdit {
                    job: arg_str(args, "job")?,
                    rank: arg_rank(args)?,
                    start_s: args["start_s"].as_f64(),
                    end_s: args["end_s"].as_f64(),
                    title: args["title"].as_str().map(str::to_string),
                    style: args["style"].as_str().map(str::to_string),
                    fixes,
                },
            )
            .await?;
            Ok(ToolOut {
                content: vec![text(
                    "Re-rendering the clip. Call wait_for_job to follow it.",
                )],
                detail: None,
            })
        }
        "add_clip" => {
            let start_s = args["start_s"].as_f64().ok_or("`start_s` is required")?;
            let end_s = args["end_s"].as_f64().ok_or("`end_s` is required")?;
            run(
                st,
                Cmd::ClipAdd {
                    job: arg_str(args, "job")?,
                    start_s,
                    end_s,
                    title: args["title"].as_str().map(str::to_string),
                    style: args["style"].as_str().map(str::to_string),
                },
            )
            .await?;
            Ok(ToolOut {
                content: vec![text(
                    "Rendering the new clip. Call wait_for_job to follow it.",
                )],
                detail: None,
            })
        }
        "get_clip_kit" => {
            let data = run(
                st,
                Cmd::ClipKit {
                    job: arg_str(args, "job")?,
                    rank: arg_rank(args)?,
                },
            )
            .await?;
            Ok(ToolOut {
                content: vec![text(data["text"].as_str().unwrap_or(""))],
                detail: None,
            })
        }
        "get_clip_preview" => {
            let job = arg_str(args, "job")?;
            let rank = arg_rank(args)?;
            let (r, _) = record(st, &job).await?;
            let c = r
                .clips
                .iter()
                .find(|c| c.rank == rank)
                .ok_or_else(|| format!("no clip #{rank} in this job"))?;
            let poster = c
                .poster
                .as_ref()
                .map(|p| PathBuf::from(&r.out_dir).join(p))
                .filter(|p| p.is_file())
                .ok_or("this clip has no poster yet (still rendering?)")?;
            let bytes = std::fs::read(&poster).map_err(|e| e.to_string())?;
            let mime = if poster
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("png"))
            {
                "image/png"
            } else {
                "image/jpeg"
            };
            use base64::Engine;
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            Ok(ToolOut {
                content: vec![
                    json!({ "type": "image", "data": data, "mimeType": mime }),
                    text(format!(
                        "#{} \"{}\" {:.1}–{:.1} s · {}",
                        c.rank,
                        c.title,
                        c.start_s,
                        c.end_s,
                        c.mp4
                            .as_ref()
                            .map(|m| PathBuf::from(&r.out_dir).join(m).display().to_string())
                            .unwrap_or_else(|| "not rendered yet".into())
                    )),
                ],
                detail: None,
            })
        }
        "get_settings" => Ok(out_json(&run(st, Cmd::SettingsGet).await?)),
        "update_settings" => {
            let mut patch = args["patch"].clone();
            let Some(obj) = patch.as_object_mut() else {
                return Err("`patch` must be an object".into());
            };
            // The MCP switch belongs to the user (and would cut this call).
            for k in ["mcp_on", "mcp_port", "mcp_token"] {
                obj.remove(k);
            }
            Ok(out_json(&run(st, Cmd::SettingsSet { patch }).await?))
        }
        "list_models" => Ok(out_json(&run(st, Cmd::ModelsState).await?)),
        "download_model" => {
            run(
                st,
                Cmd::ModelsDownload {
                    id: arg_str(args, "model")?,
                },
            )
            .await?;
            Ok(ToolOut {
                content: vec![text("Downloading; list_models shows the progress.")],
                detail: None,
            })
        }
        "delete_model" => {
            run(
                st,
                Cmd::ModelsDelete {
                    id: arg_str(args, "model")?,
                },
            )
            .await?;
            Ok(ToolOut {
                content: vec![text("Deleted.")],
                detail: None,
            })
        }
        "list_ai_models" => Ok(out_json(
            &run(
                st,
                Cmd::AiModels {
                    provider: args["provider"].as_str().map(str::to_string),
                    refresh: args["refresh"].as_bool().unwrap_or(false),
                },
            )
            .await?,
        )),
        "list_openrouter_models" => Ok(out_json(
            &run(
                st,
                Cmd::OrModels {
                    refresh: args["refresh"].as_bool().unwrap_or(false),
                },
            )
            .await?,
        )),
        "export_diagnostics" => Ok(out_json(&run(st, Cmd::Diagnostics).await?)),
        "show_in_app" => {
            let job = args["job"].as_str().map(str::to_string);
            if let Some(j) = &job {
                record(st, j).await?;
            }
            let page = match args["page"].as_str() {
                Some(p) if PAGES.contains(&p) => Some(p.to_string()),
                Some(p) => return Err(format!("unknown page [{p}] ({})", PAGES.join(", "))),
                None => None,
            };
            let _ = st.bus.send(ServerMsg::Ev {
                ev: Event::McpFocus { job, page },
            });
            Ok(ToolOut {
                content: vec![text("Shown in the DigiClip window.")],
                detail: None,
            })
        }
        other => Err(format!("unknown tool [{other}]")),
    }
}

fn options_schema() -> Value {
    json!({
        "type": "object",
        "description": "Job knobs; anything left out uses the saved settings. Unknown keys are refused.",
        "properties": {
            "mode": { "type": "string", "enum": ["clips", "full"], "description": "clips = short highlights (default); full = the whole video with burned-in subtitles." },
            "kind": { "type": "string", "enum": ["smart", "complete", "moments", "timecut"], "description": "How clips are picked (default smart)." },
            "count": { "type": "integer", "minimum": 0, "maximum": 10, "description": "Clips to make; 0 = as many as clear the bar." },
            "min_len": { "type": "number", "description": "Shortest clip, seconds (default 15)." },
            "max_len": { "type": "number", "description": "Longest clip, seconds (default 90)." },
            "focus": { "type": "string", "description": "Topic to steer picking toward, e.g. \"pricing, AI agents\"." },
            "style": { "type": "string", "enum": ["tiktok", "karaoke", "hormozi", "minimal", "beast", "neon", "highlight", "ghost"], "description": "Caption style." },
            "caption_anim": { "type": "string", "enum": ["pop", "words", "none"] },
            "look": { "type": "object", "description": "How clips look. Today only look.captions applies: x, y (0..1, centre of the captions), size (0.5..2), font (Anton, Archivo Black, Inter Medium, JetBrains Mono), case (upper, asis), color, active, accent, outline (#RRGGBB), outline_w, shadow, box (#RRGGBB, or none), box_opacity, max_words (1..8), anim (pop, words, none, fade, slide, bounce). Unset fields keep the style's look." },
            "aspect": { "type": "string", "description": "Canvas(es): 9:16 (default), 4:5, 1:1, 16:9; comma-separate for extra versions, e.g. \"9:16,1:1\"." },
            "framing": { "type": "string", "enum": ["smart", "center"], "description": "smart follows faces." },
            "layout": { "type": "string", "enum": ["auto", "single", "split"], "description": "Two-person split screen." },
            "tighten": { "type": "string", "enum": ["off", "light", "punchy"], "description": "Cut pauses (punchy also cuts filler words)." },
            "punch": { "type": "boolean", "description": "Zoom punch-ins on emphasis." },
            "lang": { "type": "string", "description": "Spoken language: whisper code (en, es…) or auto." },
            "subs_lang": { "type": "string", "description": "Translate captions into this language code (absent/off = spoken language)." },
            "model": { "type": "string", "description": "Whisper model id (see list_models)." },
            "decider": { "type": "string", "enum": ["auto", "jev", "laya", "off"], "description": "System One judge." },
            "headline": { "type": "string", "description": "\"\" = each clip's title on screen; text = that text." },
            "progress_bar": { "type": "string", "description": "Progress bar color #RRGGBB." },
            "logo": { "type": "string", "description": "Logo image path." },
            "logo_pos": { "type": "string", "enum": ["tl", "tr", "bl", "br"] },
            "music": { "type": "string", "description": "Background music file path." },
            "music_db": { "type": "number", "description": "Music level in dB relative to speech." },
            "merge": { "type": "string", "description": "\"\" = one compilation of the picks; \"12.4-28.1,40-55\" = join these ranges." },
            "span": { "type": "string", "description": "timecut only: START-END seconds." },
            "timecut_len": { "type": "number", "description": "timecut part length, seconds." },
            "kit": { "type": "boolean", "description": "Write a posting kit (captions, hashtags) per clip." },
            "gpu": { "type": "boolean" }
        },
        "additionalProperties": true
    })
}

/// Every tool with its schema and hints.
fn tools() -> Vec<Value> {
    let job = json!({ "type": "string", "description": "Job id (from list_jobs or start_job)." });
    let rank = json!({ "type": "integer", "minimum": 1, "description": "Clip number in the job (1 = best)." });
    let read = json!({ "readOnlyHint": true, "openWorldHint": false });
    let write = json!({ "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false });
    let t = |name: &str,
             title: &str,
             description: &str,
             props: Value,
             required: &[&str],
             ann: &Value| {
        let mut a = ann.clone();
        a["title"] = json!(title);
        json!({
            "name": name,
            "title": title,
            "description": description,
            "inputSchema": { "type": "object", "properties": props, "required": required },
            "annotations": a,
        })
    };
    vec![
        t("get_status", "Status", "Engine version and health, jobs by status, what is running now, and the main settings.", json!({}), &[], &read),
        t("list_jobs", "List jobs", "Jobs, newest first. Filter by status (active, done, failed, cancelled, queued, downloading, transcribing…).", json!({
            "status": { "type": "string" },
            "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Default 20." },
        }), &[], &read),
        t("get_job", "Job details", "One job with every clip: title, hook, range, scores, why it was picked, hashtags, render state and absolute file paths (mp4, poster, captions, kit, extra aspects).", json!({ "job": job }), &["job"], &read),
        t("start_job", "Make clips", "Start a job from a local video/audio file (absolute path) or a link (YouTube, TikTok… downloaded first). Optionally apply a saved preset by name and/or override options. The app shows it in its queue.", json!({
            "source": { "type": "string", "description": "Absolute file path or http(s) link." },
            "preset": { "type": "string", "description": "Name of a saved preset (see get_settings)." },
            "options": options_schema(),
        }), &["source"], &json!({ "readOnlyHint": false, "destructiveHint": false, "openWorldHint": true })),
        t("wait_for_job", "Wait for a job", "Wait until the job finishes (done, failed or cancelled) or the timeout passes, then return its details. Call again while `finished` is false.", json!({
            "job": job,
            "timeout_s": { "type": "number", "minimum": 1, "maximum": 300, "description": "Default 50." },
        }), &["job"], &read),
        t("cancel_job", "Cancel a job", "Stop a queued or running job.", json!({ "job": job }), &["job"], &json!({ "readOnlyHint": false, "destructiveHint": true, "openWorldHint": false })),
        t("retry_job", "Retry a job", "Run a failed, cancelled or finished job again with the same options.", json!({ "job": job }), &["job"], &write),
        t("remove_job", "Remove a job", "Delete a job and its output folder (clips included). Cannot be undone.", json!({ "job": job }), &["job"], &json!({ "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false })),
        t("get_transcript", "Transcript", "The job's transcript with source timestamps, as readable lines (default) or word timings. Narrow it with start_s/end_s on long videos.", json!({
            "job": job,
            "start_s": { "type": "number" },
            "end_s": { "type": "number" },
            "format": { "type": "string", "enum": ["text", "words"] },
        }), &["job"], &read),
        t("edit_clip", "Edit a clip", "Re-render one clip with a new range (source seconds), title, caption style and/or caption word fixes ({s: word start in seconds, w: new text; \"\" drops the word}).", json!({
            "job": job,
            "rank": rank,
            "start_s": { "type": "number" },
            "end_s": { "type": "number" },
            "title": { "type": "string" },
            "style": { "type": "string", "enum": ["tiktok", "karaoke", "hormozi", "minimal", "beast", "neon", "highlight", "ghost"] },
            "fixes": { "type": "array", "items": { "type": "object", "properties": { "s": { "type": "number" }, "w": { "type": "string" } }, "required": ["s", "w"] } },
        }), &["job", "rank"], &write),
        t("add_clip", "Add a clip", "Render a new clip over an exact range of the source (seconds), e.g. a moment found in get_transcript.", json!({
            "job": job,
            "start_s": { "type": "number" },
            "end_s": { "type": "number" },
            "title": { "type": "string" },
            "style": { "type": "string" },
        }), &["job", "start_s", "end_s"], &write),
        t("get_clip_kit", "Posting kit", "The clip's posting kit: caption text, hashtags and posting notes.", json!({ "job": job, "rank": rank }), &["job", "rank"], &read),
        t("get_clip_preview", "Clip preview", "The clip's poster frame as an image, plus its video path.", json!({ "job": job, "rank": rank }), &["job", "rank"], &read),
        t("get_settings", "Settings", "Saved settings and presets (API keys only show as set/not set).", json!({}), &[], &read),
        t("update_settings", "Change settings", "Change settings with a partial patch, e.g. {\"clips_count\": 5, \"caption_default\": \"hormozi\", \"presets\": [...], \"watch_dir\": \"D:/videos\", \"watch_on\": true}. Clip AI: {\"ai_provider\": \"openai\", \"ai_keys\": {\"openai\": \"sk-...\"}, \"ai_models\": {\"openai\": \"gpt-5-mini\"}, \"ai_base_urls\": {\"ollama\": \"http://localhost:11434/v1\"}} (see list_ai_models for the provider ids; ai_base_urls only for ollama, lm_studio and custom). Keys (ai_keys, openrouter_key, jev_key) are write-only; null clears them.", json!({
            "patch": { "type": "object" },
        }), &["patch"], &write),
        t("list_models", "Models", "Speech models and the Laya judge: size, downloaded, download progress.", json!({}), &[], &read),
        t("download_model", "Download a model", "Download a model by id (see list_models).", json!({ "model": { "type": "string" } }), &["model"], &json!({ "readOnlyHint": false, "destructiveHint": false, "openWorldHint": true })),
        t("delete_model", "Delete a model", "Delete a downloaded model by id.", json!({ "model": { "type": "string" } }), &["model"], &json!({ "readOnlyHint": false, "destructiveHint": true, "openWorldHint": false })),
        t("list_ai_models", "Clip AI models", "Models a Clip AI provider offers for clip scoring (fetched live, cached 24h). Provider ids: openrouter, openai, anthropic, gemini, ollama_cloud, ollama, lm_studio, groq, mistral, deepseek, xai, together, fireworks, cerebras, custom. Defaults to the active provider (see get_settings: ai_provider, ai_providers).", json!({ "provider": { "type": "string" }, "refresh": { "type": "boolean" } }), &[], &json!({ "readOnlyHint": false, "idempotentHint": true, "openWorldHint": true })),
        t("list_openrouter_models", "OpenRouter models", "Models available for clip scoring via OpenRouter (same as list_ai_models with provider openrouter).", json!({ "refresh": { "type": "boolean" } }), &[], &json!({ "readOnlyHint": true, "openWorldHint": true })),
        t("export_diagnostics", "Export diagnostics", "Write a diagnostics bundle (keys redacted) and return its path.", json!({}), &[], &write),
        t("show_in_app", "Show in DigiClip", "Bring the DigiClip window up on a job and/or a page (home, settings, health, mcp).", json!({
            "job": job,
            "page": { "type": "string", "enum": PAGES },
        }), &[], &write),
    ]
}

// ---------------------------------------------------------------------------
// stdio bridge (`digiclip --mcp`)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct ServerFile {
    on: bool,
    url: String,
    token: String,
    app: Option<String>,
}

const NOT_RUNNING: &str =
    "DigiClip isn't running. Open the DigiClip app (MCP stays on in its MCP page) and try again.";
const TURNED_OFF: &str =
    "The MCP server is turned off in DigiClip. Turn it on in the app's MCP page.";

/// Where `server.json` lives: next to this bridge copy, else under the
/// data dir.
fn bridge_dir(data_dir: Option<PathBuf>) -> PathBuf {
    if let Some(d) = data_dir {
        return d.join(DIR);
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        if dir.join(SERVER_FILE).is_file() {
            return dir;
        }
    }
    crate::provision::root().join(DIR)
}

pub fn run_bridge(data_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let dir = bridge_dir(data_dir);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(bridge(dir))
}

struct Bridge {
    dir: PathBuf,
    http: reqwest::Client,
    session: tokio::sync::Mutex<Option<String>>,
    client: tokio::sync::Mutex<Option<String>>,
    launched: tokio::sync::Mutex<Option<std::time::Instant>>,
    out: tokio::sync::Mutex<tokio::io::Stdout>,
}

async fn bridge(dir: PathBuf) -> anyhow::Result<()> {
    use tokio::io::AsyncBufReadExt;
    let b = Arc::new(Bridge {
        dir,
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(330))
            .no_proxy()
            .build()?,
        session: Default::default(),
        client: Default::default(),
        launched: Default::default(),
        out: tokio::sync::Mutex::new(tokio::io::stdout()),
    });
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut tasks = tokio::task::JoinSet::new();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                b.write(&rpc_err(Value::Null, -32700, format!("parse error: {e}")))
                    .await;
                continue;
            }
        };
        if msg["method"] == "initialize" {
            let name = msg["params"]["clientInfo"]["name"]
                .as_str()
                .map(str::to_string);
            *b.client.lock().await = name;
            // Everything after needs the session: answer this one first.
            b.forward(msg).await;
        } else {
            let b2 = b.clone();
            tasks.spawn(async move { b2.forward(msg).await });
        }
        while tasks.try_join_next().is_some() {}
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}

impl Bridge {
    async fn write(&self, v: &Value) {
        use tokio::io::AsyncWriteExt;
        let mut out = self.out.lock().await;
        let _ = out.write_all(format!("{v}\n").as_bytes()).await;
        let _ = out.flush().await;
    }

    async fn forward(&self, msg: Value) {
        let id = msg.get("id").cloned();
        let is_request = id.is_some() && msg.get("method").is_some();
        match self.post(&msg).await {
            Ok(Some(reply)) if is_request => self.write(&reply).await,
            Ok(_) => {}
            Err(e) => {
                if let (true, Some(id)) = (is_request, id) {
                    self.write(&rpc_err(id, -32000, e)).await;
                }
            }
        }
    }

    fn server(&self) -> Option<ServerFile> {
        let txt = std::fs::read_to_string(self.dir.join(SERVER_FILE)).ok()?;
        serde_json::from_str(&txt).ok()
    }

    async fn post(&self, msg: &Value) -> Result<Option<Value>, String> {
        let mut deadline: Option<std::time::Instant> = None;
        loop {
            let srv = self.server();
            if srv.as_ref().is_some_and(|s| !s.on) {
                return Err(TURNED_OFF.into());
            }
            if let Some(s) = srv.as_ref().filter(|s| !s.url.is_empty()) {
                let mut req = self
                    .http
                    .post(&s.url)
                    .bearer_auth(&s.token)
                    .header("accept", "application/json, text/event-stream")
                    .json(msg);
                if let Some(sid) = self.session.lock().await.clone() {
                    req = req.header(SESSION_HEADER, sid);
                }
                if let Some(c) = self.client.lock().await.clone() {
                    req = req.header(CLIENT_HEADER, c);
                }
                match req.send().await {
                    Ok(resp) => {
                        if let Some(sid) = resp
                            .headers()
                            .get(SESSION_HEADER)
                            .and_then(|v| v.to_str().ok())
                        {
                            *self.session.lock().await = Some(sid.to_string());
                        }
                        let status = resp.status();
                        if status == reqwest::StatusCode::ACCEPTED {
                            return Ok(None);
                        }
                        if status == reqwest::StatusCode::UNAUTHORIZED {
                            return Err(
                                "DigiClip refused the token; open its MCP page once and try again."
                                    .into(),
                            );
                        }
                        let body = resp.text().await.map_err(|e| e.to_string())?;
                        return match serde_json::from_str::<Value>(&body) {
                            Ok(v) => Ok(Some(v)),
                            Err(_) if !status.is_success() => {
                                Err(format!("DigiClip answered {status}"))
                            }
                            Err(e) => Err(format!("bad reply from DigiClip: {e}")),
                        };
                    }
                    Err(e) if e.is_connect() => {}
                    Err(e) => return Err(format!("couldn't reach DigiClip: {e}")),
                }
            }
            // Nobody home: start the app (hidden) once, then wait for it.
            let deadline = *deadline
                .get_or_insert_with(|| std::time::Instant::now() + Duration::from_secs(30));
            self.launch(srv.as_ref().and_then(|s| s.app.clone()))
                .await?;
            if std::time::Instant::now() >= deadline {
                return Err(NOT_RUNNING.into());
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
    }

    /// Start the app hidden, at most once a minute.
    async fn launch(&self, app: Option<String>) -> Result<(), String> {
        let Some(app) = app.filter(|a| Path::new(a).is_file()) else {
            return Err(NOT_RUNNING.into());
        };
        let mut last = self.launched.lock().await;
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return Ok(());
        }
        *last = Some(std::time::Instant::now());
        tracing::info!("starting DigiClip: {app}");
        spawn_detached(&app).map_err(|e| format!("couldn't start DigiClip ({e}). {NOT_RUNNING}"))
    }
}

/// The app outlives this bridge: no inherited pipes, and on Windows out
/// of the AI app's job object when it allows that.
fn spawn_detached(app: &str) -> std::io::Result<()> {
    let base = || {
        let mut c = std::process::Command::new(app);
        c.arg("--hidden")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        c
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        if base()
            .creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
            .is_ok()
        {
            return Ok(());
        }
        base().creation_flags(flags).spawn().map(|_| ())
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        base().process_group(0).spawn().map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(dir: &Path) -> Arc<AppState> {
        let (bus, _) = tokio::sync::broadcast::channel(64);
        let s = super::super::Settings {
            mcp_token: Some("t0k3n-t0k3n-t0k3n".into()),
            ..Default::default()
        };
        Arc::new(AppState {
            token: "ui".into(),
            data_dir: dir.to_path_buf(),
            jobs: Default::default(),
            settings: tokio::sync::Mutex::new(s),
            model_runs: Default::default(),
            worker: tokio::sync::Semaphore::new(1),
            bus,
            id_counter: std::sync::atomic::AtomicU64::new(1),
            mcp: Mcp::default(),
        })
    }

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("digiclip-mcp-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn handshake_and_tool_list() {
        let dir = scratch("rpc");
        let st = state(&dir);
        let init = rpc(
            &st,
            "Claude",
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2025-03-26", "clientInfo": { "name": "claude-ai" } } }),
        )
        .await
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "digiclip");
        // Unknown versions get our newest.
        let init = rpc(
            &st,
            "x",
            json!({ "id": 2, "method": "initialize", "params": { "protocolVersion": "1999" } }),
        )
        .await
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], PROTOCOLS[0]);
        // Notifications get no reply.
        assert!(
            rpc(&st, "x", json!({ "method": "notifications/initialized" }))
                .await
                .is_none()
        );
        let list = rpc(&st, "x", json!({ "id": 3, "method": "tools/list" }))
            .await
            .unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        assert!(tools.len() >= 20);
        assert!(tools.iter().any(|t| t["name"] == "list_ai_models"));
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        }
        let bad = rpc(&st, "x", json!({ "id": 4, "method": "nope" }))
            .await
            .unwrap();
        assert_eq!(bad["error"]["code"], -32601);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tool_calls_log_activity_and_report_errors() {
        let dir = scratch("call");
        let st = state(&dir);
        let r = rpc(
            &st,
            "Claude",
            json!({ "id": 1, "method": "tools/call", "params": { "name": "list_jobs", "arguments": {} } }),
        )
        .await
        .unwrap();
        assert_eq!(r["result"]["isError"], false);
        let r = rpc(
            &st,
            "Claude",
            json!({ "id": 2, "method": "tools/call", "params": { "name": "get_job", "arguments": { "job": "nope" } } }),
        )
        .await
        .unwrap();
        assert_eq!(r["result"]["isError"], true);
        let r = rpc(
            &st,
            "Claude",
            json!({ "id": 3, "method": "tools/call", "params": { "name": "start_job", "arguments": { "source": "Z:/missing.mp4" } } }),
        )
        .await
        .unwrap();
        assert_eq!(r["result"]["isError"], true);
        let p = public(&st).await;
        let feed = p["activity"].as_array().unwrap();
        assert_eq!(feed.len(), 3);
        assert_eq!(feed[0]["tool"], "start_job");
        assert_eq!(feed[0]["state"], "error");
        assert_eq!(feed[2]["state"], "ok");
        assert_eq!(feed[2]["client"], "Claude");
        for (name, want) in [
            ("codex-mcp-client", "Codex"),
            ("amp", "Amp"),
            ("example-client", "example-client"),
            ("Zed", "Zed"),
            ("authorized-thing", "authorized-thing"),
        ] {
            assert_eq!(client_label(&json!({ "name": name })), want);
        }
        // MCP can't switch itself off.
        let r = rpc(
            &st,
            "Claude",
            json!({ "id": 4, "method": "tools/call", "params": { "name": "update_settings", "arguments": { "patch": { "mcp_on": false, "clips_count": 4 } } } }),
        )
        .await
        .unwrap();
        assert_eq!(r["result"]["isError"], false);
        let s = st.settings.lock().await;
        assert!(s.mcp_on);
        assert_eq!(s.clips_count, 4);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn options_merge_over_presets() {
        let presets: Vec<super::super::Preset> = serde_json::from_value(json!([
            { "name": "Podcast", "options": { "count": 5, "style": "hormozi" } }
        ]))
        .unwrap();
        let o = build_options(&presets, Some("podcast"), &json!({ "style": "neon" })).unwrap();
        assert_eq!(o.count, Some(5));
        assert_eq!(o.style.as_deref(), Some("neon"));
        assert!(build_options(&presets, Some("vlog"), &Value::Null).is_err());
        assert!(build_options(&presets, None, &json!({ "bogus": 1 })).is_err());
    }

    #[test]
    fn config_merge_keeps_the_rest() {
        let dir = scratch("cfg");
        let p = dir.join("claude_desktop_config.json");
        std::fs::write(
            &p,
            r#"{"globalShortcut":"x","mcpServers":{"other":{"command":"o"}}}"#,
        )
        .unwrap();
        let e = apps::json_entry(apps::Shape::Std, "C:/b/digiclip-mcp.exe");
        let app = apps::app("claude_desktop").unwrap();
        apps::json_edit(&p, "mcpServers", Some(&e)).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["globalShortcut"], "x");
        assert_eq!(v["mcpServers"]["other"]["command"], "o");
        assert_eq!(v["mcpServers"]["digiclip"]["args"][0], "--mcp");
        assert_eq!(
            apps::state(app, std::slice::from_ref(&p), Some("C:/b/digiclip-mcp.exe")),
            (true, true)
        );
        apps::json_edit(&p, "mcpServers", None).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(v["mcpServers"].get("digiclip").is_none());
        // Broken JSON is never overwritten.
        std::fs::write(&p, "{ nope").unwrap();
        assert!(apps::json_edit(&p, "mcpServers", Some(&e)).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ nope");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn origins_and_labels() {
        let mut h = HeaderMap::new();
        assert!(origin_ok(&h));
        for (o, want) in [
            ("http://localhost:5173", true),
            ("http://127.0.0.1", true),
            ("http://[::1]:80", true),
            ("https://evil.com", false),
            ("http://localhost.evil.com", false),
            ("null", false),
        ] {
            h.insert("origin", HeaderValue::from_str(o).unwrap());
            assert_eq!(origin_ok(&h), want, "{o}");
        }
        assert_eq!(client_label(&json!({ "name": "claude-ai" })), "Claude");
        assert_eq!(
            client_label(&json!({ "name": "claude-code" })),
            "Claude Code"
        );
        assert_eq!(
            client_label(&json!({ "name": "x", "title": "My Agent" })),
            "My Agent"
        );
        assert_eq!(client_label(&Value::Null), "AI app");
    }

    #[test]
    fn transcript_lines() {
        let w = |t: &str, s: f64| json!({ "w": t, "s": s, "e": s + 0.3 });
        let txt = transcript_text(&[
            w("Hello", 0.0),
            w("there.", 0.4),
            w("New", 5.0),
            w("line", 5.4),
        ]);
        assert_eq!(txt, "[0.0] Hello there.\n[5.0] New line\n");
    }

    #[test]
    fn secrets_differ() {
        let (a, b) = (secret(), secret());
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }
}
