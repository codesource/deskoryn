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

/// Construct a capture backend. Falls back to [`NullCapture`] if the real
/// backend can't initialize (e.g. missing device permissions).
pub fn open_capture() -> Result<Box<dyn Capture>, InputError> {
    #[cfg(all(target_os = "linux", feature = "linux-backend"))]
    {
        match linux::EvdevCapture::open() {
            Ok(cap) => return Ok(Box::new(cap)),
            Err(e) => tracing::warn!(error = %e, "evdev capture unavailable; using null backend"),
        }
    }
    Ok(Box::new(NullCapture::default()))
}

/// Construct an injection backend. Falls back to [`NullInjector`] on failure.
pub fn open_injector() -> Result<Box<dyn Injector>, InputError> {
    #[cfg(all(target_os = "linux", feature = "linux-backend"))]
    {
        match linux::UinputInjector::open() {
            Ok(inj) => return Ok(Box::new(inj)),
            Err(e) => tracing::warn!(error = %e, "uinput injector unavailable; using null backend"),
        }
    }
    Ok(Box::new(NullInjector))
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
    //! evdev capture + uinput injection.
    //!
    //! This is the universal Linux path (works under X11 and Wayland), at the
    //! cost of needing device permissions (`input` group / udev rule on
    //! `/dev/uinput` and `/dev/input/*`). libei (the sanctioned Wayland portal
    //! path) is the preferred future addition; see `docs/OS_PROBLEMS.md`.
    //!
    //! NOTE: this backend is compile-verified but, unlike the rest of the
    //! codebase, is **not** covered by automated tests — it requires real input
    //! devices and `/dev/uinput`. Validate on hardware before relying on it.

    use super::{Capture, InputError, Injector};
    use async_trait::async_trait;
    use deskoryn_core::geometry::Point;
    use deskoryn_core::input::{Button, InputEvent, KeyCode, Modifiers, ScrollAxis};
    use evdev::{
        AttributeSet, EventType, InputEvent as EvEvent, InputEventKind, Key, RelativeAxisType,
    };
    use tokio::sync::{mpsc, watch};

    fn io(e: impl std::fmt::Display) -> InputError {
        InputError::Backend(e.to_string())
    }

    // --- Capture: read every keyboard/mouse, grab on demand, translate --------

    pub struct EvdevCapture {
        rx: mpsc::UnboundedReceiver<InputEvent>,
        grab: watch::Sender<bool>,
        mods: Modifiers,
    }

    impl EvdevCapture {
        pub fn open() -> Result<Self, InputError> {
            let (tx, rx) = mpsc::unbounded_channel();
            let (grab_tx, grab_rx) = watch::channel(false);
            let mut found = 0;

            for (_path, dev) in evdev::enumerate() {
                let is_input =
                    dev.supported_keys().is_some() || dev.supported_relative_axes().is_some();
                if !is_input {
                    continue;
                }
                let stream = match dev.into_event_stream() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                found += 1;
                spawn_reader(stream, tx.clone(), grab_rx.clone());
            }

            if found == 0 {
                return Err(InputError::Permission(
                    "no readable input devices (add the user to the `input` group)".into(),
                ));
            }
            Ok(Self { rx, grab: grab_tx, mods: Modifiers::empty() })
        }
    }

    fn spawn_reader(
        mut stream: evdev::EventStream,
        tx: mpsc::UnboundedSender<InputEvent>,
        mut grab_rx: watch::Receiver<bool>,
    ) {
        tokio::spawn(async move {
            let mut grabbed = false;
            loop {
                tokio::select! {
                    changed = grab_rx.changed() => {
                        if changed.is_err() { break; }
                        let want = *grab_rx.borrow();
                        if want != grabbed {
                            let dev = stream.device_mut();
                            let _ = if want { dev.grab() } else { dev.ungrab() };
                            grabbed = want;
                        }
                    }
                    ev = stream.next_event() => {
                        match ev {
                            Ok(ev) => {
                                if let Some(translated) = translate(&ev) {
                                    if tx.send(translated).is_err() { break; }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });
    }

    fn translate(ev: &EvEvent) -> Option<InputEvent> {
        match ev.kind() {
            InputEventKind::RelAxis(axis) => {
                if axis == RelativeAxisType::REL_X {
                    Some(InputEvent::PointerMotion { dx: ev.value(), dy: 0 })
                } else if axis == RelativeAxisType::REL_Y {
                    Some(InputEvent::PointerMotion { dx: 0, dy: ev.value() })
                } else if axis == RelativeAxisType::REL_WHEEL {
                    Some(InputEvent::Scroll { axis: ScrollAxis::Vertical, delta: ev.value(), hi_res: 0 })
                } else if axis == RelativeAxisType::REL_HWHEEL {
                    Some(InputEvent::Scroll { axis: ScrollAxis::Horizontal, delta: ev.value(), hi_res: 0 })
                } else {
                    None
                }
            }
            InputEventKind::Key(key) => {
                let pressed = ev.value() != 0; // 1 down, 2 repeat, 0 up
                match button_from(key) {
                    Some(button) => Some(InputEvent::Button { button, pressed }),
                    None => Some(InputEvent::Key {
                        code: KeyCode(key.code() as u32),
                        pressed,
                        mods: Modifiers::empty(),
                    }),
                }
            }
            _ => None,
        }
    }

    fn button_from(key: Key) -> Option<Button> {
        match key {
            Key::BTN_LEFT => Some(Button::Left),
            Key::BTN_RIGHT => Some(Button::Right),
            Key::BTN_MIDDLE => Some(Button::Middle),
            Key::BTN_SIDE => Some(Button::Back),
            Key::BTN_EXTRA => Some(Button::Forward),
            _ => None,
        }
    }

    #[async_trait]
    impl Capture for EvdevCapture {
        async fn set_grabbed(&mut self, grabbed: bool) -> Result<(), InputError> {
            self.grab.send(grabbed).map_err(io)
        }
        async fn next_event(&mut self) -> Result<InputEvent, InputError> {
            self.rx.recv().await.ok_or_else(|| InputError::Backend("capture stopped".into()))
        }
        fn modifiers(&self) -> Modifiers {
            self.mods
        }
    }

    // --- Injection: a uinput virtual device -----------------------------------

    pub struct UinputInjector {
        dev: evdev::uinput::VirtualDevice,
    }

    impl UinputInjector {
        pub fn open() -> Result<Self, InputError> {
            let mut keys = AttributeSet::<Key>::new();
            for c in 1u16..=255 {
                keys.insert(Key::new(c));
            }
            for b in [Key::BTN_LEFT, Key::BTN_RIGHT, Key::BTN_MIDDLE, Key::BTN_SIDE, Key::BTN_EXTRA] {
                keys.insert(b);
            }
            let mut rels = AttributeSet::<RelativeAxisType>::new();
            for a in [
                RelativeAxisType::REL_X,
                RelativeAxisType::REL_Y,
                RelativeAxisType::REL_WHEEL,
                RelativeAxisType::REL_HWHEEL,
            ] {
                rels.insert(a);
            }
            let dev = evdev::uinput::VirtualDeviceBuilder::new()
                .map_err(io)?
                .name("Deskoryn Virtual Input")
                .with_keys(&keys)
                .map_err(io)?
                .with_relative_axes(&rels)
                .map_err(io)?
                .build()
                .map_err(io)?;
            Ok(Self { dev })
        }
    }

    fn to_evdev(event: &InputEvent) -> Vec<EvEvent> {
        match *event {
            InputEvent::PointerMotion { dx, dy } => vec![
                EvEvent::new(EventType::RELATIVE, RelativeAxisType::REL_X.0, dx),
                EvEvent::new(EventType::RELATIVE, RelativeAxisType::REL_Y.0, dy),
            ],
            InputEvent::Button { button, pressed } => {
                vec![EvEvent::new(EventType::KEY, button_key(button).code(), pressed as i32)]
            }
            InputEvent::Key { code, pressed, .. } => {
                vec![EvEvent::new(EventType::KEY, code.0 as u16, pressed as i32)]
            }
            InputEvent::Scroll { axis, delta, .. } => {
                let code = match axis {
                    ScrollAxis::Vertical => RelativeAxisType::REL_WHEEL.0,
                    ScrollAxis::Horizontal => RelativeAxisType::REL_HWHEEL.0,
                };
                vec![EvEvent::new(EventType::RELATIVE, code, delta)]
            }
            // Absolute positioning isn't expressible on a relative uinput device.
            InputEvent::PointerPosition { .. } => Vec::new(),
        }
    }

    fn button_key(button: Button) -> Key {
        match button {
            Button::Left => Key::BTN_LEFT,
            Button::Right => Key::BTN_RIGHT,
            Button::Middle => Key::BTN_MIDDLE,
            Button::Back => Key::BTN_SIDE,
            Button::Forward => Key::BTN_EXTRA,
            Button::Other(_) => Key::BTN_LEFT,
        }
    }

    #[async_trait]
    impl Injector for UinputInjector {
        async fn warp_to(&mut self, _at: Point) -> Result<(), InputError> {
            // A relative uinput device can't be warped to an absolute point; the
            // cursor is positioned purely by the forwarded relative motion. (An
            // absolute-axis device is a future enhancement for exact entry.)
            Ok(())
        }
        async fn inject(&mut self, event: InputEvent) -> Result<(), InputError> {
            let events = to_evdev(&event);
            if events.is_empty() {
                return Ok(());
            }
            self.dev.emit(&events).map_err(io) // emit() appends SYN_REPORT
        }
        async fn release_all(&mut self) -> Result<(), InputError> {
            // Release the mouse buttons we might be holding (we can't enumerate
            // held keyboard keys; the receiver's own key-repeat will settle).
            let ups: Vec<EvEvent> = [Key::BTN_LEFT, Key::BTN_RIGHT, Key::BTN_MIDDLE]
                .into_iter()
                .map(|b| EvEvent::new(EventType::KEY, b.code(), 0))
                .collect();
            self.dev.emit(&ups).map_err(io)
        }
    }
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
