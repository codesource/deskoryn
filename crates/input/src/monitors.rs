//! Windows monitor enumeration for the arranger's `detect` command.
//!
//! `EnumDisplayMonitors` + `GetMonitorInfoW` report each monitor's rectangle in
//! the Windows **virtual-screen** coordinate space (the primary monitor's
//! top-left is the origin). Because that origin is `0,0` on both machines, the
//! arranger offsets this machine's detected set into the shared cross-machine
//! desktop (see `arrange detect --offset-x`). Built only for Windows with the
//! `windows-backend` feature; the X11 path lives in the daemon (`xrandr`).
//!
//! Compile-verified via cross-compile; needs a real Windows session to validate.

use crate::InputError;
use std::mem::size_of;
use windows::Win32::Foundation::{BOOL, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};

/// One detected monitor in the OS's virtual-screen pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorRect {
    /// Device name, e.g. `\\.\DISPLAY1` — used as the monitor label.
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Enumerate the active monitors, left-to-right.
pub fn detect() -> Result<Vec<MonitorRect>, InputError> {
    let mut out: Vec<MonitorRect> = Vec::new();
    // SAFETY: `enum_proc` receives `&mut out` back through `dwdata` and only
    // appends to it; EnumDisplayMonitors calls it synchronously per monitor.
    let _ = unsafe {
        EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(enum_proc),
            LPARAM(&mut out as *mut Vec<MonitorRect> as isize),
        )
    };
    if out.is_empty() {
        return Err(InputError::Backend("EnumDisplayMonitors found no monitors".into()));
    }
    out.sort_by_key(|m| (m.x, m.y));
    Ok(out)
}

extern "system" fn enum_proc(hmon: HMONITOR, _hdc: HDC, _clip: *mut RECT, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the `&mut Vec<MonitorRect>` passed to EnumDisplayMonitors.
    let out = unsafe { &mut *(lparam.0 as *mut Vec<MonitorRect>) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    let ok = unsafe { GetMonitorInfoW(hmon, &mut info.monitorInfo as *mut MONITORINFO) };
    if ok.as_bool() {
        let r = info.monitorInfo.rcMonitor;
        let name = String::from_utf16_lossy(&info.szDevice);
        out.push(MonitorRect {
            name: name.trim_end_matches('\0').to_string(),
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
        });
    }
    TRUE // keep enumerating
}
