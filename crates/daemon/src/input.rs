//! Input sharing pump — the cross-machine cursor/keyboard forwarding.
//!
//! Model (as in Deskflow/Input Leap/Barrier): the machine holding the physical
//! keyboard/mouse is the *controller*. While the cursor is on one of its own
//! monitors, input flows to local apps untouched and the daemon only tracks the
//! cursor to detect edge crossings. When the cursor crosses onto a monitor owned
//! by the peer, the controller **grabs** (suppresses local delivery), tells the
//! peer to take the cursor (`Enter`), and forwards every subsequent event; the
//! peer **injects** them. When the cursor crosses back, it sends `Leave` and
//! resumes local delivery.
//!
//! [`Controller`] is the pure decision logic (fully unit-tested). [`run_input`]
//! wires it to a [`Capture`]/[`Injector`] pair and the Input channel, so the same
//! pump works over the in-memory loopback (tests) and real QUIC.

use bytes::BytesMut;
use deskoryn_core::config::InputConfig;
use deskoryn_core::geometry::{Point, Rect};
use deskoryn_core::input::{InputEvent, KeyCode, Modifiers};
use deskoryn_core::{DeviceId, Monitor, VirtualDesktop};
use deskoryn_input::hotkey::Hotkey;
use deskoryn_input::{Capture, Injector};
use deskoryn_net::transport::{Session, Sink};
use deskoryn_proto::{decode_one, encode, Channel, Input};

/// A decision emitted by the controller for one captured event.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Deliver to local apps (do nothing — the OS already has it).
    PassThrough,
    /// Hand the cursor to the peer, entering at `at`.
    Enter { at: Point, mods: Modifiers },
    /// Take the cursor back from the peer.
    Leave,
    /// Forward this event to the peer for injection.
    Forward(InputEvent),
}

/// Tracks the global cursor and decides, per captured event, whether it stays
/// local or crosses to (and is forwarded to) the peer.
pub struct Controller {
    vd: VirtualDesktop,
    /// This machine's monitors in *local OS* pixel coordinates (from detection),
    /// used to warp the real cursor to the exact entry point. Keyed to `vd`
    /// monitors by `MonitorId::index`.
    local: Vec<Monitor>,
    me: DeviceId,
    pos: Point,
    /// True when the cursor is on a peer monitor and we are suppressing+forwarding.
    grabbed: bool,
    mods: Modifiers,
    /// When set, machine transitions are disabled (the `lock` hotkey). The cursor
    /// stays on whichever machine it was on; edge crossings are ignored.
    locked: bool,
    /// Pixels of outward "push" past a monitor edge required before the cursor
    /// hands off to the peer (0 disables). Prevents accidental crossings.
    edge_px: i32,
    /// Outward overshoot accumulated against the current edge while resisting.
    edge_accum: i32,
    /// Hotkey that forces the cursor to the other machine regardless of position.
    switch: Option<Hotkey>,
    /// Hotkey that toggles [`locked`].
    lock_key: Option<Hotkey>,
    /// Rising-edge guards so a held hotkey toggles once, not every key-repeat.
    switch_held: bool,
    lock_held: bool,
}

impl Controller {
    pub fn new(vd: VirtualDesktop, me: DeviceId, start: Point) -> Self {
        Self {
            vd,
            local: Vec::new(),
            me,
            pos: start,
            grabbed: false,
            mods: Modifiers::empty(),
            locked: false,
            edge_px: 0,
            edge_accum: 0,
            switch: None,
            lock_key: None,
            switch_held: false,
            lock_held: false,
        }
    }

    /// Apply the user's input policy: edge resistance and the switch/lock
    /// hotkeys. Unparseable hotkey specs are dropped (logged at the call site).
    pub fn with_input_config(mut self, cfg: &InputConfig) -> Self {
        self.edge_px = cfg.edge_resistance_px.max(0);
        self.switch = Hotkey::parse(&cfg.switch_hotkey).ok();
        self.lock_key = Hotkey::parse(&cfg.lock_hotkey).ok();
        self
    }

    /// Provide this machine's monitors in local OS coordinates, so an incoming
    /// `Enter` can warp the real cursor to the exact crossing point.
    pub fn with_local_monitors(mut self, local: Vec<Monitor>) -> Self {
        self.local = local;
        self
    }

    /// Map a virtual-desktop entry point to a normalized absolute position
    /// (`0..=65535` on each axis) over this machine's local virtual screen, so
    /// the receiver can warp its cursor to exactly where the pointer crossed in.
    ///
    /// `None` when the point isn't on one of our monitors or local geometry is
    /// unknown — the caller then falls back to relative motion only.
    pub fn entry_norm(&self, entry: Point) -> Option<Point> {
        // The virtual monitor (ours) the entry lands on, and the same physical
        // monitor in local coordinates (matched by index).
        let vm = self
            .vd
            .monitors
            .iter()
            .find(|m| m.device() == self.me && m.bounds.contains(entry))?;
        let lm = self.local.iter().find(|m| m.id.index == vm.id.index)?;

        // Position within the monitor (scaled if the arranged size differs).
        let frac = |e: i32, vo: i32, vs: i32, lo: i32, ls: i32| -> i32 {
            lo + ((e - vo) as i64 * ls as i64 / vs.max(1) as i64) as i32
        };
        let lx = frac(entry.x, vm.bounds.x, vm.bounds.w, lm.bounds.x, lm.bounds.w);
        let ly = frac(entry.y, vm.bounds.y, vm.bounds.h, lm.bounds.y, lm.bounds.h);

        // Normalize over our local virtual-screen bounding box.
        let bbox = local_bbox(&self.local)?;
        Some(Point::new(norm(lx - bbox.x, bbox.w), norm(ly - bbox.y, bbox.h)))
    }

    /// Whether the cursor is currently on a peer monitor (suppressing+forwarding).
    #[allow(dead_code)]
    pub fn grabbed(&self) -> bool {
        self.grabbed
    }
    /// Whether transitions are currently locked off.
    #[allow(dead_code)]
    pub fn locked(&self) -> bool {
        self.locked
    }
    /// Current global cursor position (for status/UI).
    #[allow(dead_code)]
    pub fn position(&self) -> Point {
        self.pos
    }

    /// Feed one locally-captured event; returns the action(s) to take.
    pub fn on_event(&mut self, event: InputEvent) -> Vec<Action> {
        match event {
            InputEvent::PointerMotion { dx, dy } => self.on_motion(dx, dy),
            InputEvent::PointerPosition { at } => {
                let dx = at.x - self.pos.x;
                let dy = at.y - self.pos.y;
                self.on_motion(dx, dy)
            }
            InputEvent::Key { code, pressed, mods } => {
                self.mods = mods;
                if let Some(action) = self.handle_hotkey(code, pressed, mods) {
                    return action;
                }
                if self.grabbed {
                    vec![Action::Forward(event)]
                } else {
                    vec![Action::PassThrough]
                }
            }
            // Buttons/scroll: forward iff the cursor is currently remote.
            _ => {
                if self.grabbed {
                    vec![Action::Forward(event)]
                } else {
                    vec![Action::PassThrough]
                }
            }
        }
    }

    /// Intercept the lock/switch hotkeys on their rising edge. Returns `Some`
    /// (consuming the key locally) when one fired, `None` to fall through to the
    /// normal key path.
    fn handle_hotkey(&mut self, code: KeyCode, pressed: bool, mods: Modifiers) -> Option<Vec<Action>> {
        if !pressed {
            // Trigger released: re-arm both guards.
            self.lock_held = false;
            self.switch_held = false;
            return None;
        }
        if let Some(hk) = self.lock_key {
            if hk.matches(code, mods) {
                if !self.lock_held {
                    self.locked = !self.locked;
                    self.lock_held = true;
                }
                return Some(vec![Action::PassThrough]);
            }
        }
        if let Some(hk) = self.switch {
            if hk.matches(code, mods) {
                if !self.switch_held {
                    self.switch_held = true;
                    return Some(self.force_switch());
                }
                return Some(vec![Action::PassThrough]);
            }
        }
        None
    }

    /// Force the cursor across the boundary (the `switch` hotkey), ignoring the
    /// lock and edge resistance: if local, jump to a peer monitor; if remote,
    /// return to one of our own.
    fn force_switch(&mut self) -> Vec<Action> {
        self.edge_accum = 0;
        if self.grabbed {
            self.grabbed = false;
            if let Some(c) = self.monitor_center(false) {
                self.pos = c;
            }
            vec![Action::Leave]
        } else if let Some(c) = self.monitor_center(true) {
            self.grabbed = true;
            self.pos = c;
            vec![Action::Enter { at: c, mods: self.mods }]
        } else {
            // No peer monitor to switch to.
            vec![Action::PassThrough]
        }
    }

    /// Swap in a re-arranged virtual desktop (e.g. the user dragged a monitor in
    /// the arranger and hit Apply). The cursor keeps its position when it still
    /// lands on a monitor; otherwise we drop any active grab and re-center on one
    /// of our own monitors so it can never get stranded off the new desktop.
    pub fn set_layout(&mut self, vd: VirtualDesktop) {
        let still_valid = vd.monitor_at(self.pos).is_some();
        self.vd = vd;
        if !still_valid {
            self.grabbed = false;
            self.edge_accum = 0;
            if let Some(c) = self.monitor_center(false) {
                self.pos = c;
            }
        }
    }

    /// Center of the first monitor owned by the peer (`peer = true`) or by us.
    fn monitor_center(&self, peer: bool) -> Option<Point> {
        self.vd
            .monitors
            .iter()
            .find(|m| (m.device() != self.me) == peer)
            .map(|m| Point::new(m.bounds.x + m.bounds.w / 2, m.bounds.y + m.bounds.h / 2))
    }

    fn on_motion(&mut self, dx: i32, dy: i32) -> Vec<Action> {
        let want = Point::new(self.pos.x + dx, self.pos.y + dy);
        // Stay inside the virtual desktop: if the target is off all monitors,
        // clamp to the current monitor (sticky outer edge).
        let new = if self.vd.monitor_at(want).is_some() {
            want
        } else {
            self.vd
                .monitor_at(self.pos)
                .map(|m| m.bounds.clamp(want))
                .unwrap_or(want)
        };
        let on_peer = self.vd.owner_at(new).is_some_and(|o| o != self.me);

        // Locked: never change machine. Keep the cursor on its current side,
        // clamping at the edge if a move would cross.
        if self.locked && on_peer != self.grabbed {
            if let Some(cur) = self.vd.monitor_at(self.pos) {
                self.pos = cur.bounds.clamp(want);
            }
            return if self.grabbed {
                vec![Action::Forward(InputEvent::PointerMotion { dx, dy })]
            } else {
                vec![Action::PassThrough]
            };
        }

        match (self.grabbed, on_peer) {
            // Crossing out to the peer — subject to edge resistance.
            (false, true) => {
                if self.edge_px > 0 {
                    let overshoot = self
                        .vd
                        .monitor_at(self.pos)
                        .map(|m| outside_distance(&m.bounds, want))
                        .unwrap_or(0);
                    self.edge_accum += overshoot;
                    if self.edge_accum < self.edge_px {
                        // Hold at the edge until enough outward push accumulates.
                        if let Some(cur) = self.vd.monitor_at(self.pos) {
                            self.pos = cur.bounds.clamp(want);
                        }
                        return vec![Action::PassThrough];
                    }
                    self.edge_accum = 0;
                }
                self.pos = new;
                self.grabbed = true;
                vec![Action::Enter { at: new, mods: self.mods }]
            }
            // Crossing back to us.
            (true, false) => {
                self.pos = new;
                self.grabbed = false;
                self.edge_accum = 0;
                vec![Action::Leave]
            }
            // Still remote: keep driving the peer cursor.
            (true, true) => {
                self.pos = new;
                vec![Action::Forward(InputEvent::PointerMotion { dx, dy })]
            }
            // Still local: pass through.
            (false, false) => {
                self.pos = new;
                self.edge_accum = 0;
                vec![Action::PassThrough]
            }
        }
    }
}

/// Normalize `v` (a 0-based offset) into `0..=65535` over `span` pixels, the
/// absolute-pointer convention used by both OS injectors.
fn norm(v: i32, span: i32) -> i32 {
    if span <= 1 {
        0
    } else {
        ((v as i64 * 65535 / (span as i64 - 1)).clamp(0, 65535)) as i32
    }
}

/// Bounding box of a set of monitors (in their own coordinate space).
fn local_bbox(mons: &[Monitor]) -> Option<Rect> {
    let mut it = mons.iter();
    let first = it.next()?.bounds;
    let (mut l, mut t, mut r, mut b) = (first.left(), first.top(), first.right(), first.bottom());
    for m in it {
        l = l.min(m.bounds.left());
        t = t.min(m.bounds.top());
        r = r.max(m.bounds.right());
        b = b.max(m.bounds.bottom());
    }
    Some(Rect::new(l, t, r - l, b - t))
}

/// How far `p` lies outside `r`, as the larger of the two axis overshoots (0 if
/// inside). Uses the half-open `[left,right)` convention of [`Rect::contains`].
fn outside_distance(r: &Rect, p: Point) -> i32 {
    let dx = (r.left() - p.x).max(p.x - (r.right() - 1)).max(0);
    let dy = (r.top() - p.y).max(p.y - (r.bottom() - 1)).max(0);
    dx.max(dy)
}

/// Run the input pump for a session: capture locally and forward across the
/// boundary, and inject events the peer forwards to us.
pub async fn run_input(
    session: &dyn Session,
    mut controller: Controller,
    mut capture: Box<dyn Capture>,
    mut injector: Box<dyn Injector>,
    mut layout_rx: tokio::sync::mpsc::Receiver<VirtualDesktop>,
) -> anyhow::Result<()> {
    let (mut sink, mut source) = session.channel(Channel::Input).await?;
    let mut seq: u32 = 0;
    // Once all senders drop (the arranger handler is gone), park this arm so the
    // closed channel doesn't spin the select loop on an immediate `None`.
    let mut layout_open = true;

    loop {
        tokio::select! {
            // Live re-arrange: the arranger pushed a new layout via SetLayout.
            maybe = async { layout_rx.recv().await }, if layout_open => {
                match maybe {
                    Some(vd) => {
                        tracing::info!(monitors = vd.monitors.len(), "applying re-arranged layout");
                        controller.set_layout(vd);
                    }
                    None => layout_open = false,
                }
            }
            ev = capture.next_event() => {
                let ev = ev?;
                for action in controller.on_event(ev) {
                    match action {
                        Action::PassThrough => {}
                        Action::Enter { at, mods } => {
                            tracing::debug!(?at, "cursor crossed onto peer; grabbing local input");
                            send(&mut sink, &Input::Enter { entry: at, mods }).await?;
                            capture.set_grabbed(true).await?;
                        }
                        Action::Leave => {
                            tracing::debug!("cursor returned to local; releasing input");
                            send(&mut sink, &Input::Leave).await?;
                            capture.set_grabbed(false).await?;
                        }
                        Action::Forward(e) => {
                            seq = seq.wrapping_add(1);
                            send(&mut sink, &Input::Events { seq, events: vec![e] }).await?;
                        }
                    }
                }
                // Position trace (enable with `RUST_LOG=deskoryn=trace`) — shows the
                // tracked cursor climbing toward the boundary so a missed handoff
                // (e.g. startup desync) is visible.
                tracing::trace!(pos = ?controller.position(), grabbed = controller.grabbed(), "cursor state");
            }
            frame = source.recv_bytes() => {
                let Some(frame) = frame? else { return Ok(()); };
                let mut b = BytesMut::from(&frame[..]);
                let Some(msg) = decode_one::<Input>(&mut b)? else { continue; };
                match msg {
                    Input::Enter { entry, .. } => {
                        // Warp the real cursor to exactly where the pointer crossed
                        // in (normalized absolute position); if we lack local
                        // geometry, fall back to the forwarded relative motion.
                        if let Some(at) = controller.entry_norm(entry) {
                            injector.warp_to(at).await?;
                        }
                    }
                    Input::Leave => injector.release_all().await?,
                    Input::Events { seq, events } => {
                        for e in events {
                            injector.inject(e).await?;
                        }
                        send(&mut sink, &Input::Ack { seq }).await?;
                    }
                    Input::Ack { .. } => {}
                }
            }
        }
    }
}

async fn send(sink: &mut Box<dyn Sink>, msg: &Input) -> anyhow::Result<()> {
    let mut buf = BytesMut::new();
    encode(msg, &mut buf)?;
    sink.send_bytes(&buf).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use deskoryn_core::geometry::{Rect, Size};
    use deskoryn_core::input::{Button, KeyCode};
    use deskoryn_core::layout::Monitor;
    use deskoryn_core::MonitorId;

    fn dev(b: u8) -> DeviceId {
        DeviceId::from_bytes([b; 16])
    }

    #[test]
    fn entry_norm_maps_crossing_point_to_local_absolute() {
        let me = dev(1);
        let peer = dev(2);
        let m = |d, i, x, y, w, h| Monitor {
            id: MonitorId::new(d, i),
            label: String::new(),
            bounds: Rect::new(x, y, w, h),
            native: Size::new(w, h),
            scale_pct: 100,
        };
        // Our monitor is arranged at virtual x=5760 (right of the peer) but sits
        // at local x=0; a single 1920x1080 display => local screen 1920x1080.
        let vd = VirtualDesktop::new(vec![
            m(peer, 0, 0, 0, 5760, 1080),
            m(me, 0, 5760, 0, 1920, 1080),
        ]);
        let c = Controller::new(vd, me, Point::new(0, 0))
            .with_local_monitors(vec![m(me, 0, 0, 0, 1920, 1080)]);

        // Crossing in at the monitor's corners maps to the absolute extremes.
        assert_eq!(c.entry_norm(Point::new(5760, 0)), Some(Point::new(0, 0)));
        assert_eq!(c.entry_norm(Point::new(5760 + 1919, 0)), Some(Point::new(65535, 0)));
        assert_eq!(c.entry_norm(Point::new(5760, 1079)), Some(Point::new(0, 65535)));
        // A point on the peer's side isn't ours to warp to.
        assert_eq!(c.entry_norm(Point::new(100, 100)), None);
    }

    fn two_machines() -> (DeviceId, DeviceId, VirtualDesktop) {
        let a = dev(1);
        let b = dev(2);
        let mon = |d, i, x: i32| Monitor {
            id: MonitorId::new(d, i),
            label: format!("m{i}"),
            bounds: Rect::new(x, 0, 1920, 1080),
            native: Size::new(1920, 1080),
            scale_pct: 100,
        };
        // A on the left (0..1920), B on the right (1920..3840).
        (a, b, VirtualDesktop::new(vec![mon(a, 0, 0), mon(b, 0, 1920)]))
    }

    fn policy(edge_px: i32) -> InputConfig {
        InputConfig { edge_resistance_px: edge_px, ..InputConfig::default() }
    }

    #[test]
    fn edge_resistance_holds_until_pushed() {
        let (a, _b, vd) = two_machines();
        // 100px of push required before handing off; start just inside A's right edge.
        let mut c = Controller::new(vd, a, Point::new(1900, 500)).with_input_config(&policy(100));

        // First nudge across the edge: held locally, no Enter yet.
        let acts = c.on_event(InputEvent::PointerMotion { dx: 50, dy: 0 });
        assert_eq!(acts, vec![Action::PassThrough]);
        assert!(!c.grabbed());
        // Cursor is clamped to A's edge while resisting.
        assert_eq!(c.position().x, 1919);

        // Keep pushing; still under the 100px threshold.
        assert_eq!(c.on_event(InputEvent::PointerMotion { dx: 50, dy: 0 }), vec![Action::PassThrough]);
        assert!(!c.grabbed());

        // Enough accumulated push now crosses.
        let acts = c.on_event(InputEvent::PointerMotion { dx: 50, dy: 0 });
        assert!(matches!(acts.as_slice(), [Action::Enter { .. }]));
        assert!(c.grabbed());
    }

    #[test]
    fn moving_back_inward_resets_resistance() {
        let (a, _b, vd) = two_machines();
        let mut c = Controller::new(vd, a, Point::new(1900, 500)).with_input_config(&policy(100));

        c.on_event(InputEvent::PointerMotion { dx: 50, dy: 0 }); // accumulate 31
        // Move well back inside A: resets the accumulator.
        assert_eq!(c.on_event(InputEvent::PointerMotion { dx: -500, dy: 0 }), vec![Action::PassThrough]);
        // A fresh push under threshold must not immediately cross.
        assert_eq!(c.on_event(InputEvent::PointerMotion { dx: 50, dy: 0 }), vec![Action::PassThrough]);
        assert!(!c.grabbed());
    }

    #[test]
    fn lock_blocks_transitions() {
        let (a, _b, vd) = two_machines();
        let cfg = policy(0);
        let mut c = Controller::new(vd, a, Point::new(100, 500)).with_input_config(&cfg);

        // Lock via the configured hotkey (Ctrl+Alt+L -> 'l').
        let lock_key = Hotkey::parse(&cfg.lock_hotkey).unwrap();
        let mods = Modifiers::CTRL | Modifiers::ALT;
        c.on_event(InputEvent::Key { code: lock_key.code, pressed: true, mods });
        assert!(c.locked());

        // A move that would cross is held local while locked.
        let acts = c.on_event(InputEvent::PointerMotion { dx: 1800, dy: 0 });
        assert_eq!(acts, vec![Action::PassThrough]);
        assert!(!c.grabbed());

        // Unlock (release then press again) and the same move crosses.
        c.on_event(InputEvent::Key { code: lock_key.code, pressed: false, mods });
        c.on_event(InputEvent::Key { code: lock_key.code, pressed: true, mods });
        assert!(!c.locked());
        let acts = c.on_event(InputEvent::PointerMotion { dx: 1800, dy: 0 });
        assert!(matches!(acts.as_slice(), [Action::Enter { .. }]));
    }

    #[test]
    fn switch_hotkey_forces_handoff_both_ways() {
        let (a, _b, vd) = two_machines();
        let cfg = policy(0);
        let mut c = Controller::new(vd, a, Point::new(100, 500)).with_input_config(&cfg);

        let sw = Hotkey::parse(&cfg.switch_hotkey).unwrap();
        let mods = Modifiers::CTRL | Modifiers::ALT;

        // Local -> forced Enter onto a peer monitor.
        let acts = c.on_event(InputEvent::Key { code: sw.code, pressed: true, mods });
        assert!(matches!(acts.as_slice(), [Action::Enter { .. }]));
        assert!(c.grabbed());

        // Held (key-repeat) does not toggle again.
        assert_eq!(c.on_event(InputEvent::Key { code: sw.code, pressed: true, mods }), vec![Action::PassThrough]);
        assert!(c.grabbed());

        // Release re-arms; pressing again switches back.
        c.on_event(InputEvent::Key { code: sw.code, pressed: false, mods });
        let acts = c.on_event(InputEvent::Key { code: sw.code, pressed: true, mods });
        assert_eq!(acts, vec![Action::Leave]);
        assert!(!c.grabbed());
    }

    #[test]
    fn crosses_out_forwards_then_returns() {
        let (a, _b, vd) = two_machines();
        let mut c = Controller::new(vd, a, Point::new(100, 500));

        // Move within A: pass-through.
        assert_eq!(c.on_event(InputEvent::PointerMotion { dx: 50, dy: 0 }), vec![Action::PassThrough]);
        assert!(!c.grabbed());

        // Cross into B: Enter.
        let acts = c.on_event(InputEvent::PointerMotion { dx: 1800, dy: 0 });
        assert!(matches!(acts.as_slice(), [Action::Enter { .. }]));
        assert!(c.grabbed());

        // A key while remote: forwarded.
        let acts = c.on_event(InputEvent::Key { code: KeyCode(30), pressed: true, mods: Modifiers::empty() });
        assert!(matches!(acts.as_slice(), [Action::Forward(InputEvent::Key { .. })]));

        // A click while remote: forwarded.
        let acts = c.on_event(InputEvent::Button { button: Button::Left, pressed: true });
        assert!(matches!(acts.as_slice(), [Action::Forward(InputEvent::Button { .. })]));

        // Move back into A: Leave.
        let acts = c.on_event(InputEvent::PointerMotion { dx: -1900, dy: 0 });
        assert_eq!(acts, vec![Action::Leave]);
        assert!(!c.grabbed());
    }

    // --- Integration: full pump over a loopback session ---------------------

    use async_trait::async_trait;
    use deskoryn_input::InputError;
    use deskoryn_net::transport::{loopback, Session};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Emits a fixed script of events, then parks.
    struct ScriptedCapture {
        events: std::vec::IntoIter<InputEvent>,
    }
    #[async_trait]
    impl Capture for ScriptedCapture {
        async fn set_grabbed(&mut self, _g: bool) -> Result<(), InputError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Result<InputEvent, InputError> {
            match self.events.next() {
                Some(e) => Ok(e),
                None => std::future::pending().await,
            }
        }
        fn modifiers(&self) -> Modifiers {
            Modifiers::empty()
        }
    }

    /// Never emits — stands in for an idle machine's hardware.
    struct IdleCapture;
    #[async_trait]
    impl Capture for IdleCapture {
        async fn set_grabbed(&mut self, _g: bool) -> Result<(), InputError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Result<InputEvent, InputError> {
            std::future::pending().await
        }
        fn modifiers(&self) -> Modifiers {
            Modifiers::empty()
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    enum Rec {
        Warp(Point),
        Inject(InputEvent),
        Release,
    }

    struct RecordingInjector {
        log: Arc<Mutex<Vec<Rec>>>,
    }
    #[async_trait]
    impl Injector for RecordingInjector {
        async fn warp_to(&mut self, at: Point) -> Result<(), InputError> {
            self.log.lock().unwrap().push(Rec::Warp(at));
            Ok(())
        }
        async fn inject(&mut self, event: InputEvent) -> Result<(), InputError> {
            self.log.lock().unwrap().push(Rec::Inject(event));
            Ok(())
        }
        async fn release_all(&mut self) -> Result<(), InputError> {
            self.log.lock().unwrap().push(Rec::Release);
            Ok(())
        }
    }

    struct NullInjector;
    #[async_trait]
    impl Injector for NullInjector {
        async fn warp_to(&mut self, _at: Point) -> Result<(), InputError> {
            Ok(())
        }
        async fn inject(&mut self, _e: InputEvent) -> Result<(), InputError> {
            Ok(())
        }
        async fn release_all(&mut self) -> Result<(), InputError> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwards_input_across_the_boundary() {
        let (a, b, vd) = two_machines();
        let (sa, sb) = loopback::loopback(a, b);
        let sa: Box<dyn Session> = Box::new(sa);
        let sb: Box<dyn Session> = Box::new(sb);

        // A is the controller; cursor starts on A, crosses to B, types, returns.
        let key_down = InputEvent::Key { code: KeyCode(30), pressed: true, mods: Modifiers::empty() };
        let key_up = InputEvent::Key { code: KeyCode(30), pressed: false, mods: Modifiers::empty() };
        let script = vec![
            InputEvent::PointerMotion { dx: 1850, dy: 0 }, // cross into B -> Enter
            key_down,
            key_up,
            InputEvent::PointerMotion { dx: -1850, dy: 0 }, // back to A -> Leave
        ];
        let ctrl_a = Controller::new(vd.clone(), a, Point::new(100, 500));
        let cap_a = Box::new(ScriptedCapture { events: script.into_iter() });

        let log = Arc::new(Mutex::new(Vec::new()));
        // B's monitor in local coordinates (so an Enter warps to the entry point).
        let b_local = vec![Monitor {
            id: MonitorId::new(b, 0),
            label: "m0".into(),
            bounds: Rect::new(0, 0, 1920, 1080),
            native: Size::new(1920, 1080),
            scale_pct: 100,
        }];
        let ctrl_b = Controller::new(vd, b, Point::new(2000, 500)).with_local_monitors(b_local);
        let inj_b = Box::new(RecordingInjector { log: log.clone() });

        // No live re-arrange in this test; hold the senders so the arms park.
        let (_tx_a, rx_a) = tokio::sync::mpsc::channel(1);
        let (_tx_b, rx_b) = tokio::sync::mpsc::channel(1);
        let pa = tokio::spawn(async move {
            run_input(sa.as_ref(), ctrl_a, cap_a, Box::new(NullInjector), rx_a).await
        });
        let pb = tokio::spawn(async move {
            run_input(sb.as_ref(), ctrl_b, Box::new(IdleCapture), inj_b, rx_b).await
        });

        // Wait until B has recorded the warp, both key events, and the release.
        let mut ok = false;
        for _ in 0..100 {
            if log.lock().unwrap().len() >= 4 {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ok, "B did not receive the forwarded input: {:?}", log.lock().unwrap());

        let recorded = log.lock().unwrap().clone();
        assert!(matches!(recorded[0], Rec::Warp(_)), "first action is the Enter warp");
        assert_eq!(recorded[1], Rec::Inject(key_down));
        assert_eq!(recorded[2], Rec::Inject(key_up));
        assert_eq!(recorded[3], Rec::Release, "Leave releases held keys");

        pa.abort();
        pb.abort();
    }
}
