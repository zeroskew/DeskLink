//! Session and power notifications: shutdown, logoff, sleep, display changes,
//! console close.
//!
//! Handlers that precede the machine going away (`EndSession`, `Suspend`,
//! `ConsoleClose`) carry an `ack`. The notifying thread blocks until it is
//! signalled (bounded), so peers get a goodbye while the network still works.
//! That makes the director react instantly instead of after a timeout.

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use std::cell::RefCell;
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::Duration;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Max time Windows' notifying thread waits for us to say goodbye.
const ACK_WAIT: Duration = Duration::from_millis(1000);

const ENDSESSION_LOGOFF_FLAG: isize = 0x8000_0000u32 as isize;
const PBT_APMSUSPEND: usize = 0x4;
const PBT_APMRESUMESUSPEND: usize = 0x7;
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;

pub enum SessionEvent {
    /// Shutdown or logoff is about to happen. Say goodbye, then signal `ack`.
    EndSession { logoff: bool, ack: Sender<()> },
    /// Another app cancelled the shutdown; we keep running.
    EndCancelled,
    Suspend { ack: Sender<()> },
    Resume,
    DisplayChanged,
    /// Ctrl+C / Ctrl+Break / console window closed: exit after `ack`.
    Quit { ack: Sender<()> },
}

pub struct SessionWatcher {
    rx: Receiver<SessionEvent>,
    tid: u32,
    thread: Option<JoinHandle<()>>,
}

thread_local! {
    static TX: RefCell<Option<Sender<SessionEvent>>> = const { RefCell::new(None) };
}

static CONSOLE_TX: OnceLock<Sender<SessionEvent>> = OnceLock::new();

/// Send an event that needs an acknowledgement and wait (bounded) for it.
fn send_and_wait(tx: &Sender<SessionEvent>, make: impl FnOnce(Sender<()>) -> SessionEvent) {
    let (ack, done) = bounded(1);
    if tx.send(make(ack)).is_ok() {
        let _ = done.recv_timeout(ACK_WAIT);
    }
}

impl SessionWatcher {
    pub fn start() -> Result<SessionWatcher, String> {
        let (tx, rx) = unbounded();
        let _ = CONSOLE_TX.set(tx.clone());
        unsafe { SetConsoleCtrlHandler(Some(console_handler), 1) };

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("dl-session".into())
            .spawn(move || {
                let hwnd = match create_window() {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                TX.set(Some(tx));
                let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));
                let mut msg: MSG = unsafe { std::mem::zeroed() };
                while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
                    unsafe {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                unsafe { DestroyWindow(hwnd) };
            })
            .map_err(|e| e.to_string())?;
        let tid = ready_rx.recv().map_err(|_| "session thread died".to_string())??;
        Ok(SessionWatcher { rx, tid, thread: Some(thread) })
    }

    pub fn events(&self) -> &Receiver<SessionEvent> {
        &self.rx
    }
}

impl Drop for SessionWatcher {
    fn drop(&mut self) {
        unsafe { PostThreadMessageW(self.tid, WM_QUIT, 0, 0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn create_window() -> Result<HWND, String> {
    let class: Vec<u16> = "DeskLinkSession\0".encode_utf16().collect();
    let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        lpszClassName: class.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    unsafe { RegisterClassW(&wc) };
    // A hidden *top-level* window: message-only windows don't receive the
    // shutdown and power broadcasts.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        Err(format!("session window: {}", std::io::Error::last_os_error()))
    } else {
        Ok(hwnd)
    }
}

fn with_tx(f: impl FnOnce(&Sender<SessionEvent>)) {
    TX.with_borrow(|tx| {
        if let Some(tx) = tx {
            f(tx)
        }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => {
            let logoff = lparam & ENDSESSION_LOGOFF_FLAG != 0;
            log::info!("session ending (logoff={logoff})");
            with_tx(|tx| send_and_wait(tx, |ack| SessionEvent::EndSession { logoff, ack }));
            return 1; // never block shutdown
        }
        WM_ENDSESSION => {
            if wparam == 0 {
                with_tx(|tx| {
                    let _ = tx.send(SessionEvent::EndCancelled);
                });
            }
            return 0;
        }
        WM_POWERBROADCAST => {
            match wparam {
                PBT_APMSUSPEND => {
                    log::info!("system suspending");
                    with_tx(|tx| send_and_wait(tx, |ack| SessionEvent::Suspend { ack }));
                }
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => with_tx(|tx| {
                    let _ = tx.send(SessionEvent::Resume);
                }),
                _ => {}
            }
            return 1;
        }
        WM_DISPLAYCHANGE => with_tx(|tx| {
            let _ = tx.send(SessionEvent::DisplayChanged);
        }),
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn console_handler(ctrl: u32) -> i32 {
    match ctrl {
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT => {
            if let Some(tx) = CONSOLE_TX.get() {
                send_and_wait(tx, |ack| SessionEvent::Quit { ack });
            }
            1
        }
        _ => 0,
    }
}
