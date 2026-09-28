//! Link import: `yt-dlp` pulls a video from a URL (YouTube, TikTok, X,
//! Vimeo…) into a folder, merged to mp4 with our ffmpeg. yt-dlp is found
//! like the other tools (provisioned `bin/`, bundled, PATH) or downloaded
//! once from its official release on first use.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;

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
pub fn download(
    tool: &Path,
    url: &str,
    dir: &Path,
    on_pct: &dyn Fn(u8),
    cancel: &crate::progress::CancelFlag,
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
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
    let f =
        found.ok_or_else(|| anyhow::anyhow!("download finished but no video file was written"))?;
    on_pct(100);
    Ok(f)
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
}
