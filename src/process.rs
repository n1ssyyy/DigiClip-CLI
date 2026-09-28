//! Hidden child processes on Windows.
//!
//! Every console binary this engine spawns (ffmpeg, ffprobe, whisper-cli,
//! nvidia-smi, powershell, cmd shims…) opens its own console window on
//! Windows by default. When the engine itself runs headless under the
//! Tauri shell that means a strobing terminal per job — so every spawn
//! goes through [`command`], which sets `CREATE_NO_WINDOW` on Windows.
//! No-op on macOS/Linux.
//!
//! Every child is also *gentle* by default: below-normal CPU priority and
//! low I/O priority (see [`gentle_self`] and [`gentle`]). Rendering pulls
//! gigabytes through the disk; at normal I/O priority that starves the
//! desktop and the machine stutters or freezes. At low priority the job
//! still gets the whole disk when nothing else wants it.
//! `DIGICLIP_PRIORITY=normal` opts out.

/// `CREATE_NO_WINDOW` (0x08000000): child gets no console window.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// `BELOW_NORMAL_PRIORITY_CLASS` (0x00004000).
#[cfg(target_os = "windows")]
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

/// Gentle mode (the default): the user opted out with
/// `DIGICLIP_PRIORITY=normal`?
pub fn gentle_on() -> bool {
    !std::env::var("DIGICLIP_PRIORITY").is_ok_and(|v| v.eq_ignore_ascii_case("normal"))
}

/// Build a child-process command that never opens a visible console
/// window on Windows (and starts below normal CPU priority in gentle
/// mode). Drop-in for `std::process::Command::new`.
pub fn command<S: AsRef<std::ffi::OsStr>>(program: S) -> std::process::Command {
    #[allow(unused_mut)] // only Windows sets creation flags
    let mut cmd = std::process::Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let prio = if gentle_on() {
            BELOW_NORMAL_PRIORITY_CLASS
        } else {
            0
        };
        cmd.creation_flags(CREATE_NO_WINDOW | prio);
    }
    cmd
}

#[cfg(target_os = "windows")]
mod win {
    use std::ffi::c_void;
    /// `ProcessIoPriority` information class; 1 = IoPriorityLow.
    pub const PROCESS_IO_PRIORITY: i32 = 33;
    pub const IO_PRIORITY_LOW: u32 = 1;
    extern "system" {
        pub fn GetCurrentProcess() -> *mut c_void;
        pub fn SetPriorityClass(h: *mut c_void, class: u32) -> i32;
        pub fn NtSetInformationProcess(
            h: *mut c_void,
            class: i32,
            buf: *const u32,
            len: u32,
        ) -> i32;
    }
    /// Low I/O priority for a process handle (best effort).
    pub fn low_io(h: *mut c_void) {
        let v = IO_PRIORITY_LOW;
        // SAFETY: `h` is a live process handle we own (ourselves or a
        // child we just spawned); the buffer is a u32 as the class expects.
        unsafe {
            NtSetInformationProcess(h, PROCESS_IO_PRIORITY, &v, 4);
        }
    }
}

/// Make this process gentle: below-normal CPU and low I/O priority.
/// Called once at startup (CLI and serve). No-op off Windows or when
/// opted out.
pub fn gentle_self() {
    #[cfg(target_os = "windows")]
    if gentle_on() {
        // SAFETY: the pseudo-handle from GetCurrentProcess is always valid.
        unsafe {
            let me = win::GetCurrentProcess();
            win::SetPriorityClass(me, BELOW_NORMAL_PRIORITY_CLASS);
            win::low_io(me);
        }
    }
}

/// Low I/O priority for a spawned child (its CPU priority is set at
/// creation by [`command`]). No-op off Windows or when opted out.
pub fn gentle(child: &std::process::Child) {
    #[cfg(target_os = "windows")]
    if gentle_on() {
        use std::os::windows::io::AsRawHandle;
        win::low_io(child.as_raw_handle());
    }
    #[cfg(not(target_os = "windows"))]
    let _ = child;
}
