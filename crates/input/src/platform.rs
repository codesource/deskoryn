//! Backend selection and a portable no-op backend.
//!
//! The default build compiles only [`NullCapture`]/[`NullInjector`] so the whole
//! workspace builds on any host. Real backends are added under the
//! `linux-backend` / `windows-backend` features and the `cfg(target_os)` gates
//! sketched below.

use crate::{Capture, InputError, Injector};
use async_trait::async_trait;
use deskoryn_core::geometry::Point;
use deskoryn_core::input::{InputEvent, Modifiers};

/// Which concrete backend was selected, for logging and the UI status line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Null,
    LinuxLibei,
    LinuxX11,
    LinuxEvdev,
    WindowsRawInput,
}

/// Detect the best available backend for the current OS/session.
pub fn detect() -> Backend {
    #[cfg(all(target_os = "linux", feature = "linux-backend"))]
    {
        // TODO(impl): prefer libei when XDG portal + Wayland present; else X11
        // when $DISPLAY set; else evdev/uinput if /dev/input is accessible.
        return Backend::LinuxLibei;
    }
    #[cfg(all(target_os = "windows", feature = "windows-backend"))]
    {
        return Backend::WindowsRawInput;
    }
    #[allow(unreachable_code)]
    Backend::Null
}

/// Construct a capture backend.
pub fn open_capture() -> Result<Box<dyn Capture>, InputError> {
    match detect() {
        Backend::Null => Ok(Box::new(NullCapture::default())),
        // TODO(impl): construct the real backends.
        _ => Ok(Box::new(NullCapture::default())),
    }
}

/// Construct an injection backend.
pub fn open_injector() -> Result<Box<dyn Injector>, InputError> {
    match detect() {
        Backend::Null => Ok(Box::new(NullInjector)),
        _ => Ok(Box::new(NullInjector)),
    }
}

/// A backend that captures nothing and injects nothing — used on the platform
/// that isn't currently the active machine, in tests, and in `--dry-run`.
#[derive(Default)]
pub struct NullCapture {
    mods: Modifiers,
}

#[async_trait]
impl Capture for NullCapture {
    async fn set_grabbed(&mut self, _grabbed: bool) -> Result<(), InputError> {
        Ok(())
    }
    async fn next_event(&mut self) -> Result<InputEvent, InputError> {
        // Never produces input; await forever so callers can select! over it.
        std::future::pending().await
    }
    fn modifiers(&self) -> Modifiers {
        self.mods
    }
}

pub struct NullInjector;

#[async_trait]
impl Injector for NullInjector {
    async fn warp_to(&mut self, _at: Point) -> Result<(), InputError> {
        Ok(())
    }
    async fn inject(&mut self, _event: InputEvent) -> Result<(), InputError> {
        Ok(())
    }
    async fn release_all(&mut self) -> Result<(), InputError> {
        Ok(())
    }
}

// --- Real backend module stubs (compiled only with their feature) ----------

#[cfg(all(target_os = "linux", feature = "linux-backend"))]
mod linux {
    //! libei / X11 / evdev backends.
    //!
    //! TODO(impl):
    //! * libei: connect via the `org.freedesktop.portal.InputCapture` and
    //!   `...RemoteDesktop` portals (reis crate); capture provides relative
    //!   motion + key/button; emulation injects. This is the only sanctioned
    //!   Wayland path and survives compositor security.
    //! * X11: XInput2 `XISelectEvents` raw events for capture, `XGrabPointer`/
    //!   keyboard for the grab, and the XTest extension for injection.
    //! * evdev/uinput: read `/dev/input/event*`, write a virtual device via
    //!   `/dev/uinput`. Needs the user in the `input` group / udev rule.
}

#[cfg(all(target_os = "windows", feature = "windows-backend"))]
mod windows {
    //! Raw Input + `SendInput` backend.
    //!
    //! TODO(impl):
    //! * Capture: register for `WM_INPUT` (Raw Input) for high-resolution
    //!   relative mouse + keyboard; install `WH_MOUSE_LL` / `WH_KEYBOARD_LL`
    //!   low-level hooks to *suppress* local delivery while grabbed.
    //! * Inject: `SendInput` with `MOUSEEVENTF_MOVE` (relative) and scancode
    //!   keyboard events. Note the secure-desktop / UAC limitation in
    //!   docs/OS_PROBLEMS.md.
}
