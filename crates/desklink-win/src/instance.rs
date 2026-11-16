//! Talking to the running DeskLink instance: a stop request, and a watchdog
//! that restores the cursor if DeskLink dies without doing it.

use crossbeam_channel::{Receiver, bounded};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, OpenProcess, PROCESS_SYNCHRONIZE, SetEvent,
    WaitForSingleObject,
};

/// One DeskLink per Windows session (it owns the input hooks).
const STOP_EVENT: &str = "Local\\DeskLinkStop";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Listen for `desklink stop`; the receiver gets one message when asked.
pub fn listen_for_stop() -> Result<Receiver<()>, String> {
    listen_on(STOP_EVENT)
}

/// Create (or open) a named auto-reset event that this user's normal
/// programs can signal. DeskLink usually runs as administrator, and Windows
/// then keeps normal programs (`desklink stop` from a terminal, a settings
/// window the user opened) from signalling events it creates, unless they
/// carry a medium integrity label.
fn create_event(event: &str) -> HANDLE {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    let name = wide(event);
    // SYSTEM, administrators, the owner and interactive users; medium label.
    let sddl = wide("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;GA;;;IU)S:(ML;;NW;;;ME)");
    let mut sd = std::ptr::null_mut();
    let have_sd = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) } != 0;
    let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
    let h = unsafe { CreateEventW(if have_sd { &sa } else { std::ptr::null() }, 0, 0, name.as_ptr()) };
    if have_sd {
        unsafe { LocalFree(sd) };
    }
    h
}

fn listen_on(event: &str) -> Result<Receiver<()>, String> {
    let h = create_event(event);
    if h.is_null() {
        return Err(format!("stop event: {}", std::io::Error::last_os_error()));
    }
    let h = h as usize; // HANDLE isn't Send; the thread owns it from here
    let (tx, rx) = bounded(1);
    std::thread::Builder::new()
        .name("dl-stop".into())
        .spawn(move || {
            if unsafe { WaitForSingleObject(h as HANDLE, INFINITE) } == WAIT_OBJECT_0 {
                let _ = tx.send(());
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(rx)
}

/// Listen on a named event for as long as the process runs: the receiver
/// gets a message each time it is signalled.
fn listen_repeating(event: &str) -> Result<Receiver<()>, String> {
    let h = create_event(event);
    if h.is_null() {
        return Err(format!("{event}: {}", std::io::Error::last_os_error()));
    }
    let h = h as usize;
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("dl-event".into())
        .spawn(move || {
            while unsafe { WaitForSingleObject(h as HANDLE, INFINITE) } == WAIT_OBJECT_0 {
                if tx.send(()).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(rx)
}

/// Signal a named event if someone listens; false if nobody does.
fn signal(event: &str) -> bool {
    let name = wide(event);
    let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if h.is_null() {
        return false;
    }
    unsafe {
        SetEvent(h);
        CloseHandle(h);
    }
    true
}

/// The settings window asks the running DeskLink to check for an update now.
const UPDATE_CHECK_EVENT: &str = "Local\\DeskLinkUpdateCheck";
/// ... or to install the downloaded update (DeskLink restarts).
const UPDATE_APPLY_EVENT: &str = "Local\\DeskLinkUpdateApply";

/// Running DeskLink: requests to check for an update.
pub fn listen_for_update_check() -> Result<Receiver<()>, String> {
    listen_repeating(UPDATE_CHECK_EVENT)
}

/// Running DeskLink: requests to install the downloaded update.
pub fn listen_for_update_apply() -> Result<Receiver<()>, String> {
    listen_repeating(UPDATE_APPLY_EVENT)
}

/// Settings window: ask the running DeskLink to check for an update.
pub fn request_update_check() -> bool {
    signal(UPDATE_CHECK_EVENT)
}

/// Settings window: ask the running DeskLink to install its downloaded update.
pub fn request_update_apply() -> bool {
    signal(UPDATE_APPLY_EVENT)
}

/// Ask the running DeskLink to exit cleanly and wait (bounded) until it has.
/// Returns false if no DeskLink was listening.
pub fn request_stop(wait: Duration) -> bool {
    request_on(STOP_EVENT, wait)
}

fn request_on(event: &str, wait: Duration) -> bool {
    let name = wide(event);
    let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if h.is_null() {
        return false;
    }
    unsafe {
        SetEvent(h);
        CloseHandle(h);
    }
    // The event disappears when the instance has exited.
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
        if h.is_null() {
            return true;
        }
        unsafe { CloseHandle(h) };
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// Block until process `pid` has exited (or can't be watched).
pub fn wait_for_process(pid: u32) {
    let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if h.is_null() {
        return;
    }
    unsafe {
        WaitForSingleObject(h, INFINITE);
        CloseHandle(h);
    }
}


/// Windows shows the "app starting" cursor after launching a windowed app
/// until it waits for input on an empty message queue (or ~5 s pass). The
/// tray runs on another thread, so signal "ready" from the main thread now.
pub fn end_startup_cursor() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MSG, MsgWaitForMultipleObjects, PM_NOREMOVE, PeekMessageW, QS_ALLINPUT};
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    unsafe {
        PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
        // Briefly wait for input on the (empty) queue: that is "idle".
        MsgWaitForMultipleObjects(0, std::ptr::null(), 0, 1, QS_ALLINPUT);
    }
}

/// DeskLink is a windowed app; when started from a terminal, print there
/// (`desklink info`, errors).
/// Output redirected to a file or pipe (`desklink --version > v.txt`, a
/// script reading it) stays there.
pub fn attach_parent_console() {
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_DISK, FILE_TYPE_PIPE, GetFileType};
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE};
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        if !out.is_null() && out != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE && matches!(GetFileType(out), FILE_TYPE_DISK | FILE_TYPE_PIPE) {
            return;
        }
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Started inside a job that dies with its owner (terminals put what runs in
/// them in one, killed outright when the terminal closes)? Then ask Explorer
/// to start us instead, as if double-clicked: its child, outside the job.
/// True if a new copy was asked for (this one should exit).
pub fn escape_job() -> bool {
    use windows_sys::Win32::System::JobObjects::{
        IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        QueryInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;
    unsafe {
        let mut in_job = 0;
        if IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) == 0 || in_job == 0 {
            return false;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        let ok = QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        );
        let flags = info.BasicLimitInformation.LimitFlags;
        let doomed = ok != 0 && flags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0;
        let can_leave = flags & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK) != 0;
        if !doomed {
            return false;
        }
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(_) => return false,
        };
        if can_leave {
            use std::os::windows::process::CommandExt;
            const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
            return std::process::Command::new(&exe).creation_flags(CREATE_BREAKAWAY_FROM_JOB).spawn().is_ok();
        }
        // Explorer hands the request to the running shell; without a shell
        // (no taskbar) there is no one to ask, so stay.
        if FindWindowW(wide("Shell_TrayWnd").as_ptr(), std::ptr::null()).is_null() {
            return false;
        }
        std::process::Command::new("explorer.exe").arg(&exe).spawn().is_ok()
    }
}

/// Let go of the terminal we were started from: the tray app must outlive
/// it (closing a console window ends the programs attached to it).
pub fn detach_console() {
    use windows_sys::Win32::System::Console::FreeConsole;
    unsafe { FreeConsole() };
}

/// Is a DeskLink already running in this Windows session?
pub fn is_running() -> bool {
    let name = wide(STOP_EVENT);
    let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if h.is_null() {
        return false;
    }
    unsafe { CloseHandle(h) };
    true
}

/// Held by the open settings window.
pub struct SettingsLock(HANDLE);

impl Drop for SettingsLock {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// One settings window per session: `None` if one is open already (it is
/// brought to the front instead).
pub fn settings_lock(window_title: &str) -> Option<SettingsLock> {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow};
    let name = wide("Local\\DeskLinkSettings");
    let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if h.is_null() || unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
        return Some(SettingsLock(h));
    }
    unsafe { CloseHandle(h) };
    let title = wide(window_title);
    // The window may still be opening.
    for _ in 0..20 {
        let w = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
        if !w.is_null() {
            unsafe {
                if IsIconic(w) != 0 {
                    ShowWindow(w, SW_RESTORE);
                }
                SetForegroundWindow(w);
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}


/// DeskLink exiting asks the open settings window to close too.
const CLOSE_SETTINGS_EVENT: &str = "Local\\DeskLinkCloseSettings";

/// Settings window: the receiver gets a message when DeskLink exits.
pub fn listen_for_close_settings() -> Result<Receiver<()>, String> {
    listen_on(CLOSE_SETTINGS_EVENT)
}

/// DeskLink is exiting: close the settings window, if open.
pub fn close_settings() {
    let name = wide(CLOSE_SETTINGS_EVENT);
    let h = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if !h.is_null() {
        unsafe {
            SetEvent(h);
            CloseHandle(h);
        }
    }
}
/// Let the process we are about to start bring its window to the front
/// (Windows only allows that to the process the user last interacted with).
pub fn allow_foreground_handoff() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
    unsafe { AllowSetForegroundWindow(ASFW_ANY) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_request_reaches_the_listener() {
        // Its own event: the real one would stop a DeskLink running here.
        let event = format!("Local\\DeskLinkStopTest{}", std::process::id());
        let rx = listen_on(&event).expect("listen");
        // The listener keeps the event open, so this waits out its timeout.
        assert!(request_on(&event, Duration::from_millis(300)));
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok());
    }
}
