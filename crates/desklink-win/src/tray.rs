//! Notification-area (tray) icon. Double-click opens the settings window;
//! right-click shows Open / Disable or Enable / Check for updates (or Restart
//! to update) / Exit. Runs its own thread and window, and comes back if
//! Explorer restarts.

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use std::cell::RefCell;
use std::sync::Mutex;
use std::thread::JoinHandle;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ON_ICO: &[u8] = include_bytes!("../assets/on.ico");
const IDLE_ICO: &[u8] = include_bytes!("../assets/idle.ico");
const OFF_ICO: &[u8] = include_bytes!("../assets/off.ico");

const WM_TRAY: u32 = WM_APP + 1;
const WM_UPDATE: u32 = WM_APP + 2;
const WM_REMOVE: u32 = WM_APP + 3;
const WM_BALLOON: u32 = WM_APP + 4;
const ICON_ID: u32 = 1;
/// Retries adding the icon until the taskbar takes it (at sign-in DeskLink
/// may start before the taskbar exists).
const RETRY_TIMER: usize = 1;
/// Enter or Space on the focused icon (NIN_SELECT | NINF_KEY).
const NIN_KEYSELECT: u32 = 0x401;
const ID_OPEN: usize = 1;
const ID_TOGGLE: usize = 2;
const ID_EXIT: usize = 3;
const ID_CHECK_UPDATE: usize = 4;
const ID_APPLY_UPDATE: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    /// Double-click or "Open DeskLink".
    Open,
    /// "Disable" / "Enable".
    Toggle,
    /// "Check for updates".
    CheckUpdate,
    /// "Restart to update": install the downloaded update.
    ApplyUpdate,
    Exit,
}

/// What the icon shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayState {
    /// Light blue: connected to at least one PC.
    Connected,
    /// Dark blue: running, no PC connected.
    Waiting,
    /// Grey: sharing disabled from the menu.
    Disabled,
}

/// Latest state and tooltip from `set_status`, picked up by the tray thread.
static PENDING: Mutex<Option<(TrayState, String)>> = Mutex::new(None);
/// A downloaded update waiting for a restart: its version, for the menu.
static UPDATE_READY: Mutex<Option<String>> = Mutex::new(None);
/// Balloon to show (title, text), picked up by the tray thread.
static BALLOON: Mutex<Option<(String, String)>> = Mutex::new(None);

/// What other threads may do with the tray (the self-update's thread).
#[derive(Clone, Copy)]
pub struct TrayHandle(usize);

impl TrayHandle {
    /// A downloaded update (its version) puts "Restart to update" in the
    /// menu; `None` takes it out.
    pub fn set_update_ready(&self, version: Option<String>) {
        *UPDATE_READY.lock().unwrap_or_else(|e| e.into_inner()) = version;
    }

    /// A notification balloon from the icon.
    pub fn notify(&self, title: &str, text: &str) {
        *BALLOON.lock().unwrap_or_else(|e| e.into_inner()) = Some((title.to_string(), text.to_string()));
        unsafe { PostMessageW(self.0 as HWND, WM_BALLOON, 0, 0) };
    }
}

pub struct Tray {
    hwnd: usize,
    thread: Option<JoinHandle<()>>,
    events: Receiver<TrayEvent>,
}

impl Tray {
    pub fn start() -> Result<Tray, String> {
        let (tx, rx) = unbounded();
        let (ready_tx, ready_rx) = bounded(1);
        let thread = std::thread::Builder::new()
            .name("dl-tray".into())
            .spawn(move || run(tx, ready_tx))
            .map_err(|e| e.to_string())?;
        let hwnd = ready_rx.recv().map_err(|_| "tray thread died".to_string())??;
        Ok(Tray { hwnd, thread: Some(thread), events: rx })
    }

    pub fn events(&self) -> &Receiver<TrayEvent> {
        &self.events
    }

    pub fn handle(&self) -> TrayHandle {
        TrayHandle(self.hwnd)
    }

    /// Icon colour and tooltip ("Enable" in the menu while disabled).
    pub fn set_status(&self, state: TrayState, tooltip: &str) {
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((state, tooltip.to_string()));
        unsafe { PostMessageW(self.hwnd as HWND, WM_UPDATE, 0, 0) };
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // Remove the icon now; Windows only notices a dead process's icon
        // when the mouse passes over it.
        unsafe { PostMessageW(self.hwnd as HWND, WM_REMOVE, 0, 0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct State {
    tx: Sender<TrayEvent>,
    status: TrayState,
    tip: String,
    /// Connected, waiting, disabled.
    icons: [HICON; 3],
    taskbar_created: u32,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn run(tx: Sender<TrayEvent>, ready: Sender<Result<usize, String>>) {
    follow_system_dark_mode();
    let class = wide("DeskLinkTray");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
        lpszClassName: class.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    unsafe { RegisterClassW(&wc) };
    // A real (hidden) top-level window: message-only windows don't get the
    // TaskbarCreated broadcast, and the menu needs a window to own it.
    let hwnd = unsafe {
        CreateWindowExW(0, class.as_ptr(), wide("").as_ptr(), WS_OVERLAPPED, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), wc.hInstance, std::ptr::null())
    };
    if hwnd.is_null() {
        let _ = ready.send(Err(format!("tray window: {}", std::io::Error::last_os_error())));
        return;
    }
    // Windows applies the show mode DeskLink was started with to the first
    // window it shows. The elevated copy is started hidden (no console
    // flash), which would hide the first dialog (updates, a message). Use it
    // up on this window, which stays hidden anyway.
    unsafe { ShowWindow(hwnd, SW_HIDE) };
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) };
    STATE.set(Some(State {
        tx,
        status: TrayState::Waiting,
        tip: "DeskLink".into(),
        icons: [ON_ICO, IDLE_ICO, OFF_ICO].map(|ico| icon_from_ico(ico, size)),
        taskbar_created: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
    }));
    // Running as administrator, Windows blocks messages from Explorer (a
    // normal program) unless let through: its "taskbar created" broadcast,
    // and the icon's clicks.
    STATE.with_borrow(|s| {
        if let Some(s) = s {
            for m in [s.taskbar_created, WM_TRAY] {
                unsafe { ChangeWindowMessageFilterEx(hwnd, m, MSGFLT_ALLOW, std::ptr::null_mut()) };
            }
        }
    });
    show_icon(hwnd);
    let _ = ready.send(Ok(hwnd as usize));

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    if let Some(s) = STATE.take() {
        for icon in s.icons {
            unsafe { DestroyIcon(icon) };
        }
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = ICON_ID;
    nid
}

/// Icon, tooltip and callback for the current state.
fn filled(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = icon_data(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_TRAY;
    STATE.with_borrow(|s| {
        let s = s.as_ref().unwrap();
        nid.hIcon = s.icons[match s.status {
            TrayState::Connected => 0,
            TrayState::Waiting => 1,
            TrayState::Disabled => 2,
        }];
        // Room for 127 characters plus the terminator.
        for (d, c) in nid.szTip.iter_mut().take(127).zip(s.tip.encode_utf16()) {
            *d = c;
        }
    });
    nid
}

/// Show the icon in its current state; if the taskbar won't take it yet
/// (not there, or busy rebuilding after a display change), keep trying
/// every 2 s.
fn show_icon(hwnd: HWND) {
    if update_or_add_icon(hwnd) {
        unsafe { KillTimer(hwnd, RETRY_TIMER) };
    } else {
        unsafe { SetTimer(hwnd, RETRY_TIMER, 2000, None) };
    }
}

/// Update the icon if the taskbar has it, else add it. (Adding one that
/// exists fails, so a failed update mustn't be taken to mean it's gone.)
fn update_or_add_icon(hwnd: HWND) -> bool {
    let mut nid = filled(hwnd);
    unsafe {
        if Shell_NotifyIconW(NIM_MODIFY, &nid) != 0 {
            return true;
        }
        if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
            return false;
        }
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &nid);
    }
    true
}

fn send(ev: TrayEvent) {
    STATE.with_borrow(|s| {
        if let Some(s) = s {
            let _ = s.tx.send(ev);
        }
    });
}

fn show_menu(hwnd: HWND, x: i32, y: i32) {
    let enabled = STATE.with_borrow(|s| s.as_ref().is_some_and(|s| s.status != TrayState::Disabled));
    unsafe {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, ID_OPEN, wide("Open DeskLink").as_ptr());
        SetMenuDefaultItem(menu, ID_OPEN as u32, 0);
        AppendMenuW(menu, MF_STRING, ID_TOGGLE, wide(if enabled { "Disable" } else { "Enable" }).as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        match UPDATE_READY.lock().unwrap_or_else(|e| e.into_inner()).as_deref() {
            Some(v) => AppendMenuW(menu, MF_STRING, ID_APPLY_UPDATE, wide(&format!("Restart to update ({v})")).as_ptr()),
            None => AppendMenuW(menu, MF_STRING, ID_CHECK_UPDATE, wide("Check for updates").as_ptr()),
        };
        AppendMenuW(menu, MF_STRING, ID_EXIT, wide("Exit").as_ptr());
        // Without this the menu doesn't close when clicking elsewhere.
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, x, y, 0, hwnd, std::ptr::null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match cmd as usize {
            ID_OPEN => send(TrayEvent::Open),
            ID_TOGGLE => send(TrayEvent::Toggle),
            ID_CHECK_UPDATE => send(TrayEvent::CheckUpdate),
            ID_APPLY_UPDATE => send(TrayEvent::ApplyUpdate),
            ID_EXIT => send(TrayEvent::Exit),
            _ => {}
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            // NOTIFYICON_VERSION_4: the event is in LOWORD(lParam), the
            // anchor point in wParam.
            match (lp & 0xFFFF) as u32 {
                WM_LBUTTONDBLCLK | NIN_KEYSELECT => send(TrayEvent::Open),
                WM_CONTEXTMENU => {
                    let (x, y) = ((wp & 0xFFFF) as i16 as i32, ((wp >> 16) & 0xFFFF) as i16 as i32);
                    show_menu(hwnd, x, y);
                }
                _ => {}
            }
            0
        }
        WM_UPDATE => {
            let pending = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some((status, tip)) = pending {
                STATE.with_borrow_mut(|s| {
                    if let Some(s) = s {
                        s.status = status;
                        s.tip = tip;
                    }
                });
            }
            // Retried until the taskbar takes it, so the state can't get stuck.
            show_icon(hwnd);
            0
        }
        WM_BALLOON => {
            let pending = BALLOON.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some((title, text)) = pending {
                let mut nid = icon_data(hwnd);
                nid.uFlags = NIF_INFO;
                nid.dwInfoFlags = NIIF_INFO;
                for (d, c) in nid.szInfoTitle.iter_mut().take(63).zip(title.encode_utf16()) {
                    *d = c;
                }
                for (d, c) in nid.szInfo.iter_mut().take(255).zip(text.encode_utf16()) {
                    *d = c;
                }
                unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
            }
            0
        }
        WM_REMOVE => {
            let nid = icon_data(hwnd);
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &nid);
                DestroyWindow(hwnd);
                PostQuitMessage(0);
            }
            0
        }
        WM_TIMER if wp == RETRY_TIMER => {
            show_icon(hwnd);
            0
        }
        _ if STATE.with_borrow(|s| s.as_ref().is_some_and(|s| s.taskbar_created == msg)) => {
            // The taskbar appeared (sign-in) or Explorer restarted: the
            // notification area is new and empty.
            show_icon(hwnd);
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// An icon from an .ico file's bytes, at the entry closest to `size` px.
fn icon_from_ico(ico: &[u8], size: i32) -> HICON {
    let count = ico.get(4..6).map_or(0, |b| u16::from_le_bytes([b[0], b[1]])) as usize;
    let entries: Vec<(i32, usize, usize)> = (0..count)
        .filter_map(|i| {
            let e = ico.get(6 + 16 * i..6 + 16 * (i + 1))?;
            let w = if e[0] == 0 { 256 } else { e[0] as i32 };
            let len = u32::from_le_bytes(e[8..12].try_into().ok()?) as usize;
            let off = u32::from_le_bytes(e[12..16].try_into().ok()?) as usize;
            (off + len <= ico.len()).then_some((w, off, len))
        })
        .collect();
    let best = entries
        .iter()
        .filter(|e| e.0 >= size)
        .min_by_key(|e| e.0)
        .or_else(|| entries.iter().max_by_key(|e| e.0));
    let Some(&(_, off, len)) = best else {
        return std::ptr::null_mut();
    };
    unsafe { CreateIconFromResourceEx(ico[off..].as_ptr(), len as u32, 1, 0x0003_0000, size, size, LR_DEFAULTCOLOR) }
}

/// Dark context menu when Windows apps use dark mode (uxtheme's unnamed
/// SetPreferredAppMode, ordinal 135; AllowDark = 1). Harmless if missing.
fn follow_system_dark_mode() {
    unsafe {
        let lib = LoadLibraryW(wide("uxtheme.dll").as_ptr());
        if lib.is_null() {
            return;
        }
        if let Some(f) = GetProcAddress(lib, 135 as *const u8) {
            let set_preferred_app_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set_preferred_app_mode(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_load_at_tray_sizes() {
        for ico in [ON_ICO, IDLE_ICO, OFF_ICO] {
            for size in [16, 20, 24, 32, 40] {
                let h = icon_from_ico(ico, size);
                assert!(!h.is_null(), "icon at {size}px");
                unsafe { DestroyIcon(h) };
            }
        }
    }
}
