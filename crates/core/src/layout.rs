//! The unified virtual desktop: the abstraction that makes five monitors across
//! two machines behave like one desktop.
//!
//! ## Model
//!
//! Every monitor — regardless of which machine it is physically attached to — is
//! placed as a [`Rect`] in one shared coordinate space ([`geometry`](crate::geometry)).
//! The cursor has a single global position. The machine that *owns the monitor
//! currently under the cursor* is the **active** machine: it injects pointer
//! motion locally and captures real input, forwarding everything else over the
//! wire. When the cursor reaches a monitor edge that is adjacent to a monitor
//! owned by the other machine, control hands off ([`Transition`]).
//!
//! This is the same topology concept Deskflow / Input Leap / Barrier use, but
//! generalized to an N-machine mesh keyed on [`DeviceId`](crate::ids::DeviceId).

use crate::geometry::{Edge, Point, Rect};
use crate::ids::{DeviceId, MonitorId};
use serde::{Deserialize, Serialize};

/// A single physical monitor placed into the virtual desktop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Monitor {
    pub id: MonitorId,
    /// User-facing label, e.g. "Linux-Center" or "Win-Right".
    pub label: String,
    /// Placement in virtual-desktop pixels.
    pub bounds: Rect,
    /// Native pixel resolution (may differ from `bounds` under fractional scale).
    pub native: crate::geometry::Size,
    /// Display scale factor in percent (100 = 1.0x, 150 = 1.5x). Used to map
    /// virtual coordinates onto the owning OS's logical/native pointer space.
    pub scale_pct: u16,
}

impl Monitor {
    pub fn device(&self) -> DeviceId {
        self.id.device
    }
}

/// The result of a cursor crossing a monitor edge into territory owned by a
/// different (or the same) machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition {
    /// The monitor the cursor is entering.
    pub target: MonitorId,
    /// The device that should become active.
    pub device: DeviceId,
    /// The edge of the *source* monitor that was crossed.
    pub via: Edge,
    /// The clamped landing point inside the target monitor.
    pub entry: Point,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VirtualDesktop {
    pub monitors: Vec<Monitor>,
}

impl VirtualDesktop {
    pub fn new(monitors: Vec<Monitor>) -> Self {
        Self { monitors }
    }

    pub fn monitor(&self, id: MonitorId) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.id == id)
    }

    /// The monitor whose bounds contain `p`, if any.
    pub fn monitor_at(&self, p: Point) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.bounds.contains(p))
    }

    /// The device that owns the monitor under `p`.
    pub fn owner_at(&self, p: Point) -> Option<DeviceId> {
        self.monitor_at(p).map(Monitor::device)
    }

    /// The full bounding box of the virtual desktop.
    pub fn bounding_box(&self) -> Option<Rect> {
        let mut it = self.monitors.iter();
        let first = it.next()?.bounds;
        let (mut l, mut t, mut r, mut b) =
            (first.left(), first.top(), first.right(), first.bottom());
        for m in it {
            l = l.min(m.bounds.left());
            t = t.min(m.bounds.top());
            r = r.max(m.bounds.right());
            b = b.max(m.bounds.bottom());
        }
        Some(Rect::new(l, t, r - l, b - t))
    }

    /// Resolve a *desired* cursor move from `from` toward `to`.
    ///
    /// Returns `Some(Transition)` when the move leaves the current monitor and
    /// lands on (or projects onto) a different monitor — including a monitor on
    /// the other machine. Returns `None` when the move stays within the current
    /// monitor. When the move would leave the virtual desktop entirely, the
    /// caller should clamp to the current monitor (the desktop edge is "sticky").
    ///
    /// Note: this is a geometric resolver; the daemon's focus state machine
    /// layers hysteresis / hotkey overrides on top (see `deskoryn-daemon`).
    pub fn resolve_move(&self, from: Point, to: Point) -> Option<Transition> {
        let current = self.monitor_at(from)?;
        if current.bounds.contains(to) {
            return None; // still on the same monitor
        }

        // Determine which edge we exited through (dominant axis of travel).
        let via = exit_edge(current.bounds, from, to);

        // Find the monitor the target point falls into directly...
        if let Some(target) = self.monitor_at(to) {
            return Some(Transition {
                target: target.id,
                device: target.device(),
                via,
                entry: target.bounds.clamp(to),
            });
        }

        // ...otherwise project onto the nearest monitor adjacent across `via`,
        // preserving the orthogonal coordinate (so vertical position carries
        // across a left/right hop). This is what makes diagonally-misaligned
        // monitors feel continuous.
        self.adjacent_across(current, via, to).map(|target| Transition {
            target: target.id,
            device: target.device(),
            via,
            entry: target.bounds.clamp(project(via, current.bounds, to)),
        })
    }

    /// Pick the best monitor neighbouring `current` across edge `via`, scored by
    /// overlap on the shared axis and proximity on the crossing axis.
    fn adjacent_across(&self, current: &Monitor, via: Edge, to: Point) -> Option<&Monitor> {
        self.monitors
            .iter()
            .filter(|m| m.id != current.id)
            .filter(|m| is_neighbour(current.bounds, m.bounds, via))
            .min_by_key(|m| {
                // Prefer the candidate whose span best contains the crossing
                // coordinate, then the closest one.
                let c = m.bounds;
                match via {
                    Edge::Left | Edge::Right => {
                        let dy = axis_distance(to.y, c.top(), c.bottom());
                        let dx = (current.bounds.x - c.x).abs();
                        dy as i64 * 10_000 + dx as i64
                    }
                    Edge::Top | Edge::Bottom => {
                        let dx = axis_distance(to.x, c.left(), c.right());
                        let dy = (current.bounds.y - c.y).abs();
                        dx as i64 * 10_000 + dy as i64
                    }
                }
            })
    }
}

/// Layout-editing helpers used by the monitor arranger. These are pure geometry
/// over the placed rectangles — no I/O — so the arranger CLI and (later) the GUI
/// share one tested implementation.
impl VirtualDesktop {
    /// The first pair of monitors whose bounds overlap, if any. A valid layout
    /// has none: monitors tile the plane edge-to-edge without overlapping.
    pub fn first_overlap(&self) -> Option<(MonitorId, MonitorId)> {
        for (i, a) in self.monitors.iter().enumerate() {
            for b in &self.monitors[i + 1..] {
                if rects_overlap(a.bounds, b.bounds) {
                    return Some((a.id, b.id));
                }
            }
        }
        None
    }

    /// Translate every monitor so the desktop's bounding box starts at the
    /// origin. Keeps saved layouts canonical regardless of how they were built.
    pub fn normalize(&mut self) {
        let Some(bb) = self.bounding_box() else { return };
        if bb.x == 0 && bb.y == 0 {
            return;
        }
        for m in &mut self.monitors {
            m.bounds.x -= bb.x;
            m.bounds.y -= bb.y;
        }
    }

    /// Compute the bounds for a new `size` monitor placed flush against the
    /// `side` edge of `anchor`, aligned to the anchor's top-left corner. Returns
    /// `None` if `anchor` isn't in this desktop.
    pub fn place_beside(&self, anchor: MonitorId, side: Edge, size: crate::geometry::Size) -> Option<Rect> {
        let a = self.monitor(anchor)?.bounds;
        let (w, h) = (size.w, size.h);
        let (x, y) = match side {
            Edge::Right => (a.right(), a.top()),
            Edge::Left => (a.left() - w, a.top()),
            Edge::Bottom => (a.left(), a.bottom()),
            Edge::Top => (a.left(), a.top() - h),
        };
        Some(Rect::new(x, y, w, h))
    }
}

/// Do two rectangles share any interior area? (Touching edges do not overlap.)
pub fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.left() < b.right() && b.left() < a.right() && a.top() < b.bottom() && b.top() < a.bottom()
}

/// Which edge of `bounds` the segment `from -> to` exits through.
fn exit_edge(bounds: Rect, from: Point, to: Point) -> Edge {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    // Compare horizontal vs vertical overshoot to choose the dominant edge.
    let over_x = if dx > 0 {
        to.x - (bounds.right() - 1)
    } else {
        bounds.left() - to.x
    }
    .max(0);
    let over_y = if dy > 0 {
        to.y - (bounds.bottom() - 1)
    } else {
        bounds.top() - to.y
    }
    .max(0);

    if over_x >= over_y {
        if dx >= 0 {
            Edge::Right
        } else {
            Edge::Left
        }
    } else if dy >= 0 {
        Edge::Bottom
    } else {
        Edge::Top
    }
}

/// Is `cand` positioned on the `via` side of `src` (with axis overlap)?
fn is_neighbour(src: Rect, cand: Rect, via: Edge) -> bool {
    match via {
        Edge::Right => cand.left() >= src.right() && spans_overlap(src.top(), src.bottom(), cand.top(), cand.bottom()),
        Edge::Left => cand.right() <= src.left() && spans_overlap(src.top(), src.bottom(), cand.top(), cand.bottom()),
        Edge::Bottom => cand.top() >= src.bottom() && spans_overlap(src.left(), src.right(), cand.left(), cand.right()),
        Edge::Top => cand.bottom() <= src.top() && spans_overlap(src.left(), src.right(), cand.left(), cand.right()),
    }
}

/// The landing point on `dst` when the cursor crosses *out of* `src` heading
/// toward `to`, mapped so it lands **relative to where it left**:
///
/// * The coordinate *along* the shared edge is kept **proportional** — leaving
///   90% of the way down a 1080-tall monitor enters 90% of the way down a
///   1440-tall one, so a handoff between displays of different size/resolution
///   feels continuous instead of jumping to an absolute pixel row.
/// * The *crossing* coordinate snaps to `dst`'s near edge, so the cursor enters
///   exactly at the boundary it crossed rather than some pixels deep inside the
///   next monitor.
///
/// This is the entry math for a machine-boundary handoff (see the daemon's input
/// controller). Same-machine monitor-to-monitor moves stay continuous and don't
/// use this.
pub fn relative_entry(src: Rect, dst: Rect, from: Point, to: Point) -> Point {
    let via = exit_edge(src, from, to);
    // Fraction along the source edge the cursor sat at when it crossed, taken
    // from the post-move point clamped back into the source span.
    let lerp = |v: i32, s_lo: i32, s_span: i32, d_lo: i32, d_span: i32| -> i32 {
        let f = (v - s_lo).clamp(0, (s_span - 1).max(0));
        d_lo + (f as i64 * d_span as i64 / s_span.max(1) as i64) as i32
    };
    match via {
        Edge::Left | Edge::Right => {
            let y = lerp(to.y, src.top(), src.h, dst.top(), dst.h);
            let x = if matches!(via, Edge::Right) { dst.left() } else { dst.right() - 1 };
            dst.clamp(Point::new(x, y))
        }
        Edge::Top | Edge::Bottom => {
            let x = lerp(to.x, src.left(), src.w, dst.left(), dst.w);
            let y = if matches!(via, Edge::Bottom) { dst.top() } else { dst.bottom() - 1 };
            dst.clamp(Point::new(x, y))
        }
    }
}

/// Project `to` so the crossing coordinate sits just inside the neighbour while
/// the orthogonal coordinate is preserved from the original target point.
fn project(via: Edge, _src: Rect, to: Point) -> Point {
    // The orthogonal coordinate is kept; the crossing coordinate gets clamped by
    // `Rect::clamp` at the call site, so we just pass `to` through here. Kept as
    // a named step for clarity and future per-edge adjustments.
    let _ = via;
    to
}

fn spans_overlap(a0: i32, a1: i32, b0: i32, b1: i32) -> bool {
    a0.max(b0) < a1.min(b1)
}

/// Distance from `v` to the `[lo,hi)` interval (0 if inside).
fn axis_distance(v: i32, lo: i32, hi: i32) -> i32 {
    if v < lo {
        lo - v
    } else if v >= hi {
        v - hi + 1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Size;

    fn dev(b: u8) -> DeviceId {
        DeviceId::from_bytes([b; 16])
    }

    fn mon(device: DeviceId, idx: u16, label: &str, x: i32, y: i32, w: i32, h: i32) -> Monitor {
        Monitor {
            id: MonitorId::new(device, idx),
            label: label.into(),
            bounds: Rect::new(x, y, w, h),
            native: Size::new(w, h),
            scale_pct: 100,
        }
    }

    /// Three Linux monitors on the left, two Windows monitors on the right.
    fn sample() -> (DeviceId, DeviceId, VirtualDesktop) {
        let lin = dev(1);
        let win = dev(2);
        let vd = VirtualDesktop::new(vec![
            mon(lin, 0, "Lin-L", 0, 0, 1920, 1080),
            mon(lin, 1, "Lin-C", 1920, 0, 1920, 1080),
            mon(lin, 2, "Lin-R", 3840, 0, 1920, 1080),
            mon(win, 0, "Win-L", 5760, 0, 2560, 1440),
            mon(win, 1, "Win-R", 8320, 0, 2560, 1440),
        ]);
        (lin, win, vd)
    }

    #[test]
    fn stays_on_monitor() {
        let (_, _, vd) = sample();
        assert!(vd.resolve_move(Point::new(100, 100), Point::new(200, 200)).is_none());
    }

    #[test]
    fn crosses_within_same_machine() {
        let (lin, _, vd) = sample();
        let t = vd
            .resolve_move(Point::new(1900, 500), Point::new(1930, 500))
            .expect("should cross into Lin-C");
        assert_eq!(t.device, lin);
        assert_eq!(t.target.index, 1);
    }

    #[test]
    fn crosses_machine_boundary_to_windows() {
        let (_, win, vd) = sample();
        let t = vd
            .resolve_move(Point::new(5750, 500), Point::new(5780, 500))
            .expect("should cross into Win-L");
        assert_eq!(t.device, win);
        assert_eq!(t.target.index, 0);
    }

    #[test]
    fn detects_overlap() {
        let d = dev(1);
        let mut vd = VirtualDesktop::new(vec![
            mon(d, 0, "A", 0, 0, 1920, 1080),
            mon(d, 1, "B", 1920, 0, 1920, 1080),
        ]);
        assert!(vd.first_overlap().is_none(), "edge-to-edge tiles do not overlap");

        // Nudge B back so it overlaps A.
        vd.monitors[1].bounds.x = 1900;
        assert!(vd.first_overlap().is_some());
    }

    #[test]
    fn normalize_shifts_bbox_to_origin() {
        let d = dev(1);
        let mut vd = VirtualDesktop::new(vec![
            mon(d, 0, "A", -500, -200, 1920, 1080),
            mon(d, 1, "B", 1420, -200, 1920, 1080),
        ]);
        vd.normalize();
        let bb = vd.bounding_box().unwrap();
        assert_eq!((bb.x, bb.y), (0, 0));
        // Relative placement is preserved.
        assert_eq!(vd.monitors[0].bounds.x, 0);
        assert_eq!(vd.monitors[1].bounds.x, 1920);
    }

    #[test]
    fn place_beside_each_edge() {
        let d = dev(1);
        let vd = VirtualDesktop::new(vec![mon(d, 0, "A", 0, 0, 1920, 1080)]);
        let anchor = vd.monitors[0].id;
        let size = Size::new(2560, 1440);

        assert_eq!(vd.place_beside(anchor, Edge::Right, size).unwrap(), Rect::new(1920, 0, 2560, 1440));
        assert_eq!(vd.place_beside(anchor, Edge::Left, size).unwrap(), Rect::new(-2560, 0, 2560, 1440));
        assert_eq!(vd.place_beside(anchor, Edge::Bottom, size).unwrap(), Rect::new(0, 1080, 2560, 1440));
        assert_eq!(vd.place_beside(anchor, Edge::Top, size).unwrap(), Rect::new(0, -1440, 2560, 1440));
        assert!(vd.place_beside(MonitorId::new(dev(9), 0), Edge::Right, size).is_none());
    }

    #[test]
    fn relative_entry_is_proportional_along_the_edge() {
        // Leaving a 1920x1080 monitor on its right edge, entering a taller
        // 2560x1440 monitor placed to the right.
        let src = Rect::new(0, 0, 1920, 1080);
        let dst = Rect::new(1920, 0, 2560, 1440);

        // Crossing at the very top stays at the top.
        let top = relative_entry(src, dst, Point::new(1900, 0), Point::new(1925, 0));
        assert_eq!(top, Point::new(1920, 0));

        // Crossing at the bottom (~100% down 1080) lands ~100% down 1440.
        let bottom = relative_entry(src, dst, Point::new(1900, 1079), Point::new(1925, 1079));
        assert_eq!(bottom.x, 1920, "enters at the near (left) edge of dst");
        assert!(bottom.y >= 1438, "~bottom of the 1440-tall monitor, got {}", bottom.y);

        // Crossing at the middle lands at the middle, not an absolute pixel row.
        let mid = relative_entry(src, dst, Point::new(1900, 540), Point::new(1925, 540));
        assert!((715..=725).contains(&mid.y), "~half of 1440, got {}", mid.y);
    }

    #[test]
    fn relative_entry_snaps_to_the_near_edge() {
        // A big overshoot deep into the next monitor still enters at its edge.
        let src = Rect::new(0, 0, 1920, 1080);
        let dst = Rect::new(1920, 0, 1920, 1080);
        let e = relative_entry(src, dst, Point::new(1900, 500), Point::new(2400, 500));
        assert_eq!(e.x, 1920, "snaps to dst's left edge, not 480px deep");
        assert_eq!(e.y, 500);
    }

    #[test]
    fn projects_vertical_offset_across_resolution_change() {
        // Windows monitors are 1440 tall vs 1080 on Linux; a crossing near the
        // bottom of the Linux row should land (clamped) inside the Windows panel.
        let (_, win, vd) = sample();
        let t = vd
            .resolve_move(Point::new(5750, 1000), Point::new(5800, 1000))
            .expect("cross into Win-L near bottom");
        assert_eq!(t.device, win);
        assert!(t.entry.y < 1440);
    }
}
