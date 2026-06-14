//! Translation between the **evdev** keycode space (the canonical wire space —
//! see [`deskoryn_core::input::KeyCode`]) and Windows **virtual-key** codes.
//!
//! The Windows backend lives behind `cfg(windows)` and can only be compiled with
//! the Windows target, so the actual capture/inject code is hard to exercise off
//! a Windows host. This table is the part most likely to be wrong, so it is kept
//! here as plain data with no platform gating: it compiles and is unit-tested on
//! every host (the round-trip test below runs in normal `cargo test`).
//!
//! Coverage is the common 101/104-key set: letters, digits, function keys, the
//! modifier pairs, navigation cluster, and ASCII punctuation. Keys outside the
//! table fall through untranslated (the caller forwards the raw evdev code and a
//! best-effort path still works for most apps). Extend [`TABLE`] as gaps surface
//! on real hardware.

/// evdev keycodes (`linux/input-event-codes.h`).
mod ev {
    pub const ESC: u32 = 1;
    pub const N1: u32 = 2; // digits 1..9,0 are contiguous 2..11
    pub const MINUS: u32 = 12;
    pub const EQUAL: u32 = 13;
    pub const BACKSPACE: u32 = 14;
    pub const TAB: u32 = 15;
    pub const Q: u32 = 16; // QWERTY row 16..25
    pub const LEFTBRACE: u32 = 26;
    pub const RIGHTBRACE: u32 = 27;
    pub const ENTER: u32 = 28;
    pub const LEFTCTRL: u32 = 29;
    pub const A: u32 = 30; // ASDF row 30..38
    pub const SEMICOLON: u32 = 39;
    pub const APOSTROPHE: u32 = 40;
    pub const GRAVE: u32 = 41;
    pub const LEFTSHIFT: u32 = 42;
    pub const BACKSLASH: u32 = 43;
    pub const Z: u32 = 44; // ZXCV row 44..50
    pub const COMMA: u32 = 51;
    pub const DOT: u32 = 52;
    pub const SLASH: u32 = 53;
    pub const RIGHTSHIFT: u32 = 54;
    pub const LEFTALT: u32 = 56;
    pub const SPACE: u32 = 57;
    pub const CAPSLOCK: u32 = 58;
    pub const F1: u32 = 59; // F1..F10 are 59..68
    pub const NUMLOCK: u32 = 69;
    pub const SCROLLLOCK: u32 = 70;
    pub const F11: u32 = 87;
    pub const F12: u32 = 88;
    pub const RIGHTCTRL: u32 = 97;
    pub const RIGHTALT: u32 = 100;
    pub const HOME: u32 = 102;
    pub const UP: u32 = 103;
    pub const PAGEUP: u32 = 104;
    pub const LEFT: u32 = 105;
    pub const RIGHT: u32 = 106;
    pub const END: u32 = 107;
    pub const DOWN: u32 = 108;
    pub const PAGEDOWN: u32 = 109;
    pub const INSERT: u32 = 110;
    pub const DELETE: u32 = 111;
    pub const LEFTMETA: u32 = 125;
    pub const RIGHTMETA: u32 = 126;
}

/// Windows virtual-key codes (`winuser.h`).
mod vk {
    pub const BACK: u16 = 0x08;
    pub const TAB: u16 = 0x09;
    pub const RETURN: u16 = 0x0D;
    pub const CAPITAL: u16 = 0x14;
    pub const ESCAPE: u16 = 0x1B;
    pub const SPACE: u16 = 0x20;
    pub const PRIOR: u16 = 0x21; // Page Up
    pub const NEXT: u16 = 0x22; // Page Down
    pub const END: u16 = 0x23;
    pub const HOME: u16 = 0x24;
    pub const LEFT: u16 = 0x25;
    pub const UP: u16 = 0x26;
    pub const RIGHT: u16 = 0x27;
    pub const DOWN: u16 = 0x28;
    pub const INSERT: u16 = 0x2D;
    pub const DELETE: u16 = 0x2E;
    pub const LWIN: u16 = 0x5B;
    pub const RWIN: u16 = 0x5C;
    pub const F1: u16 = 0x70; // F1..F12 are 0x70..0x7B
    pub const NUMLOCK: u16 = 0x90;
    pub const SCROLL: u16 = 0x91;
    pub const LSHIFT: u16 = 0xA0;
    pub const RSHIFT: u16 = 0xA1;
    pub const LCONTROL: u16 = 0xA2;
    pub const RCONTROL: u16 = 0xA3;
    pub const LMENU: u16 = 0xA4; // Left Alt
    pub const RMENU: u16 = 0xA5; // Right Alt
    pub const OEM_1: u16 = 0xBA; // ;:
    pub const OEM_PLUS: u16 = 0xBB; // =+
    pub const OEM_COMMA: u16 = 0xBC; // ,<
    pub const OEM_MINUS: u16 = 0xBD; // -_
    pub const OEM_PERIOD: u16 = 0xBE; // .>
    pub const OEM_2: u16 = 0xBF; // /?
    pub const OEM_3: u16 = 0xC0; // `~
    pub const OEM_4: u16 = 0xDB; // [{
    pub const OEM_5: u16 = 0xDC; // \|
    pub const OEM_6: u16 = 0xDD; // ]}
    pub const OEM_7: u16 = 0xDE; // '"
}

/// Explicit (evdev, vk) pairs for keys that aren't covered by the contiguous
/// letter/digit/function-row ranges in [`evdev_to_vk`] / [`vk_to_evdev`].
const TABLE: &[(u32, u16)] = &[
    (ev::ESC, vk::ESCAPE),
    (ev::MINUS, vk::OEM_MINUS),
    (ev::EQUAL, vk::OEM_PLUS),
    (ev::BACKSPACE, vk::BACK),
    (ev::TAB, vk::TAB),
    (ev::LEFTBRACE, vk::OEM_4),
    (ev::RIGHTBRACE, vk::OEM_6),
    (ev::ENTER, vk::RETURN),
    (ev::LEFTCTRL, vk::LCONTROL),
    (ev::SEMICOLON, vk::OEM_1),
    (ev::APOSTROPHE, vk::OEM_7),
    (ev::GRAVE, vk::OEM_3),
    (ev::LEFTSHIFT, vk::LSHIFT),
    (ev::BACKSLASH, vk::OEM_5),
    (ev::COMMA, vk::OEM_COMMA),
    (ev::DOT, vk::OEM_PERIOD),
    (ev::SLASH, vk::OEM_2),
    (ev::RIGHTSHIFT, vk::RSHIFT),
    (ev::LEFTALT, vk::LMENU),
    (ev::SPACE, vk::SPACE),
    (ev::CAPSLOCK, vk::CAPITAL),
    (ev::NUMLOCK, vk::NUMLOCK),
    (ev::SCROLLLOCK, vk::SCROLL),
    (ev::RIGHTCTRL, vk::RCONTROL),
    (ev::RIGHTALT, vk::RMENU),
    (ev::HOME, vk::HOME),
    (ev::UP, vk::UP),
    (ev::PAGEUP, vk::PRIOR),
    (ev::LEFT, vk::LEFT),
    (ev::RIGHT, vk::RIGHT),
    (ev::END, vk::END),
    (ev::DOWN, vk::DOWN),
    (ev::PAGEDOWN, vk::NEXT),
    (ev::INSERT, vk::INSERT),
    (ev::DELETE, vk::DELETE),
    (ev::LEFTMETA, vk::LWIN),
    (ev::RIGHTMETA, vk::RWIN),
];

/// Map an evdev keycode to a Windows virtual-key, or `None` if unmapped.
pub fn evdev_to_vk(code: u32) -> Option<u16> {
    // Contiguous ranges first (cheaper and keeps the table small).
    if (ev::N1..=ev::N1 + 8).contains(&code) {
        return Some(0x31 + (code - ev::N1) as u16); // '1'..'9'
    }
    if code == ev::N1 + 9 {
        return Some(0x30); // '0' (evdev KEY_0 == 11)
    }
    if let Some(vk) = letter_evdev_to_vk(code) {
        return Some(vk);
    }
    if (ev::F1..=ev::F1 + 9).contains(&code) {
        return Some(vk::F1 + (code - ev::F1) as u16); // F1..F10
    }
    if code == ev::F11 {
        return Some(vk::F1 + 10);
    }
    if code == ev::F12 {
        return Some(vk::F1 + 11);
    }
    TABLE.iter().find(|(e, _)| *e == code).map(|(_, v)| *v)
}

/// Map a Windows virtual-key to an evdev keycode, or `None` if unmapped.
pub fn vk_to_evdev(vk: u16) -> Option<u32> {
    if (0x31..=0x39).contains(&vk) {
        return Some(ev::N1 + (vk - 0x31) as u32); // '1'..'9'
    }
    if vk == 0x30 {
        return Some(ev::N1 + 9); // '0'
    }
    if let Some(code) = letter_vk_to_evdev(vk) {
        return Some(code);
    }
    if (vk::F1..=vk::F1 + 9).contains(&vk) {
        return Some(ev::F1 + (vk - vk::F1) as u32); // F1..F10
    }
    if vk == vk::F1 + 10 {
        return Some(ev::F11);
    }
    if vk == vk::F1 + 11 {
        return Some(ev::F12);
    }
    TABLE.iter().find(|(_, v)| *v == vk).map(|(e, _)| *e)
}

/// Letters are non-contiguous in evdev (split across the QWERTY/ASDF/ZXCV rows)
/// but contiguous in VK (`'A'..'Z'` == 0x41..0x5A), so map via the ASCII letter.
const LETTERS: &[(u32, char)] = &[
    (ev::Q, 'q'),
    (ev::Q + 1, 'w'),
    (ev::Q + 2, 'e'),
    (ev::Q + 3, 'r'),
    (ev::Q + 4, 't'),
    (ev::Q + 5, 'y'),
    (ev::Q + 6, 'u'),
    (ev::Q + 7, 'i'),
    (ev::Q + 8, 'o'),
    (ev::Q + 9, 'p'),
    (ev::A, 'a'),
    (ev::A + 1, 's'),
    (ev::A + 2, 'd'),
    (ev::A + 3, 'f'),
    (ev::A + 4, 'g'),
    (ev::A + 5, 'h'),
    (ev::A + 6, 'j'),
    (ev::A + 7, 'k'),
    (ev::A + 8, 'l'),
    (ev::Z, 'z'),
    (ev::Z + 1, 'x'),
    (ev::Z + 2, 'c'),
    (ev::Z + 3, 'v'),
    (ev::Z + 4, 'b'),
    (ev::Z + 5, 'n'),
    (ev::Z + 6, 'm'),
];

fn letter_evdev_to_vk(code: u32) -> Option<u16> {
    LETTERS
        .iter()
        .find(|(e, _)| *e == code)
        .map(|(_, c)| c.to_ascii_uppercase() as u16)
}

fn letter_vk_to_evdev(vk: u16) -> Option<u32> {
    if !(0x41..=0x5A).contains(&vk) {
        return None;
    }
    let c = (vk as u8 as char).to_ascii_lowercase();
    LETTERS.iter().find(|(_, lc)| *lc == c).map(|(e, _)| *e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_anchors() {
        assert_eq!(evdev_to_vk(ev::A), Some(0x41)); // a -> 'A'
        assert_eq!(evdev_to_vk(ev::N1), Some(0x31)); // 1 -> '1'
        assert_eq!(evdev_to_vk(11), Some(0x30)); // KEY_0 -> '0'
        assert_eq!(evdev_to_vk(ev::F12), Some(0x7B)); // F12
        assert_eq!(evdev_to_vk(ev::LEFTMETA), Some(vk::LWIN));
        assert_eq!(evdev_to_vk(0xFFFF), None); // unmapped
    }

    #[test]
    fn round_trips_every_mapped_evdev_code() {
        // Every evdev code we can produce must map back to itself through VK.
        let codes: Vec<u32> = (1u32..=130).chain([ev::F11, ev::F12]).collect();
        for code in codes {
            if let Some(vk) = evdev_to_vk(code) {
                assert_eq!(
                    vk_to_evdev(vk),
                    Some(code),
                    "evdev {code} -> vk {vk:#x} did not round-trip"
                );
            }
        }
    }

    #[test]
    fn vk_letters_and_digits_round_trip() {
        for vk in (0x30u16..=0x39).chain(0x41..=0x5A) {
            let code = vk_to_evdev(vk).expect("letter/digit should map");
            assert_eq!(evdev_to_vk(code), Some(vk));
        }
    }
}
