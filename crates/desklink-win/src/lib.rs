//! Windows platform layer for DeskLink.

#![cfg(windows)]

pub mod autostart;
pub mod capture;
pub mod clipboard;
pub mod cursor;
pub mod desktop;
pub mod display;
pub mod elevation;
pub mod fullscreen;
pub mod http;
pub mod dragdrop;
pub mod inject;
pub mod instance;
pub mod intent;
pub mod keymap;
pub mod pipe;
pub mod session;
pub mod tray;
pub mod transfer;
pub mod ui;
pub mod vfiles;

/// Tag placed in `dwExtraInfo` of every event we inject, so our own hooks can
/// recognise and ignore them (a machine can be director and client at once).
pub const INJECT_TAG: usize = 0x444C_4E4B; // "DLNK"

/// Opt into per-monitor DPI awareness so all coordinates (hooks, cursor, monitor
/// rects) are physical pixels. Must run before any window is created.
pub fn init_dpi_awareness() -> bool {
    use windows_sys::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != 0 }
}

static CRASH_HANDLER: std::sync::OnceLock<fn(String)> = std::sync::OnceLock::new();

/// Call `handler` with a description when the process hits a Windows
/// exception nobody handles (an access violation, ...), before it dies.
/// Rust panics don't come here; they go to the panic hook.
pub fn on_hard_crash(handler: fn(String)) {
    use windows_sys::Win32::System::Diagnostics::Debug::{EXCEPTION_POINTERS, SetUnhandledExceptionFilter};
    unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
        let what = unsafe {
            info.as_ref().and_then(|p| p.ExceptionRecord.as_ref()).map(|r| {
                format!("Windows exception 0x{:08X} at address {:?}", r.ExceptionCode as u32, r.ExceptionAddress)
            })
        };
        if let Some(handler) = CRASH_HANDLER.get() {
            handler(what.unwrap_or_else(|| "Windows exception".into()));
        }
        0 // EXCEPTION_CONTINUE_SEARCH: let Windows report and end it
    }
    let _ = CRASH_HANDLER.set(handler);
    unsafe { SetUnhandledExceptionFilter(Some(filter)) };
}

/// Windows priority class for the whole process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessPriority {
    Normal,
    AboveNormal,
    High,
}

/// Set DeskLink's priority class: it uses little CPU, but every mouse move
/// waits for it, so a busy PC must not make it wait its turn.
pub fn set_process_priority(p: ProcessPriority) {
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, HIGH_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, SetPriorityClass,
    };
    let class = match p {
        ProcessPriority::Normal => NORMAL_PRIORITY_CLASS,
        ProcessPriority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
        ProcessPriority::High => HIGH_PRIORITY_CLASS,
    };
    if unsafe { SetPriorityClass(GetCurrentProcess(), class) } == 0 {
        log::warn!("couldn't set the process priority to {p:?}: {}", std::io::Error::last_os_error());
    }
}

/// Raise the calling thread: `critical` for the input hooks (Windows holds
/// every mouse move until they return), else above normal (the main loop,
/// network; file transfers run on those too, so not higher).
pub fn raise_thread_priority(critical: bool) {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_TIME_CRITICAL,
    };
    let level = if critical { THREAD_PRIORITY_TIME_CRITICAL } else { THREAD_PRIORITY_ABOVE_NORMAL };
    unsafe { SetThreadPriority(GetCurrentThread(), level) };
}
