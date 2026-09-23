//! Hidden child processes on Windows.
//!
//! Every console binary this engine spawns (ffmpeg, ffprobe, whisper-cli,
//! nvidia-smi, powershell, cmd shims…) opens its own console window on
//! Windows by default. When the engine itself runs headless under the
//! Tauri shell that means a strobing terminal per job — so every spawn
//! goes through [`command`], which sets `CREATE_NO_WINDOW` on Windows.
//! No-op on macOS/Linux.

/// `CREATE_NO_WINDOW` (0x08000000): child gets no console window.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Build a child-process command that never opens a visible console
/// window on Windows. Drop-in for `std::process::Command::new`.
pub fn command<S: AsRef<std::ffi::OsStr>>(program: S) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}
