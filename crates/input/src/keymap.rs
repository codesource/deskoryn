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

    // Numpad (note: evdev's keypad digits are not in numeric order).
    pub const KPASTERISK: u32 = 55;
    pub const KP7: u32 = 71;
    pub const KP8: u32 = 72;
    pub const KP9: u32 = 73;
    pub const KPMINUS: u32 = 74;
    pub const KP4: u32 = 75;
    pub const KP5: u32 = 76;
    pub const KP6: u32 = 77;
    pub const KPPLUS: u32 = 78;
    pub const KP1: u32 = 79;
    pub const KP2: u32 = 80;
    pub const KP3: u32 = 81;
    pub const KP0: u32 = 82;
    pub const KPDOT: u32 = 83;
    pub const KPSLASH: u32 = 98;

    // System / editing.
    pub const SYSRQ: u32 = 99; // Print Screen
    pub const PAUSE: u32 = 119;
    pub const COMPOSE: u32 = 127; // Menu / Apps key

    // Media.
    pub const MUTE: u32 = 113;
    pub const VOLUMEDOWN: u32 = 114;
    pub const VOLUMEUP: u32 = 115;
    pub const NEXTSONG: u32 = 163;
    pub const PLAYPAUSE: u32 = 164;
    pub const PREVIOUSSONG: u32 = 165;
    pub const STOPCD: u32 = 166;
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

    // Numpad (`VK_NUMPAD0..9` == 0x60..0x69).
    pub const NUMPAD0: u16 = 0x60;
    pub const MULTIPLY: u16 = 0x6A;
    pub const ADD: u16 = 0x6B;
    pub const SUBTRACT: u16 = 0x6D;
    pub const DECIMAL: u16 = 0x6E;
    pub const DIVIDE: u16 = 0x6F;

    // System / editing.
    pub const PAUSE: u16 = 0x13;
    pub const SNAPSHOT: u16 = 0x2C; // Print Screen
    pub const APPS: u16 = 0x5D; // Menu key

    // Media.
    pub const VOLUME_MUTE: u16 = 0xAD;
    pub const VOLUME_DOWN: u16 = 0xAE;
    pub const VOLUME_UP: u16 = 0xAF;
    pub const MEDIA_NEXT_TRACK: u16 = 0xB0;
    pub const MEDIA_PREV_TRACK: u16 = 0xB1;
    pub const MEDIA_STOP: u16 = 0xB2;
    pub const MEDIA_PLAY_PAUSE: u16 = 0xB3;
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
    // Numpad.
    (ev::KP0, vk::NUMPAD0),
    (ev::KP1, vk::NUMPAD0 + 1),
    (ev::KP2, vk::NUMPAD0 + 2),
    (ev::KP3, vk::NUMPAD0 + 3),
    (ev::KP4, vk::NUMPAD0 + 4),
    (ev::KP5, vk::NUMPAD0 + 5),
    (ev::KP6, vk::NUMPAD0 + 6),
    (ev::KP7, vk::NUMPAD0 + 7),
    (ev::KP8, vk::NUMPAD0 + 8),
    (ev::KP9, vk::NUMPAD0 + 9),
    (ev::KPASTERISK, vk::MULTIPLY),
    (ev::KPPLUS, vk::ADD),
    (ev::KPMINUS, vk::SUBTRACT),
    (ev::KPDOT, vk::DECIMAL),
    (ev::KPSLASH, vk::DIVIDE),
    // System / editing.
    (ev::SYSRQ, vk::SNAPSHOT),
    (ev::PAUSE, vk::PAUSE),
    (ev::COMPOSE, vk::APPS),
    // Media.
    (ev::MUTE, vk::VOLUME_MUTE),
    (ev::VOLUMEDOWN, vk::VOLUME_DOWN),
    (ev::VOLUMEUP, vk::VOLUME_UP),
    (ev::NEXTSONG, vk::MEDIA_NEXT_TRACK),
    (ev::PREVIOUSSONG, vk::MEDIA_PREV_TRACK),
    (ev::STOPCD, vk::MEDIA_STOP),
    (ev::PLAYPAUSE, vk::MEDIA_PLAY_PAUSE),
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
        // Extended set: numpad, media, system.
        assert_eq!(evdev_to_vk(ev::KP5), Some(0x65)); // VK_NUMPAD5
        assert_eq!(evdev_to_vk(ev::KPPLUS), Some(vk::ADD));
        assert_eq!(evdev_to_vk(ev::VOLUMEUP), Some(vk::VOLUME_UP));
        assert_eq!(evdev_to_vk(ev::SYSRQ), Some(vk::SNAPSHOT)); // Print Screen
        assert_eq!(evdev_to_vk(ev::COMPOSE), Some(vk::APPS)); // Menu
    }

    #[test]
    fn round_trips_every_mapped_evdev_code() {
        // Every evdev code we can produce must map back to itself through VK.
        // Range covers the base set, numpad, system, and the media block (163+).
        let codes: Vec<u32> = (1u32..=170).chain([ev::F11, ev::F12]).collect();
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
