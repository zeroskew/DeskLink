//! Parse hotkey strings like `LCtrl+LAlt+Pause`.

use desklink_proto::Key;

fn modifier(name: &str) -> Option<Key> {
    Some(Key(match name {
        "lctrl" | "ctrl" | "control" => 0xE0,
        "lshift" | "shift" => 0xE1,
        "lalt" | "alt" => 0xE2,
        "lwin" | "win" => 0xE3,
        "rctrl" => 0xE4,
        "rshift" => 0xE5,
        "ralt" | "altgr" => 0xE6,
        "rwin" => 0xE7,
        _ => return None,
    }))
}

fn key(name: &str) -> Option<Key> {
    let b = name.as_bytes();
    if b.len() == 1 {
        return match b[0] {
            c @ b'a'..=b'z' => Some(Key(0x04 + (c - b'a') as u16)),
            b'0' => Some(Key(0x27)),
            c @ b'1'..=b'9' => Some(Key(0x1E + (c - b'1') as u16)),
            _ => None,
        };
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()) {
        return match n {
            1..=12 => Some(Key(0x3A + n - 1)),
            13..=24 => Some(Key(0x68 + n - 13)),
            _ => None,
        };
    }
    Some(Key(match name {
        "pause" | "break" => 0x48,
        "scrolllock" | "scroll" => 0x47,
        "insert" | "ins" => 0x49,
        "delete" | "del" => 0x4C,
        "home" => 0x4A,
        "end" => 0x4D,
        "pageup" | "pgup" => 0x4B,
        "pagedown" | "pgdn" => 0x4E,
        "left" => 0x50,
        "right" => 0x4F,
        "up" => 0x52,
        "down" => 0x51,
        "esc" | "escape" => 0x29,
        "space" => 0x2C,
        "tab" => 0x2B,
        "enter" | "return" => 0x28,
        "printscreen" | "prtsc" => 0x46,
        _ => return None,
    }))
}

/// Returns (modifiers, key).
pub fn parse(s: &str) -> Result<(Vec<Key>, Key), String> {
    let parts: Vec<String> = s.split('+').map(|p| p.trim().to_ascii_lowercase()).collect();
    let (last, mods) = parts.split_last().ok_or("empty hotkey")?;
    let modifiers = mods
        .iter()
        .map(|m| modifier(m).ok_or_else(|| format!("unknown modifier '{m}' in '{s}'")))
        .collect::<Result<Vec<_>, _>>()?;
    let key = key(last).ok_or_else(|| format!("unknown key '{last}' in '{s}'"))?;
    Ok((modifiers, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_return_hotkey() {
        assert_eq!(parse("LCtrl+LAlt+Pause").unwrap(), (vec![Key(0xE0), Key(0xE2)], Key(0x48)));
        assert_eq!(parse("ctrl + win + l").unwrap(), (vec![Key(0xE0), Key(0xE3)], Key(0x0F)));
        assert_eq!(parse("F13").unwrap(), (vec![], Key(0x68)));
        assert!(parse("Hyper+X").is_err());
    }
}
