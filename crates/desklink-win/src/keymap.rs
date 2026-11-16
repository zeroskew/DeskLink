//! Windows scancode (set 1, with E0 extension) <-> USB HID usage mapping.
//!
//! Scancodes rather than virtual keys are sent on the wire so the receiving
//! machine applies its own keyboard layout, exactly like a physical keyboard.

use desklink_proto::Key;
use std::sync::OnceLock;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

/// Consumer-page keys (usage <= 0xFF) are encoded as 0x0C00 | usage. They have
/// no reliable scancode on many keyboards, so they are injected by virtual key.
pub const CONSUMER: u16 = 0x0C00;

pub const HID_PAUSE: Key = Key(0x48);

/// A Windows key as seen by the hook: set-1 scancode plus the E0 flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WinKey {
    pub scan: u16,
    pub extended: bool,
}

impl WinKey {
    fn index(self) -> usize {
        (self.scan as usize & 0xFF) | if self.extended { 0x100 } else { 0 }
    }
}

// (scan, extended, hid)
#[rustfmt::skip]
const TABLE: &[(u16, bool, u16)] = &[
    (0x01, false, 0x29), // Esc
    (0x02, false, 0x1E), (0x03, false, 0x1F), (0x04, false, 0x20), (0x05, false, 0x21),
    (0x06, false, 0x22), (0x07, false, 0x23), (0x08, false, 0x24), (0x09, false, 0x25),
    (0x0A, false, 0x26), (0x0B, false, 0x27), // 1..9, 0
    (0x0C, false, 0x2D), (0x0D, false, 0x2E), // - =
    (0x0E, false, 0x2A), (0x0F, false, 0x2B), // Backspace, Tab
    (0x10, false, 0x14), (0x11, false, 0x1A), (0x12, false, 0x08), (0x13, false, 0x15),
    (0x14, false, 0x17), (0x15, false, 0x1C), (0x16, false, 0x18), (0x17, false, 0x0C),
    (0x18, false, 0x12), (0x19, false, 0x13), // Q W E R T Y U I O P
    (0x1A, false, 0x2F), (0x1B, false, 0x30), // [ ]
    (0x1C, false, 0x28), // Enter
    (0x1D, false, 0xE0), // LCtrl
    (0x1E, false, 0x04), (0x1F, false, 0x16), (0x20, false, 0x07), (0x21, false, 0x09),
    (0x22, false, 0x0A), (0x23, false, 0x0B), (0x24, false, 0x0D), (0x25, false, 0x0E),
    (0x26, false, 0x0F), // A S D F G H J K L
    (0x27, false, 0x33), (0x28, false, 0x34), (0x29, false, 0x35), // ; ' `
    (0x2A, false, 0xE1), // LShift
    (0x2B, false, 0x31), // backslash
    (0x2C, false, 0x1D), (0x2D, false, 0x1B), (0x2E, false, 0x06), (0x2F, false, 0x19),
    (0x30, false, 0x05), (0x31, false, 0x11), (0x32, false, 0x10), // Z X C V B N M
    (0x33, false, 0x36), (0x34, false, 0x37), (0x35, false, 0x38), // , . /
    (0x36, false, 0xE5), // RShift
    (0x37, false, 0x55), // KP *
    (0x38, false, 0xE2), // LAlt
    (0x39, false, 0x2C), // Space
    (0x3A, false, 0x39), // CapsLock
    (0x3B, false, 0x3A), (0x3C, false, 0x3B), (0x3D, false, 0x3C), (0x3E, false, 0x3D),
    (0x3F, false, 0x3E), (0x40, false, 0x3F), (0x41, false, 0x40), (0x42, false, 0x41),
    (0x43, false, 0x42), (0x44, false, 0x43), // F1..F10
    (0x46, false, 0x47), // ScrollLock
    (0x47, false, 0x5F), (0x48, false, 0x60), (0x49, false, 0x61), (0x4A, false, 0x56),
    (0x4B, false, 0x5C), (0x4C, false, 0x5D), (0x4D, false, 0x5E), (0x4E, false, 0x57),
    (0x4F, false, 0x59), (0x50, false, 0x5A), (0x51, false, 0x5B), (0x52, false, 0x62),
    (0x53, false, 0x63), // keypad
    (0x56, false, 0x64), // non-US backslash
    (0x57, false, 0x44), (0x58, false, 0x45), // F11, F12
    (0x59, false, 0x67), // KP =
    (0x64, false, 0x68), (0x65, false, 0x69), (0x66, false, 0x6A), (0x67, false, 0x6B),
    (0x68, false, 0x6C), (0x69, false, 0x6D), (0x6A, false, 0x6E), (0x6B, false, 0x6F),
    (0x6C, false, 0x70), (0x6D, false, 0x71), (0x6E, false, 0x72), (0x76, false, 0x73), // F13..F24
    (0x70, false, 0x88), (0x73, false, 0x87), (0x79, false, 0x8A), (0x7B, false, 0x8B),
    (0x7D, false, 0x89), // JIS keys
    (0x1C, true, 0x58), // KP Enter
    (0x1D, true, 0xE4), // RCtrl
    (0x35, true, 0x54), // KP /
    (0x37, true, 0x46), // PrintScreen
    (0x38, true, 0xE6), // RAlt / AltGr
    (0x45, true, 0x53), // NumLock (hook reports it extended)
    (0x46, true, 0x48), // Ctrl+Pause = Break, reported as E0 46
    (0x47, true, 0x4A), (0x48, true, 0x52), (0x49, true, 0x4B), (0x4B, true, 0x50),
    (0x4D, true, 0x4F), (0x4F, true, 0x4D), (0x50, true, 0x51), (0x51, true, 0x4E),
    (0x52, true, 0x49), (0x53, true, 0x4C), // nav cluster
    (0x5B, true, 0xE3), (0x5C, true, 0xE7), (0x5D, true, 0x65), // LWin RWin Apps
];

#[rustfmt::skip]
const CONSUMER_VK: &[(u16, u16)] = &[
    (0xE2, VK_VOLUME_MUTE), (0xE9, VK_VOLUME_UP), (0xEA, VK_VOLUME_DOWN),
    (0xCD, VK_MEDIA_PLAY_PAUSE), (0xB5, VK_MEDIA_NEXT_TRACK), (0xB6, VK_MEDIA_PREV_TRACK),
    (0xB7, VK_MEDIA_STOP),
];

fn forward() -> &'static [u16; 512] {
    static F: OnceLock<[u16; 512]> = OnceLock::new();
    F.get_or_init(|| {
        let mut t = [0u16; 512];
        for &(scan, ext, hid) in TABLE {
            t[WinKey { scan, extended: ext }.index()] = hid;
        }
        t
    })
}

/// Map a key from the low-level keyboard hook to a HID key.
pub fn from_hook(vk: u32, scan: u32, extended: bool) -> Option<Key> {
    let vk = vk as u16;
    // The hook reports Right Shift as E0 36 (extended); its virtual key is
    // unambiguous, unlike the scancode.
    if vk == VK_RSHIFT {
        return Some(Key(0xE5));
    }
    if vk == VK_PAUSE {
        return Some(HID_PAUSE);
    }
    if let Some(&(usage, _)) = CONSUMER_VK.iter().find(|&&(_, v)| v == vk) {
        return Some(Key(CONSUMER | usage));
    }
    let hid = forward()[WinKey { scan: scan as u16, extended }.index()];
    (hid != 0).then_some(Key(hid))
}

/// How to inject a HID key on Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Injectable {
    Scan(WinKey),
    Vk(u16),
}

pub fn to_injectable(key: Key) -> Option<Injectable> {
    if key == HID_PAUSE {
        return Some(Injectable::Vk(VK_PAUSE));
    }
    if key.0 & 0xFF00 == CONSUMER {
        let usage = key.0 & 0x00FF;
        return CONSUMER_VK
            .iter()
            .find(|&&(u, _)| u == usage)
            .map(|&(_, vk)| Injectable::Vk(vk));
    }
    TABLE
        .iter()
        .find(|&&(_, _, hid)| hid == key.0)
        .map(|&(scan, extended, _)| Injectable::Scan(WinKey { scan, extended }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_usages_are_unique() {
        let mut seen = std::collections::HashMap::new();
        for &(scan, ext, hid) in TABLE {
            if let Some(prev) = seen.insert(hid, (scan, ext)) {
                panic!("hid {hid:#x} mapped by {prev:?} and {:?}", (scan, ext));
            }
        }
    }

    #[test]
    fn roundtrip_letters_and_nav() {
        // 'A'
        let k = from_hook(0x41, 0x1E, false).unwrap();
        assert_eq!(k, Key(0x04));
        assert_eq!(
            to_injectable(k),
            Some(Injectable::Scan(WinKey { scan: 0x1E, extended: false }))
        );
        // Right arrow vs KP6
        assert_eq!(from_hook(0x27, 0x4D, true), Some(Key(0x4F)));
        assert_eq!(from_hook(0x66, 0x4D, false), Some(Key(0x5E)));
    }

    #[test]
    fn right_shift_with_or_without_e0() {
        assert_eq!(from_hook(VK_RSHIFT as u32, 0x36, true), Some(Key(0xE5)));
        assert_eq!(from_hook(VK_RSHIFT as u32, 0x36, false), Some(Key(0xE5)));
        assert_eq!(to_injectable(Key(0xE5)), Some(Injectable::Scan(WinKey { scan: 0x36, extended: false })));
    }

    #[test]
    fn pause_and_break_are_the_same_key() {
        assert_eq!(from_hook(VK_PAUSE as u32, 0x45, false), Some(HID_PAUSE));
        assert_eq!(from_hook(VK_CANCEL as u32, 0x46, true), Some(HID_PAUSE));
        assert_eq!(to_injectable(HID_PAUSE), Some(Injectable::Vk(VK_PAUSE)));
    }

    #[test]
    fn media_keys() {
        let k = from_hook(VK_VOLUME_UP as u32, 0x30, true).unwrap();
        assert_eq!(k, Key(CONSUMER | 0xE9));
        assert_eq!(to_injectable(k), Some(Injectable::Vk(VK_VOLUME_UP)));
    }
}
