//! `deskorynd arrange` — the stopgap monitor arranger.
//!
//! Until the GUI monitor-arranger lands (see `docs/UI.md`), this CLI lets the
//! user describe **this machine's** monitors and position them in the shared
//! virtual-desktop coordinate space. The layout is persisted to `config.layout`
//! and sent to the peer in the `Hello` handshake, where the two monitor sets are
//! union'd into one desktop (see [`crate::session`]).
//!
//! Because each machine arranges its own monitors into one *shared* space, the
//! two sides must not occupy overlapping coordinates — e.g. Linux owns
//! `0..5760`, Windows owns `5760..`. We surface overlaps as warnings rather than
//! refusing the edit, so an in-progress layout can be built up step by step.
//!
//! The placement math (`place_beside`, overlap detection) lives in
//! `deskoryn-core::layout` and is unit-tested there; this module is only the I/O
//! and argument plumbing.

use deskoryn_core::config::AppConfig;
use deskoryn_core::geometry::{Edge, Point, Rect, Size};
use deskoryn_core::layout::{Monitor, VirtualDesktop};
use deskoryn_core::{DeviceId, MonitorId};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(clap::Subcommand)]
pub enum ArrangeCmd {
    /// Print the current saved layout.
    Show,
    /// Auto-detect this machine's monitors (X11) and replace its entries in the
    /// layout. Peer monitors, if any, are left untouched.
    Detect,
    /// Add a local monitor, positioned absolutely (`--at`) or beside an existing
    /// one (`--right-of`/`--left-of`/`--above`/`--below`). With none of those, it
    /// is placed to the right of the rightmost existing monitor (or at 0,0).
    Add {
        /// Friendly label, e.g. "Lin-Center".
        #[arg(long)]
        label: String,
        /// Size in pixels, `WxH` (e.g. `1920x1080`).
        #[arg(long, value_parser = parse_size)]
        size: Size,
        /// Display scale percent (100 = 1.0x, 150 = 1.5x).
        #[arg(long, default_value_t = 100)]
        scale: u16,
        /// Absolute top-left position `X,Y`.
        #[arg(long, value_parser = parse_point, conflicts_with_all = ["right_of", "left_of", "above", "below"])]
        at: Option<Point>,
        /// Place flush to the right of the monitor with this label.
        #[arg(long, conflicts_with_all = ["left_of", "above", "below"])]
        right_of: Option<String>,
        /// Place flush to the left of the monitor with this label.
        #[arg(long, conflicts_with_all = ["above", "below"])]
        left_of: Option<String>,
        /// Place flush above the monitor with this label.
        #[arg(long, conflicts_with = "below")]
        above: Option<String>,
        /// Place flush below the monitor with this label.
        #[arg(long)]
        below: Option<String>,
    },
    /// Remove a local monitor by label.
    Remove {
        /// Label of the monitor to remove.
        label: String,
    },
    /// Remove all local monitors.
    Clear,
    /// Write the current layout as JSON (to `--path`, or stdout).
    Export {
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Replace the local layout from a JSON file produced by `export`.
    Import {
        /// JSON file to read.
        path: PathBuf,
    },
}

pub fn run(config: Arc<AppConfig>, config_path: &Path, cmd: ArrangeCmd) -> anyhow::Result<()> {
    let me = config.device.id;
    let mut config = (*config).clone();

    match cmd {
        ArrangeCmd::Show => {
            print_layout(&config.layout);
            return Ok(());
        }
        ArrangeCmd::Export { path } => {
            let json = serde_json::to_string_pretty(&config.layout)?;
            match path {
                Some(p) => {
                    std::fs::write(&p, json)?;
                    println!("wrote {}", p.display());
                }
                None => println!("{json}"),
            }
            return Ok(());
        }
        ArrangeCmd::Import { path } => {
            let json = std::fs::read_to_string(&path)?;
            config.layout = serde_json::from_str(&json)?;
        }
        ArrangeCmd::Clear => {
            config.layout.monitors.clear();
        }
        ArrangeCmd::Detect => {
            let detected = crate::monitors::detect()?;
            // Replace only this device's monitors with the fresh read.
            config.layout.monitors.retain(|m| m.device() != me);
            for (i, d) in detected.iter().enumerate() {
                config.layout.monitors.push(Monitor {
                    id: MonitorId::new(me, i as u16),
                    label: d.name.clone(),
                    bounds: Rect::new(d.x, d.y, d.w, d.h),
                    native: Size::new(d.w, d.h),
                    scale_pct: 100,
                });
            }
            println!("detected {} monitor(s)", detected.len());
        }
        ArrangeCmd::Remove { label } => {
            let before = config.layout.monitors.len();
            config.layout.monitors.retain(|m| m.label != label);
            if config.layout.monitors.len() == before {
                anyhow::bail!("no monitor labelled {label:?}");
            }
        }
        ArrangeCmd::Add {
            label,
            size,
            scale,
            at,
            right_of,
            left_of,
            above,
            below,
        } => {
            if config.layout.monitors.iter().any(|m| m.label == label) {
                anyhow::bail!("a monitor labelled {label:?} already exists");
            }
            let bounds = resolve_bounds(&config.layout, size, at, right_of, left_of, above, below)?;
            let index = next_index(&config.layout, me);
            config.layout.monitors.push(Monitor {
                id: MonitorId::new(me, index),
                label,
                bounds,
                native: size,
                scale_pct: scale,
            });
        }
    }

    if let Some((a, b)) = config.layout.first_overlap() {
        eprintln!(
            "warning: monitors {} and {} overlap — the two machines must occupy \
             non-overlapping regions of the shared desktop",
            a.index, b.index
        );
    }

    config.save(config_path)?;
    print_layout(&config.layout);
    Ok(())
}

/// Resolve the bounds for a new monitor from the placement flags. Absolute `--at`
/// wins; then any relative flag against a labelled anchor; otherwise it goes to
/// the right of the rightmost existing monitor (or the origin if there is none).
fn resolve_bounds(
    layout: &VirtualDesktop,
    size: Size,
    at: Option<Point>,
    right_of: Option<String>,
    left_of: Option<String>,
    above: Option<String>,
    below: Option<String>,
) -> anyhow::Result<Rect> {
    if let Some(p) = at {
        return Ok(Rect::new(p.x, p.y, size.w, size.h));
    }
    let relative = [
        (right_of, Edge::Right),
        (left_of, Edge::Left),
        (above, Edge::Top),
        (below, Edge::Bottom),
    ]
    .into_iter()
    .find_map(|(label, side)| label.map(|l| (l, side)));

    if let Some((label, side)) = relative {
        let anchor = layout
            .monitors
            .iter()
            .find(|m| m.label == label)
            .ok_or_else(|| anyhow::anyhow!("no monitor labelled {label:?} to place beside"))?;
        return layout
            .place_beside(anchor.id, side, size)
            .ok_or_else(|| anyhow::anyhow!("anchor monitor vanished"));
    }

    // Default: extend the row to the right.
    let x = layout
        .monitors
        .iter()
        .map(|m| m.bounds.right())
        .max()
        .unwrap_or(0);
    Ok(Rect::new(x, 0, size.w, size.h))
}

/// Next free monitor index for `me`.
fn next_index(layout: &VirtualDesktop, me: DeviceId) -> u16 {
    layout
        .monitors
        .iter()
        .filter(|m| m.device() == me)
        .map(|m| m.id.index)
        .max()
        .map(|i| i + 1)
        .unwrap_or(0)
}

fn print_layout(layout: &VirtualDesktop) {
    if layout.monitors.is_empty() {
        println!("(no monitors arranged — use `deskorynd arrange add ...`)");
        return;
    }
    for m in &layout.monitors {
        let b = m.bounds;
        println!(
            "[{}] {:<16} {}x{} @ ({},{})  scale {}%",
            m.id.index, m.label, b.w, b.h, b.x, b.y, m.scale_pct
        );
    }
    if let Some(bb) = layout.bounding_box() {
        println!("desktop: {}x{} from ({},{})", bb.w, bb.h, bb.x, bb.y);
    }
}

/// Parse a `WxH` size string (e.g. `1920x1080`).
fn parse_size(s: &str) -> Result<Size, String> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("expected WxH, got {s:?}"))?;
    let w = w
        .trim()
        .parse()
        .map_err(|_| format!("bad width in {s:?}"))?;
    let h = h
        .trim()
        .parse()
        .map_err(|_| format!("bad height in {s:?}"))?;
    Ok(Size::new(w, h))
}

/// Parse an `X,Y` point string (e.g. `5760,0`).
fn parse_point(s: &str) -> Result<Point, String> {
    let (x, y) = s
        .split_once(',')
        .ok_or_else(|| format!("expected X,Y, got {s:?}"))?;
    let x = x.trim().parse().map_err(|_| format!("bad x in {s:?}"))?;
    let y = y.trim().parse().map_err(|_| format!("bad y in {s:?}"))?;
    Ok(Point::new(x, y))
}
