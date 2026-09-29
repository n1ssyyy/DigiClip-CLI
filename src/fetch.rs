//! Link import: `yt-dlp` pulls a video from a URL (YouTube, TikTok, X,
//! Vimeo…) into a folder, merged to mp4 with our ffmpeg. yt-dlp is found
//! like the other tools (provisioned `bin/`, bundled, PATH) or downloaded
//! once from its official release on first use. That managed copy is kept
//! fresh with `yt-dlp -U` (YouTube breaks old builds, often as 403s).

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Re-check the managed yt-dlp for updates at most this often.
const UPDATE_EVERY: Duration = Duration::from_secs(3 * 24 * 3600);
/// A forced update (after a 403) is skipped if one just ran.
const UPDATE_JUST_RAN: Duration = Duration::from_secs(60);
/// `yt-dlp -U` never holds a download up longer than this.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(90);
/// One self-update at a time (jobs can start together).
static UPDATING: Mutex<()> = Mutex::new(());

/// Official standalone builds (no Python needed).
const RELEASE: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/";

fn asset() -> &'static str {
    if cfg!(windows) {
        "yt-dlp.exe"
    } else if cfg!(target_os = "macos") {
        "yt-dlp_macos"
    } else {
        "yt-dlp_linux"
    }
}

/// `http(s)://…` with a host.
pub fn is_url(s: &str) -> bool {
    let s = s.trim();
    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"));
    rest.is_some_and(|r| r.split('/').next().is_some_and(|h| h.contains('.')) && !r.contains(' '))
}

/// A short job name for a link before its title is known (`youtube.com`).
pub fn host(url: &str) -> String {
    let rest = url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let h = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    h.trim_start_matches("www.")
        .trim_start_matches("m.")
        .to_string()
}

/// yt-dlp on this machine (`DIGICLIP_YTDLP` overrides, for tests).
pub fn tool() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("DIGICLIP_YTDLP").map(PathBuf::from) {
        return p.is_file().then_some(p);
    }
    crate::binaries::resolve("yt-dlp")
}

/// yt-dlp, downloading the official build into `bin/` once if missing.
pub async fn ensure_tool(progress: Option<&crate::progress::ByteFn>) -> anyhow::Result<PathBuf> {
    if let Some(p) = tool() {
        return Ok(p);
    }
    let dest = crate::provision::bin_dir().join(if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    });
    tracing::info!("yt-dlp not found — downloading the official build (once)…");
    crate::provision::download_to(&format!("{RELEASE}{}", asset()), &dest, "yt-dlp", progress)
        .await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(dest)
}

/// Is `tool` our own downloaded copy (in `bin`)? Only that one may
/// self-update — never a PATH/system or `DIGICLIP_YTDLP` build.
fn is_managed_in(tool: &Path, bin: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    tool.parent().is_some_and(|d| canon(d) == canon(bin))
}

fn is_managed(tool: &Path) -> bool {
    std::env::var_os("DIGICLIP_YTDLP").is_none()
        && is_managed_in(tool, &crate::provision::bin_dir())
}

/// When the managed copy was last checked (next to it, `yt-dlp.checked`).
fn stamp_for(tool: &Path) -> PathBuf {
    tool.with_file_name("yt-dlp.checked")
}

/// Stamp missing, unreadable, from the future or older than `max_age`.
fn is_stale(stamp: &Path, max_age: Duration, now: SystemTime) -> bool {
    let Ok(t) = std::fs::metadata(stamp).and_then(|m| m.modified()) else {
        return true;
    };
    now.duration_since(t).map_or(true, |age| age >= max_age)
}

/// Does a failed download look like YouTube refusing stale URLs?
fn is_forbidden(err: &str) -> bool {
    err.contains("403") || err.to_ascii_lowercase().contains("forbidden")
}

/// `tool -U` if its stamp is older than `max_age`; refreshes the stamp on
/// success. Bounded and failure-tolerant: a failed update only logs and
/// the download goes ahead with the current build. Blocking.
fn self_update(tool: &Path, max_age: Duration) -> bool {
    let _one = UPDATING.lock().unwrap_or_else(|e| e.into_inner());
    let stamp = stamp_for(tool);
    if !is_stale(&stamp, max_age, SystemTime::now()) {
        return true;
    }
    tracing::info!("checking for a yt-dlp update…");
    let mut child = match crate::process::command(tool)
        .arg("-U")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("yt-dlp update failed to start: {e}");
            return false;
        }
    };
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let deadline = Instant::now() + UPDATE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let Some(status) = status else {
        tracing::warn!("yt-dlp update timed out; using the current build");
        return false;
    };
    let text = format!(
        "{}\n{}",
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default()
    );
    let last = text
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("");
    if !status.success() {
        tracing::warn!("yt-dlp update failed ({status}): {last}");
        return false;
    }
    tracing::info!("yt-dlp: {last}");
    if let Err(e) = std::fs::write(&stamp, b"") {
        tracing::warn!("could not write {}: {e}", stamp.display());
    }
    true
}

/// Read a child's pipe to the end on its own thread.
fn drain<R: Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut r) = r {
            let _ = r.read_to_string(&mut s);
        }
        s
    })
}

/// `DLPCT  42.3%` progress lines.
pub fn parse_pct(line: &str) -> Option<f64> {
    let v = line.strip_prefix("DLPCT")?.trim().trim_end_matches('%');
    v.parse::<f64>().ok().filter(|p| p.is_finite())
}

/// Overall percent over yt-dlp's passes (video, then audio): the first
/// pass is most of the bytes.
#[derive(Default)]
struct Passes {
    pass: u32,
    last: f64,
}

impl Passes {
    fn feed(&mut self, p: f64) -> u8 {
        if p + 50.0 < self.last {
            self.pass += 1;
        }
        self.last = p;
        let overall = if self.pass == 0 {
            p * 0.9
        } else {
            90.0 + p * 0.09
        };
        overall.clamp(0.0, 99.0) as u8
    }
}

fn is_video(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "mp4" | "mkv" | "webm" | "mov" | "m4v"
        )
    })
}

/// Download `url` into `dir` (created); returns the video file. `on_pct`
/// gets 0-99 while it runs. Blocking: call from a blocking thread.
/// Updates the managed yt-dlp first (every few days) and retries once,
/// after a forced update, when the first attempt is refused (403).
pub fn download(
    tool: &Path,
    url: &str,
    dir: &Path,
    on_pct: &dyn Fn(u8),
    cancel: &crate::progress::CancelFlag,
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let managed = is_managed(tool);
    if managed {
        self_update(tool, UPDATE_EVERY);
    }
    let f = match attempt(tool, url, dir, on_pct, cancel) {
        Err(e) if !cancel.is_cancelled() && is_forbidden(&e.to_string()) => {
            tracing::warn!("{e} — retrying once with fresh URLs");
            if managed {
                self_update(tool, UPDATE_JUST_RAN);
            }
            attempt(tool, url, dir, on_pct, cancel)?
        }
        r => r?,
    };
    on_pct(100);
    Ok(f)
}

/// One yt-dlp run; its progress stays at or below 99.
fn attempt(
    tool: &Path,
    url: &str,
    dir: &Path,
    on_pct: &dyn Fn(u8),
    cancel: &crate::progress::CancelFlag,
) -> anyhow::Result<PathBuf> {
    let mut cmd = crate::process::command(tool);
    cmd.args([
        "--no-playlist",
        "--newline",
        "--no-colors",
        "--progress",
        "--progress-template",
        "download:DLPCT %(progress._percent_str)s",
        "--print",
        "after_move:DLFILE %(filepath)s",
        "-f",
        "bv*[height<=1080][ext=mp4]+ba[ext=m4a]/bv*[height<=1080]+ba/b[height<=1080]/b",
        "--merge-output-format",
        "mp4",
        "-o",
        "%(title).80B.%(ext)s",
    ]);
    cmd.arg("-P").arg(dir);
    if let Some(ff) = crate::binaries::resolve("ffmpeg") {
        cmd.arg("--ffmpeg-location").arg(ff);
    }
    cmd.arg("--").arg(url);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("yt-dlp failed to start: {e}"))?;
    let (tx, rx) = mpsc::channel::<String>();
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let tx2 = tx.clone();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    std::thread::spawn(move || {
        for l in BufReader::new(err).lines().map_while(Result::ok) {
            let _ = tx2.send(format!("ERR {l}"));
        }
    });
    let mut passes = Passes::default();
    let mut file: Option<PathBuf> = None;
    let mut errors: Vec<String> = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(l) => {
                let l = l.trim_end();
                if let Some(p) = parse_pct(l) {
                    on_pct(passes.feed(p));
                } else if let Some(f) = l.strip_prefix("DLFILE ") {
                    file = Some(PathBuf::from(f.trim()));
                } else if let Some(e) = l.strip_prefix("ERR ") {
                    if !e.trim().is_empty() {
                        errors.push(e.trim().to_string());
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("cancelled");
        }
    }
    let status = child.wait()?;
    if !status.success() {
        let why = errors
            .iter()
            .rev()
            .find(|e| e.starts_with("ERROR"))
            .or(errors.last())
            .cloned()
            .unwrap_or_else(|| format!("exit {status}"));
        anyhow::bail!("download failed: {}", why.trim_start_matches("ERROR: "));
    }
    // The printed path; else the newest video in the folder.
    let found = file.filter(|f| f.is_file()).or_else(|| {
        std::fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| is_video(p))
            .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
    });
    found.ok_or_else(|| anyhow::anyhow!("download finished but no video file was written"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_told_from_paths() {
        assert!(is_url("https://www.youtube.com/watch?v=abc"));
        assert!(is_url(" http://vimeo.com/1 "));
        assert!(!is_url("C:\\vids\\a.mp4"));
        assert!(!is_url("https://localhost"));
        assert!(!is_url("https://a.com/x y"));
        assert_eq!(host("https://www.youtube.com/watch?v=abc"), "youtube.com");
        assert_eq!(host("https://m.tiktok.com/@a/video/1"), "tiktok.com");
    }

    #[test]
    fn progress_spans_both_passes() {
        assert_eq!(parse_pct("DLPCT  42.5%"), Some(42.5));
        assert_eq!(parse_pct("[download] 42%"), None);
        let mut p = Passes::default();
        assert_eq!(p.feed(50.0), 45);
        assert_eq!(p.feed(100.0), 90);
        // Audio pass restarts at 0.
        assert_eq!(p.feed(3.0), 90);
        assert_eq!(p.feed(100.0), 99);
    }

    #[test]
    fn forbidden_failures_are_recognised() {
        assert!(is_forbidden(
            "download failed: unable to download video data: HTTP Error 403: Forbidden"
        ));
        assert!(is_forbidden("download failed: Forbidden"));
        assert!(!is_forbidden("download failed: Video unavailable"));
        assert!(!is_forbidden("cancelled"));
    }

    #[test]
    fn update_stamp_ages_out() {
        let d = tempfile::tempdir().unwrap();
        let tool = d.path().join("yt-dlp.exe");
        let stamp = stamp_for(&tool);
        assert_eq!(stamp, d.path().join("yt-dlp.checked"));
        let now = SystemTime::now();
        // Never checked.
        assert!(is_stale(&stamp, UPDATE_EVERY, now));
        std::fs::write(&stamp, b"").unwrap();
        assert!(!is_stale(&stamp, UPDATE_EVERY, now));
        assert!(!is_stale(&stamp, UPDATE_JUST_RAN, now));
        // Four days on it is due again.
        let later = now + Duration::from_secs(4 * 24 * 3600);
        assert!(is_stale(&stamp, UPDATE_EVERY, later));
        assert!(!is_stale(&stamp, UPDATE_EVERY, now + UPDATE_JUST_RAN));
        // A stamp from the future (clock moved back) counts as stale.
        let earlier = now - Duration::from_secs(3600);
        assert!(is_stale(&stamp, UPDATE_EVERY, earlier));
    }

    #[test]
    fn only_our_own_copy_self_updates() {
        let bin = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let ours = bin.path().join("yt-dlp.exe");
        let theirs = other.path().join("yt-dlp.exe");
        std::fs::write(&ours, b"").unwrap();
        std::fs::write(&theirs, b"").unwrap();
        assert!(is_managed_in(&ours, bin.path()));
        assert!(!is_managed_in(&theirs, bin.path()));
        // Not a direct child either.
        let nested = bin.path().join("sub").join("yt-dlp.exe");
        assert!(!is_managed_in(&nested, bin.path()));
    }
}
