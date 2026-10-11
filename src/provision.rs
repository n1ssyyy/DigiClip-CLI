//! First-run provisioning: keep the exe small, download the heavy bits.
//!
//! Layout under the OS user-data dir (`%APPDATA%/digiclip` on Windows,
//! `~/.local/share/digiclip` on Linux, `~/Library/Application Support/digiclip`
//! on macOS — see [`root`]):
//! - `bin/ffmpeg(.exe)`, `bin/ffprobe(.exe)`: a portable build, downloaded
//!   when no ffmpeg is found on the machine (Windows: gyan essentials,
//!   ~115MB; macOS/Linux: Martin Riedl's static builds, ~60MB)
//! - `bin/yt-dlp(.exe)` (link import, see [`crate::fetch`])
//! - `yunet_2026may.onnx` (face detector, ~230KB)
//! - `models/ggml-*.bin` (whisper weights, via [`crate::models`]) and
//!   `models/laya/` (see [`crate::decide`])
//! - `fonts/*.ttf` (the bundled fonts, embedded in the exe, ~2.3MB — written
//!   out for libass — and the creator's own `.ttf`/`.otf`, see [`crate::fonts`])
//!
//! Everything is cached; later runs are fully offline. Every download goes
//! through [`crate::download`] (resumed, retried, checked). In serve mode
//! the engine fetches what is missing by itself once it is up; a job that
//! needs something first waits for it. If a download fails for good (offline
//! machine), callers fall back gracefully: system PATH ffmpeg, center-crop
//! framing, heuristic scoring.
//!
//! `DIGICLIP_HOME` moves the whole folder (tests, trying a fresh install).

use std::path::{Path, PathBuf};

use crate::download::Hooks;

pub const FFMPEG_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";
pub const YUNET_URL: &str = "https://github.com/opencv/opencv_zoo/raw/main/models/face_detection_yunet/face_detection_yunet_2026may.onnx";
pub const YUNET_FILE: &str = "yunet_2026may.onnx";
const YUNET_MIN_BYTES: u64 = 100_001;
/// `<this>/<os>/<cpu>/release/<tool>.zip`: one static binary per archive.
const FFMPEG_UNIX_URL: &str = "https://ffmpeg.martin-riedl.de/redirect/latest";

/// Root of the provisioned user-data dir.
pub fn root() -> PathBuf {
    if let Some(home) = std::env::var_os("DIGICLIP_HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(home);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("digiclip")
}

pub fn bin_dir() -> PathBuf {
    root().join("bin")
}

/// The data dir the engine runs with, once `--data-dir` has set it.
static DATA_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Make `dir` the data dir for this process (serve does it at boot): the
/// fonts folder, and so what libass is given, follows it.
pub fn set_data_dir(dir: PathBuf) {
    let _ = DATA_DIR.set(dir);
}

/// The data dir in use: `--data-dir`, else [`root`].
pub fn data_dir() -> PathBuf {
    DATA_DIR.get().cloned().unwrap_or_else(root)
}

/// The one folder libass reads fonts from: the bundled fonts and the
/// creator's own (see [`crate::fonts`]), under the data dir in use.
pub fn fonts_dir() -> PathBuf {
    data_dir().join("fonts")
}

pub fn yunet_path() -> PathBuf {
    root().join(YUNET_FILE)
}

/// Write the bundled fonts out for libass (no-op when already present and
/// whole). The creator's fonts are in the same folder.
pub fn ensure_fonts() -> anyhow::Result<PathBuf> {
    ensure_fonts_in(&fonts_dir())
}

/// [`ensure_fonts`] for a given folder.
///
/// One caller at a time: the server writes the fonts in the background after
/// it is up, and a render that starts meanwhile calls this too. It waits for
/// the writer (never reads a half-written file) and then finds them whole.
pub fn ensure_fonts_in(dir: &Path) -> anyhow::Result<PathBuf> {
    static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::create_dir_all(dir)?;
    for f in crate::fonts::BUNDLED {
        let p = dir.join(f.file);
        let whole =
            std::fs::metadata(&p).is_ok_and(|m| m.is_file() && m.len() == f.bytes.len() as u64);
        if !whole {
            std::fs::write(&p, f.bytes)?;
        }
    }
    Ok(dir.to_path_buf())
}

/// Ensure the YuNet face model is on disk (downloads ~230KB once).
pub async fn ensure_yunet() -> anyhow::Result<PathBuf> {
    ensure_yunet_with(Hooks::default()).await
}

pub async fn ensure_yunet_with(hooks: Hooks<'_>) -> anyhow::Result<PathBuf> {
    let dest = yunet_path();
    crate::download::fetch(YUNET_URL, &dest, "face model", YUNET_MIN_BYTES, hooks).await?;
    Ok(dest)
}

pub fn yunet_ready() -> bool {
    std::fs::metadata(yunet_path()).is_ok_and(|m| m.is_file() && m.len() >= YUNET_MIN_BYTES)
}

/// One archive of a portable ffmpeg, and the files wanted from it
/// (`name inside the archive`, `name in bin/`).
struct Pack {
    url: String,
    zip: String,
    min_bytes: u64,
    files: Vec<(&'static str, &'static str)>,
}

/// Where a portable ffmpeg with libass comes from on this system: gyan's
/// essentials build on Windows, Martin Riedl's static builds on macOS and
/// Linux. `None`: no build for this system, the creator installs one.
fn ffmpeg_packs(os: &str, arch: &str) -> Option<Vec<Pack>> {
    match (os, arch) {
        ("windows", _) => Some(vec![Pack {
            url: FFMPEG_URL.into(),
            zip: "ffmpeg-essentials.zip".into(),
            min_bytes: 40 << 20,
            files: vec![
                ("bin/ffmpeg.exe", "ffmpeg.exe"),
                ("bin/ffprobe.exe", "ffprobe.exe"),
            ],
        }]),
        ("macos" | "linux", "aarch64" | "x86_64") => {
            let cpu = if arch == "aarch64" { "arm64" } else { "amd64" };
            Some(
                ["ffmpeg", "ffprobe"]
                    .into_iter()
                    .map(|tool| Pack {
                        url: format!("{FFMPEG_UNIX_URL}/{os}/{cpu}/release/{tool}.zip"),
                        zip: format!("{tool}.zip"),
                        min_bytes: 8 << 20,
                        files: vec![(tool, tool)],
                    })
                    .collect(),
            )
        }
        _ => None,
    }
}

/// Is there a portable ffmpeg DigiClip can fetch for this system?
pub fn ffmpeg_downloadable() -> bool {
    ffmpeg_packs(std::env::consts::OS, std::env::consts::ARCH).is_some()
}

fn exe_name(tool: &str) -> String {
    if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    }
}

/// Take `files` out of the archive into `dir`. Nothing gets its final name
/// until every file is out and whole (the archive's checksums are checked
/// as each one is read).
fn unpack(zip_path: &Path, dir: &Path, files: &[(&str, &str)]) -> anyhow::Result<()> {
    let out = unpack_staged(zip_path, dir, files);
    if out.is_err() {
        for (_, name) in files {
            let _ = std::fs::remove_file(dir.join(format!("{name}.new")));
        }
    }
    out
}

fn unpack_staged(zip_path: &Path, dir: &Path, files: &[(&str, &str)]) -> anyhow::Result<()> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(zip_path)?)?;
    let mut staged = Vec::new();
    for (inner, out) in files {
        let tail = format!("/{inner}");
        let name = zip
            .file_names()
            .find(|n| n == inner || n.ends_with(&tail))
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("the archive has no {inner}"))?;
        let tmp = dir.join(format!("{out}.new"));
        let mut entry = zip.by_name(&name)?;
        let mut f = std::fs::File::create(&tmp)?;
        std::io::copy(&mut entry, &mut f)?;
        f.sync_all()?;
        staged.push((tmp, dir.join(out)));
    }
    for (tmp, out) in staged {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&tmp, &out)?;
    }
    Ok(())
}

/// Ensure a usable ffmpeg: a provisioned, bundled or system one wins, else
/// a portable build is downloaded into `bin/` (once).
pub async fn ensure_ffmpeg() -> anyhow::Result<PathBuf> {
    ensure_ffmpeg_with(Hooks::default()).await
}

pub async fn ensure_ffmpeg_with(hooks: Hooks<'_>) -> anyhow::Result<PathBuf> {
    if let Some(p) = crate::binaries::resolve("ffmpeg") {
        return Ok(p);
    }
    ensure_ffmpeg_in(&bin_dir(), hooks).await
}

/// Download and unpack the portable ffmpeg (and ffprobe) into `dir`.
pub async fn ensure_ffmpeg_in(dir: &Path, hooks: Hooks<'_>) -> anyhow::Result<PathBuf> {
    let ffmpeg = dir.join(exe_name("ffmpeg"));
    let ffprobe = dir.join(exe_name("ffprobe"));
    let _one = crate::download::lock(&dir.join("ffmpeg.install"), hooks.cancel).await?;
    if ffmpeg.is_file() && ffprobe.is_file() {
        return Ok(ffmpeg);
    }
    let Some(packs) = ffmpeg_packs(std::env::consts::OS, std::env::consts::ARCH) else {
        anyhow::bail!(
            "ffmpeg not found (checked provisioned bin/ and PATH), and there is no portable build \
             for this system to download. Install it first: macOS `brew install ffmpeg`, \
             Debian/Ubuntu `sudo apt install ffmpeg`, Fedora `sudo dnf install ffmpeg`. It must \
             include libass (`ffmpeg -hide_banner -buildconf | grep libass`) or caption burn-in fails."
        );
    };
    tracing::info!("ffmpeg not found — downloading a portable build (once)…");
    std::fs::create_dir_all(dir)?;
    let n = packs.len() as u64;
    for (i, pack) in packs.iter().enumerate() {
        // One bar over all the archives (they are near enough the same size).
        let over_all = move |done: u64, total: u64| {
            if let Some(p) = hooks.progress {
                p(i as u64 * total + done, n * total);
            }
        };
        let hooks = Hooks {
            progress: Some(&over_all),
            ..hooks
        };
        let zip_path = dir.join(&pack.zip);
        let mut second_try = false;
        loop {
            crate::download::fetch(&pack.url, &zip_path, "ffmpeg", pack.min_bytes, hooks).await?;
            let (z, d, files) = (zip_path.clone(), dir.to_path_buf(), pack.files.clone());
            let out = tokio::task::spawn_blocking(move || unpack(&z, &d, &files)).await?;
            let _ = std::fs::remove_file(&zip_path);
            match out {
                Ok(()) => break,
                // A damaged archive: fetch it once more from nothing.
                Err(e) if !second_try => {
                    tracing::warn!(
                        "ffmpeg: the archive could not be unpacked ({e}) — fetching it again"
                    );
                    second_try = true;
                }
                Err(e) => {
                    anyhow::bail!("ffmpeg: the downloaded archive could not be unpacked: {e}")
                }
            }
        }
    }
    if !ffmpeg.is_file() || !ffprobe.is_file() {
        anyhow::bail!("ffmpeg was downloaded but is not in {}", dir.display());
    }
    Ok(ffmpeg)
}

/// Prefetch everything (ffmpeg + fonts + YuNet + STT model), then report.
pub async fn provision_all(model: &str) -> anyhow::Result<()> {
    ensure_fonts()?;
    println!("fonts: {}", fonts_dir().display());
    let ff = ensure_ffmpeg().await?;
    println!("ffmpeg: {}", ff.display());
    let yu = ensure_yunet().await?;
    println!(
        "yunet: {} ({} bytes)",
        yu.display(),
        std::fs::metadata(&yu).map(|m| m.len()).unwrap_or(0)
    );
    if !crate::models::is_downloaded(model) {
        crate::models::download(model).await?;
    }
    println!(
        "model {model}: {}",
        crate::models::file_for(model)?.display()
    );
    println!("provisioned: {}", root().display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("digiclip-prov-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn zip_of(path: &Path, entries: &[(&str, &[u8])]) {
        let mut z = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            z.start_file(*name, stored).unwrap();
            z.write_all(bytes).unwrap();
        }
        z.finish().unwrap();
    }

    #[test]
    fn unpack_takes_the_wanted_files_under_their_final_names() {
        let d = scratch("unpack");
        let zip = d.join("a.zip");
        zip_of(
            &zip,
            &[
                ("ffmpeg-8.0-essentials_build/README.txt", b"hello"),
                ("ffmpeg-8.0-essentials_build/bin/ffmpeg.exe", b"FFMPEG"),
                ("ffmpeg-8.0-essentials_build/bin/ffplay.exe", b"FFPLAY"),
                ("ffmpeg-8.0-essentials_build/bin/ffprobe.exe", b"FFPROBE"),
            ],
        );
        unpack(
            &zip,
            &d,
            &[
                ("bin/ffmpeg.exe", "ffmpeg.exe"),
                ("bin/ffprobe.exe", "ffprobe.exe"),
            ],
        )
        .unwrap();
        assert_eq!(std::fs::read(d.join("ffmpeg.exe")).unwrap(), b"FFMPEG");
        assert_eq!(std::fs::read(d.join("ffprobe.exe")).unwrap(), b"FFPROBE");
        assert!(!d.join("ffplay.exe").exists());
        assert!(!d.join("ffmpeg.exe.new").exists());
        // One binary at the root of its own archive (the macOS / Linux builds).
        let one = d.join("b.zip");
        zip_of(&one, &[("ffmpeg", b"ELF")]);
        unpack(&one, &d, &[("ffmpeg", "ffmpeg")]).unwrap();
        assert_eq!(std::fs::read(d.join("ffmpeg")).unwrap(), b"ELF");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn an_archive_that_is_short_a_file_or_damaged_installs_nothing() {
        let d = scratch("broken");
        let zip = d.join("a.zip");
        zip_of(&zip, &[("x/bin/ffmpeg.exe", b"FFMPEG")]);
        let wanted = [
            ("bin/ffmpeg.exe", "ffmpeg.exe"),
            ("bin/ffprobe.exe", "ffprobe.exe"),
        ];
        let e = unpack(&zip, &d, &wanted).unwrap_err();
        assert!(e.to_string().contains("ffprobe"), "{e}");
        assert!(!d.join("ffmpeg.exe").exists());
        assert!(!d.join("ffmpeg.exe.new").exists());

        // A flipped byte inside a stored file: the archive's checksum catches it.
        let body = vec![7u8; 4000];
        zip_of(
            &zip,
            &[("bin/ffmpeg.exe", &body), ("bin/ffprobe.exe", &body)],
        );
        let mut raw = std::fs::read(&zip).unwrap();
        let at = raw.windows(4).position(|w| w == [7, 7, 7, 7]).unwrap() + 100;
        raw[at] = 9;
        std::fs::write(&zip, raw).unwrap();
        assert!(unpack(&zip, &d, &wanted).is_err());
        assert!(!d.join("ffmpeg.exe").exists());
        assert!(!d.join("ffprobe.exe").exists());
        assert!(!d.join("ffmpeg.exe.new").exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn every_system_the_app_ships_for_has_a_portable_ffmpeg() {
        let win = ffmpeg_packs("windows", "x86_64").unwrap();
        assert_eq!(win.len(), 1);
        assert_eq!(win[0].files.len(), 2);
        for (os, arch, cpu) in [
            ("macos", "aarch64", "arm64"),
            ("macos", "x86_64", "amd64"),
            ("linux", "x86_64", "amd64"),
            ("linux", "aarch64", "arm64"),
        ] {
            let packs = ffmpeg_packs(os, arch).unwrap();
            assert_eq!(packs.len(), 2, "{os} {arch}");
            assert!(packs[0]
                .url
                .ends_with(&format!("/{os}/{cpu}/release/ffmpeg.zip")));
            assert!(packs[1]
                .url
                .ends_with(&format!("/{os}/{cpu}/release/ffprobe.zip")));
        }
        assert!(ffmpeg_packs("freebsd", "x86_64").is_none());
        assert!(ffmpeg_packs("linux", "riscv64").is_none());
    }
}
