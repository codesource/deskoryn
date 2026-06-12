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
use deskoryn_core::geometry::Point;
use deskoryn_core::input::{InputEvent, Modifiers};
use deskoryn_core::{DeviceId, VirtualDesktop};
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
    me: DeviceId,
    pos: Point,
    /// True when the cursor is on a peer monitor and we are suppressing+forwarding.
    grabbed: bool,
    mods: Modifiers,
}

impl Controller {
    pub fn new(vd: VirtualDesktop, me: DeviceId, start: Point) -> Self {
        Self { vd, me, pos: start, grabbed: false, mods: Modifiers::empty() }
    }

    /// Whether the cursor is currently on a peer monitor (suppressing+forwarding).
    #[allow(dead_code)]
    pub fn grabbed(&self) -> bool {
        self.grabbed
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
                let _ = (code, pressed);
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
        let owner = self.vd.owner_at(new);
        self.pos = new;

        let on_peer = owner.is_some_and(|o| o != self.me);
        match (self.grabbed, on_peer) {
            // Crossing out to the peer.
            (false, true) => {
                self.grabbed = true;
                vec![Action::Enter { at: new, mods: self.mods }]
            }
            // Crossing back to us.
            (true, false) => {
                self.grabbed = false;
                vec![Action::Leave]
            }
            // Still remote: keep driving the peer cursor.
            (true, true) => vec![Action::Forward(InputEvent::PointerMotion { dx, dy })],
            // Still local: pass through.
            (false, false) => vec![Action::PassThrough],
        }
    }
}

/// Run the input pump for a session: capture locally and forward across the
/// boundary, and inject events the peer forwards to us.
pub async fn run_input(
    session: &dyn Session,
    mut controller: Controller,
    mut capture: Box<dyn Capture>,
    mut injector: Box<dyn Injector>,
) -> anyhow::Result<()> {
    let (mut sink, mut source) = session.channel(Channel::Input).await?;
    let mut seq: u32 = 0;

    loop {
        tokio::select! {
            ev = capture.next_event() => {
                let ev = ev?;
                for action in controller.on_event(ev) {
                    match action {
                        Action::PassThrough => {}
                        Action::Enter { at, mods } => {
                            send(&mut sink, &Input::Enter { entry: at, mods }).await?;
                            capture.set_grabbed(true).await?;
                        }
                        Action::Leave => {
                            send(&mut sink, &Input::Leave).await?;
                            capture.set_grabbed(false).await?;
                        }
                        Action::Forward(e) => {
                            seq = seq.wrapping_add(1);
                            send(&mut sink, &Input::Events { seq, events: vec![e] }).await?;
                        }
                    }
                }
            }
            frame = source.recv_bytes() => {
                let Some(frame) = frame? else { return Ok(()); };
                let mut b = BytesMut::from(&frame[..]);
                let Some(msg) = decode_one::<Input>(&mut b)? else { continue; };
                match msg {
                    Input::Enter { entry, .. } => injector.warp_to(entry).await?,
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
        let ctrl_b = Controller::new(vd, b, Point::new(2000, 500));
        let inj_b = Box::new(RecordingInjector { log: log.clone() });

        let pa = tokio::spawn(async move {
            run_input(sa.as_ref(), ctrl_a, cap_a, Box::new(NullInjector)).await
        });
        let pb = tokio::spawn(async move {
            run_input(sb.as_ref(), ctrl_b, Box::new(IdleCapture), inj_b).await
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
