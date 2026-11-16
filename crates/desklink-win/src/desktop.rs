//! Screens DeskLink can't type into: a screensaver, the lock screen, a UAC
//! prompt. Windows shows them on their own desktops, which refuse input from
//! programs (also elevated ones), so a controlling PC would be stuck here.

use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_READOBJECTS, DESKTOP_WRITEOBJECTS, EnumDesktopWindows, GetUserObjectInformationW, HDESK,
    OpenDesktopW, OpenInputDesktop, UOI_NAME,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, IsWindowVisible, PostMessageW, SPI_GETSCREENSAVERRUNNING, SystemParametersInfoW, WM_CLOSE,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Does keyboard and mouse input go to a desktop other than ours (the
/// sign-in screen, a secure screensaver, UAC)? Those can't even be opened.
pub fn input_elsewhere() -> bool {
    let desk = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if desk.is_null() {
        return true;
    }
    let name = desktop_name(desk);
    unsafe { CloseDesktop(desk) };
    !name.eq_ignore_ascii_case("Default")
}

fn desktop_name(desk: HDESK) -> String {
    let mut buf = [0u16; 64];
    let mut needed = 0u32;
    let ok = unsafe { GetUserObjectInformationW(desk, UOI_NAME, buf.as_mut_ptr().cast(), (buf.len() * 2) as u32, &mut needed) };
    if ok == 0 {
        return String::new();
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

pub fn screensaver_running() -> bool {
    let mut running = 0i32;
    let ok = unsafe { SystemParametersInfoW(SPI_GETSCREENSAVERRUNNING, 0, (&mut running as *mut i32).cast(), 0) };
    ok != 0 && running != 0
}

/// Close a running screensaver: Microsoft's documented way (close its
/// windows on its own desktop, or the window in front if it has none), and a
/// small mouse move sent from inside its desktop. If it asks for the password
/// on resume, the lock screen follows. Says what happened, for the log.
pub fn close_screensaver() -> String {
    unsafe extern "system" fn close(hwnd: HWND, _: LPARAM) -> i32 {
        unsafe {
            if IsWindowVisible(hwnd) != 0 {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
        }
        1
    }
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::StationsAndDesktops::DESKTOP_JOURNALPLAYBACK;
    unsafe {
        let name = wide("Screen-saver");
        let mut desk = OpenDesktopW(name.as_ptr(), 0, 0, DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS | DESKTOP_JOURNALPLAYBACK);
        if desk.is_null() {
            desk = OpenDesktopW(name.as_ptr(), 0, 0, DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS);
        }
        if desk.is_null() {
            let error = GetLastError();
            PostMessageW(GetForegroundWindow(), WM_CLOSE, 0, 0);
            return format!("its desktop can't be opened (error {error}); asked the window in front to close");
        }
        let closed = EnumDesktopWindows(desk, Some(close), 0) != 0;
        // Input only reaches a desktop from a thread on it: move the mouse
        // from a fresh thread there (one without windows may switch).
        let desk_ptr = desk as usize;
        let moved = std::thread::spawn(move || {
            use windows_sys::Win32::System::StationsAndDesktops::SetThreadDesktop;
            if SetThreadDesktop(desk_ptr as HDESK) == 0 {
                return format!("couldn't join it (error {})", GetLastError());
            }
            let wiggle = |dx: i32| crate::inject::raw_move(dx, 0);
            wiggle(1);
            wiggle(-1);
            "moved the mouse there".to_string()
        })
        .join()
        .unwrap_or_else(|_| "mouse move failed".into());
        CloseDesktop(desk);
        format!("{}; {moved}", if closed { "asked its windows to close" } else { "couldn't list its windows" })
    }
}
