//! Global input capture with low-level hooks, guarded by a Raw Input watchdog.
//!
//! Design rules (each one addresses a real-world failure of existing tools):
//!
//! * The hook callbacks never block: they classify the event, push it onto an
//!   unbounded channel and return. Windows silently removes a low-level hook
//!   whose callback exceeds `LowLevelHooksTimeout`, which is how other KVMs
//!   end up with a cursor "stuck" on the client until restarted.
//! * A watchdog thread counts physical input via Raw Input. If Raw Input sees
//!   input but a hook does not, the hook was removed and is reinstalled.
//! * Grab transitions track which keys/buttons are physically down, so a key
//!   pressed on one side and released on the other never sticks anywhere.
//! * Hotkeys are matched inside the hook, so the trigger key can be swallowed
//!   and the emergency release works even if the consumer thread is wedged.
//! * If the consumer stops draining events while grabbed, the hook releases
//!   the grab by itself: local control is never lost.

use crate::INJECT_TAG;
use crate::keymap;
use desklink_proto::{InputEvent, Key, MouseButton};
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering::*};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_HEADER, RIDEV_INPUTSINK,
    RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Debug, PartialEq)]
pub enum CaptureEvent {
    /// Input to forward (only produced while grabbed).
    Input(InputEvent),
    /// The local cursor moved (only while not grabbed), physical pixels.
    CursorMoved { x: i32, y: i32 },
    /// A registered hotkey was pressed. Its trigger key was swallowed.
    Hotkey(u32),
    /// The hook released the grab on its own.
    GrabReleased(ReleaseReason),
    /// The watchdog found a hook removed by Windows and reinstalled it.
    HookRestored { mouse: bool, keyboard: bool },
    /// Physical local input happened after `watch_activity` (reported once).
    LocalActivity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseReason {
    /// A hotkey with `release_grab` fired.
    Hotkey(u32),
    /// The consumer stopped draining events.
    ConsumerStalled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub id: u32,
    /// Exact set of modifier keys (HID 0xE0..=0xE7) that must be held.
    pub modifiers: Vec<Key>,
    pub key: Key,
    /// Release the grab inside the hook when fired (return-to-director).
    pub release_grab: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub hook_mouse: u64,
    pub hook_keyboard: u64,
    pub raw_mouse: u64,
    pub raw_keyboard: u64,
    pub restores: u32,
    pub pending: usize,
}

/// Pending events at which a grab is considered abandoned by the consumer
/// (several seconds of 1 kHz mouse input).
const STALL_LIMIT: usize = 4000;
pub const DEFAULT_DRIFT_MIN_PX: u32 = 16;
pub const DEFAULT_DRIFT_WINDOW_MS: u32 = 500;
const WATCHDOG_TICK_MS: u32 = 1000;
const RESTORE_MIN_INTERVAL: Duration = Duration::from_secs(2);

const WM_APP_REINSTALL: u32 = WM_APP + 1;
const WM_APP_SYNC: u32 = WM_APP + 2;
const WM_APP_DROP_HOOKS: u32 = WM_APP + 3;
const WM_APP_REPARK: u32 = WM_APP + 4;

const REINSTALL_MOUSE: usize = 1;
const REINSTALL_KEYBOARD: usize = 2;

/// Unassigned virtual key, injected to stop a lone Win-up opening Start after
/// we swallowed the key it was combined with.
const VK_MASK: u16 = 0xE8;

struct Shared {
    tx: Sender<CaptureEvent>,
    grabbed: AtomicBool,
    anchor_x: AtomicI32,
    anchor_y: AtomicI32,
    carry_buttons: AtomicBool,
    /// The physical left button is down (whatever the routing).
    left_physical: AtomicBool,
    allow_synthesized: AtomicBool,
    hotkeys: Mutex<Vec<Hotkey>>,
    hook_mouse: AtomicU64,
    hook_keyboard: AtomicU64,
    raw_mouse: AtomicU64,
    raw_keyboard: AtomicU64,
    restores: AtomicU32,
    stall_ms: AtomicU32,
    stall_count: AtomicU32,
    /// Report the next physical mouse / keyboard input as `LocalActivity`.
    watch_mouse: AtomicBool,
    watch_buttons: AtomicBool,
    watch_keyboard: AtomicBool,
    /// Micro-drift filter: net movement needed within the window.
    drift_min_px: AtomicU32,
    drift_window_ms: AtomicU32,
}

pub struct Capture {
    shared: Arc<Shared>,
    rx: Receiver<CaptureEvent>,
    hook_tid: u32,
    watchdog_tid: u32,
    threads: Vec<JoinHandle<()>>,
}

static RUNNING: AtomicBool = AtomicBool::new(false);

impl Capture {
    /// Installs the hooks and starts the watchdog. Only one capture may run
    /// per process.
    pub fn start() -> Result<Capture, String> {
        if RUNNING.swap(true, AcqRel) {
            return Err("capture already running".into());
        }
        let (tx, rx) = unbounded();
        let shared = Arc::new(Shared {
            tx,
            grabbed: AtomicBool::new(false),
            anchor_x: AtomicI32::new(0),
            anchor_y: AtomicI32::new(0),
            carry_buttons: AtomicBool::new(false),
            left_physical: AtomicBool::new(false),
            allow_synthesized: AtomicBool::new(false),
            hotkeys: Mutex::new(Vec::new()),
            hook_mouse: AtomicU64::new(0),
            hook_keyboard: AtomicU64::new(0),
            raw_mouse: AtomicU64::new(0),
            raw_keyboard: AtomicU64::new(0),
            restores: AtomicU32::new(0),
            stall_ms: AtomicU32::new(0),
            stall_count: AtomicU32::new(0),
            watch_mouse: AtomicBool::new(false),
            watch_buttons: AtomicBool::new(false),
            watch_keyboard: AtomicBool::new(false),
            drift_min_px: AtomicU32::new(DEFAULT_DRIFT_MIN_PX),
            drift_window_ms: AtomicU32::new(DEFAULT_DRIFT_WINDOW_MS),
        });

        let (hook_tid, hook_thread) = spawn_msg_thread("dl-hooks", shared.clone(), hook_thread_main)
            .inspect_err(|_| RUNNING.store(false, Release))?;
        let (watchdog_tid, watchdog_thread) =
            match spawn_msg_thread("dl-watchdog", shared.clone(), move |s| watchdog_main(s, hook_tid)) {
                Ok(v) => v,
                Err(e) => {
                    unsafe { PostThreadMessageW(hook_tid, WM_QUIT, 0, 0) };
                    let _ = hook_thread.join();
                    RUNNING.store(false, Release);
                    return Err(e);
                }
            };

        Ok(Capture {
            shared,
            rx,
            hook_tid,
            watchdog_tid,
            threads: vec![hook_thread, watchdog_thread],
        })
    }

    pub fn events(&self) -> &Receiver<CaptureEvent> {
        &self.rx
    }

    /// Start swallowing local input and forwarding it as `CaptureEvent::Input`.
    /// The cursor is parked at `anchor` (physical pixels); pick a point well
    /// inside a monitor so motion is never clipped by a screen edge.
    pub fn grab(&self, anchor: (i32, i32)) {
        self.grab_carrying(anchor, false);
    }

    /// Like `grab`; with `carry_buttons`, mouse buttons held right now count
    /// as pressed on the peer (a drag continuing there): their release is
    /// forwarded rather than delivered locally.
    pub fn grab_carrying(&self, anchor: (i32, i32), carry_buttons: bool) {
        self.shared.carry_buttons.store(carry_buttons, Relaxed);
        self.shared.anchor_x.store(anchor.0, Relaxed);
        self.shared.anchor_y.store(anchor.1, Relaxed);
        self.shared.grabbed.store(true, Release);
        unsafe { PostThreadMessageW(self.hook_tid, WM_APP_SYNC, 0, 0) };
    }

    /// Move the parked cursor while grabbed (e.g. once a drag check is done).
    pub fn repark(&self, anchor: (i32, i32)) {
        self.shared.anchor_x.store(anchor.0, Relaxed);
        self.shared.anchor_y.store(anchor.1, Relaxed);
        unsafe { PostThreadMessageW(self.hook_tid, WM_APP_REPARK, 0, 0) };
    }

    pub fn ungrab(&self) {
        self.ungrab_carrying(false);
    }

    /// Like `ungrab`; with `carry_buttons`, mouse buttons pressed during the
    /// grab and still held are released locally (a drag coming back here)
    /// instead of their release being swallowed.
    pub fn ungrab_carrying(&self, carry_buttons: bool) {
        self.shared.carry_buttons.store(carry_buttons, Relaxed);
        self.shared.grabbed.store(false, Release);
        unsafe { PostThreadMessageW(self.hook_tid, WM_APP_SYNC, 0, 0) };
    }

    /// Is the physical left button down (also while grabbed, when Windows
    /// itself doesn't see the press)?
    pub fn left_held(&self) -> bool {
        self.shared.left_physical.load(Relaxed)
    }

    pub fn is_grabbed(&self) -> bool {
        self.shared.grabbed.load(Acquire)
    }

    /// Emit one `LocalActivity` on the next physical mouse and/or keyboard
    /// input (not ours, not while grabbed). `mouse`: movement; `buttons`:
    /// mouse buttons and the wheel. Clears all when it fires.
    pub fn watch_activity(&self, mouse: bool, buttons: bool, keyboard: bool) {
        self.shared.watch_mouse.store(mouse, Relaxed);
        self.shared.watch_buttons.store(buttons, Relaxed);
        self.shared.watch_keyboard.store(keyboard, Relaxed);
    }

    /// Micro-drift protection: mouse movement only counts as local activity
    /// after a net move of `min_px` (physical pixels) within `window_ms`.
    /// Keys, buttons and the wheel always count. `min_px = 0` disables it.
    pub fn set_drift_filter(&self, min_px: u32, window_ms: u32) {
        self.shared.drift_min_px.store(min_px, Relaxed);
        self.shared.drift_window_ms.store(window_ms.max(1), Relaxed);
    }

    pub fn set_hotkeys(&self, hotkeys: Vec<Hotkey>) {
        *self.shared.hotkeys.lock().unwrap() = hotkeys;
    }

    /// Forward input synthesized by other software (macro keyboards, AHK).
    pub fn set_allow_synthesized(&self, allow: bool) {
        self.shared.allow_synthesized.store(allow, Relaxed);
    }

    pub fn stats(&self) -> Stats {
        let s = &self.shared;
        Stats {
            hook_mouse: s.hook_mouse.load(Relaxed),
            hook_keyboard: s.hook_keyboard.load(Relaxed),
            raw_mouse: s.raw_mouse.load(Relaxed),
            raw_keyboard: s.raw_keyboard.load(Relaxed),
            restores: s.restores.load(Relaxed),
            pending: self.rx.len(),
        }
    }

    /// Testing aid: make the next `count` hook callbacks sleep `ms`, to provoke
    /// Windows into silently removing the hook.
    pub fn simulate_stall(&self, ms: u32, count: u32) {
        self.shared.stall_ms.store(ms, Relaxed);
        self.shared.stall_count.store(count, Relaxed);
    }

    /// Testing aid: remove both hooks without telling anyone, exactly what
    /// Windows does to a hook it considers unresponsive.
    pub fn simulate_hook_loss(&self) {
        unsafe { PostThreadMessageW(self.hook_tid, WM_APP_DROP_HOOKS, 0, 0) };
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shared.grabbed.store(false, Release);
        unsafe {
            PostThreadMessageW(self.watchdog_tid, WM_QUIT, 0, 0);
            PostThreadMessageW(self.hook_tid, WM_QUIT, 0, 0);
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        RUNNING.store(false, Release);
    }
}

/// Spawns a thread with a message queue. `body` sets up its resources and
/// returns `Ok` before entering the message loop; the thread id is returned.
fn spawn_msg_thread(
    name: &str,
    shared: Arc<Shared>,
    body: impl FnOnce(Arc<Shared>) -> Result<Box<dyn FnOnce()>, String> + Send + 'static,
) -> Result<(u32, JoinHandle<()>), String> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let handle = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut msg: MSG = unsafe { std::mem::zeroed() };
            // Force creation of the thread message queue before reporting ready.
            unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), WM_USER, WM_USER, PM_NOREMOVE) };
            match body(shared) {
                Ok(cleanup) => {
                    let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));
                    message_loop();
                    cleanup();
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        })
        .map_err(|e| e.to_string())?;
    match ready_rx.recv() {
        Ok(Ok(tid)) => Ok((tid, handle)),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => Err("capture thread died during startup".into()),
    }
}

fn message_loop() {
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        let r = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if r == 0 || r == -1 {
            return;
        }
        if msg.hwnd.is_null() {
            match msg.message {
                WM_APP_REINSTALL => HOOK.with_borrow_mut(|c| {
                    if let Some(c) = c {
                        c.reinstall(msg.wParam)
                    }
                }),
                WM_APP_SYNC => HOOK.with_borrow_mut(|c| {
                    if let Some(c) = c {
                        c.sync_grab()
                    }
                }),
                WM_APP_REPARK => HOOK.with_borrow(|c| {
                    // On the hook thread, so it is ordered with the deltas.
                    if let Some(c) = c
                        && c.grabbed
                    {
                        let (x, y) = (c.shared.anchor_x.load(Relaxed), c.shared.anchor_y.load(Relaxed));
                        unsafe { SetCursorPos(x, y) };
                    }
                }),
                WM_APP_DROP_HOOKS => HOOK.with_borrow(|c| {
                    if let Some(c) = c {
                        // Handles stay in the context, now stale, just as
                        // after a silent removal by Windows.
                        unsafe {
                            UnhookWindowsHookEx(c.mouse_hook);
                            UnhookWindowsHookEx(c.keyboard_hook);
                        }
                    }
                }),
                _ => {}
            }
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// ---------------------------------------------------------------------------
// Hook thread
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Control {
    Key(Key),
    Button(MouseButton),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// Let it through to local apps; don't forward.
    Local,
    /// Swallow and forward.
    Remote,
    /// Swallow; don't forward.
    Drop,
}

struct HookCtx {
    shared: Arc<Shared>,
    mouse_hook: HHOOK,
    keyboard_hook: HHOOK,
    grabbed: bool,
    /// Physically held right now (keys and buttons), regardless of routing.
    physical: HashSet<Control>,
    /// Held when the grab began: their ups go to local apps.
    pass_ups: HashSet<Control>,
    /// Pressed during the grab (forwarded downs): their ups are forwarded,
    /// or swallowed if the grab ended meanwhile.
    remote_down: HashSet<Control>,
    /// Ups to swallow without forwarding (remote presses after ungrab,
    /// swallowed hotkey triggers).
    drop_ups: HashSet<Control>,
    last_restore: Option<Instant>,
    drift: DriftFilter,
}

thread_local! {
    static HOOK: RefCell<Option<HookCtx>> = const { RefCell::new(None) };
}

fn hook_thread_main(shared: Arc<Shared>) -> Result<Box<dyn FnOnce()>, String> {
    // Windows holds every mouse move and key until the hooks return.
    crate::raise_thread_priority(true);
    let mouse_hook = install(WH_MOUSE_LL, mouse_proc)?;
    let keyboard_hook = match install(WH_KEYBOARD_LL, keyboard_proc) {
        Ok(h) => h,
        Err(e) => {
            unsafe { UnhookWindowsHookEx(mouse_hook) };
            return Err(e);
        }
    };
    HOOK.set(Some(HookCtx {
        shared,
        mouse_hook,
        keyboard_hook,
        grabbed: false,
        physical: HashSet::new(),
        pass_ups: HashSet::new(),
        remote_down: HashSet::new(),
        drop_ups: HashSet::new(),
        last_restore: None,
        drift: DriftFilter::default(),
    }));
    Ok(Box::new(|| {
        if let Some(c) = HOOK.take() {
            unsafe {
                UnhookWindowsHookEx(c.mouse_hook);
                UnhookWindowsHookEx(c.keyboard_hook);
            }
        }
    }))
}

fn install(
    id: WINDOWS_HOOK_ID,
    proc_: unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT,
) -> Result<HHOOK, String> {
    let h = unsafe { SetWindowsHookExW(id, Some(proc_), GetModuleHandleW(std::ptr::null()), 0) };
    if h.is_null() {
        Err(format!("SetWindowsHookExW({id}) failed: {}", std::io::Error::last_os_error()))
    } else {
        Ok(h)
    }
}

impl HookCtx {
    fn emit(&self, ev: CaptureEvent) {
        let _ = self.shared.tx.try_send(ev);
    }

    /// Reinstalls hooks the watchdog believes Windows removed. The new hook is
    /// installed before the old one is removed, so there is no gap.
    fn reinstall(&mut self, which: usize) {
        if self.last_restore.is_some_and(|t| t.elapsed() < RESTORE_MIN_INTERVAL) {
            return;
        }
        self.last_restore = Some(Instant::now());
        let mouse = which & REINSTALL_MOUSE != 0;
        let keyboard = which & REINSTALL_KEYBOARD != 0;
        if mouse {
            match install(WH_MOUSE_LL, mouse_proc) {
                Ok(h) => unsafe {
                    UnhookWindowsHookEx(std::mem::replace(&mut self.mouse_hook, h));
                },
                Err(e) => log::error!("mouse hook reinstall: {e}"),
            }
        }
        if keyboard {
            match install(WH_KEYBOARD_LL, keyboard_proc) {
                Ok(h) => unsafe {
                    UnhookWindowsHookEx(std::mem::replace(&mut self.keyboard_hook, h));
                },
                Err(e) => log::error!("keyboard hook reinstall: {e}"),
            }
        }
        self.shared.restores.fetch_add(1, Relaxed);
        log::warn!("hook watchdog: reinstalled hooks (mouse={mouse} keyboard={keyboard})");
        self.emit(CaptureEvent::HookRestored { mouse, keyboard });
    }

    /// Applies a grab state change requested from another thread.
    fn sync_grab(&mut self) {
        let want = self.shared.grabbed.load(Acquire);
        if want != self.grabbed {
            self.set_grabbed(want);
        }
    }

    fn set_grabbed(&mut self, on: bool) {
        self.grabbed = on;
        self.shared.grabbed.store(on, Release);
        if on {
            self.pass_ups = self.physical.clone();
            self.remote_down.clear();
            self.drop_ups.clear();
            // A drag carried to the peer: held buttons belong to it now, so
            // their release is forwarded instead of going to local apps.
            if self.shared.carry_buttons.load(Relaxed) {
                let held: Vec<Control> = self.pass_ups.iter().copied().filter(|c| matches!(c, Control::Button(_))).collect();
                for c in held {
                    self.pass_ups.remove(&c);
                    self.remote_down.insert(c);
                }
            }
            let (x, y) = (self.shared.anchor_x.load(Relaxed), self.shared.anchor_y.load(Relaxed));
            unsafe { SetCursorPos(x, y) };
        } else {
            let carry = self.shared.carry_buttons.load(Relaxed);
            // Carried buttons: their release falls through to local apps.
            self.drop_ups.extend(self.remote_down.drain().filter(|c| !(carry && matches!(c, Control::Button(_)))));
            self.pass_ups.clear();
        }
    }

    fn route(&mut self, c: Control, down: bool) -> Route {
        if c == Control::Button(MouseButton::Left) {
            self.shared.left_physical.store(down, Relaxed);
        }
        if down {
            let repeat = !self.physical.insert(c);
            if !self.grabbed {
                return Route::Local;
            }
            if repeat && self.pass_ups.contains(&c) {
                // Auto-repeat of a key held since before the grab.
                return Route::Local;
            }
            self.remote_down.insert(c);
            Route::Remote
        } else {
            self.physical.remove(&c);
            if self.drop_ups.remove(&c) {
                Route::Drop
            } else if !self.grabbed || self.pass_ups.remove(&c) {
                Route::Local
            } else if self.remote_down.remove(&c) {
                Route::Remote
            } else {
                Route::Drop
            }
        }
    }

    /// Returns true if `key` (just pressed, not a repeat) completes a hotkey.
    fn match_hotkey(&mut self, key: Key) -> bool {
        let held_mods: HashSet<Key> = self
            .physical
            .iter()
            .filter_map(|c| match c {
                Control::Key(k) if (0xE0..=0xE7).contains(&k.0) => Some(*k),
                _ => None,
            })
            .collect();
        // try_lock: never block in the hook; a config update in progress just
        // means this one keystroke isn't checked against hotkeys.
        let Ok(hotkeys) = self.shared.hotkeys.try_lock() else {
            return false;
        };
        let Some(hk) = hotkeys.iter().find(|h| {
            h.key == key
                && h.modifiers.len() == held_mods.len()
                && h.modifiers.iter().all(|m| held_mods.contains(m))
        }) else {
            return false;
        };
        let (id, release) = (hk.id, hk.release_grab);
        drop(hotkeys);

        if held_mods.iter().any(|m| m.0 == 0xE3 || m.0 == 0xE7) {
            inject_mask_key();
        }
        self.emit(CaptureEvent::Hotkey(id));
        if release && self.grabbed {
            self.set_grabbed(false);
            self.emit(CaptureEvent::GrabReleased(ReleaseReason::Hotkey(id)));
        }
        true
    }

    fn check_consumer(&mut self) {
        if self.grabbed && self.shared.tx.len() > STALL_LIMIT {
            log::error!("capture consumer stalled; releasing grab");
            self.set_grabbed(false);
            self.emit(CaptureEvent::GrabReleased(ReleaseReason::ConsumerStalled));
        }
    }

    fn watching_mouse(&self) -> bool {
        self.shared.watch_mouse.load(Relaxed)
    }

    fn check_activity(&mut self, kind: Activity) {
        let s = &self.shared;
        let w = match kind {
            Activity::Move => &s.watch_mouse,
            Activity::Button => &s.watch_buttons,
            Activity::Key => &s.watch_keyboard,
        };
        if !self.grabbed && w.load(Relaxed) && w.swap(false, Relaxed) {
            s.watch_mouse.store(false, Relaxed);
            s.watch_buttons.store(false, Relaxed);
            s.watch_keyboard.store(false, Relaxed);
            self.emit(CaptureEvent::LocalActivity);
        }
    }

    fn maybe_stall(&self) {
        let s = &self.shared;
        if s.stall_count.load(Relaxed) > 0 && s.stall_count.fetch_sub(1, Relaxed) > 0 {
            std::thread::sleep(Duration::from_millis(s.stall_ms.load(Relaxed) as u64));
        }
    }

    /// Returns true to swallow the event.
    fn on_keyboard(&mut self, kb: &KBDLLHOOKSTRUCT, msg: u32) -> bool {
        self.shared.hook_keyboard.fetch_add(1, Relaxed);
        self.maybe_stall();
        self.sync_grab();
        self.check_consumer();
        self.check_activity(Activity::Key);

        let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
        if down {
            crate::intent::note();
        }
        let extended = kb.flags & LLKHF_EXTENDED != 0;
        let Some(key) = keymap::from_hook(kb.vkCode, kb.scanCode, extended) else {
            // Unknown key: forward nothing, but don't let it leak locally
            // while grabbed.
            if down && self.grabbed {
                log::info!("unknown key not forwarded: vk {:#x} scan {:#x} extended {extended}", kb.vkCode, kb.scanCode);
            }
            return self.grabbed;
        };
        let c = Control::Key(key);

        if down && !self.physical.contains(&c) && self.match_hotkey(key) {
            self.physical.insert(c);
            self.drop_ups.insert(c);
            return true;
        }

        match self.route(c, down) {
            Route::Local => false,
            Route::Drop => true,
            Route::Remote => {
                self.emit(CaptureEvent::Input(InputEvent::Key { key, down }));
                true
            }
        }
    }

    fn on_mouse(&mut self, ms: &MSLLHOOKSTRUCT, msg: u32) -> bool {
        self.shared.hook_mouse.fetch_add(1, Relaxed);
        self.maybe_stall();
        let was_grabbed = self.grabbed;
        self.sync_grab();
        self.check_consumer();
        if msg == WM_MOUSEMOVE {
            // Movement must clear the micro-drift filter to count as activity.
            if self.watching_mouse() && !self.grabbed {
                let s = &self.shared;
                let (px, win) = (s.drift_min_px.load(Relaxed), s.drift_window_ms.load(Relaxed));
                if self.drift.feed((ms.pt.x, ms.pt.y), ms.time, px, win) {
                    self.check_activity(Activity::Move);
                }
            }
        } else {
            self.check_activity(Activity::Button);
        }

        let button = |down| {
            let b = match msg {
                WM_LBUTTONDOWN | WM_LBUTTONUP => MouseButton::Left,
                WM_RBUTTONDOWN | WM_RBUTTONUP => MouseButton::Right,
                WM_MBUTTONDOWN | WM_MBUTTONUP => MouseButton::Middle,
                _ if (ms.mouseData >> 16) as u16 == XBUTTON2 => MouseButton::X2,
                _ => MouseButton::X1,
            };
            (b, down)
        };

        let (b, down) = match msg {
            WM_MOUSEMOVE => {
                if !self.grabbed {
                    self.emit(CaptureEvent::CursorMoved { x: ms.pt.x, y: ms.pt.y });
                    return false;
                }
                if !was_grabbed {
                    // First event after the grab started: cursor was just
                    // re-parked, so this delta is meaningless.
                    return true;
                }
                // The hook runs before the cursor moves, so the delta is the
                // target point minus where the cursor is now (normally the
                // anchor, unless something else moved it meanwhile).
                let (cx, cy) = crate::inject::cursor_pos();
                let dx = ms.pt.x - cx;
                let dy = ms.pt.y - cy;
                if dx != 0 || dy != 0 {
                    self.emit(CaptureEvent::Input(InputEvent::MouseMove { dx, dy }));
                }
                return true;
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                if !self.grabbed {
                    return false;
                }
                let delta = (ms.mouseData >> 16) as u16 as i16 as i32;
                let ev = if msg == WM_MOUSEWHEEL {
                    InputEvent::Wheel { dx: 0, dy: delta }
                } else {
                    InputEvent::Wheel { dx: delta, dy: 0 }
                };
                self.emit(CaptureEvent::Input(ev));
                return true;
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
                crate::intent::note();
                button(true)
            }
            WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP => button(false),
            _ => return self.grabbed,
        };

        match self.route(Control::Button(b), down) {
            Route::Local => false,
            Route::Drop => true,
            Route::Remote => {
                self.emit(CaptureEvent::Input(InputEvent::MouseButton { button: b, down }));
                true
            }
        }
    }
}

/// Kinds of local input `watch_activity` can listen for.
#[derive(Clone, Copy)]
enum Activity {
    Move,
    Button,
    Key,
}

fn inject_mask_key() {
    let ki = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VK_MASK, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: INJECT_TAG },
        },
    };
    let inputs = [ki(0), ki(KEYEVENTF_KEYUP)];
    unsafe { SendInput(2, inputs.as_ptr(), size_of::<INPUT>() as i32) };
}

/// Decides whether an event is ours to process. Our own injections always pass
/// through untouched; other synthesized input only if allowed.
fn is_ours_to_process(injected: bool, extra: usize, shared: &Shared) -> bool {
    if extra == INJECT_TAG {
        return false;
    }
    !injected || shared.allow_synthesized.load(Relaxed)
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let swallow = HOOK.with_borrow_mut(|c| {
            let c = c.as_mut()?;
            let injected = kb.flags & LLKHF_INJECTED != 0;
            let ours = is_ours_to_process(injected, kb.dwExtraInfo, &c.shared);
            if !ours && c.grabbed && kb.dwExtraInfo != INJECT_TAG && matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
                // Another program's synthetic key (key remappers, keyboard
                // software): it stays on this PC.
                log::info!("key from another program not forwarded: vk {:#x} scan {:#x}", kb.vkCode, kb.scanCode);
            }
            ours.then(|| c.on_keyboard(kb, wparam as u32))
        });
        if swallow == Some(true) {
            return 1;
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let ms = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        let swallow = HOOK.with_borrow_mut(|c| {
            let c = c.as_mut()?;
            let injected = ms.flags & LLMHF_INJECTED != 0;
            is_ours_to_process(injected, ms.dwExtraInfo, &c.shared)
                .then(|| c.on_mouse(ms, wparam as u32))
        });
        if swallow == Some(true) {
            return 1;
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

// ---------------------------------------------------------------------------
// Watchdog thread
// ---------------------------------------------------------------------------

struct WatchCtx {
    shared: Arc<Shared>,
    hook_tid: u32,
    hwnd: HWND,
    mouse: SilenceDetector,
    keyboard: SilenceDetector,
}

/// Counts physical (Raw Input) events seen since the hook last saw anything.
/// Rates are useless here: while a hook is timing out, Windows processes
/// roughly one input event per second system-wide.
#[derive(Default)]
struct SilenceDetector {
    last_hook: u64,
    last_raw: u64,
    raw_while_silent: u64,
}

impl SilenceDetector {
    /// Returns true if the hook looks removed.
    fn update(&mut self, hook: u64, raw: u64, threshold: u64) -> bool {
        let raw_delta = raw - self.last_raw;
        self.last_raw = raw;
        if hook != self.last_hook {
            self.last_hook = hook;
            self.raw_while_silent = 0;
            return false;
        }
        self.raw_while_silent += raw_delta;
        if self.raw_while_silent >= threshold {
            self.raw_while_silent = 0;
            return true;
        }
        false
    }
}

const SILENT_MOUSE_THRESHOLD: u64 = 5;
const SILENT_KEYBOARD_THRESHOLD: u64 = 3;

thread_local! {
    static WATCH: RefCell<Option<WatchCtx>> = const { RefCell::new(None) };
}

fn watchdog_main(shared: Arc<Shared>, hook_tid: u32) -> Result<Box<dyn FnOnce()>, String> {
    let class: Vec<u16> = "DeskLinkWatchdog\0".encode_utf16().collect();
    let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(watchdog_wndproc),
        hInstance: hinst,
        lpszClassName: class.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    unsafe { RegisterClassW(&wc) };
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err(format!("watchdog window: {}", std::io::Error::last_os_error()));
    }
    let devices = [
        RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x02, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd },
        RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x06, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd },
    ];
    if unsafe { RegisterRawInputDevices(devices.as_ptr(), 2, size_of::<RAWINPUTDEVICE>() as u32) } == 0 {
        let e = std::io::Error::last_os_error();
        unsafe { DestroyWindow(hwnd) };
        return Err(format!("RegisterRawInputDevices: {e}"));
    }
    unsafe { SetTimer(hwnd, 1, WATCHDOG_TICK_MS, None) };
    WATCH.set(Some(WatchCtx {
        shared,
        hook_tid,
        hwnd,
        mouse: SilenceDetector::default(),
        keyboard: SilenceDetector::default(),
    }));
    Ok(Box::new(|| {
        if let Some(w) = WATCH.take() {
            unsafe { DestroyWindow(w.hwnd) };
        }
    }))
}

impl WatchCtx {
    fn tick(&mut self) {
        let s = &self.shared;
        let mut which = 0;
        if self.mouse.update(
            s.hook_mouse.load(Relaxed),
            s.raw_mouse.load(Relaxed),
            SILENT_MOUSE_THRESHOLD,
        ) {
            which |= REINSTALL_MOUSE;
        }
        if self.keyboard.update(
            s.hook_keyboard.load(Relaxed),
            s.raw_keyboard.load(Relaxed),
            SILENT_KEYBOARD_THRESHOLD,
        ) {
            which |= REINSTALL_KEYBOARD;
        }
        if which != 0 {
            unsafe { PostThreadMessageW(self.hook_tid, WM_APP_REINSTALL, which, 0) };
        }
    }

    fn raw_input(&self, lparam: LPARAM) {
        let mut header: RAWINPUTHEADER = unsafe { std::mem::zeroed() };
        let mut size = size_of::<RAWINPUTHEADER>() as u32;
        let r = unsafe {
            GetRawInputData(
                lparam as HRAWINPUT,
                RID_HEADER,
                &mut header as *mut _ as *mut _,
                &mut size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        // hDevice is null for synthesized input; count physical devices only.
        if r == u32::MAX || header.hDevice.is_null() {
            return;
        }
        match header.dwType {
            RIM_TYPEMOUSE => self.shared.raw_mouse.fetch_add(1, Relaxed),
            RIM_TYPEKEYBOARD => self.shared.raw_keyboard.fetch_add(1, Relaxed),
            _ => 0,
        };
    }
}

unsafe extern "system" fn watchdog_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_INPUT => WATCH.with_borrow(|w| {
            if let Some(w) = w {
                w.raw_input(lparam)
            }
        }),
        WM_TIMER => WATCH.with_borrow_mut(|w| {
            if let Some(w) = w {
                w.tick()
            }
        }),
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Tells deliberate mouse movement from sensor drift / desk bumps: fires once
/// the cursor is `min_px` away (straight line) from where it was at the start
/// of the current window. Jitter cancels out; slow drift restarts the window.
#[derive(Default)]
struct DriftFilter {
    anchor: Option<((i32, i32), u32)>,
}

impl DriftFilter {
    /// `time` is the hook timestamp in ms (wraps).
    fn feed(&mut self, p: (i32, i32), time: u32, min_px: u32, window_ms: u32) -> bool {
        if min_px == 0 {
            return true;
        }
        let a = match self.anchor {
            Some((a, t0)) if time.wrapping_sub(t0) <= window_ms => a,
            _ => {
                self.anchor = Some((p, time));
                return false;
            }
        };
        let (dx, dy) = ((p.0 - a.0) as i64, (p.1 - a.1) as i64);
        if dx * dx + dy * dy >= (min_px as i64).pow(2) {
            self.anchor = None;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::SilenceDetector;

    #[test]
    fn silence_accumulates_across_slow_ticks() {
        // One raw event per tick, hook frozen: fires on the 5th tick.
        let mut d = SilenceDetector::default();
        d.update(1, 1, 5); // hook saw an event: baseline
        let fired: Vec<bool> = (2..=6).map(|raw| d.update(1, raw, 5)).collect();
        assert_eq!(fired, [false, false, false, false, true]);
    }

    #[test]
    fn any_hook_activity_resets() {
        let mut d = SilenceDetector::default();
        d.update(0, 4, 5);
        assert!(!d.update(1, 8, 5)); // hook alive
        assert!(!d.update(1, 12, 5)); // 4 silent
        assert!(d.update(1, 13, 5)); // 5 silent
    }
}

#[cfg(test)]
mod drift_tests {
    use super::DriftFilter;

    #[test]
    fn jitter_never_fires() {
        let mut f = DriftFilter::default();
        // +-2 px wobble every 8 ms for 3 s.
        for i in 0..375u32 {
            let p = (100 + (i % 3) as i32 - 1, 200 + (i % 2) as i32 * 2);
            assert!(!f.feed(p, i * 8, 16, 500), "fired at step {i}");
        }
    }

    #[test]
    fn slow_drift_never_fires() {
        let mut f = DriftFilter::default();
        // 1 px every 100 ms in one direction: 5 px per window.
        for i in 0..100u32 {
            assert!(!f.feed((100 + i as i32, 200), i * 100, 16, 500), "fired at {i}");
        }
    }

    #[test]
    fn deliberate_move_fires_quickly() {
        let mut f = DriftFilter::default();
        let fired_at = (0..20u32).position(|i| f.feed((100 + 6 * i as i32, 200), i * 8, 16, 500));
        assert_eq!(fired_at, Some(3), "18 px after 3 events (~24 ms)");
    }

    #[test]
    fn zero_threshold_disables() {
        let mut f = DriftFilter::default();
        assert!(f.feed((0, 0), 0, 0, 500));
    }

    #[test]
    fn timestamp_wraparound() {
        let mut f = DriftFilter::default();
        assert!(!f.feed((0, 0), u32::MAX - 10, 16, 500));
        assert!(f.feed((20, 0), 5, 16, 500));
    }
}
