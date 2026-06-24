//! The monitor arranger: a draggable-tile canvas where every monitor of both
//! machines is a tile in one shared virtual-desktop space. Drag a tile and its
//! edges snap to its neighbours; "Apply" serializes the arrangement into a
//! `VirtualDesktop` and pushes it with `SetLayout { peer }`.
//!
//! The working model is built from the daemon's `Arrangement` reply for the
//! selected peer ([`tiles_from_layout`]) — real monitors with resolutions, and
//! the saved-or-seeded placement. With no peer connected, the solo view shows
//! this device's own monitors read-only ([`tiles_from_views`]).

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

/// Build editable tiles from a combined layout (the daemon's `Arrangement`
/// reply). `own_device` is this machine's device id in hex, used to colour each
/// monitor as local (`dev = 0`) or peer (`dev = 1`).
pub fn tiles_from_layout(layout: &serde_json::Value, own_device: &str) -> Vec<MonTile> {
    let Some(mons) = layout.get("monitors").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    mons.iter()
        .map(|m| {
            let id = m.get("id");
            let dev_hex = id
                .and_then(|i| i.get("device"))
                .map(device_hex)
                .unwrap_or_default();
            let index = id.and_then(|i| i.get("index")).and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let label = m.get("label").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            let b = m.get("bounds");
            let g = |k: &str| b.and_then(|b| b.get(k)).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            MonTile {
                dev: if dev_hex == own_device { 0 } else { 1 },
                index,
                label,
                x: g("x"),
                y: g("y"),
                w: g("w"),
                h: g("h"),
            }
        })
        .collect()
}

/// Build read-only tiles from a flat monitor list (used for the solo view, where
/// only this device's monitors are shown).
pub fn tiles_from_views(views: &[crate::ipc::MonitorView]) -> Vec<MonTile> {
    views
        .iter()
        .map(|v| MonTile {
            dev: v.dev,
            index: v.index,
            label: v.label.clone(),
            x: v.x,
            y: v.y,
            w: v.w,
            h: v.h,
        })
        .collect()
}

/// Hex string for a serialized `DeviceId` (a JSON array of 16 byte values).
fn device_hex(device: &serde_json::Value) -> String {
    device
        .as_array()
        .map(|a| a.iter().map(|n| format!("{:02x}", n.as_u64().unwrap_or(0) as u8)).collect())
        .unwrap_or_default()
}

/// Parse a 32-char hex device id into its 16 bytes (zero-padded on error).
fn hex_to_bytes16(hex: &str) -> Vec<u8> {
    let b = hex.as_bytes();
    let mut out = Vec::with_capacity(16);
    let mut i = 0;
    while i + 1 < b.len() && out.len() < 16 {
        match ((b[i] as char).to_digit(16), (b[i + 1] as char).to_digit(16)) {
            (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
            _ => break,
        }
        i += 2;
    }
    out.resize(16, 0);
    out
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
/// shape the daemon deserializes, stamping each monitor with the **real** device
/// id (own vs the selected peer) so the daemon's focus logic can tell sides apart.
pub fn to_virtual_desktop(tiles: &[MonTile], own_device: &str, peer_device: &str) -> serde_json::Value {
    let monitors: Vec<_> = tiles
        .iter()
        .map(|m| {
            let dev = hex_to_bytes16(if m.dev == 0 { own_device } else { peer_device });
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
    /// When false (no peer connected), tiles are display-only: drags are ignored.
    pub editable: bool,
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
        if !self.editable {
            return (canvas::event::Status::Ignored, None);
        }
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
