//! Input injection via `SendInput`.
//!
//! The injector remembers every key and button it pressed, so `release_all`
//! can always undo them. It is called on every leave, disconnect, session
//! change and drop. Stuck keys must be impossible by construction.

use crate::INJECT_TAG;
use crate::keymap::{Injectable, to_injectable};
use desklink_proto::{InputEvent, Key, MouseButton};
use std::collections::{BTreeSet, VecDeque};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SetCursorPos, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};

#[derive(Default)]
pub struct Injector {
    held_keys: BTreeSet<Key>,
    held_buttons: Vec<MouseButton>,
    /// Where we last put the cursor. `GetCursorPos` lags behind events we have
    /// just injected, so during bursts we trust this instead.
    last_pos: Option<((i32, i32), Instant)>,
    /// This burst's recent positions (and where it started): the real cursor
    /// showing one of these is just lagging behind us; anywhere else, someone
    /// else moved it (a dialog's "snap to default button", an app, the local
    /// mouse).
    recent: VecDeque<(i32, i32)>,
    /// When we last told Windows someone is using this PC (see `keep_awake`).
    awake: Option<Instant>,
}

/// How many of our recent positions `GetCursorPos` may still be showing.
const RECENT: usize = 8;

/// After this long without injecting, re-read the real cursor position (the
/// local mouse or an app may have moved it).
const RESYNC_AFTER: Duration = Duration::from_millis(50);

/// How often injected input resets the display and sleep timers.
const KEEP_AWAKE_EVERY: Duration = Duration::from_secs(5);

impl Injector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wake the screens and keep the PC from sleeping while it's used from
    /// another PC: injected input doesn't always count as activity (a
    /// screen that went dark stayed dark when the cursor arrived).
    fn keep_awake(&mut self) {
        use windows_sys::Win32::System::Power::{ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
        if self.awake.is_some_and(|t| t.elapsed() < KEEP_AWAKE_EVERY) {
            return;
        }
        self.awake = Some(Instant::now());
        // Without ES_CONTINUOUS: a one-off reset of the idle timers.
        unsafe { SetThreadExecutionState(ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED) };
    }

    pub fn apply(&mut self, ev: InputEvent) {
        self.keep_awake();
        match ev {
            InputEvent::MouseMove { dx, dy } => {
                self.move_relative(dx, dy);
            }
            InputEvent::MouseButton { button, down } => self.button(button, down),
            InputEvent::Wheel { dx, dy } => {
                if dy != 0 {
                    send_mouse(0, 0, dy as u32, MOUSEEVENTF_WHEEL);
                }
                if dx != 0 {
                    send_mouse(0, 0, dx as u32, MOUSEEVENTF_HWHEEL);
                }
            }
            InputEvent::Key { key, down } => self.key(key, down),
        }
    }

    /// Moves the cursor by a relative amount using an *absolute* injected move,
    /// so the target machine's pointer acceleration is not applied a second
    /// time. Returns the resulting cursor position.
    pub fn move_relative(&mut self, dx: i32, dy: i32) -> (i32, i32) {
        let (x, y) = self.position();
        self.move_to(x + dx, y + dy)
    }

    /// Best-known cursor position: where we last injected it during a burst,
    /// otherwise the real one. If something else moved the cursor meanwhile,
    /// the real one: carrying on from our own idea would yank it back.
    pub fn position(&mut self) -> (i32, i32) {
        let real = cursor_pos();
        match self.last_pos {
            Some((p, t)) if t.elapsed() < RESYNC_AFTER => {
                let ours = self.recent.iter().any(|q| (q.0 - real.0).abs() <= 2 && (q.1 - real.1).abs() <= 2);
                if ours {
                    p
                } else {
                    self.last_pos = None;
                    self.recent.clear();
                    real
                }
            }
            _ => real,
        }
    }

    /// Absolute move in physical virtual-desktop pixels (clamped to it).
    pub fn move_to(&mut self, x: i32, y: i32) -> (i32, i32) {
        self.keep_awake();
        let vd = VirtualDesktop::get();
        let x = x.clamp(vd.x, vd.x + vd.w - 1);
        let y = y.clamp(vd.y, vd.y + vd.h - 1);
        let (nx, ny) = vd.normalize(x, y);
        // A new burst: where the cursor is now counts as ours too (it may
        // still show there until our first move is processed).
        if !self.last_pos.is_some_and(|(_, t)| t.elapsed() < RESYNC_AFTER) {
            self.recent.clear();
            self.recent.push_back(cursor_pos());
        }
        send_mouse(
            nx,
            ny,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        );
        self.last_pos = Some(((x, y), Instant::now()));
        if self.recent.len() == RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back((x, y));
        (x, y)
    }

    pub fn key(&mut self, key: Key, down: bool) {
        if down {
            crate::intent::note(); // the remote user is acting on this machine
            self.held_keys.insert(key);
        } else if !self.held_keys.remove(&key) {
            // Up for a key we never pressed: drop it rather than confuse apps
            // (e.g. a lone Alt-up opens menus).
            return;
        }
        send_key(key, down);
    }

    pub fn button(&mut self, button: MouseButton, down: bool) {
        if down {
            crate::intent::note();
            if !self.held_buttons.contains(&button) {
                self.held_buttons.push(button);
            }
        } else if let Some(i) = self.held_buttons.iter().position(|&b| b == button) {
            self.held_buttons.swap_remove(i);
        } else {
            return;
        }
        send_button(button, down);
    }

    /// Count `button` as pressed by us without sending anything (someone
    /// else pressed it, e.g. a drag start), so its release gets through and
    /// `release_all` covers it.
    pub fn mark_held(&mut self, button: MouseButton) {
        if !self.held_buttons.contains(&button) {
            self.held_buttons.push(button);
        }
    }

    /// Releases every key and button this injector pressed. Idempotent.
    pub fn release_all(&mut self) {
        for key in std::mem::take(&mut self.held_keys).into_iter().rev() {
            send_key(key, false);
        }
        for button in std::mem::take(&mut self.held_buttons) {
            send_button(button, false);
        }
    }

    pub fn held_count(&self) -> usize {
        self.held_keys.len() + self.held_buttons.len()
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

pub fn cursor_pos() -> (i32, i32) {
    let mut p = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

/// Move the cursor without generating input (director side placement).
pub fn set_cursor_pos(p: (i32, i32)) {
    unsafe { SetCursorPos(p.0, p.1) };
}

pub fn lock_keys() -> desklink_proto::LockKeys {
    let on = |vk: u16| unsafe { GetKeyState(vk as i32) } & 1 != 0;
    desklink_proto::LockKeys { caps: on(VK_CAPITAL), num: on(VK_NUMLOCK), scroll: on(VK_SCROLL) }
}

#[derive(Clone, Copy, Debug)]
pub struct VirtualDesktop {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl VirtualDesktop {
    pub fn get() -> Self {
        unsafe {
            Self {
                x: GetSystemMetrics(SM_XVIRTUALSCREEN),
                y: GetSystemMetrics(SM_YVIRTUALSCREEN),
                w: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
                h: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
            }
        }
    }

    /// Pixel -> 0..=65535 absolute coordinates. Windows maps back with
    /// `pixel = n * (size) / 65536`, so aim for the pixel centre to avoid
    /// off-by-one landings.
    fn normalize(&self, x: i32, y: i32) -> (i32, i32) {
        let f = |p: i32, o: i32, size: i32| (((p - o) as i64 * 65536 + 32768) / size as i64) as i32;
        (f(x, self.x, self.w), f(y, self.y, self.h))
    }
}

fn send(inputs: &[INPUT]) {
    let sent = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        // Blocked by UIPI (elevated target / secure desktop) or another injector.
        log::debug!("SendInput injected {sent}/{} events", inputs.len());
    }
}

fn send_mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) {
    send(&[INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }]);
}

fn send_button(button: MouseButton, down: bool) {
    let (flags, data) = match (button, down) {
        (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as u32),
        (MouseButton::X1, false) => (MOUSEEVENTF_XUP, XBUTTON1 as u32),
        (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as u32),
        (MouseButton::X2, false) => (MOUSEEVENTF_XUP, XBUTTON2 as u32),
    };
    send_mouse(0, 0, data, flags);
}

fn send_key(key: Key, down: bool) {
    let up = if down { 0 } else { KEYEVENTF_KEYUP };
    let ki = match to_injectable(key) {
        Some(Injectable::Scan(k)) => KEYBDINPUT {
            wVk: 0,
            wScan: k.scan,
            dwFlags: KEYEVENTF_SCANCODE | up | if k.extended { KEYEVENTF_EXTENDEDKEY } else { 0 },
            time: 0,
            dwExtraInfo: INJECT_TAG,
        },
        Some(Injectable::Vk(vk)) => KEYBDINPUT {
            wVk: vk,
            wScan: 0,
            dwFlags: up,
            time: 0,
            dwExtraInfo: INJECT_TAG,
        },
        None => {
            log::debug!("no Windows mapping for {key:?}");
            return;
        }
    };
    send(&[INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki } }]);
}

/// One tagged button event outside any `Injector` bookkeeping (e.g. to end
/// a local drag over our own drop zone).
pub fn raw_button(button: MouseButton, down: bool) {
    send_button(button, down);
}

/// Is the left mouse button down right now (physical or injected)?
pub fn left_button_down() -> bool {
    unsafe { GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000 != 0 }
}

/// One tagged relative mouse move outside any `Injector` bookkeeping.
pub fn raw_move(dx: i32, dy: i32) {
    send_mouse(dx, dy, 0, MOUSEEVENTF_MOVE);
}
