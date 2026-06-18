//! OS-native file-list clipboard — the one format `arboard` can't reach.
//!
//! * **Linux/X11**: read the `text/uri-list` (or GNOME `x-special/gnome-copied-
//!   files`) target of the `CLIPBOARD` selection; write by becoming the
//!   selection owner and serving those targets from a background thread. The
//!   owner coexists with `arboard`'s text/image owner as *last-writer-wins*:
//!   copying files takes the selection (our owner serves the file list); copying
//!   text hands it back to `arboard` (our owner sees `SelectionClear` and exits).
//! * **Windows**: `CF_HDROP` via the Win32 clipboard API.
//!
//! The default / portable build (and a mismatched feature/target combo) gets the
//! no-op fallback, so nothing here touches a real clipboard unless the matching
//! backend is both enabled *and* built for its own OS.

use std::path::PathBuf;

#[cfg(all(feature = "linux-backend", target_os = "linux"))]
pub use linux::{read, write, OwnerGuard};

#[cfg(all(feature = "windows-backend", target_os = "windows"))]
pub use win::{read, write, OwnerGuard};

#[cfg(not(any(
    all(feature = "linux-backend", target_os = "linux"),
    all(feature = "windows-backend", target_os = "windows")
)))]
pub use fallback::{read, write, OwnerGuard};

#[cfg(not(any(
    all(feature = "linux-backend", target_os = "linux"),
    all(feature = "windows-backend", target_os = "windows")
)))]
mod fallback {
    use super::PathBuf;
    /// Nothing to retain on a backend that can't own a file selection.
    pub struct OwnerGuard;
    pub fn read() -> Option<Vec<PathBuf>> {
        None
    }
    pub fn write(_paths: &[PathBuf]) -> Option<OwnerGuard> {
        None
    }
}

// ---------------------------------------------------------------------------
// Shared `file://` URI <-> path encoding (used by the X11 backend).
// ---------------------------------------------------------------------------

#[cfg(all(feature = "linux-backend", target_os = "linux"))]
mod uri {
    use super::PathBuf;
    use std::path::Path;

    /// `file://` URI for an absolute path, percent-encoding everything outside
    /// the RFC 3986 unreserved set (path separators kept literal).
    pub fn from_path(p: &Path) -> String {
        let mut out = String::from("file://");
        for &b in p.to_string_lossy().as_bytes() {
            match b {
                b'/' => out.push('/'),
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    /// `text/uri-list` (CRLF-separated, trailing CRLF) or the GNOME
    /// `x-special/gnome-copied-files` form (`copy\n` verb + LF-separated URIs).
    pub fn encode_list(paths: &[PathBuf], gnome: bool) -> Vec<u8> {
        let uris: Vec<String> = paths.iter().map(|p| from_path(p)).collect();
        if gnome {
            format!("copy\n{}", uris.join("\n")).into_bytes()
        } else {
            let mut s = uris.join("\r\n");
            s.push_str("\r\n");
            s.into_bytes()
        }
    }

    /// Parse a uri-list / gnome-copied-files blob into absolute paths. `gnome`
    /// skips the leading `copy`/`cut` verb line.
    pub fn parse_list(bytes: &[u8], gnome: bool) -> Vec<PathBuf> {
        let text = String::from_utf8_lossy(bytes);
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if gnome && i == 0 {
                continue; // the verb ("copy" / "cut")
            }
            let Some(rest) = line.strip_prefix("file://") else { continue };
            // Drop an optional host component: file://host/path -> /path.
            let path_part = match rest.find('/') {
                Some(idx) => &rest[idx..],
                None => rest,
            };
            if let Some(decoded) = percent_decode(path_part) {
                out.push(PathBuf::from(decoded));
            }
        }
        out
    }

    fn percent_decode(s: &str) -> Option<String> {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                out.push(hex(b[i + 1])? * 16 + hex(b[i + 2])?);
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).ok()
    }

    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn uri_round_trip_with_spaces() {
            let paths = vec![PathBuf::from("/home/me/a file.txt"), PathBuf::from("/tmp/x")];
            let bytes = encode_list(&paths, false);
            assert_eq!(parse_list(&bytes, false), paths);
        }

        #[test]
        fn gnome_form_skips_verb() {
            let paths = vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")];
            let bytes = encode_list(&paths, true);
            assert!(bytes.starts_with(b"copy\n"));
            assert_eq!(parse_list(&bytes, true), paths);
        }
    }
}

// ---------------------------------------------------------------------------
// Linux / X11 backend.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "linux-backend", target_os = "linux"))]
mod linux {
    use super::{uri, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
        SelectionNotifyEvent, SelectionRequestEvent, WindowClass, SELECTION_NOTIFY_EVENT,
    };
    use x11rb::protocol::Event;
    use x11rb::wrapper::ConnectionExt as _;
    use x11rb::{CURRENT_TIME, NONE};

    struct Atoms {
        clipboard: Atom,
        targets: Atom,
        uri_list: Atom,
        gnome: Atom,
        prop: Atom,
    }

    fn intern<C: Connection>(conn: &C, name: &[u8]) -> Option<Atom> {
        Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom)
    }

    fn atoms<C: Connection>(conn: &C) -> Option<Atoms> {
        Some(Atoms {
            clipboard: intern(conn, b"CLIPBOARD")?,
            targets: intern(conn, b"TARGETS")?,
            uri_list: intern(conn, b"text/uri-list")?,
            gnome: intern(conn, b"x-special/gnome-copied-files")?,
            prop: intern(conn, b"DESKORYN_CLIP")?,
        })
    }

    fn hidden_window<C: Connection>(conn: &C, screen: usize) -> Option<u32> {
        let root = conn.setup().roots[screen].root;
        let visual = conn.setup().roots[screen].root_visual;
        let win = conn.generate_id().ok()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new(),
        )
        .ok()?;
        Some(win)
    }

    /// One-shot read of the file list currently on the `CLIPBOARD` selection.
    pub fn read() -> Option<Vec<PathBuf>> {
        let (conn, screen) = x11rb::connect(None).ok()?;
        let win = hidden_window(&conn, screen)?;
        let a = atoms(&conn)?;

        // Prefer the GNOME form (carries copy/cut), fall back to plain uri-list.
        for (target, gnome) in [(a.gnome, true), (a.uri_list, false)] {
            if conn.convert_selection(win, a.clipboard, target, a.prop, CURRENT_TIME).is_err() {
                continue;
            }
            let _ = conn.flush();
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                match conn.poll_for_event().ok()? {
                    Some(Event::SelectionNotify(e)) => {
                        if e.property == NONE {
                            break; // owner can't supply this target
                        }
                        let reply = conn
                            .get_property(true, win, a.prop, AtomEnum::ANY, 0, u32::MAX)
                            .ok()?
                            .reply()
                            .ok()?;
                        let paths = uri::parse_list(&reply.value, gnome);
                        if !paths.is_empty() {
                            return Some(paths);
                        }
                        break;
                    }
                    Some(_) => {}
                    None => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        }
        None
    }

    /// Retains `CLIPBOARD` ownership for the served file list; dropping it stops
    /// the owner thread (relinquishing the selection).
    pub struct OwnerGuard {
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl Drop for OwnerGuard {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    pub fn write(paths: &[PathBuf]) -> Option<OwnerGuard> {
        let uri_list = uri::encode_list(paths, false);
        let gnome = uri::encode_list(paths, true);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::Builder::new()
            .name("deskoryn-clip-x11".into())
            .spawn(move || {
                let _ = own_selection(uri_list, gnome, stop_thread);
            })
            .ok()?;
        Some(OwnerGuard { stop, handle: Some(handle) })
    }

    fn own_selection(uri_list: Vec<u8>, gnome: Vec<u8>, stop: Arc<AtomicBool>) -> Option<()> {
        let (conn, screen) = x11rb::connect(None).ok()?;
        let win = hidden_window(&conn, screen)?;
        let a = atoms(&conn)?;

        conn.set_selection_owner(win, a.clipboard, CURRENT_TIME).ok()?;
        let _ = conn.flush();
        if conn.get_selection_owner(a.clipboard).ok()?.reply().ok()?.owner != win {
            return None; // someone else grabbed it first
        }

        while !stop.load(Ordering::SeqCst) {
            match conn.poll_for_event().ok()? {
                // Lost ownership (e.g. the user copied text -> arboard took over).
                Some(Event::SelectionClear(_)) => break,
                Some(Event::SelectionRequest(e)) => serve(&conn, &a, &uri_list, &gnome, &e),
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        Some(())
    }

    fn serve<C: Connection>(
        conn: &C,
        a: &Atoms,
        uri_list: &[u8],
        gnome: &[u8],
        e: &SelectionRequestEvent,
    ) {
        // Obsolete clients send property == None; reply on the target atom.
        let property = if e.property == NONE { e.target } else { e.property };

        let ok = if e.target == a.targets {
            conn.change_property32(
                PropMode::REPLACE,
                e.requestor,
                property,
                AtomEnum::ATOM,
                &[a.targets, a.uri_list, a.gnome],
            )
            .is_ok()
        } else if e.target == a.uri_list {
            conn.change_property8(PropMode::REPLACE, e.requestor, property, a.uri_list, uri_list)
                .is_ok()
        } else if e.target == a.gnome {
            conn.change_property8(PropMode::REPLACE, e.requestor, property, a.gnome, gnome)
                .is_ok()
        } else {
            false
        };

        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if ok { property } else { NONE },
        };
        let _ = conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify);
        let _ = conn.flush();
    }
}

// ---------------------------------------------------------------------------
// Windows / CF_HDROP backend.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "windows-backend", target_os = "windows"))]
mod win {
    use super::PathBuf;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows::Win32::Foundation::{HANDLE, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::{DragQueryFileW, DROPFILES, HDROP};

    /// The Win32 clipboard owns the data after `SetClipboardData`, so there is
    /// nothing to keep alive here.
    pub struct OwnerGuard;

    pub fn read() -> Option<Vec<PathBuf>> {
        unsafe {
            OpenClipboard(HWND(std::ptr::null_mut())).ok()?;
            let out = read_hdrop();
            let _ = CloseClipboard();
            out.filter(|p| !p.is_empty())
        }
    }

    unsafe fn read_hdrop() -> Option<Vec<PathBuf>> {
        let handle = GetClipboardData(CF_HDROP.0 as u32).ok()?;
        let hdrop = HDROP(handle.0);
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        let mut paths = Vec::new();
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len + 1];
            let got = DragQueryFileW(hdrop, i, Some(buf.as_mut_slice())) as usize;
            buf.truncate(got);
            paths.push(PathBuf::from(std::ffi::OsString::from_wide(&buf)));
        }
        Some(paths)
    }

    pub fn write(paths: &[PathBuf]) -> Option<OwnerGuard> {
        // DROPFILES header followed by a double-null-terminated wide path list.
        let mut wide: Vec<u16> = Vec::new();
        for p in paths {
            wide.extend(p.as_os_str().encode_wide());
            wide.push(0);
        }
        wide.push(0);

        let header = std::mem::size_of::<DROPFILES>();
        let total = header + wide.len() * std::mem::size_of::<u16>();

        unsafe {
            let hglobal = GlobalAlloc(GMEM_MOVEABLE, total).ok()?;
            let base = GlobalLock(hglobal) as *mut u8;
            if base.is_null() {
                return None;
            }
            let df = base as *mut DROPFILES;
            (*df).pFiles = header as u32;
            (*df).fWide = true.into();
            std::ptr::copy_nonoverlapping(wide.as_ptr(), base.add(header) as *mut u16, wide.len());
            let _ = GlobalUnlock(hglobal);

            if OpenClipboard(HWND(std::ptr::null_mut())).is_err() {
                return None;
            }
            let _ = EmptyClipboard();
            let ok = SetClipboardData(CF_HDROP.0 as u32, HANDLE(hglobal.0)).is_ok();
            let _ = CloseClipboard();
            ok.then_some(OwnerGuard)
        }
    }
}
