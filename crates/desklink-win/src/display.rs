//! Monitor enumeration (physical pixels; requires per-monitor DPI awareness).

use desklink_proto::Monitor;
use windows_sys::Win32::Foundation::{LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn cb(hmon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> i32 {
        let out = unsafe { &mut *(data as *mut Vec<Monitor>) };
        let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(hmon, &mut info as *mut _ as *mut _) } == 0 {
            return 1;
        }
        let (mut dx, mut dy) = (96u32, 96u32);
        unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
        let r = info.monitorInfo.rcMonitor;
        let name_len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
        out.push(Monitor {
            x: r.left,
            y: r.top,
            width: r.right - r.left,
            height: r.bottom - r.top,
            scale_percent: (dx * 100 / 96) as u16,
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            name: String::from_utf16_lossy(&info.szDevice[..name_len]),
        });
        1
    }

    let mut out: Vec<Monitor> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(cb),
            &mut out as *mut _ as LPARAM,
        )
    };
    out
}

/// Where to put a window of `width` x `height` logical pixels (client area,
/// plus `title_bar` logical px of frame on top) so it is centred in the work
/// area (screen minus taskbar) of the screen DeskLink's tray icon is on: the
/// window's top-left corner in physical pixels. If the icon can't be found
/// (hidden in the overflow area), the screen of the main taskbar.
pub fn centred_on_tray_screen(width: f32, height: f32, title_bar: f32) -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint};
    use windows_sys::Win32::UI::Shell::{NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect};
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowRect};
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let centre = |r: RECT| POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 };
    let mut at = None;
    let tray = unsafe { FindWindowW(wide(crate::tray::WINDOW_CLASS).as_ptr(), std::ptr::null()) };
    if !tray.is_null() {
        let mut id: NOTIFYICONIDENTIFIER = unsafe { std::mem::zeroed() };
        id.cbSize = size_of::<NOTIFYICONIDENTIFIER>() as u32;
        id.hWnd = tray;
        id.uID = crate::tray::ICON_ID;
        let mut r: RECT = unsafe { std::mem::zeroed() };
        if unsafe { Shell_NotifyIconGetRect(&id, &mut r) } == 0 && r.right > r.left {
            at = Some(centre(r));
        }
    }
    if at.is_none() {
        let taskbar = unsafe { FindWindowW(wide("Shell_TrayWnd").as_ptr(), std::ptr::null()) };
        let mut r: RECT = unsafe { std::mem::zeroed() };
        if !taskbar.is_null() && unsafe { GetWindowRect(taskbar, &mut r) } != 0 {
            at = Some(centre(r));
        }
    }
    let hmon = unsafe { MonitorFromPoint(at.unwrap_or(POINT { x: 0, y: 0 }), MONITOR_DEFAULTTOPRIMARY) };
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(hmon, &mut info) } == 0 {
        return None;
    }
    let (mut dx, mut dy) = (96u32, 96u32);
    unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
    let scale = dx as f32 / 96.0;
    let (w, h) = ((width * scale) as i32, ((height + title_bar) * scale) as i32);
    let r = info.rcWork;
    // The position includes Windows' invisible resize border on the left
    // (about 7 logical px), outside the visible frame.
    let border = (7.0 * scale) as i32;
    Some((r.left + (r.right - r.left - w) / 2 - border, r.top + (r.bottom - r.top - h) / 2))
}
