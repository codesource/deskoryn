//! Parsing of textual hotkey specs like `"Ctrl+Alt+S"` into a matcher the
//! capture loop can test each key event against.

use deskoryn_core::input::{KeyCode, Modifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: Modifiers,
    pub code: KeyCode,
}

#[derive(Debug, thiserror::Error)]
pub enum HotkeyParseError {
    #[error("empty hotkey")]
    Empty,
    #[error("unknown key token: {0}")]
    UnknownKey(String),
}

impl Hotkey {
    /// Parse `"Ctrl+Alt+S"`. Modifier tokens (case-insensitive): ctrl/control,
    /// alt, shift, meta/super/win/cmd. The final token is the trigger key.
    pub fn parse(spec: &str) -> Result<Self, HotkeyParseError> {
        let mut mods = Modifiers::empty();
        let mut key: Option<KeyCode> = None;
        for raw in spec.split('+') {
            let tok = raw.trim();
            if tok.is_empty() {
                continue;
            }
            match tok.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => mods.insert(Modifiers::CTRL),
                "alt" => mods.insert(Modifiers::ALT),
                "shift" => mods.insert(Modifiers::SHIFT),
                "meta" | "super" | "win" | "cmd" => mods.insert(Modifiers::META),
                other => key = Some(keycode_for(other).ok_or_else(|| HotkeyParseError::UnknownKey(other.into()))?),
            }
        }
        Ok(Self {
            mods,
            code: key.ok_or(HotkeyParseError::Empty)?,
        })
    }

    pub fn matches(&self, code: KeyCode, mods: Modifiers) -> bool {
        self.code == code && mods.contains(self.mods)
    }
}

/// Map a single-character or named key token to an evdev key code (the canonical
/// wire space — see [`deskoryn_core::input::KeyCode`]).
///
/// Only the handful of tokens needed for default hotkeys are mapped here; the
/// real backend ships full evdev tables.
fn keycode_for(tok: &str) -> Option<KeyCode> {
    // A tiny subset of `linux/input-event-codes.h` (KEY_A == 30, ... KEY_Z).
    const KEY_A: u32 = 30;
    let c = tok.chars().next()?;
    if tok.len() == 1 && c.is_ascii_alphabetic() {
        // evdev orders the letter row a,b,c... starting at KEY_A for 'a'.
        let idx = (c.to_ascii_lowercase() as u32) - ('a' as u32);
        return Some(KeyCode(KEY_A + idx));
    }
    match tok {
        "esc" | "escape" => Some(KeyCode(1)),
        "space" => Some(KeyCode(57)),
        "f12" => Some(KeyCode(88)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifier_combo() {
        let h = Hotkey::parse("Ctrl+Alt+S").unwrap();
        assert!(h.mods.contains(Modifiers::CTRL));
        assert!(h.mods.contains(Modifiers::ALT));
        // 's' is the 19th letter (0-indexed 18) after 'a' == 30.
        assert_eq!(h.code, KeyCode(30 + 18));
    }

    #[test]
    fn matcher_requires_all_mods() {
        let h = Hotkey::parse("Ctrl+Alt+S").unwrap();
        assert!(h.matches(KeyCode(48), Modifiers::CTRL | Modifiers::ALT));
        assert!(!h.matches(KeyCode(48), Modifiers::CTRL));
    }
}
