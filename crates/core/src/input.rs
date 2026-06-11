//! Platform-neutral input events.
//!
//! The input crate translates OS-native events (evdev, libei, Windows Raw Input)
//! into these on the capture side, and back into OS injection calls (uinput,
//! libei, `SendInput`) on the receiving side. Keeping the wire vocabulary OS-
//! neutral is what lets a key pressed on the Linux keyboard land in a Windows app.

use crate::geometry::Point;
use serde::{Deserialize, Serialize};

/// A hardware-neutral key code.
///
/// We standardize on the Linux evdev key code space as the canonical wire value
/// (it is stable, well documented, and a superset in practice). The Windows
/// agent maps to/from Virtual-Key + scan codes at the edge. See
/// `deskoryn-input` for the translation tables.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Serialize, Deserialize)]
pub struct KeyCode(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    /// Extra buttons by index for high-button mice.
    Other(u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ScrollAxis {
    Vertical,
    Horizontal,
}

/// Modifier state carried alongside key events so the receiver can keep a
/// consistent view even if it missed an edge during a handoff.
///
/// A tiny hand-rolled bitset (kept dependency-free on purpose).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash, Serialize, Deserialize)]
pub struct Modifiers(pub u16);

impl Modifiers {
    pub const SHIFT: Modifiers = Modifiers(1 << 0);
    pub const CTRL: Modifiers = Modifiers(1 << 1);
    pub const ALT: Modifiers = Modifiers(1 << 2);
    pub const META: Modifiers = Modifiers(1 << 3); // Super / Windows / Command
    pub const CAPS: Modifiers = Modifiers(1 << 4);
    pub const NUM: Modifiers = Modifiers(1 << 5);

    pub const fn empty() -> Self {
        Modifiers(0)
    }
    pub const fn contains(self, other: Modifiers) -> bool {
        self.0 & other.0 == other.0
    }
    pub fn insert(&mut self, other: Modifiers) {
        self.0 |= other.0;
    }
    pub fn remove(&mut self, other: Modifiers) {
        self.0 &= !other.0;
    }
    pub fn set(&mut self, other: Modifiers, on: bool) {
        if on {
            self.insert(other);
        } else {
            self.remove(other);
        }
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Modifiers;
    fn bitor(self, rhs: Modifiers) -> Modifiers {
        Modifiers(self.0 | rhs.0)
    }
}

/// A single input event in virtual-desktop terms.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub enum InputEvent {
    /// Absolute pointer position in virtual-desktop pixels. Sent on handoff and
    /// periodically to correct drift.
    PointerPosition { at: Point },
    /// Relative pointer motion in virtual-desktop pixels. Preferred during
    /// normal movement (immune to rounding under display scaling).
    PointerMotion { dx: i32, dy: i32 },
    Button { button: Button, pressed: bool },
    Scroll { axis: ScrollAxis, delta: i32, /// high-resolution sub-step (0 if N/A)
        hi_res: i32 },
    Key { code: KeyCode, pressed: bool, mods: Modifiers },
}
