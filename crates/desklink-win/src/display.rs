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
