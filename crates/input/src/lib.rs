//! # deskoryn-input
//!
//! Two halves of software-KVM input sharing, behind OS-neutral traits:
//!
//! * [`Capture`] — on the machine that currently owns the cursor, grab pointer +
//!   keyboard events at a low level (and *suppress* local delivery once the
//!   cursor has left this machine's monitors). Emits [`InputEvent`]s.
//! * [`Injector`] — on the machine receiving control, synthesize OS input events
//!   from incoming [`InputEvent`]s.
//!
//! Backends are picked at runtime by [`platform::detect`]:
//!
//! | OS / session    | Capture                          | Inject                        |
//! |-----------------|----------------------------------|-------------------------------|
//! | Linux/Wayland   | libei (input-capture portal)     | libei (input emulation)       |
//! | Linux/X11       | XInput2 raw events + XGrab        | XTest                         |
//! | Linux fallback  | evdev (read /dev/input)          | uinput (virtual device)       |
//! | Windows         | Raw Input + low-level hooks      | `SendInput`                   |
//!
//! See `docs/OS_PROBLEMS.md` for why each path exists and its caveats (Wayland
//! security model, uinput permissions, UAC/secure-desktop, etc.).

pub mod hotkey;
pub mod keymap;
pub mod platform;

use async_trait::async_trait;
use deskoryn_core::input::InputEvent;
use deskoryn_core::geometry::Point;

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("no usable input backend on this platform/session")]
    NoBackend,
    #[error("permission denied (see docs/OS_PROBLEMS.md): {0}")]
    Permission(String),
    #[error("backend error: {0}")]
    Backend(String),
}

/// Captures local input and reports events while this machine owns the cursor.
#[async_trait]
pub trait Capture: Send {
    /// Start capturing. While `grabbed` is true the backend must prevent events
    /// from reaching local applications (the cursor is "on the other machine").
    async fn set_grabbed(&mut self, grabbed: bool) -> Result<(), InputError>;

    /// Await the next local input event (motion is relative where possible).
    async fn next_event(&mut self) -> Result<InputEvent, InputError>;

    /// Current modifier state (queried on handoff to keep peers consistent).
    fn modifiers(&self) -> deskoryn_core::input::Modifiers;
}

/// Injects synthetic input on the machine currently receiving control.
#[async_trait]
pub trait Injector: Send {
    /// Warp the local cursor to a virtual-desktop point that maps onto a local
    /// monitor (called on `Enter`).
    async fn warp_to(&mut self, at: Point) -> Result<(), InputError>;

    async fn inject(&mut self, event: InputEvent) -> Result<(), InputError>;

    /// Release any held keys/buttons (called on `Leave` and on disconnect so a
    /// dropped connection can't leave a key stuck down).
    async fn release_all(&mut self) -> Result<(), InputError>;
}
