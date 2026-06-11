//! The cursor focus state machine — the brain of the "one desktop" illusion.
//!
//! At any moment exactly one machine is **active** (owns the real cursor and
//! captures input). This machine tracks the global cursor position. On each
//! pointer motion it consults the [`VirtualDesktop`] to see whether the cursor
//! has crossed onto a monitor owned by the *other* machine; if so it produces a
//! [`FocusAction::HandOff`], stops injecting locally, and tells the peer to take
//! over. The peer, on receiving an `Enter`, becomes active.
//!
//! Hotkeys can force a switch or lock the cursor in place. Edge resistance adds
//! hysteresis so you don't fly onto the other machine by overshooting.
//!
//! This module is pure logic (no I/O), so it is thoroughly unit-testable; the
//! daemon feeds it events and executes the [`FocusAction`]s it returns.

// Several accessors/transitions are part of the intended API but not yet wired
// into the skeleton's single demo loop.
#![allow(dead_code)]

use deskoryn_core::geometry::Point;
use deskoryn_core::input::Modifiers;
use deskoryn_core::{DeviceId, VirtualDesktop};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// This machine currently owns the cursor.
    Active,
    /// The other machine owns the cursor; we are injecting nothing and waiting
    /// for an `Enter`.
    Idle,
}

/// What the daemon should do in response to a fed event.
#[derive(Clone, Debug, PartialEq)]
pub enum FocusAction {
    /// Nothing changes; keep going.
    None,
    /// Move the local OS cursor to this virtual point (we stay active).
    MoveLocal(Point),
    /// Hand control to `to`, entering at `entry`. We become idle and must send
    /// `Input::Enter { entry }` to the peer and stop grabbing input.
    HandOff { to: DeviceId, entry: Point, mods: Modifiers },
    /// We just received control: become active and warp the local cursor here.
    Take { entry: Point },
}

pub struct FocusMachine {
    me: DeviceId,
    layout: VirtualDesktop,
    role: Role,
    cursor: Point,
    /// When true, edge transitions are disabled (lock hotkey).
    locked: bool,
    edge_resistance_px: i32,
    /// Accumulated overshoot against the desktop edge, for resistance/hysteresis.
    pending_push: i32,
}

impl FocusMachine {
    pub fn new(me: DeviceId, layout: VirtualDesktop, start_active: bool, edge_resistance_px: i32) -> Self {
        Self {
            me,
            layout,
            role: if start_active { Role::Active } else { Role::Idle },
            cursor: Point::new(0, 0),
            locked: false,
            edge_resistance_px,
            pending_push: 0,
        }
    }

    pub fn role(&self) -> Role {
        self.role
    }
    pub fn cursor(&self) -> Point {
        self.cursor
    }
    pub fn set_layout(&mut self, layout: VirtualDesktop) {
        self.layout = layout;
    }
    pub fn toggle_lock(&mut self) {
        self.locked = !self.locked;
    }

    /// Feed a relative pointer motion captured locally (only meaningful while
    /// active). Returns the action to perform.
    pub fn on_motion(&mut self, dx: i32, dy: i32, mods: Modifiers) -> FocusAction {
        if self.role != Role::Active {
            return FocusAction::None;
        }
        let from = self.cursor;
        let want = Point::new(from.x + dx, from.y + dy);

        match self.layout.resolve_move(from, want) {
            // Crossing onto a monitor owned by someone else → hand off (unless
            // locked or held back by edge resistance).
            Some(t) if t.device != self.me => {
                if self.locked {
                    return self.stay_clamped(from, want);
                }
                if self.edge_resistance_px > 0 && self.pending_push < self.edge_resistance_px {
                    self.pending_push += dx.abs().max(dy.abs());
                    return self.stay_clamped(from, want);
                }
                self.pending_push = 0;
                self.role = Role::Idle;
                self.cursor = t.entry;
                FocusAction::HandOff {
                    to: t.device,
                    entry: t.entry,
                    mods,
                }
            }
            // Crossing within our own machine (monitor→monitor) — just move.
            Some(t) => {
                self.pending_push = 0;
                self.cursor = t.entry;
                FocusAction::MoveLocal(t.entry)
            }
            // Stayed on the same monitor, or would leave the desktop entirely.
            None => self.stay_clamped(from, want),
        }
    }

    fn stay_clamped(&mut self, from: Point, want: Point) -> FocusAction {
        // Clamp to the current monitor so the cursor sticks at the outer edge.
        let clamped = self
            .layout
            .monitor_at(from)
            .map(|m| m.bounds.clamp(want))
            .unwrap_or(want);
        self.cursor = clamped;
        FocusAction::MoveLocal(clamped)
    }

    /// The peer handed control to us at `entry`.
    pub fn on_enter(&mut self, entry: Point) -> FocusAction {
        self.role = Role::Active;
        self.cursor = entry;
        self.pending_push = 0;
        FocusAction::Take { entry }
    }

    /// The peer took control from us (we acknowledged a hand-off, or the peer
    /// forced a switch). We go idle.
    pub fn on_leave(&mut self) {
        self.role = Role::Idle;
    }

    /// Force the cursor onto the other machine's first monitor (switch hotkey).
    pub fn force_switch(&mut self, to: DeviceId, entry: Point, mods: Modifiers) -> FocusAction {
        if self.role == Role::Active && to != self.me {
            self.role = Role::Idle;
            self.cursor = entry;
            FocusAction::HandOff { to, entry, mods }
        } else {
            FocusAction::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deskoryn_core::geometry::{Rect, Size};
    use deskoryn_core::layout::Monitor;
    use deskoryn_core::MonitorId;

    fn dev(b: u8) -> DeviceId {
        DeviceId::from_bytes([b; 16])
    }

    fn two_machine_layout() -> (DeviceId, DeviceId, VirtualDesktop) {
        let lin = dev(1);
        let win = dev(2);
        let m = |d, i, x: i32| Monitor {
            id: MonitorId::new(d, i),
            label: format!("m{i}"),
            bounds: Rect::new(x, 0, 1920, 1080),
            native: Size::new(1920, 1080),
            scale_pct: 100,
        };
        let vd = VirtualDesktop::new(vec![m(lin, 0, 0), m(lin, 1, 1920), m(win, 0, 3840)]);
        (lin, win, vd)
    }

    #[test]
    fn hands_off_at_machine_boundary() {
        let (lin, win, vd) = two_machine_layout();
        let mut fm = FocusMachine::new(lin, vd, true, 0);
        // Start near the right edge of the Linux machine's second monitor.
        fm.on_motion(3800, 500, Modifiers::empty()); // move into Lin m1
        let action = fm.on_motion(100, 0, Modifiers::empty()); // cross into Win
        match action {
            FocusAction::HandOff { to, .. } => assert_eq!(to, win),
            other => panic!("expected hand-off, got {other:?}"),
        }
        assert_eq!(fm.role(), Role::Idle);
    }

    #[test]
    fn edge_resistance_delays_handoff() {
        let (lin, _win, vd) = two_machine_layout();
        let mut fm = FocusMachine::new(lin, vd, true, 50);
        fm.on_motion(3839, 500, Modifiers::empty()); // sit at far right of Lin
        // A small nudge across the boundary should be resisted first.
        let a = fm.on_motion(10, 0, Modifiers::empty());
        assert!(matches!(a, FocusAction::MoveLocal(_)));
        assert_eq!(fm.role(), Role::Active);
    }

    #[test]
    fn idle_machine_ignores_motion_until_enter() {
        let (lin, _win, vd) = two_machine_layout();
        let mut fm = FocusMachine::new(lin, vd, false, 0);
        assert_eq!(fm.on_motion(10, 10, Modifiers::empty()), FocusAction::None);
        let a = fm.on_enter(Point::new(100, 100));
        assert!(matches!(a, FocusAction::Take { .. }));
        assert_eq!(fm.role(), Role::Active);
    }
}
