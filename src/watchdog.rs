//! Parent-process watchdog.
//!
//! The desktop shell spawns the engine as a sidecar and relies on
//! `kill_on_drop`. That covers clean exits, but a hard shell crash (task
//! manager, power loss mid-boot) leaves the engine running as an orphan
//! with `resources\digiclip.exe` still open — which then makes the next
//! install/update fail with "Error opening file for writing".
//!
//! `serve` mode therefore watches the process that spawned it — and only
//! that one. Its own ancestors are irrelevant: Explorer's parent
//! (userinit.exe) exits right after login, and an installer that launches
//! the app closes a moment later; neither may take the engine down.

/// Spawn a daemon thread that exits the process when the parent dies.
pub fn watch_parent() {
    #[cfg(windows)]
    windows::watch();
    #[cfg(unix)]
    unix::watch();
}

#[cfg(unix)]
mod unix {
    /// Orphans get re-parented (to init or a subreaper), so a changed
    /// parent PID means the spawner is gone.
    pub fn watch() {
        let original = std::os::unix::process::parent_id();
        if original <= 1 {
            // Already orphaned / started by init: nothing to watch.
            return;
        }
        std::thread::Builder::new()
            .name("parent-watch".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if std::os::unix::process::parent_id() != original {
                    tracing::info!("parent process gone — exiting orphaned engine");
                    std::process::exit(0);
                }
            })
            .ok();
    }
}

#[cfg(windows)]
mod windows {
    use core::ffi::c_void;

    #[repr(C)]
    struct Pbi {
        reserved1: *mut c_void,
        peb_base_address: *mut c_void,
        reserved2: [*mut c_void; 2],
        unique_process_id: *mut c_void,
        inherited_from_unique_process_id: *mut c_void,
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const PROCESS_BASIC_INFORMATION: i32 = 0;
    const WAIT_OBJECT_0: u32 = 0;
    const STILL_ACTIVE: u32 = 259;

    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetLastError() -> u32;
        fn TerminateProcess(h: *mut c_void, code: u32) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn CloseHandle(h: *mut c_void) -> i32;
        fn WaitForSingleObject(h: *mut c_void, ms: u32) -> u32;
        fn GetExitCodeProcess(h: *mut c_void, code: *mut u32) -> i32;
        fn NtQueryInformationProcess(
            h: *mut c_void,
            class: i32,
            buf: *mut Pbi,
            len: u32,
            ret_len: *mut u32,
        ) -> i32;
    }

    /// PID of the process that created us (Win32 exposes it only through
    /// NtQueryInformationProcess; no extra crate needed).
    fn parent_pid() -> Option<u32> {
        unsafe {
            let mut pbi = core::mem::zeroed::<Pbi>();
            let status = NtQueryInformationProcess(
                GetCurrentProcess(),
                PROCESS_BASIC_INFORMATION,
                &mut pbi,
                size_of::<Pbi>() as u32,
                core::ptr::null_mut(),
            );
            let pid = pbi.inherited_from_unique_process_id as usize as u32;
            (status == 0 && pid != 0).then_some(pid)
        }
    }

    pub fn watch() {
        let Some(pid) = parent_pid() else {
            tracing::warn!("parent watchdog off: no parent pid");
            return;
        };
        // Hold a handle to the parent itself: waiting on it can't be fooled
        // by PID reuse, and it costs nothing while the parent lives.
        let handle =
            unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            // Parent already gone (or not ours to open): nothing to watch.
            let err = unsafe { GetLastError() };
            tracing::warn!("parent watchdog off: cannot open parent pid {pid} (error {err})");
            return;
        }
        tracing::info!("parent watchdog: watching pid {pid}");
        let handle = handle as usize;
        std::thread::Builder::new()
            .name("parent-watch".into())
            .spawn(move || {
                let h = handle as *mut c_void;
                // Once a second: the handle is signalled when the parent has
                // fully exited, its exit code is set as soon as it starts to.
                loop {
                    if unsafe { WaitForSingleObject(h, 1000) } == WAIT_OBJECT_0 {
                        break;
                    }
                    let mut code = STILL_ACTIVE;
                    if unsafe { GetExitCodeProcess(h, &mut code) } != 0 && code != STILL_ACTIVE {
                        break;
                    }
                }
                unsafe { CloseHandle(h) };
                // Terminate, don't unwind or log: the parent (which read our
                // output) is gone, and CRT/static teardown must never keep
                // an orphan alive holding digiclip.exe open.
                unsafe { TerminateProcess(GetCurrentProcess(), 0) };
            })
            .ok();
    }
}
