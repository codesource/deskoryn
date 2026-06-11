//! # deskoryn-core
//!
//! Platform-agnostic domain model shared by every other crate. It deliberately
//! has **no** networking, OS, or async dependencies so it can be reused from the
//! daemon, the tray UI, tests, and tooling alike.
//!
//! The central idea of Deskoryn lives here: [`layout::VirtualDesktop`], a single
//! coordinate space that unifies all monitors from all machines. Higher layers
//! never reason about "the Linux box" or "the Windows box" — they reason about a
//! point in the virtual desktop and ask *which device owns the monitor under it*.

pub mod config;
pub mod geometry;
pub mod ids;
pub mod input;
pub mod layout;
pub mod trust;

pub use geometry::{Edge, Point, Rect, Size};
pub use ids::{DeviceId, MonitorId};
pub use input::{Button, InputEvent, KeyCode, Modifiers, ScrollAxis};
pub use layout::{Monitor, Transition, VirtualDesktop};
