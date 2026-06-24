//! Local monitor auto-detection for the arranger.
//!
//! Reads this machine's connected monitors and their positions so
//! `deskorynd arrange detect` can seed `config.layout` instead of the user
//! typing every `--size`/`--at` by hand.
//!
//! Today this is **X11 only**, by parsing `xrandr --query` — no extra native
//! dependency, and it covers the project's Linux box (3 monitors on X11). The
//! parser ([`parse_xrandr`]) is pure and unit-tested; the subprocess call is the
//! only platform-specific part. Wayland (via the compositor's output protocol)
//! and Windows (`EnumDisplayMonitors`) are future backends — `detect` returns a
//! clear error there so the user falls back to `arrange add`.

/// Detect this machine's monitors as virtual-desktop [`Monitor`]s owned by
/// `device`, ready to drop into a [`VirtualDesktop`] / `Hello`. Bounds are the
/// OS framebuffer coordinates as detected (the arranger places them relative to
/// the peer); `native` is the same pixel size and `scale_pct` defaults to 100.
///
/// [`Monitor`]: deskoryn_core::layout::Monitor
/// [`VirtualDesktop`]: deskoryn_core::VirtualDesktop
pub fn detect_monitors(device: deskoryn_core::DeviceId) -> anyhow::Result<Vec<deskoryn_core::layout::Monitor>> {
    use deskoryn_core::geometry::{Rect, Size};
    use deskoryn_core::ids::MonitorId;
    use deskoryn_core::layout::Monitor;
    Ok(detect()?
        .into_iter()
        .enumerate()
        .map(|(i, m)| Monitor {
            id: MonitorId::new(device, i as u16),
            label: m.name,
            bounds: Rect::new(m.x, m.y, m.w, m.h),
            native: Size { w: m.w, h: m.h },
            scale_pct: 100,
        })
        .collect())
}

/// One detected monitor in the OS's framebuffer-pixel coordinate space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorInfo {
    /// Connector/output name, e.g. "DP-0" — used as the monitor label.
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Detect this machine's active monitors, left-to-right.
#[cfg(target_os = "linux")]
pub fn detect() -> anyhow::Result<Vec<MonitorInfo>> {
    use anyhow::Context;
    let out = std::process::Command::new("xrandr")
        .arg("--query")
        .output()
        .context("running `xrandr --query` (is xrandr installed and $DISPLAY set?)")?;
    if !out.status.success() {
        anyhow::bail!("xrandr failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let mut mons = parse_xrandr(&String::from_utf8_lossy(&out.stdout));
    if mons.is_empty() {
        anyhow::bail!("xrandr reported no active monitors (a Wayland session? add them with `arrange add`)");
    }
    // Left-to-right, then top-to-bottom, for intuitive monitor indices.
    mons.sort_by_key(|m| (m.x, m.y));
    Ok(mons)
}

/// Detect this machine's active monitors via the Windows display API
/// (`EnumDisplayMonitors`, in `deskoryn-input`). Coordinates are in Windows'
/// virtual-screen space (primary at 0,0); the arranger offsets them into the
/// shared desktop (`arrange detect --offset-x`).
#[cfg(all(target_os = "windows", feature = "windows"))]
pub fn detect() -> anyhow::Result<Vec<MonitorInfo>> {
    let rects = deskoryn_input::monitors::detect()
        .map_err(|e| anyhow::anyhow!("monitor detection failed: {e}"))?;
    Ok(rects
        .into_iter()
        .map(|r| MonitorInfo { name: r.name, x: r.x, y: r.y, w: r.w, h: r.h })
        .collect())
}

/// Auto-detect isn't wired for this host/build (e.g. Wayland, or a Windows build
/// without the `windows` feature).
#[cfg(not(any(target_os = "linux", all(target_os = "windows", feature = "windows"))))]
pub fn detect() -> anyhow::Result<Vec<MonitorInfo>> {
    anyhow::bail!("monitor auto-detect needs Linux/X11 or a Windows build; add monitors manually with `arrange add`")
}

/// Parse the connected, active outputs out of `xrandr --query` output.
///
/// Monitor header lines start at column 0 with `NAME connected ... WxH+X+Y ...`;
/// mode lines are indented and skipped, as are `disconnected` outputs and
/// connected-but-inactive outputs (no `WxH+X+Y` geometry token).
#[cfg(target_os = "linux")]
fn parse_xrandr(output: &str) -> Vec<MonitorInfo> {
    let mut mons = Vec::new();
    for line in output.lines() {
        // Mode lines (and blanks) are indented; headers begin at column 0.
        if line.is_empty() || line.starts_with(char::is_whitespace) {
            continue;
        }
        let mut toks = line.split_whitespace();
        let Some(name) = toks.next() else { continue };
        if toks.next() != Some("connected") {
            continue; // "Screen 0:", "<name> disconnected", etc.
        }
        // The geometry token is the only `WxH+X+Y` on the header line; a
        // connected-but-off output has none, so it is skipped.
        if let Some((w, h, x, y)) = line.split_whitespace().find_map(parse_geometry) {
            mons.push(MonitorInfo { name: name.to_string(), x, y, w, h });
        }
    }
    mons
}

/// Parse an xrandr geometry token `"WxH+X+Y"` into `(w, h, x, y)`.
#[cfg(target_os = "linux")]
fn parse_geometry(tok: &str) -> Option<(i32, i32, i32, i32)> {
    let (size, rest) = tok.split_once('+')?; // "WxH", "X+Y"
    let (xs, ys) = rest.split_once('+')?; // "X", "Y"
    let (ws, hs) = size.split_once('x')?; // "W", "H"
    Some((ws.parse().ok()?, hs.parse().ok()?, xs.parse().ok()?, ys.parse().ok()?))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    // A representative `xrandr --query` dump: the Screen line, three active
    // outputs (one "primary", offsets out of order), a connected-but-off output,
    // a disconnected one, and indented mode lines that must all be ignored.
    const SAMPLE: &str = "\
Screen 0: minimum 320 x 200, current 5760 x 1080, maximum 16384 x 16384
DP-2.1 connected 1920x1080+3840+0 (normal left inverted right x axis y axis) 598mm x 336mm
   1920x1080     60.00*+  59.94
   1680x1050     59.95
DP-2.2 connected 1920x1080+1920+0 (normal left inverted right x axis y axis) 544mm x 303mm
   1920x1080     60.00*+
DP-0 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 598mm x 336mm
   1920x1080     60.00*+
DP-4 connected (normal left inverted right x axis y axis)
HDMI-0 disconnected (normal left inverted right x axis y axis)
";

    #[test]
    fn parses_active_outputs_and_skips_the_rest() {
        let mons = parse_xrandr(SAMPLE);
        assert_eq!(mons.len(), 3, "3 active outputs; DP-4 (off) and HDMI-0 (disconnected) skipped");

        let by_name = |n: &str| mons.iter().find(|m| m.name == n).unwrap();
        assert_eq!(by_name("DP-0"), &MonitorInfo { name: "DP-0".into(), x: 0, y: 0, w: 1920, h: 1080 });
        assert_eq!(by_name("DP-2.2").x, 1920);
        assert_eq!(by_name("DP-2.1").x, 3840);
        assert!(mons.iter().all(|m| m.w == 1920 && m.h == 1080));
    }

    #[test]
    fn geometry_token_parsing() {
        assert_eq!(parse_geometry("1920x1080+3840+0"), Some((1920, 1080, 3840, 0)));
        assert_eq!(parse_geometry("2560x1440+0+0"), Some((2560, 1440, 0, 0)));
        assert_eq!(parse_geometry("(normal"), None);
        assert_eq!(parse_geometry("598mm"), None);
        assert_eq!(parse_geometry("1920x1080"), None); // a mode, not a placement
    }
}
