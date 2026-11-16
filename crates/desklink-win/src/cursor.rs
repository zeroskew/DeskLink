//! Hide/show the mouse cursor system-wide.
//!
//! Windows has no global "hide cursor" call. We replace the standard system
//! cursors with a transparent one and restore the user's scheme with
//! `SPI_SETCURSORS`. Restoring is idempotent and harmless, so it is done
//! generously: on startup (repairs a previous crash), exit, panic, and
//! session end.

use std::sync::atomic::{AtomicBool, Ordering::*};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateCursor, SPI_SETCURSORS, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetSystemCursor,
    SystemParametersInfoW,
};

static HIDDEN: AtomicBool = AtomicBool::new(false);

/// OCR_* ids of the standard cursors.
const SYSTEM_CURSORS: [u32; 16] = [
    32512, // NORMAL
    32513, // IBEAM
    32514, // WAIT
    32515, // CROSS
    32516, // UP
    32631, // PEN (handwriting)
    32642, // SIZENWSE
    32643, // SIZENESW
    32644, // SIZEWE
    32645, // SIZENS
    32646, // SIZEALL
    32648, // NO
    32649, // HAND
    32650, // APPSTARTING
    32651, // HELP
    32671, // PIN
];

pub fn hide() {
    if HIDDEN.swap(true, AcqRel) {
        return;
    }
    // 32x32 monochrome: AND mask all ones (keep screen), XOR all zeros.
    let and = [0xFFu8; 128];
    let xor = [0u8; 128];
    let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
    for id in SYSTEM_CURSORS {
        // SetSystemCursor takes ownership of (and destroys) the handle, so
        // each id needs its own copy.
        let blank = unsafe { CreateCursor(hinst, 0, 0, 32, 32, and.as_ptr().cast(), xor.as_ptr().cast()) };
        if !blank.is_null() {
            unsafe { SetSystemCursor(blank, id) };
        }
    }
}

/// Restore the cursor. Returns true if it was hidden. Windows then still
/// draws it where it was last *visible* until the next mouse input, so
/// callers should follow with an input event at the real position.
pub fn show() -> bool {
    let was_hidden = HIDDEN.swap(false, AcqRel);
    if was_hidden {
        reload_scheme();
    }
    was_hidden
}

/// Unconditionally restore the user's cursors. Call at startup to repair a
/// previous run that died (killed, power loss) while the cursor was hidden.
pub fn repair() {
    HIDDEN.store(false, Release);
    reload_scheme();
}

fn reload_scheme() {
    unsafe {
        SystemParametersInfoW(SPI_SETCURSORS, 0, std::ptr::null_mut(), 0 as SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS)
    };
}

pub fn is_hidden() -> bool {
    HIDDEN.load(Acquire)
}
