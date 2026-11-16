//! Games that hold the mouse (Unreal's Game mode, shooters): the cursor
//! stays in them. A program holds the mouse when it hides the pointer and
//! either captures the mouse (`SetCapture`) or locks the pointer inside its
//! window (`ClipCursor`). In menus (UI mode) it shows the pointer again and
//! the cursor moves freely, full screen or not.

use std::sync::Mutex;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromWindow};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, ClipCursor, GA_ROOT, GUITHREADINFO, GetAncestor, GetClassNameW, GetClipCursor, GetCursorInfo,
    GetForegroundWindow, GetGUIThreadInfo, GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

type Rect = (i32, i32, i32, i32);

/// The clip we set ourselves (it says nothing about the program).
static OURS: Mutex<Option<Rect>> = Mutex::new(None);

/// What the foreground window is doing with the mouse (for `screen`, and
/// `dl-probe`).
#[derive(Debug)]
pub struct Front {
    pub class: String,
    pub window: Rect,
    pub monitor: Rect,
    pub pointer_hidden: bool,
    /// It has the Windows mouse capture.
    pub captured: bool,
    /// It locked the pointer inside its window.
    pub locked: bool,
}

impl Front {
    /// Holding the mouse, as a game does while you play.
    pub fn holds_mouse(&self) -> bool {
        !is_shell(&self.class) && self.pointer_hidden && (self.captured || self.locked)
    }

    /// Where the cursor is kept meanwhile: the window's part of its screen.
    pub fn area(&self) -> Rect {
        let (w, m) = (self.window, self.monitor);
        (w.0.max(m.0), w.1.max(m.1), w.2.min(m.2), w.3.min(m.3))
    }
}

fn rect(r: RECT) -> Rect {
    (r.left, r.top, r.right, r.bottom)
}

/// The foreground window, unless minimised or hidden.
pub fn front() -> Option<Front> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() || IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return None;
        }
        let mut win: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut win) == 0 {
            return None;
        }
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL);
        if monitor.is_null() {
            return None;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        let mut cursor: CURSORINFO = std::mem::zeroed();
        cursor.cbSize = size_of::<CURSORINFO>() as u32;
        let pointer_hidden = GetCursorInfo(&mut cursor) != 0 && (cursor.flags & CURSOR_SHOWING == 0 || cursor.hCursor.is_null());
        // Mouse capture is per thread: ask the window's own thread.
        let thread = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());
        let mut gui: GUITHREADINFO = std::mem::zeroed();
        gui.cbSize = size_of::<GUITHREADINFO>() as u32;
        let captured = GetGUIThreadInfo(thread, &mut gui) != 0
            && !gui.hwndCapture.is_null()
            && GetAncestor(gui.hwndCapture, GA_ROOT) == hwnd;
        let window = rect(win);
        let clip = rect(current());
        let inside = clip.0 >= window.0 && clip.1 >= window.1 && clip.2 <= window.2 && clip.3 <= window.3;
        // Unclipped, the clip is the whole desktop (which a full-screen window
        // on a one-screen PC also covers).
        let whole = {
            let (x, y) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
            clip == (x, y, x + GetSystemMetrics(SM_CXVIRTUALSCREEN), y + GetSystemMetrics(SM_CYVIRTUALSCREEN))
        };
        let locked = inside && !whole && *OURS.lock().unwrap() != Some(clip);
        Some(Front { class: class_name(hwnd), window, monitor: rect(info.rcMonitor), pointer_hidden, captured, locked })
    }
}

/// Where to keep the cursor while the program in front holds the mouse.
pub fn screen() -> Option<Rect> {
    front().filter(Front::holds_mouse).map(|f| f.area())
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// The desktop and the taskbar aren't games.
fn is_shell(class: &str) -> bool {
    matches!(class, "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")
}

/// Keeps the cursor in a program that holds the mouse.
#[derive(Default)]
pub struct Confine {
    rect: Option<RECT>,
}

impl Confine {
    /// Confine to `area`, or release (`None`). Re-applied if something
    /// else changed the clip (the program itself, or Windows on a switch).
    pub fn set(&mut self, screen: Option<Rect>) {
        match screen {
            Some((left, top, right, bottom)) => {
                let want = RECT { left, top, right, bottom };
                let now = current();
                if same(&now, &want) {
                    self.rect = Some(want);
                } else if inside(&now, &want) {
                    // The program's own, smaller clip: leave it alone (and
                    // don't undo it later).
                    self.rect = None;
                } else {
                    unsafe { ClipCursor(&want) };
                    *OURS.lock().unwrap() = Some(rect(want));
                    self.rect = Some(want);
                }
            }
            None => self.release(),
        }
    }

    pub fn is_on(&self) -> bool {
        self.rect.is_some()
    }

    pub fn release(&mut self) {
        // Only undo our own clip.
        if let Some(r) = self.rect.take()
            && same(&current(), &r)
        {
            unsafe { ClipCursor(std::ptr::null()) };
            *OURS.lock().unwrap() = None;
        }
    }
}

impl Drop for Confine {
    fn drop(&mut self) {
        self.release();
    }
}

fn current() -> RECT {
    let mut r: RECT = unsafe { std::mem::zeroed() };
    unsafe { GetClipCursor(&mut r) };
    r
}
fn same(a: &RECT, b: &RECT) -> bool {
    (a.left, a.top, a.right, a.bottom) == (b.left, b.top, b.right, b.bottom)
}
fn inside(a: &RECT, b: &RECT) -> bool {
    a.left >= b.left && a.top >= b.top && a.right <= b.right && a.bottom <= b.bottom
}
