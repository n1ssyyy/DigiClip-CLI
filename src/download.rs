//! One download path for everything the engine fetches (ffmpeg, the speech
//! models, Laya, the face model, yt-dlp), built for a connection that drops.
//!
//! What a download does here:
//! - writes to `<file>.part` and renames it into place only when it is whole,
//!   so nothing ever reads a half file;
//! - carries on from the bytes already on disk after any failure (`Range`,
//!   guarded by `If-Range` with the validator the first answer gave, so a
//!   file that changed on the server starts over instead of being spliced);
//! - tries again by itself, waiting a little longer each time, and only gives
//!   up after [`Policy::tries`] failures in a row that added no bytes;
//! - treats silence as a failure: no answer or no bytes for
//!   [`Policy::stall`] ends the attempt (it does not hang);
//! - checks the result: the size the server announced, a minimum size, and
//!   the SHA-256 when the host names one (Hugging Face's `X-Linked-ETag`);
//! - runs once per file: a second caller waits for the first and finds the
//!   file there;
//! - stops within a moment when the job that asked is cancelled.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue};
use tokio::io::AsyncWriteExt;

use crate::progress::CancelFlag;

/// What a download is waiting on: `Some(why)` while it waits to try again,
/// `None` when bytes flow.
pub type StatusFn<'a> = dyn Fn(Option<&str>) + Send + Sync + 'a;
/// `(bytes on disk, bytes in all)`; the total is 0 when unknown.
pub type BytesFn<'a> = dyn Fn(u64, u64) + Send + Sync + 'a;

/// Who listens to a download. All optional.
#[derive(Clone, Copy, Default)]
pub struct Hooks<'a> {
    pub progress: Option<&'a BytesFn<'a>>,
    pub cancel: Option<&'a CancelFlag>,
    pub status: Option<&'a StatusFn<'a>>,
}

impl<'a> Hooks<'a> {
    pub fn progress(progress: Option<&'a BytesFn<'a>>) -> Self {
        Self {
            progress,
            ..Self::default()
        }
    }

    pub fn with_cancel(mut self, cancel: &'a CancelFlag) -> Self {
        self.cancel = Some(cancel);
        self
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.is_cancelled())
    }

    fn say(&self, why: Option<&str>) {
        if let Some(s) = self.status {
            s(why);
        }
    }
}

/// How patient a download is.
#[derive(Clone, Debug)]
pub struct Policy {
    /// No answer, or no bytes, for this long ends the attempt.
    pub stall: Duration,
    /// Failures in a row that added no bytes before giving up.
    pub tries: u32,
    /// The wait before the second attempt; it doubles up to `backoff_cap`.
    pub backoff: Duration,
    pub backoff_cap: Duration,
    /// A connection slower than this (bytes a second, measured over
    /// `crawl_window`) is dropped for a new one that carries on from the
    /// same byte: some connections crawl while the next one flies. 0 = never.
    pub crawl_floor: u64,
    pub crawl_window: Duration,
    /// Slow connections in a row before the line itself is taken to be slow
    /// and the download is left to finish at its own pace.
    pub crawl_tries: u32,
    /// Not worth a new connection when this little time is left.
    pub crawl_min_left: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            stall: Duration::from_secs(40),
            tries: 10,
            backoff: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(30),
            crawl_floor: 200 * 1024,
            crawl_window: Duration::from_secs(12),
            crawl_tries: 3,
            crawl_min_left: Duration::from_secs(45),
        }
    }
}

/// Watches the speed of the connection in use (see [`Policy::crawl_floor`]).
struct Crawl {
    /// Slow connections that may still be swapped for a new one.
    left: u32,
    since: Instant,
    at: u64,
}

impl Crawl {
    fn new(pol: &Policy) -> Self {
        Self {
            left: pol.crawl_tries,
            since: Instant::now(),
            at: 0,
        }
    }

    /// A connection starts delivering from byte `done`.
    fn begin(&mut self, done: u64) {
        self.since = Instant::now();
        self.at = done;
    }

    /// Call per chunk. `true`: this connection crawls, try another.
    fn too_slow(&mut self, done: u64, total: Option<u64>, pol: &Policy) -> bool {
        let took = self.since.elapsed();
        if pol.crawl_floor == 0 || took < pol.crawl_window {
            return false;
        }
        let speed = ((done - self.at) as f64 / took.as_secs_f64()) as u64;
        self.begin(done);
        if speed >= pol.crawl_floor {
            // A good connection: a later slow one gets its chances again.
            self.left = pol.crawl_tries;
            return false;
        }
        let secs_left = total
            .map(|t| t.saturating_sub(done) / speed.max(1))
            .unwrap_or(u64::MAX);
        if self.left == 0 || secs_left < pol.crawl_min_left.as_secs() {
            return false;
        }
        self.left -= 1;
        true
    }
}

const TICK: Duration = Duration::from_millis(200);
const CANCELLED: &str = "cancelled by user";

/// Fetch `url` into `dest`. `min_bytes` is the smallest size a real copy of
/// the file has: a `dest` already that big is left alone, and a download that
/// ends smaller is thrown away (an error page, not the file).
pub async fn fetch(
    url: &str,
    dest: &Path,
    label: &str,
    min_bytes: u64,
    hooks: Hooks<'_>,
) -> anyhow::Result<()> {
    fetch_with(url, dest, label, min_bytes, hooks, &Policy::default()).await
}

pub async fn fetch_with(
    url: &str,
    dest: &Path,
    label: &str,
    min_bytes: u64,
    hooks: Hooks<'_>,
    pol: &Policy,
) -> anyhow::Result<()> {
    let _one = lock(dest, hooks.cancel).await?;
    if is_whole(dest, min_bytes) {
        return Ok(());
    }
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p)?;
    }
    let part = part_path(dest);
    adopt_legacy_part(dest, &part);

    let mut fails = 0u32;
    let mut restarts = 0u32;
    let mut crawl = Crawl::new(pol);
    loop {
        if hooks.cancelled() {
            anyhow::bail!(CANCELLED);
        }
        let before = len(&part);
        let step = match attempt(url, &part, label, &hooks, pol, &mut crawl).await {
            Ok(sha) => finish(&part, dest, min_bytes, sha).await,
            Err(f) => Err(f),
        };
        match step {
            Ok(()) => {
                hooks.say(None);
                return Ok(());
            }
            Err(Fail::Cancelled) => anyhow::bail!(CANCELLED),
            Err(Fail::Fatal(e)) => return Err(e),
            Err(Fail::Restart(why)) => {
                forget(&part);
                restarts += 1;
                if restarts > 2 {
                    anyhow::bail!("{label} download failed: {why}");
                }
                tracing::warn!("{label}: {why} — starting the download over");
            }
            Err(Fail::Retry { why, wait }) => {
                // An attempt that added bytes is progress, not a failure.
                fails = if len(&part) > before { 0 } else { fails + 1 };
                if fails >= pol.tries {
                    anyhow::bail!(
                        "{label} download failed: {why}. What was fetched is kept, and the next try carries on from there."
                    );
                }
                let pause = wait.unwrap_or_else(|| backoff(pol, fails.max(1)));
                tracing::warn!(
                    "{label}: {why} — trying again in {:.0}s ({} of {} bytes so far)",
                    pause.as_secs_f64().ceil(),
                    len(&part),
                    Meta::read(&part).total.unwrap_or(0)
                );
                hooks.say(Some(&why));
                nap(pause, &hooks).await?;
            }
        }
    }
}

/// The lock of one download target: whoever holds it is the only one
/// fetching (or unpacking) `key`. Waiting ends when `cancel` is set.
pub async fn lock(
    key: &Path,
    cancel: Option<&CancelFlag>,
) -> anyhow::Result<tokio::sync::OwnedMutexGuard<()>> {
    type Locks = Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>;
    static LOCKS: OnceLock<Locks> = OnceLock::new();
    let m = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key.to_path_buf())
        .or_default()
        .clone();
    loop {
        if let Ok(g) = m.clone().try_lock_owned() {
            return Ok(g);
        }
        if cancel.is_some_and(|c| c.is_cancelled()) {
            anyhow::bail!(CANCELLED);
        }
        tokio::time::sleep(TICK).await;
    }
}

/// `<file>.part`, beside the file (the whole name is kept, so `a.onnx` and
/// `a.onnx.data` never share one).
pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

fn meta_path(part: &Path) -> PathBuf {
    let mut name = part.file_name().unwrap_or_default().to_os_string();
    name.push(".meta");
    part.with_file_name(name)
}

/// Releases before 2.9 named the partial file by swapping the extension
/// (`laya.onnx.data` → `laya.onnx.part`). Carry on from it.
fn adopt_legacy_part(dest: &Path, part: &Path) {
    let old = dest.with_extension("part");
    if old != part && old.is_file() && !part.exists() {
        let _ = std::fs::rename(&old, part);
    }
}

fn is_whole(dest: &Path, min_bytes: u64) -> bool {
    min_bytes > 0 && std::fs::metadata(dest).is_ok_and(|m| m.is_file() && m.len() >= min_bytes)
}

fn len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn forget(part: &Path) {
    let _ = std::fs::remove_file(part);
    let _ = std::fs::remove_file(meta_path(part));
}

fn backoff(pol: &Policy, fails: u32) -> Duration {
    let shift = fails.saturating_sub(1).min(16);
    pol.backoff
        .saturating_mul(1u32 << shift)
        .min(pol.backoff_cap)
}

async fn nap(d: Duration, hooks: &Hooks<'_>) -> anyhow::Result<()> {
    let until = Instant::now() + d;
    loop {
        if hooks.cancelled() {
            anyhow::bail!(CANCELLED);
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        tokio::time::sleep(left.min(TICK)).await;
    }
}

/// What the first answer said about the file, kept beside the partial file
/// so a later run resumes the same file and checks the same sum.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Meta {
    #[serde(default)]
    validator: Option<String>,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    sha256: Option<String>,
}

impl Meta {
    fn read(part: &Path) -> Meta {
        std::fs::read_to_string(meta_path(part))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write(&self, part: &Path) {
        if let Ok(s) = serde_json::to_string(self) {
            let _ = std::fs::write(meta_path(part), s);
        }
    }
}

enum Fail {
    /// Worth another attempt from the bytes on disk.
    Retry {
        why: String,
        wait: Option<Duration>,
    },
    /// The partial file is no use: throw it away and start over.
    Restart(String),
    Fatal(anyhow::Error),
    Cancelled,
}

fn retry(why: impl Into<String>) -> Fail {
    Fail::Retry {
        why: why.into(),
        wait: None,
    }
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // Redirects are followed here: the first hop of a Hugging Face
            // link carries the file's SHA-256 and size.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(20))
            .user_agent(concat!("digiclip/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default()
    })
}

/// Why a request failed, without the URL reqwest puts in its message.
fn brief(e: &reqwest::Error) -> String {
    if e.is_connect() {
        "no connection".into()
    } else if e.is_timeout() {
        "the connection timed out".into()
    } else if e.is_body() || e.is_decode() {
        "the connection dropped".into()
    } else {
        let mut src: &dyn std::error::Error = e;
        while let Some(s) = src.source() {
            src = s;
        }
        format!("the connection failed ({src})")
    }
}

#[derive(Default)]
struct Linked {
    sha256: Option<String>,
    size: Option<u64>,
}

fn header<'h>(h: &'h HeaderMap, name: &str) -> Option<&'h str> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::trim)
}

fn sha_of(etag: &str) -> Option<String> {
    let s = etag.trim_matches('"');
    (s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
}

/// A strong `ETag`, else `Last-Modified`: what `If-Range` takes.
fn validator_of(h: &HeaderMap) -> Option<String> {
    header(h, "etag")
        .filter(|e| !e.starts_with("W/") && !e.is_empty())
        .or_else(|| header(h, "last-modified"))
        .map(str::to_string)
}

/// `bytes 100-999/1000` → `(100, Some(1000))`.
fn content_range(h: &HeaderMap) -> Option<(u64, Option<u64>)> {
    let v = header(h, "content-range")?.strip_prefix("bytes")?.trim();
    let (span, total) = v.split_once('/')?;
    let start = span.split_once('-')?.0.trim().parse().ok()?;
    Some((start, total.trim().parse().ok()))
}

/// `bytes */1000` (the answer to a range past the end) → `1000`.
fn unsatisfied_total(h: &HeaderMap) -> Option<u64> {
    header(h, "content-range")?
        .strip_prefix("bytes")?
        .trim()
        .strip_prefix("*/")?
        .trim()
        .parse()
        .ok()
}

fn retry_after(h: &HeaderMap) -> Option<Duration> {
    let s: u64 = header(h, "retry-after")?.parse().ok()?;
    Some(Duration::from_secs(s.clamp(1, 60)))
}

/// `fut`, given up on after `stall`; a cancel is seen within a [`TICK`].
async fn wait<T>(
    fut: impl std::future::Future<Output = T>,
    stall: Duration,
    hooks: &Hooks<'_>,
) -> Result<T, Fail> {
    tokio::pin!(fut);
    let until = Instant::now() + stall;
    loop {
        if hooks.cancelled() {
            return Err(Fail::Cancelled);
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(retry("the connection stalled"));
        }
        if let Ok(v) = tokio::time::timeout(left.min(TICK), &mut fut).await {
            return Ok(v);
        }
    }
}

/// Ask for `url`, following redirects by hand.
async fn open(
    url: &str,
    headers: &HeaderMap,
    hooks: &Hooks<'_>,
    pol: &Policy,
) -> Result<(reqwest::Response, Linked), Fail> {
    let mut at =
        reqwest::Url::parse(url).map_err(|e| Fail::Fatal(anyhow::anyhow!("bad URL {url}: {e}")))?;
    let mut linked = Linked::default();
    for _ in 0..10 {
        let sent = client().get(at.clone()).headers(headers.clone()).send();
        let resp = wait(sent, pol.stall, hooks)
            .await?
            .map_err(|e| retry(brief(&e)))?;
        let h = resp.headers();
        if linked.sha256.is_none() {
            linked.sha256 = header(h, "x-linked-etag").and_then(sha_of);
        }
        if linked.size.is_none() {
            linked.size = header(h, "x-linked-size").and_then(|s| s.parse().ok());
        }
        if !resp.status().is_redirection() {
            return Ok((resp, linked));
        }
        let Some(to) = header(h, "location") else {
            return Err(Fail::Fatal(anyhow::anyhow!(
                "the server redirected without saying where (HTTP {})",
                resp.status().as_u16()
            )));
        };
        at = at
            .join(to)
            .map_err(|e| Fail::Fatal(anyhow::anyhow!("bad redirect {to}: {e}")))?;
    }
    Err(Fail::Fatal(anyhow::anyhow!("too many redirects")))
}

/// One attempt: carry on from the partial file until the stream ends. `Ok`
/// is the SHA-256 to check (when known) of a partial file that is now as
/// long as the server said.
async fn attempt(
    url: &str,
    part: &Path,
    label: &str,
    hooks: &Hooks<'_>,
    pol: &Policy,
    crawl: &mut Crawl,
) -> Result<Option<String>, Fail> {
    let have = len(part);
    let meta = if have > 0 {
        Meta::read(part)
    } else {
        Meta::default()
    };
    let mut headers = HeaderMap::new();
    if have > 0 {
        if let Ok(v) = HeaderValue::from_str(&format!("bytes={have}-")) {
            headers.insert(reqwest::header::RANGE, v);
        }
        if let Some(v) = meta
            .validator
            .as_deref()
            .and_then(|v| HeaderValue::from_str(v).ok())
        {
            headers.insert(reqwest::header::IF_RANGE, v);
        }
    }
    let (resp, linked) = open(url, &headers, hooks, pol).await?;
    let status = resp.status().as_u16();
    let validator = validator_of(resp.headers());
    let (offset, total) = match status {
        // The whole file: a first attempt, a server without ranges, or a
        // file that changed since the partial one was started.
        200 => (0, resp.content_length().filter(|n| *n > 0).or(linked.size)),
        206 => {
            let Some((start, total)) = content_range(resp.headers()) else {
                return Err(Fail::Restart(
                    "the server's answer to resuming could not be read".into(),
                ));
            };
            if start != have {
                return Err(Fail::Restart(
                    "the server resumed from the wrong place".into(),
                ));
            }
            if meta.validator.is_some() && validator.is_some() && meta.validator != validator {
                return Err(Fail::Restart("the file changed on the server".into()));
            }
            (have, total.or(meta.total))
        }
        // Past the end: the partial file is already the whole file, or it
        // belongs to another one.
        416 => {
            return if have > 0 && unsatisfied_total(resp.headers()) == Some(have) {
                Ok(linked.sha256.or(meta.sha256))
            } else {
                Err(Fail::Restart(
                    "the partial file does not match the server's".into(),
                ))
            };
        }
        408 | 425 | 429 | 500..=599 => {
            return Err(Fail::Retry {
                why: format!("the server answered HTTP {status}"),
                wait: retry_after(resp.headers()),
            })
        }
        _ => {
            return Err(Fail::Fatal(anyhow::anyhow!(
                "{label} download failed: HTTP {status}"
            )))
        }
    };
    let sha = linked
        .sha256
        .or(if offset > 0 { meta.sha256 } else { None });
    Meta {
        validator: if offset > 0 {
            meta.validator.or(validator)
        } else {
            validator
        },
        total,
        sha256: sha.clone(),
    }
    .write(part);

    let disk =
        |e: std::io::Error| Fail::Fatal(anyhow::anyhow!("{label}: could not write to disk: {e}"));
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(offset > 0)
        .truncate(offset == 0)
        .open(part)
        .await
        .map_err(disk)?;
    // Chunks arrive small (a network packet or two) and every write to the
    // file is a trip to another thread, which caps the speed far below the
    // connection's. Gather a megabyte before each trip.
    let mut file = tokio::io::BufWriter::with_capacity(1 << 20, file);
    let bar = indicatif::ProgressBar::new(total.unwrap_or(0).max(1));
    if let Ok(style) =
        indicatif::ProgressStyle::with_template("{msg} [{bar:40}] {bytes}/{total_bytes} ({eta})")
    {
        bar.set_style(style);
    }
    bar.set_message(label.to_string());
    bar.set_position(offset);
    hooks.say(None);

    let mut stream = resp.bytes_stream();
    let mut done = offset;
    crawl.begin(offset);
    let end = loop {
        match wait(stream.next(), pol.stall, hooks).await {
            Err(f) => break Err(f),
            Ok(None) => break Ok(()),
            Ok(Some(Err(e))) => break Err(retry(brief(&e))),
            Ok(Some(Ok(chunk))) => {
                if let Err(e) = file.write_all(&chunk).await {
                    break Err(disk(e));
                }
                done += chunk.len() as u64;
                bar.set_position(done);
                if let Some(p) = hooks.progress {
                    p(done, total.unwrap_or(0));
                }
                if crawl.too_slow(done, total, pol) {
                    break Err(Fail::Retry {
                        why: "the connection was very slow".into(),
                        wait: Some(Duration::ZERO),
                    });
                }
            }
        }
    };
    // What reached the file stays, whatever ended the attempt.
    let flushed = file.flush().await;
    drop(file);
    bar.finish_and_clear();
    end?;
    flushed.map_err(disk)?;
    match total {
        Some(t) if done < t => Err(retry("the connection closed early")),
        Some(t) if done > t => Err(Fail::Restart("more arrived than the file holds".into())),
        _ => Ok(sha),
    }
}

/// Check the finished partial file and move it into place.
async fn finish(part: &Path, dest: &Path, min_bytes: u64, sha: Option<String>) -> Result<(), Fail> {
    let size = len(part);
    if size < min_bytes.max(1) {
        return Err(Fail::Restart(format!(
            "the server sent {size} bytes where at least {min_bytes} were expected"
        )));
    }
    if let Some(want) = sha {
        let p = part.to_path_buf();
        let got = tokio::task::spawn_blocking(move || sha256_file(&p))
            .await
            .map_err(|e| Fail::Fatal(e.into()))?
            .map_err(|e| Fail::Fatal(e.into()))?;
        if got != want {
            return Err(Fail::Restart(
                "the file arrived damaged (its checksum does not match)".into(),
            ));
        }
    }
    // A virus scanner may hold the new file for a moment.
    let mut moved = tokio::fs::rename(part, dest).await;
    for _ in 0..10 {
        if moved.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        moved = tokio::fs::rename(part, dest).await;
    }
    moved.map_err(|e| {
        Fail::Fatal(anyhow::anyhow!(
            "could not move the download into place at {}: {e}",
            dest.display()
        ))
    })?;
    let _ = std::fs::remove_file(meta_path(part));
    Ok(())
}

pub fn sha256_file(p: &Path) -> std::io::Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{HeaderMap as ReqHeaders, StatusCode};
    use axum::response::Response;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// How the test server ends one answer.
    enum Cut {
        /// Send this many bytes, then break the connection.
        Drop(usize),
        /// Send this many bytes, then go silent.
        Stall(usize),
        /// Send the file a kilobyte every 40 ms.
        Crawl,
        /// Answer with this status and nothing else.
        Status(u16),
    }

    struct Srv {
        data: Mutex<Vec<u8>>,
        etag: Mutex<String>,
        /// One per request, in order; none left = a whole, honest answer.
        cuts: Mutex<VecDeque<Cut>>,
        ignore_range: AtomicBool,
        /// What `/r` (a redirect to `/f`) says the file's SHA-256 is.
        linked: Mutex<Option<String>>,
        hits: AtomicUsize,
        ranges: Mutex<Vec<Option<String>>>,
    }

    fn payload(n: usize, seed: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    async fn file(State(s): State<Arc<Srv>>, h: ReqHeaders) -> Response {
        s.hits.fetch_add(1, Ordering::SeqCst);
        let range = h
            .get("range")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        s.ranges.lock().unwrap().push(range.clone());
        let cut = s.cuts.lock().unwrap().pop_front();
        if let Some(Cut::Status(code)) = cut {
            return Response::builder()
                .status(StatusCode::from_u16(code).unwrap())
                .body(Body::empty())
                .unwrap();
        }
        let data = s.data.lock().unwrap().clone();
        let etag = s.etag.lock().unwrap().clone();
        let total = data.len();
        let if_range = h.get("if-range").and_then(|v| v.to_str().ok());
        let start = range
            .as_deref()
            .filter(|_| !s.ignore_range.load(Ordering::SeqCst))
            .filter(|_| if_range.is_none_or(|v| v == etag))
            .and_then(|r| {
                r.strip_prefix("bytes=")?
                    .strip_suffix('-')?
                    .parse::<usize>()
                    .ok()
            });
        let mut b = Response::builder().header("etag", &etag);
        let from = match start {
            Some(n) if n >= total => {
                return b
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header("content-range", format!("bytes */{total}"))
                    .body(Body::empty())
                    .unwrap();
            }
            Some(n) => {
                b = b
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header("content-range", format!("bytes {n}-{}/{total}", total - 1));
                n
            }
            None => 0,
        };
        let body = data[from..].to_vec();
        b = b.header("content-length", body.len());
        type Piece = Result<Vec<u8>, std::io::Error>;
        let out = match cut {
            Some(Cut::Drop(n)) => {
                let n = n.min(body.len());
                let first: Piece = Ok(body[..n].to_vec());
                // The bytes go out before the break does.
                let broken = futures_util::stream::once(async {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    Piece::Err(std::io::Error::other("cut"))
                });
                Body::from_stream(futures_util::stream::iter([first]).chain(broken))
            }
            Some(Cut::Stall(n)) => {
                let n = n.min(body.len());
                let first: Piece = Ok(body[..n].to_vec());
                Body::from_stream(
                    futures_util::stream::iter([first]).chain(futures_util::stream::pending()),
                )
            }
            Some(Cut::Crawl) => {
                let pieces: Vec<Vec<u8>> = body.chunks(1024).map(<[u8]>::to_vec).collect();
                Body::from_stream(futures_util::stream::iter(pieces).then(|p| async move {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    Piece::Ok(p)
                }))
            }
            _ => Body::from(body),
        };
        b.body(out).unwrap()
    }

    async fn redirect(State(s): State<Arc<Srv>>) -> Response {
        let mut b = Response::builder()
            .status(StatusCode::FOUND)
            .header("location", "/f");
        if let Some(sha) = s.linked.lock().unwrap().clone() {
            b = b.header("x-linked-etag", format!("\"{sha}\""));
        }
        b.body(Body::empty()).unwrap()
    }

    async fn serve(data: Vec<u8>) -> (Arc<Srv>, String) {
        let s = Arc::new(Srv {
            data: Mutex::new(data),
            etag: Mutex::new("\"v1\"".into()),
            cuts: Mutex::new(VecDeque::new()),
            ignore_range: AtomicBool::new(false),
            linked: Mutex::new(None),
            hits: AtomicUsize::new(0),
            ranges: Mutex::new(Vec::new()),
        });
        let app = axum::Router::new()
            .route("/f", axum::routing::get(file))
            .route("/r", axum::routing::get(redirect))
            .with_state(s.clone());
        let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        (s, base)
    }

    fn quick() -> Policy {
        Policy {
            stall: Duration::from_millis(600),
            tries: 4,
            backoff: Duration::from_millis(10),
            backoff_cap: Duration::from_millis(40),
            // 25 KB/s from the crawling test server is far below this.
            crawl_floor: 100 * 1024,
            crawl_window: Duration::from_millis(250),
            crawl_tries: 2,
            crawl_min_left: Duration::ZERO,
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "digiclip-dl-{name}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    async fn get(url: &str, dest: &Path, min: u64) -> anyhow::Result<()> {
        fetch_with(url, dest, "test file", min, Hooks::default(), &quick()).await
    }

    #[tokio::test]
    async fn a_plain_download_lands_whole_and_leaves_nothing_behind() {
        let data = payload(200_000, 1);
        let (s, base) = serve(data.clone()).await;
        let dir = tmp("plain");
        let dest = dir.join("sub").join("model.bin");
        get(&format!("{base}/f"), &dest, 1000).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert!(!part_path(&dest).exists());
        assert!(!meta_path(&part_path(&dest)).exists());
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
        // Already there: no second request.
        get(&format!("{base}/f"), &dest, 1000).await.unwrap();
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_connection_that_keeps_dropping_is_resumed_until_the_file_is_whole() {
        let data = payload(300_000, 2);
        let (s, base) = serve(data.clone()).await;
        // More drops than `tries`: each adds bytes, so none counts against it.
        *s.cuts.lock().unwrap() = (0..7).map(|_| Cut::Drop(40_000)).collect();
        let dir = tmp("drops");
        let dest = dir.join("model.bin");
        get(&format!("{base}/f"), &dest, 1000).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        let ranges = s.ranges.lock().unwrap().clone();
        assert_eq!(ranges.len(), 8);
        assert_eq!(ranges[0], None);
        assert_eq!(ranges[1].as_deref(), Some("bytes=40000-"));
        assert_eq!(ranges[7].as_deref(), Some("bytes=280000-"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_silent_connection_is_given_up_on_and_resumed() {
        let data = payload(120_000, 3);
        let (s, base) = serve(data.clone()).await;
        s.cuts.lock().unwrap().push_back(Cut::Stall(50_000));
        let dir = tmp("stall");
        let dest = dir.join("model.bin");
        let t = Instant::now();
        get(&format!("{base}/f"), &dest, 1000).await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(10));
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(s.ranges.lock().unwrap()[1].as_deref(), Some("bytes=50000-"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_crawling_connection_is_swapped_for_a_new_one() {
        let data = payload(400_000, 15);
        let (s, base) = serve(data.clone()).await;
        // At 25 KB/s this file takes 16 s; the second connection is quick.
        s.cuts.lock().unwrap().push_back(Cut::Crawl);
        let dir = tmp("crawl");
        let dest = dir.join("model.bin");
        let t = Instant::now();
        get(&format!("{base}/f"), &dest, 1000).await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        let ranges = s.ranges.lock().unwrap().clone();
        assert_eq!(ranges.len(), 2);
        assert!(ranges[1]
            .as_deref()
            .is_some_and(|r| r.starts_with("bytes=")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_line_that_is_just_slow_is_left_to_finish() {
        let data = payload(40_000, 16);
        let (s, base) = serve(data.clone()).await;
        *s.cuts.lock().unwrap() = (0..10).map(|_| Cut::Crawl).collect();
        let dir = tmp("slowline");
        let dest = dir.join("model.bin");
        get(&format!("{base}/f"), &dest, 1000).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        // The first connection and `crawl_tries` swaps, then no more.
        assert_eq!(
            s.hits.load(Ordering::SeqCst),
            1 + quick().crawl_tries as usize
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_server_without_ranges_starts_over_instead_of_appending() {
        let data = payload(90_000, 4);
        let (s, base) = serve(data.clone()).await;
        s.cuts.lock().unwrap().push_back(Cut::Drop(30_000));
        s.ignore_range.store(true, Ordering::SeqCst);
        let dir = tmp("norange");
        let dest = dir.join("tool.exe");
        get(&format!("{base}/f"), &dest, 1000).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_file_that_changed_on_the_server_is_not_spliced() {
        let old = payload(100_000, 5);
        let new = payload(110_000, 6);
        let (s, base) = serve(old).await;
        *s.cuts.lock().unwrap() = [Cut::Drop(30_000), Cut::Status(503)].into();
        let dir = tmp("changed");
        let dest = dir.join("ffmpeg.zip");
        // Gives up after the drop: the partial file and its validator stay.
        let once = Policy {
            tries: 1,
            ..quick()
        };
        let url = format!("{base}/f");
        assert!(fetch_with(&url, &dest, "t", 1000, Hooks::default(), &once)
            .await
            .is_err());
        assert_eq!(len(&part_path(&dest)), 30_000);
        *s.data.lock().unwrap() = new.clone();
        *s.etag.lock().unwrap() = "\"v2\"".into();
        get(&url, &dest, 1000).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), new);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_partial_file_that_is_already_whole_is_finished_not_failed() {
        let data = payload(50_000, 7);
        let (s, base) = serve(data.clone()).await;
        let dir = tmp("whole");
        let dest = dir.join("model.bin");
        std::fs::write(part_path(&dest), &data).unwrap();
        get(&format!("{base}/f"), &dest, 1000).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_partial_file_from_an_older_release_is_carried_on() {
        let data = payload(80_000, 8);
        let (s, base) = serve(data.clone()).await;
        let dir = tmp("legacy");
        let dest = dir.join("laya.onnx.data");
        std::fs::write(dir.join("laya.onnx.part"), &data[..25_000]).unwrap();
        get(&format!("{base}/f"), &dest, 1000).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(s.ranges.lock().unwrap()[0].as_deref(), Some("bytes=25000-"));
        assert!(!dir.join("laya.onnx.part").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_missing_file_fails_at_once() {
        let (s, base) = serve(payload(10, 0)).await;
        s.cuts.lock().unwrap().push_back(Cut::Status(404));
        let dir = tmp("404");
        let e = get(&format!("{base}/f"), &dir.join("x.bin"), 1)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("HTTP 404"), "{e}");
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_busy_server_is_asked_again() {
        let data = payload(20_000, 9);
        let (s, base) = serve(data.clone()).await;
        *s.cuts.lock().unwrap() = [Cut::Status(503), Cut::Status(429)].into();
        let dir = tmp("busy");
        let dest = dir.join("x.bin");
        get(&format!("{base}/f"), &dest, 1000).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(s.hits.load(Ordering::SeqCst), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn no_progress_at_all_gives_up_and_keeps_what_it_has() {
        let (s, base) = serve(payload(60_000, 10)).await;
        s.cuts.lock().unwrap().push_back(Cut::Drop(20_000));
        s.cuts
            .lock()
            .unwrap()
            .extend((0..10).map(|_| Cut::Status(503)));
        let dir = tmp("giveup");
        let dest = dir.join("x.bin");
        let e = get(&format!("{base}/f"), &dest, 1000).await.unwrap_err();
        assert!(e.to_string().contains("carries on from there"), "{e}");
        assert!(!dest.exists());
        assert_eq!(len(&part_path(&dest)), 20_000);
        // The drop added bytes (not counted), then `tries` failures.
        assert_eq!(s.hits.load(Ordering::SeqCst), 1 + quick().tries as usize);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_error_page_is_not_kept_as_the_file() {
        let (_s, base) = serve(b"<html>sorry</html>".to_vec()).await;
        let dir = tmp("tiny");
        let dest = dir.join("model.bin");
        let e = get(&format!("{base}/f"), &dest, 100_000).await.unwrap_err();
        assert!(e.to_string().contains("at least 100000"), "{e}");
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_checksum_a_redirect_names_is_checked() {
        let data = payload(70_000, 11);
        let (s, base) = serve(data.clone()).await;
        let dir = tmp("sha");
        let good = dir.join("good.bin");
        std::fs::write(dir.join("ref"), &data).unwrap();
        *s.linked.lock().unwrap() = Some(sha256_file(&dir.join("ref")).unwrap());
        // Resumed across a drop, and still the right sum.
        s.cuts.lock().unwrap().push_back(Cut::Drop(10_000));
        get(&format!("{base}/r"), &good, 1000).await.unwrap();
        assert_eq!(std::fs::read(&good).unwrap(), data);

        *s.linked.lock().unwrap() = Some("0".repeat(64));
        let bad = dir.join("bad.bin");
        let e = get(&format!("{base}/r"), &bad, 1000).await.unwrap_err();
        assert!(e.to_string().contains("checksum"), "{e}");
        assert!(!bad.exists());
        assert!(!part_path(&bad).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn cancel_stops_a_stalled_download_within_a_moment() {
        let (s, base) = serve(payload(60_000, 12)).await;
        s.cuts.lock().unwrap().push_back(Cut::Stall(5_000));
        let dir = tmp("cancel");
        let dest = dir.join("x.bin");
        let cancel = CancelFlag::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            c2.cancel();
        });
        let slow = Policy {
            stall: Duration::from_secs(60),
            ..quick()
        };
        let t = Instant::now();
        let e = fetch_with(
            &format!("{base}/f"),
            &dest,
            "t",
            1000,
            Hooks::default().with_cancel(&cancel),
            &slow,
        )
        .await
        .unwrap_err();
        assert_eq!(e.to_string(), CANCELLED);
        assert!(t.elapsed() < Duration::from_secs(5));
        assert_eq!(len(&part_path(&dest)), 5_000);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn two_callers_for_one_file_download_it_once() {
        let data = payload(150_000, 13);
        let (s, base) = serve(data.clone()).await;
        let dir = tmp("twice");
        let dest = dir.join("model.bin");
        let url = format!("{base}/f");
        let (a, b) = tokio::join!(get(&url, &dest, 1000), get(&url, &dest, 1000));
        a.unwrap();
        b.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn waiting_and_flowing_are_reported() {
        let data = payload(40_000, 14);
        let (s, base) = serve(data).await;
        s.cuts.lock().unwrap().push_back(Cut::Status(503));
        let dir = tmp("status");
        let seen = Mutex::new(Vec::<Option<String>>::new());
        let status = |w: Option<&str>| seen.lock().unwrap().push(w.map(str::to_string));
        let bytes = AtomicUsize::new(0);
        let progress = |done: u64, total: u64| {
            assert_eq!(total, 40_000);
            bytes.store(done as usize, Ordering::SeqCst);
        };
        let hooks = Hooks {
            progress: Some(&progress),
            cancel: None,
            status: Some(&status),
        };
        fetch_with(
            &format!("{base}/f"),
            &dir.join("x"),
            "t",
            1,
            hooks,
            &quick(),
        )
        .await
        .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].as_deref(), Some("the server answered HTTP 503"));
        assert_eq!(seen.last().unwrap(), &None);
        assert_eq!(bytes.load(Ordering::SeqCst), 40_000);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn headers_are_read() {
        let mut h = HeaderMap::new();
        h.insert("content-range", "bytes 100-999/1000".parse().unwrap());
        assert_eq!(content_range(&h), Some((100, Some(1000))));
        h.insert("content-range", "bytes 5-9/*".parse().unwrap());
        assert_eq!(content_range(&h), Some((5, None)));
        h.insert("content-range", "bytes */1000".parse().unwrap());
        assert_eq!(unsatisfied_total(&h), Some(1000));
        h.insert("etag", "W/\"weak\"".parse().unwrap());
        h.insert(
            "last-modified",
            "Sat, 19 Sep 2026 07:10:16 GMT".parse().unwrap(),
        );
        assert_eq!(
            validator_of(&h).as_deref(),
            Some("Sat, 19 Sep 2026 07:10:16 GMT")
        );
        h.insert("etag", "\"strong\"".parse().unwrap());
        assert_eq!(validator_of(&h).as_deref(), Some("\"strong\""));
        assert_eq!(sha_of("\"abc\""), None);
        assert_eq!(
            sha_of(&format!("\"{}\"", "AB".repeat(32))),
            Some("ab".repeat(32))
        );
        assert_eq!(
            part_path(Path::new("m/laya.onnx.data")),
            Path::new("m/laya.onnx.data.part")
        );
    }
}
