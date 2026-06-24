//! The monitor arranger: a draggable-tile canvas where every monitor of both
//! machines is a tile in one shared virtual-desktop space. Drag a tile and its
//! edges snap to its neighbours; "Apply" serializes the arrangement into a
//! `VirtualDesktop` and pushes it with `SetLayout`.
//!
//! PROTOCOL GAP (same as the daemon notes): `Status` reports peer *names* but
//! neither device ids nor the current `VirtualDesktop`, so this edits a working
//! model seeded from the bring-up rig and pushes with placeholder device ids;
//! it can't read the live layout back yet. The serialization already matches the
//! wire shape, so only the read path is missing.

use crate::Message;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke, Text};
use iced::{mouse, Color, Point, Rectangle, Renderer, Size, Theme};

/// One monitor placed in virtual-desktop pixels.
#[derive(Clone, Debug)]
pub struct MonTile {
    /// 0 = this machine, 1 = the peer (drives the placeholder device id + colour).
    pub dev: u8,
    pub index: u16,
    pub label: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Seed model: three 1080p displays on this machine, two 1440p on the peer to
/// their right - the bring-up rig.
pub fn starter() -> Vec<MonTile> {
    let mut v = Vec::new();
    for i in 0..3 {
        v.push(MonTile {
            dev: 0,
            index: i,
            label: format!("Linux-{}", ["L", "C", "R"][i as usize]),
            x: i as i32 * 1920,
            y: 0,
            w: 1920,
            h: 1080,
        });
    }
    for i in 0..2 {
        v.push(MonTile {
            dev: 1,
            index: i,
            label: format!("Win-{}", ["L", "R"][i as usize]),
            x: 5760 + i as i32 * 2560,
            y: 0,
            w: 2560,
            h: 1440,
        });
    }
    v
}

const SNAP: i32 = 60; // virtual px within which edges snap together
const PAD: f32 = 24.0;
const REF_WIDTH: f32 = 13_000.0; // virtual width the canvas scales to fit

/// Stable virtual→screen transform (independent of tile positions, so dragging
/// doesn't make the whole canvas jitter).
fn scale_of(bounds: Rectangle) -> f32 {
    ((bounds.width - 2.0 * PAD) / REF_WIDTH).max(0.001)
}

/// Snap the moved tile's edges to nearby edges of the others.
pub fn snap(idx: usize, mut x: i32, mut y: i32, tiles: &[MonTile]) -> (i32, i32) {
    let m = &tiles[idx];
    for (j, o) in tiles.iter().enumerate() {
        if j == idx {
            continue;
        }
        if (x - (o.x + o.w)).abs() < SNAP {
            x = o.x + o.w;
        }
        if (x + m.w - o.x).abs() < SNAP {
            x = o.x - m.w;
        }
        if (x - o.x).abs() < SNAP {
            x = o.x;
        }
        if (y - (o.y + o.h)).abs() < SNAP {
            y = o.y + o.h;
        }
        if (y + m.h - o.y).abs() < SNAP {
            y = o.y - m.h;
        }
        if (y - o.y).abs() < SNAP {
            y = o.y;
        }
    }
    (x, y)
}

/// Serialize the working model into the `deskoryn_core::VirtualDesktop` JSON
/// shape the daemon deserializes (placeholder device ids - see the gap above).
pub fn to_virtual_desktop(tiles: &[MonTile]) -> serde_json::Value {
    let monitors: Vec<_> = tiles
        .iter()
        .map(|m| {
            let dev: Vec<u8> = vec![if m.dev == 0 { 0 } else { 1 }; 16];
            serde_json::json!({
                "id": { "device": dev, "index": m.index },
                "label": m.label,
                "bounds": { "x": m.x, "y": m.y, "w": m.w, "h": m.h },
                "native": { "w": m.w, "h": m.h },
                "scale_pct": 100,
            })
        })
        .collect();
    serde_json::json!({ "monitors": monitors })
}

/// Transient drag state: the grabbed tile and the grab offset in virtual px.
#[derive(Default)]
pub struct DragState {
    active: Option<(usize, f32, f32)>,
}

/// The canvas program; borrows the app's tiles for drawing/hit-testing.
pub struct Arranger<'a> {
    pub tiles: &'a [MonTile],
}

impl Arranger<'_> {
    fn to_virtual(&self, bounds: Rectangle, p: Point) -> (f32, f32) {
        let s = scale_of(bounds);
        ((p.x - PAD) / s, (p.y - PAD) / s)
    }

    fn tile_at(&self, vx: f32, vy: f32) -> Option<usize> {
        // Topmost (last drawn) first.
        self.tiles.iter().enumerate().rev().find_map(|(i, m)| {
            let inside = vx >= m.x as f32
                && vx < (m.x + m.w) as f32
                && vy >= m.y as f32
                && vy < (m.y + m.h) as f32;
            inside.then_some(i)
        })
    }
}

impl canvas::Program<Message> for Arranger<'_> {
    type State = DragState;

    fn update(
        &self,
        state: &mut DragState,
        event: canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        let Some(pos) = cursor.position_in(bounds) else {
            // Cursor left the canvas: drop any drag.
            if matches!(event, canvas::Event::Mouse(mouse::Event::ButtonReleased(_))) {
                state.active = None;
            }
            return (canvas::event::Status::Ignored, None);
        };
        match event {
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let (vx, vy) = self.to_virtual(bounds, pos);
                if let Some(idx) = self.tile_at(vx, vy) {
                    let m = &self.tiles[idx];
                    state.active = Some((idx, vx - m.x as f32, vy - m.y as f32));
                    return (canvas::event::Status::Captured, None);
                }
                (canvas::event::Status::Ignored, None)
            }
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if let Some((idx, gx, gy)) = state.active {
                    let (vx, vy) = self.to_virtual(bounds, pos);
                    let (x, y) = snap(idx, (vx - gx).round() as i32, (vy - gy).round() as i32, self.tiles);
                    return (canvas::event::Status::Captured, Some(Message::ArrMoved { idx, x, y }));
                }
                (canvas::event::Status::Ignored, None)
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(_)) => {
                state.active = None;
                (canvas::event::Status::Ignored, None)
            }
            _ => (canvas::event::Status::Ignored, None),
        }
    }

    fn draw(
        &self,
        _state: &DragState,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let s = scale_of(bounds);
        let local = Color::from_rgb8(0x1b, 0x5f, 0xa8);
        let peer = Color::from_rgb8(0x5a, 0x4f, 0xb0);
        let border = Color::from_rgb8(0xcc, 0xcc, 0xcc);

        // Backdrop.
        frame.fill_rectangle(Point::ORIGIN, bounds.size(), Color::from_rgb8(0xf0, 0xf2, 0xf5));

        for m in self.tiles {
            let top_left = Point::new(PAD + m.x as f32 * s, PAD + m.y as f32 * s);
            let size = Size::new(m.w as f32 * s, m.h as f32 * s);
            let rect = Path::rectangle(top_left, size);
            frame.fill(&rect, if m.dev == 0 { local } else { peer });
            frame.stroke(&rect, Stroke::default().with_color(border).with_width(1.5));
            frame.fill_text(Text {
                content: format!("{}\n{}x{}", m.label, m.w, m.h),
                position: Point::new(top_left.x + size.width / 2.0, top_left.y + size.height / 2.0),
                color: Color::WHITE,
                size: 12.0.into(),
                horizontal_alignment: iced::alignment::Horizontal::Center,
                vertical_alignment: iced::alignment::Vertical::Center,
                ..Text::default()
            });
        }
        vec![frame.into_geometry()]
    }
}
