//! Locate bundled-or-system media binaries.
//!
//! Order: `<exe-dir>/resources/bin/<platform>/` (ships with installers)
//! then next to the exe, then system `PATH`. Never requires sudo.
//!
//! On Windows this resolves `ffmpeg.exe` / `ffprobe.exe` /
//! `whisper-cli.exe` / `whisper-cli-vulkan.exe`. The `win-x64/` dir is
//! empty in git and filled by CI (gyan ffmpeg + pinned whisper.cpp);
//! a normal dev machine falls back to `PATH` (`where.exe`).

use std::path::{Path, PathBuf};

pub fn platform_dir() -> &'static str {
    if cfg!(windows) {
        "win-x64"
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "mac-arm64"
        } else {
            "mac-x64"
        }
    } else if cfg!(target_arch = "aarch64") {
        "linux-arm64"
    } else {
        "linux-x64"
    }
}

fn exe_name(name: &str) -> String {
    if cfg!(windows) && !name.ends_with(".exe") {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn bundled_candidates(name: &str) -> Vec<PathBuf> {
    let file = exe_name(name);
    let mut out = Vec::new();
    // 0. Explicit override (portable installs, tests).
    if let Ok(ov) = std::env::var("DIGICLIP_FFMPEG") {
        if name == "ffmpeg" || name == "ffprobe" {
            let p = PathBuf::from(&ov);
            let p = if name == "ffprobe" {
                p.with_file_name(if cfg!(windows) {
                    "ffprobe.exe"
                } else {
                    "ffprobe"
                })
            } else {
                p
            };
            out.push(p);
        }
    }
    // 1. Provisioned user-data dir (single-exe first-run downloads).
    out.push(crate::provision::bin_dir().join(&file));
    // 2. <current-exe-dir>/resources/bin/<platform>/ (installed layout)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(
                dir.join("resources")
                    .join("bin")
                    .join(platform_dir())
                    .join(&file),
            );
            // 3. <current-exe-dir>/bin/<platform>/ (dev layout)
            out.push(dir.join("bin").join(platform_dir()).join(&file));
        }
    }
    // 4. <cwd>/resources/bin/<platform>/ (cargo run from repo root)
    if let Ok(cwd) = std::env::current_dir() {
        out.push(
            cwd.join("resources")
                .join("bin")
                .join(platform_dir())
                .join(&file),
        );
    }
    out
}

/// Package-manager bin dirs a GUI launch doesn't put on PATH: apps opened
/// from Finder/Dock get only `/usr/bin:/bin:/usr/sbin:/sbin`, so a
/// Homebrew/MacPorts ffmpeg would otherwise never be found.
fn extra_unix_dirs() -> Vec<PathBuf> {
    if cfg!(windows) {
        return Vec::new();
    }
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/local/bin"),
        PathBuf::from("/home/linuxbrew/.linuxbrew/bin"),
        PathBuf::from("/snap/bin"),
    ];
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join(".nix-profile").join("bin"));
    }
    dirs
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    // `where.exe` on Windows returns CRLF lines; PATH search handles .exe.
    // Try plain + .exe so `whisper-cli` resolves `whisper-cli.exe`.
    let mut names = vec![name.to_string()];
    if cfg!(windows) && !name.ends_with(".exe") {
        names.push(format!("{name}.exe"));
    }
    for n in &names {
        let path_dirs = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .unwrap_or_default();
        for dir in path_dirs.into_iter().chain(extra_unix_dirs()) {
            let p = dir.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
        // Fall back to the shell lookup for shims (scoop/choco/winget).
        // NOTE: no Unix `2>/dev/null` redirect here — that syntax breaks
        // `cmd /C where` on Windows. Stderr is simply discarded below.
        let out = if cfg!(windows) {
            crate::process::command("cmd")
                .args(["/C", "where", n])
                .output()
        } else {
            let script = format!("command -v {n}");
            crate::process::command("sh").args(["-c", &script]).output()
        };
        if let Ok(out) = out {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines() {
                    let line = line.trim().trim_matches('"');
                    if !line.is_empty() && Path::new(line).is_file() {
                        return Some(PathBuf::from(line));
                    }
                }
            }
        }
    }
    None
}

/// Resolve `name` to a bundled file or a PATH binary. `None` when missing.
pub fn resolve(name: &str) -> Option<PathBuf> {
    for p in bundled_candidates(name) {
        if p.is_file() {
            return Some(p);
        }
    }
    // Also accept the bare name (without .exe) if it exists bundled.
    if cfg!(windows) {
        for p in bundled_candidates(&exe_name(name)) {
            if p.is_file() {
                return Some(p);
            }
        }
    }
    find_on_path(name)
}

/// Resolve or return a production-ready error pointing at /health equivalents.
pub fn require(name: &str) -> anyhow::Result<PathBuf> {
    resolve(name).ok_or_else(|| {
        anyhow::anyhow!(
            "{name} not found (checked resources/bin/{}/ and PATH). Install it or drop \
             {exe} into resources/bin/{}/. ffmpeg: https://www.gyan.dev/ffmpeg/builds (Windows, \
             needs libass); whisper-cli: build https://github.com/ggerganov/whisper.cpp \
             (cmake -B build && cmake --build build --config Release).",
            platform_dir(),
            platform_dir(),
            exe = exe_name(name),
        )
    })
}

/// Does this ffmpeg include libass (`ass`/`subtitles` filters)?
/// Primary signal is `--enable-libass` in the build config (stable text);
/// the `-filters` listing is the fallback (its layout varies by build).
pub fn ffmpeg_has_libass(ffmpeg: &Path) -> bool {
    let conf = crate::process::command(ffmpeg)
        .args(["-hide_banner", "-buildconf"])
        .output();
    if let Ok(o) = conf {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        if text.contains("--enable-libass") {
            return true;
        }
    }
    let out = crate::process::command(ffmpeg)
        .args(["-hide_banner", "-filters"])
        .output();
    match out {
        Ok(o) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            text.contains(" ass") || text.contains("subtitles")
        }
        Err(_) => false,
    }
}
