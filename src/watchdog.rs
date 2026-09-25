//! Parent-process watchdog.
//!
//! The desktop shell spawns the engine as a sidecar and relies on
//! `kill_on_drop`. That covers clean exits, but a hard shell crash (task
//! manager, power loss mid-boot) leaves the engine running as an orphan
//! with `resources\digiclip.exe` still open — which then makes the next
//! install/update fail with "Error opening file for writing".
//!
//! `serve` mode therefore watches its parent: if the parent process is
//! gone, exit. Cheap, platform-correct, and it makes the orphan window
//! impossible on Windows (where nothing else would reap the process).

/// Spawn a daemon thread that exits the process when the parent dies.
/// Windows only for now (the orphan problem is a Windows one; on Unix the
/// shell's process group / `kill_on_drop` behaves).
pub fn watch_parent() {
    #[cfg(windows)]
    {
        std::thread::Builder::new()
            .name("parent-watch".into())
            .spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                if !parent_alive() {
                    tracing::info!("parent process gone — exiting orphaned engine");
                    std::process::exit(0);
                }
            })
            .ok();
    }
}

#[cfg(windows)]
fn parent_alive() -> bool {
    // Query the parent PID via NtQueryInformationProcess (user32/kernel32
    // don't expose it; this is the standard NtQuery route and needs no
    // extra crate).
    #[repr(C)]
    struct Pbi {
        reserved1: *mut core::ffi::c_void,
        peb_base_address: *mut core::ffi::c_void,
        reserved2: [*mut core::ffi::c_void; 2],
        unique_process_id: *mut core::ffi::c_void,
        inherited_from_unique_process_id: *mut core::ffi::c_void,
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const PROCESS_BASIC_INFORMATION: i32 = 0;

    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut core::ffi::c_void;
        fn CloseHandle(h: *mut core::ffi::c_void) -> i32;
        fn NtQueryInformationProcess(
            h: *mut core::ffi::c_void,
            class: i32,
            buf: *mut Pbi,
            len: u32,
            ret_len: *mut u32,
        ) -> i32;
    }
    unsafe {
        let cur = std::process::id();
        // Walk the PID chain a bounded number of times to find a live
        // ancestor even if an intermediate process already exited.
        let mut pid = cur;
        for _ in 0..8 {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut pbi = core::mem::zeroed::<Pbi>();
            let ok = NtQueryInformationProcess(
                h,
                PROCESS_BASIC_INFORMATION,
                &mut pbi,
                size_of::<Pbi>() as u32,
                core::ptr::null_mut(),
            );
            CloseHandle(h);
            if ok != 0 {
                return false;
            }
            let parent = pbi.inherited_from_unique_process_id as usize;
            if parent == 0 {
                return false; // reached PID 0 (system) — chain broken
            }
            // 4 == console host: our parent is a console, not our shell;
            // treat a live console as "alive" so we don't self-exit.
            if parent == 4 {
                return true;
            }
            // Check the parent is alive (OpenProcess succeeded above for
            // the *current* pid, so probe the next iteration instead).
            pid = parent as u32;
        }
        true
    }
}
