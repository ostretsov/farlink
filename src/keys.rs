//! Key names used on the wire, and their codes on Linux (X11 keysyms) and
//! Windows (virtual-key codes).

/// (name, X11 keysym, Windows virtual key, Windows "extended" flag)
const TABLE: &[(&str, u32, u16, bool)] = &[
    ("Enter", 0xff0d, 0x0D, false),
    ("Escape", 0xff1b, 0x1B, false),
    ("Backspace", 0xff08, 0x08, false),
    ("Tab", 0xff09, 0x09, false),
    ("Space", 0x0020, 0x20, false),
    ("Insert", 0xff63, 0x2D, true),
    ("Delete", 0xffff, 0x2E, true),
    ("Home", 0xff50, 0x24, true),
    ("End", 0xff57, 0x23, true),
    ("PageUp", 0xff55, 0x21, true),
    ("PageDown", 0xff56, 0x22, true),
    ("ArrowLeft", 0xff51, 0x25, true),
    ("ArrowUp", 0xff52, 0x26, true),
    ("ArrowRight", 0xff53, 0x27, true),
    ("ArrowDown", 0xff54, 0x28, true),
    ("Shift", 0xffe1, 0xA0, false),
    ("Control", 0xffe3, 0xA2, false),
    ("Alt", 0xffe9, 0xA4, false),
    ("Meta", 0xffeb, 0x5B, true),
    ("CapsLock", 0xffe5, 0x14, false),
    ("Minus", 0x002d, 0xBD, false),
    ("Equals", 0x003d, 0xBB, false),
    ("OpenBracket", 0x005b, 0xDB, false),
    ("CloseBracket", 0x005d, 0xDD, false),
    ("Backslash", 0x005c, 0xDC, false),
    ("Semicolon", 0x003b, 0xBA, false),
    ("Quote", 0x0027, 0xDE, false),
    ("Comma", 0x002c, 0xBC, false),
    ("Period", 0x002e, 0xBE, false),
    ("Slash", 0x002f, 0xBF, false),
    ("Backtick", 0x0060, 0xC0, false),
    ("PrintScreen", 0xff61, 0x2C, true),
    ("Pause", 0xff13, 0x13, false),
    ("NumLock", 0xff7f, 0x90, true),
    ("ScrollLock", 0xff14, 0x91, false),
    ("ContextMenu", 0xff67, 0x5D, true),
];

/// X11 keysym for a key name.
pub fn keysym(name: &str) -> Option<u32> {
    if let Some(&(_, ks, _, _)) = TABLE.iter().find(|e| e.0 == name) {
        return Some(ks);
    }
    if let Some(n) = fkey(name) {
        return Some(0xffbe + n - 1);
    }
    let b = single_ascii(name)?;
    match b {
        b'A'..=b'Z' => Some((b + 32) as u32),
        b'0'..=b'9' => Some(b as u32),
        _ => None,
    }
}

/// Key name for an X11 keysym (the key's first, unshifted symbol), for keys
/// read straight from the X server. Right-hand modifiers and Shift+Tab
/// count as the plain ones.
pub fn name_for_keysym(ks: u32) -> Option<String> {
    let ks = match ks {
        0xffe2 => 0xffe1, // Shift_R
        0xffe4 => 0xffe3, // Control_R
        0xffea => 0xffe9, // Alt_R
        0xffec => 0xffeb, // Super_R
        0xfe20 => 0xff09, // ISO_Left_Tab (Shift+Tab)
        0xff8d => 0xff0d, // KP_Enter
        k => k,
    };
    if let Some(&(name, ..)) = TABLE.iter().find(|e| e.1 == ks) {
        return Some(name.to_string());
    }
    match ks {
        0xffbe..=0xffd5 => Some(format!("F{}", ks - 0xffbe + 1)),
        0x61..=0x7a => Some(((ks - 32) as u8 as char).to_string()),
        0x41..=0x5a | 0x30..=0x39 => Some((ks as u8 as char).to_string()),
        _ => None,
    }
}

/// Windows virtual-key code and "extended key" flag for a key name.
pub fn windows_vk(name: &str) -> Option<(u16, bool)> {
    if let Some(&(_, _, vk, ext)) = TABLE.iter().find(|e| e.0 == name) {
        return Some((vk, ext));
    }
    if let Some(n) = fkey(name) {
        return Some((0x70 + n as u16 - 1, false));
    }
    let b = single_ascii(name)?;
    match b {
        b'A'..=b'Z' | b'0'..=b'9' => Some((b as u16, false)),
        _ => None,
    }
}

/// Keysym for typing one character on X11.
pub fn char_keysym(c: char) -> u32 {
    let cp = c as u32;
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        _ if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) => cp,
        _ => 0x0100_0000 | cp,
    }
}

fn single_ascii(name: &str) -> Option<u8> {
    let b = name.as_bytes();
    (b.len() == 1).then(|| b[0].to_ascii_uppercase())
}

fn fkey(name: &str) -> Option<u32> {
    let n: u32 = name.strip_prefix('F')?.parse().ok()?;
    (1..=24).contains(&n).then_some(n)
}

/// Every key name the protocol knows (used by tests and the viewer).
pub fn all_names() -> Vec<String> {
    let mut v: Vec<String> = TABLE.iter().map(|e| e.0.to_string()).collect();
    v.extend((b'A'..=b'Z').map(|c| (c as char).to_string()));
    v.extend((b'0'..=b'9').map(|c| (c as char).to_string()));
    v.extend((1..=12).map(|n| format!("F{n}")));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysyms_name_their_keys_back() {
        for name in all_names() {
            let ks = keysym(&name).unwrap();
            assert_eq!(name_for_keysym(ks).as_deref(), Some(name.as_str()), "{name}");
        }
        assert_eq!(name_for_keysym(0x61).as_deref(), Some("A"), "lower-case letter");
        assert_eq!(name_for_keysym(0xfe20).as_deref(), Some("Tab"), "Shift+Tab");
        assert_eq!(name_for_keysym(0xffea).as_deref(), Some("Alt"), "right Alt");
        assert_eq!(name_for_keysym(0xffc1).as_deref(), Some("F4"));
        assert_eq!(name_for_keysym(0x6c1), None, "Cyrillic letter: no key name");
    }

    #[test]
    fn letters_digits_and_function_keys() {
        assert_eq!(keysym("A"), Some(0x61));
        assert_eq!(keysym("7"), Some(0x37));
        assert_eq!(keysym("F1"), Some(0xffbe));
        assert_eq!(keysym("F12"), Some(0xffc9));
        assert_eq!(windows_vk("A"), Some((0x41, false)));
        assert_eq!(windows_vk("F5"), Some((0x74, false)));
        assert_eq!(windows_vk("ArrowLeft"), Some((0x25, true)));
        assert_eq!(keysym("Nope"), None);
        assert_eq!(windows_vk("F99"), None);
    }

    #[test]
    fn every_name_maps_on_both_systems() {
        for n in all_names() {
            assert!(keysym(&n).is_some(), "x11 {n}");
            assert!(windows_vk(&n).is_some(), "windows {n}");
        }
    }

    #[test]
    fn characters_to_keysyms() {
        assert_eq!(char_keysym('a'), 0x61);
        assert_eq!(char_keysym('é'), 0xe9);
        assert_eq!(char_keysym('ж'), 0x0100_0436);
        assert_eq!(char_keysym('\n'), 0xff0d);
    }
}
