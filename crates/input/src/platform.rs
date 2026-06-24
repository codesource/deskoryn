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
    WindowsHooks,
}

/// Detect the best available backend for the current OS/session.
///
/// Reports the backend that [`open_capture`]/[`open_injector`] will actually use,
/// so the value is meaningful in logs and the status line. Today the only real
/// Linux backend is evdev/uinput; libei (the sanctioned Wayland portal path) and
/// X11/XTest are future additions and would be preferred here once implemented:
///
/// * libei when an XDG portal + Wayland session is present,
/// * else X11/XTest when `$DISPLAY` is set,
/// * else evdev/uinput when `/dev/input` + `/dev/uinput` are accessible.
pub fn detect() -> Backend {
    #[cfg(all(target_os = "linux", feature = "linux-backend"))]
    {
        return Backend::LinuxEvdev;
    }
    #[cfg(all(target_os = "windows", feature = "windows-backend"))]
    {
        return Backend::WindowsHooks;
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
    #[cfg(all(target_os = "windows", feature = "windows-backend"))]
    {
        match windows_backend::HookCapture::open() {
            Ok(cap) => return Ok(Box::new(cap)),
            Err(e) => tracing::warn!(error = %e, "hook capture unavailable; using null backend"),
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
    #[cfg(all(target_os = "windows", feature = "windows-backend"))]
    {
        return Ok(Box::new(windows_backend::SendInputInjector));
    }
    #[allow(unreachable_code)]
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
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::Arc;
    use tokio::sync::{mpsc, watch};

    fn io(e: impl std::fmt::Display) -> InputError {
        InputError::Backend(e.to_string())
    }

    // --- Capture: read every keyboard/mouse, grab on demand, translate --------

    pub struct EvdevCapture {
        rx: mpsc::UnboundedReceiver<InputEvent>,
        grab: watch::Sender<bool>,
        /// Live modifier state ([`Modifiers`] bits), shared with the per-device
        /// reader tasks so every key event carries the current modifiers and the
        /// switch/lock hotkeys can match.
        mods: Arc<AtomicU16>,
    }

    impl EvdevCapture {
        pub fn open() -> Result<Self, InputError> {
            let (tx, rx) = mpsc::unbounded_channel();
            let (grab_tx, grab_rx) = watch::channel(false);
            let mods = Arc::new(AtomicU16::new(0));
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
                spawn_reader(stream, tx.clone(), grab_rx.clone(), mods.clone());
            }

            if found == 0 {
                return Err(InputError::Permission(
                    "no readable input devices (add the user to the `input` group)".into(),
                ));
            }
            Ok(Self { rx, grab: grab_tx, mods })
        }
    }

    fn spawn_reader(
        mut stream: evdev::EventStream,
        tx: mpsc::UnboundedSender<InputEvent>,
        mut grab_rx: watch::Receiver<bool>,
        mods: Arc<AtomicU16>,
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
                                    let translated = stamp_mods(translated, &mods);
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

    /// Update the shared modifier state from a key event and stamp the current
    /// modifiers onto it. Non-key events pass through unchanged.
    fn stamp_mods(ev: InputEvent, mods: &AtomicU16) -> InputEvent {
        if let InputEvent::Key { code, pressed, .. } = ev {
            if let Some(bit) = modifier_bit(code.0) {
                let mut m = Modifiers(mods.load(Ordering::Relaxed));
                m.set(bit, pressed);
                mods.store(m.0, Ordering::Relaxed);
            }
            InputEvent::Key { code, pressed, mods: Modifiers(mods.load(Ordering::Relaxed)) }
        } else {
            ev
        }
    }

    /// Map an evdev modifier keycode to its [`Modifiers`] bit. Left/right pairs
    /// share a bit, so holding one while releasing the other clears it early — an
    /// accepted simplification (the hotkey matcher only needs the combined bit).
    fn modifier_bit(code: u32) -> Option<Modifiers> {
        match code {
            29 | 97 => Some(Modifiers::CTRL),   // LEFT/RIGHT CTRL
            42 | 54 => Some(Modifiers::SHIFT),  // LEFT/RIGHT SHIFT
            56 | 100 => Some(Modifiers::ALT),   // LEFT/RIGHT ALT
            125 | 126 => Some(Modifiers::META), // LEFT/RIGHT META (Super)
            _ => None,
        }
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
            Modifiers(self.mods.load(Ordering::Relaxed))
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
            // No-op for now: a relative uinput device can't warp, and a separate
            // absolute uinput device gets misclassified by libinput (it synthesized
            // phantom button events). Exact entry on Linux needs a libinput-safe
            // path (e.g. XTest on X11); until then the cursor reaches the entry
            // point via the forwarded relative motion.
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
mod windows_backend {
    //! Low-level-hook capture + `SendInput` injection.
    //!
    //! **Capture** installs `WH_MOUSE_LL` + `WH_KEYBOARD_LL` hooks on a dedicated
    //! thread running a message loop. The hooks translate every event to the
    //! evdev wire space and forward it to the async [`Capture`]. While *grabbed*
    //! they return non-zero to **suppress** local delivery and recenter the
    //! cursor each motion, so relative deltas keep flowing without the pointer
    //! piling up against a screen edge. Events flagged injected (our own
    //! `SendInput`, or another tool's) are ignored, so a machine never re-captures
    //! what it just injected.
    //!
    //! **Injection** uses `SendInput` with relative `MOUSEEVENTF_MOVE`, the button
    //! / wheel flags, and virtual-key keyboard events (translated from evdev via
    //! [`crate::keymap`]).
    //!
    //! NOTE: like the Linux backend, this is compile-verified (incl. cross-compile)
    //! but **not** covered by automated tests — it needs a real Windows session.
    //! See `docs/OS_PROBLEMS.md` for the UAC / secure-desktop limitation (injection
    //! into elevated windows requires the daemon to be elevated too). Assumes a
    //! single live [`HookCapture`] at a time (the daemon holds one per session).

    use super::{Capture, InputError, Injector};
    use crate::keymap;
    use async_trait::async_trait;
    use deskoryn_core::geometry::Point;
    use deskoryn_core::input::{Button, InputEvent, KeyCode, Modifiers, ScrollAxis};
    use std::mem::size_of;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::sync::Mutex;
    use tokio::sync::mpsc;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE,
        KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_VIRTUALDESK,
        MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
        MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU,
        VK_RWIN, VK_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, GetSystemMetrics, PostThreadMessageW, SetCursorPos, SetWindowsHookExW,
        UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, MSLLHOOKSTRUCT,
        SM_CXSCREEN, SM_CYSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
        WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
        WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN,
        WM_XBUTTONUP,
    };

    /// Injected low-level mouse events carry this flag in `MSLLHOOKSTRUCT::flags`.
    const LLMHF_INJECTED: u32 = 0x0000_0001;
    /// `WHEEL_DELTA`: one wheel notch.
    const WHEEL_DELTA: i32 = 120;
    /// X-button identifiers (`mouseData` high word on `WM_XBUTTON*`).
    const XBUTTON1: u32 = 0x0001;
    const XBUTTON2: u32 = 0x0002;

    // --- Shared state between the hook thread and the async side ---------------
    //
    // The hook procs are plain `extern "system"` functions, so they reach the
    // active capture through process-global state. We assume one live capture.
    static EVENTS: Mutex<Option<mpsc::UnboundedSender<InputEvent>>> = Mutex::new(None);
    static GRABBED: AtomicBool = AtomicBool::new(false);
    static CENTER_X: AtomicI32 = AtomicI32::new(0);
    static CENTER_Y: AtomicI32 = AtomicI32::new(0);
    static LAST_X: AtomicI32 = AtomicI32::new(0);
    static LAST_Y: AtomicI32 = AtomicI32::new(0);

    fn emit(ev: InputEvent) {
        if let Ok(guard) = EVENTS.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(ev);
            }
        }
    }

    fn io(e: impl std::fmt::Display) -> InputError {
        InputError::Backend(e.to_string())
    }

    // --- Capture ---------------------------------------------------------------

    pub struct HookCapture {
        rx: mpsc::UnboundedReceiver<InputEvent>,
        thread_id: u32,
        join: Option<std::thread::JoinHandle<()>>,
        mods: Modifiers,
    }

    impl HookCapture {
        pub fn open() -> Result<Self, InputError> {
            let (tx, rx) = mpsc::unbounded_channel();
            *EVENTS.lock().map_err(|_| io("event slot poisoned"))? = Some(tx);
            GRABBED.store(false, Ordering::SeqCst);

            let (id_tx, id_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
            let join = std::thread::Builder::new()
                .name("deskoryn-input-hooks".into())
                .spawn(move || hook_thread(id_tx))
                .map_err(io)?;

            let thread_id = match id_rx.recv() {
                Ok(Ok(id)) => id,
                Ok(Err(e)) => return Err(InputError::Backend(e)),
                Err(_) => return Err(io("hook thread exited before reporting ready")),
            };
            Ok(Self { rx, thread_id, join: Some(join), mods: Modifiers::empty() })
        }
    }

    impl Drop for HookCapture {
        fn drop(&mut self) {
            // Ask the hook thread's message loop to quit, then join it.
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
            if let Ok(mut guard) = EVENTS.lock() {
                *guard = None;
            }
            GRABBED.store(false, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl Capture for HookCapture {
        async fn set_grabbed(&mut self, grabbed: bool) -> Result<(), InputError> {
            if grabbed && !GRABBED.load(Ordering::SeqCst) {
                // Anchor recenter at the middle of the primary screen and warp
                // there so deltas are measured from a stable point.
                let (cx, cy) = unsafe { (GetSystemMetrics(SM_CXSCREEN) / 2, GetSystemMetrics(SM_CYSCREEN) / 2) };
                CENTER_X.store(cx, Ordering::SeqCst);
                CENTER_Y.store(cy, Ordering::SeqCst);
                unsafe { SetCursorPos(cx, cy).map_err(io)? };
            }
            GRABBED.store(grabbed, Ordering::SeqCst);
            Ok(())
        }
        async fn next_event(&mut self) -> Result<InputEvent, InputError> {
            let ev = self.rx.recv().await.ok_or_else(|| io("capture stopped"))?;
            // Track modifier state from forwarded key events for status/UI.
            if let InputEvent::Key { mods, .. } = ev {
                self.mods = mods;
            }
            Ok(ev)
        }
        fn modifiers(&self) -> Modifiers {
            self.mods
        }
    }

    /// Runs on the dedicated hook thread: install hooks, report the thread id,
    /// pump messages until `WM_QUIT`, then unhook.
    fn hook_thread(id_tx: std::sync::mpsc::Sender<Result<u32, String>>) {
        let hmod = match unsafe { GetModuleHandleW(PCWSTR::null()) } {
            Ok(h) => HINSTANCE(h.0),
            Err(e) => {
                let _ = id_tx.send(Err(format!("GetModuleHandleW: {e}")));
                return;
            }
        };
        let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hmod, 0) };
        let kbd = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(kbd_proc), hmod, 0) };
        let (mouse, kbd) = match (mouse, kbd) {
            (Ok(m), Ok(k)) => (m, k),
            (m, k) => {
                if let Ok(h) = m {
                    unsafe { let _ = UnhookWindowsHookEx(h); }
                }
                if let Ok(h) = k {
                    unsafe { let _ = UnhookWindowsHookEx(h); }
                }
                let _ = id_tx.send(Err("SetWindowsHookExW failed".into()));
                return;
            }
        };
        let _ = id_tx.send(Ok(unsafe { GetCurrentThreadId() }));

        // Standard message loop; GetMessageW returns 0 on WM_QUIT.
        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {}

        unsafe {
            let _ = UnhookWindowsHookEx(mouse);
            let _ = UnhookWindowsHookEx(kbd);
        }
    }

    extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code < 0 {
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        }
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        // Never re-capture injected motion/clicks (ours or another tool's).
        if info.flags & LLMHF_INJECTED != 0 {
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        }
        let grabbed = GRABBED.load(Ordering::SeqCst);
        let msg = wparam.0 as u32;

        if msg == WM_MOUSEMOVE {
            if grabbed {
                let (cx, cy) = (CENTER_X.load(Ordering::SeqCst), CENTER_Y.load(Ordering::SeqCst));
                let (dx, dy) = (info.pt.x - cx, info.pt.y - cy);
                if dx == 0 && dy == 0 {
                    // The recenter we just issued; swallow without emitting.
                    return LRESULT(1);
                }
                emit(InputEvent::PointerMotion { dx, dy });
                unsafe { let _ = SetCursorPos(cx, cy); }
                return LRESULT(1);
            } else {
                let (lx, ly) = (LAST_X.swap(info.pt.x, Ordering::SeqCst), LAST_Y.swap(info.pt.y, Ordering::SeqCst));
                emit(InputEvent::PointerMotion { dx: info.pt.x - lx, dy: info.pt.y - ly });
                return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
            }
        }

        if let Some(ev) = mouse_event(msg, info) {
            emit(ev);
        }
        if grabbed {
            LRESULT(1)
        } else {
            unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
        }
    }

    fn mouse_event(msg: u32, info: &MSLLHOOKSTRUCT) -> Option<InputEvent> {
        let button = |button, pressed| Some(InputEvent::Button { button, pressed });
        match msg {
            WM_LBUTTONDOWN => button(Button::Left, true),
            WM_LBUTTONUP => button(Button::Left, false),
            WM_RBUTTONDOWN => button(Button::Right, true),
            WM_RBUTTONUP => button(Button::Right, false),
            WM_MBUTTONDOWN => button(Button::Middle, true),
            WM_MBUTTONUP => button(Button::Middle, false),
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let xbtn = (info.mouseData >> 16) & 0xFFFF;
                let b = if xbtn == XBUTTON1 { Button::Back } else { Button::Forward };
                button(b, msg == WM_XBUTTONDOWN)
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let raw = (info.mouseData >> 16) as i16 as i32;
                let axis = if msg == WM_MOUSEWHEEL { ScrollAxis::Vertical } else { ScrollAxis::Horizontal };
                Some(InputEvent::Scroll { axis, delta: raw / WHEEL_DELTA, hi_res: raw })
            }
            _ => None,
        }
    }

    extern "system" fn kbd_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code < 0 {
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        }
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if info.flags.0 & LLKHF_INJECTED.0 != 0 {
            return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
        }
        let msg = wparam.0 as u32;
        let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let released = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        if pressed || released {
            if let Some(evcode) = keymap::vk_to_evdev(info.vkCode as u16) {
                emit(InputEvent::Key { code: KeyCode(evcode), pressed, mods: current_mods() });
            }
        }
        if GRABBED.load(Ordering::SeqCst) {
            LRESULT(1)
        } else {
            unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
        }
    }

    fn current_mods() -> Modifiers {
        let down = |vk: VIRTUAL_KEY| (unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000) != 0;
        let mut m = Modifiers::empty();
        m.set(Modifiers::SHIFT, down(VK_SHIFT));
        m.set(Modifiers::CTRL, down(VK_CONTROL));
        m.set(Modifiers::ALT, down(VK_MENU));
        m.set(Modifiers::META, down(VK_LWIN) || down(VK_RWIN));
        m
    }

    // --- Injection -------------------------------------------------------------

    pub struct SendInputInjector;

    impl SendInputInjector {
        fn send_mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> Result<(), InputError> {
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 },
                },
            };
            let n = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
            if n == 1 { Ok(()) } else { Err(io("SendInput (mouse) rejected")) }
        }

        fn send_key(vk: u16, up: bool) -> Result<(), InputError> {
            let flags = if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 },
                },
            };
            let n = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
            if n == 1 { Ok(()) } else { Err(io("SendInput (key) rejected")) }
        }
    }

    #[async_trait]
    impl Injector for SendInputInjector {
        async fn warp_to(&mut self, at: Point) -> Result<(), InputError> {
            // `at` is normalized 0..=65535 over the virtual desktop, exactly the
            // convention MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK expects, so
            // the cursor lands where the pointer crossed in.
            Self::send_mouse(
                at.x,
                at.y,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            )
        }
        async fn inject(&mut self, event: InputEvent) -> Result<(), InputError> {
            match event {
                InputEvent::PointerMotion { dx, dy } => {
                    Self::send_mouse(dx, dy, 0, MOUSEEVENTF_MOVE)
                }
                InputEvent::Button { button, pressed } => {
                    let (flags, data) = button_flags(button, pressed);
                    Self::send_mouse(0, 0, data, flags)
                }
                InputEvent::Scroll { axis, delta, hi_res } => {
                    let amount = if hi_res != 0 { hi_res } else { delta * WHEEL_DELTA };
                    let flags = match axis {
                        ScrollAxis::Vertical => MOUSEEVENTF_WHEEL,
                        ScrollAxis::Horizontal => MOUSEEVENTF_HWHEEL,
                    };
                    Self::send_mouse(0, 0, amount as u32, flags)
                }
                InputEvent::Key { code, pressed, .. } => match keymap::evdev_to_vk(code.0) {
                    Some(vk) => Self::send_key(vk, !pressed),
                    None => Ok(()), // unmapped key: drop rather than mis-inject
                },
                // Absolute positioning isn't used in the relative model.
                InputEvent::PointerPosition { .. } => Ok(()),
            }
        }
        async fn release_all(&mut self) -> Result<(), InputError> {
            // Release the mouse buttons we might be holding; held keyboard keys
            // can't be enumerated, so the receiver's key-repeat settles them.
            for (b, _) in [
                (Button::Left, ()),
                (Button::Right, ()),
                (Button::Middle, ()),
            ] {
                let (flags, data) = button_flags(b, false);
                Self::send_mouse(0, 0, data, flags)?;
            }
            Ok(())
        }
    }

    fn button_flags(button: Button, pressed: bool) -> (MOUSE_EVENT_FLAGS, u32) {
        match button {
            Button::Left => (if pressed { MOUSEEVENTF_LEFTDOWN } else { MOUSEEVENTF_LEFTUP }, 0),
            Button::Right => (if pressed { MOUSEEVENTF_RIGHTDOWN } else { MOUSEEVENTF_RIGHTUP }, 0),
            Button::Middle => (if pressed { MOUSEEVENTF_MIDDLEDOWN } else { MOUSEEVENTF_MIDDLEUP }, 0),
            Button::Back => (if pressed { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON1),
            Button::Forward => (if pressed { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON2),
            Button::Other(_) => (if pressed { MOUSEEVENTF_LEFTDOWN } else { MOUSEEVENTF_LEFTUP }, 0),
        }
    }
}
