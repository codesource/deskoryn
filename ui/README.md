# Deskoryn tray UI (iced)

A thin desktop UI that controls the local `deskorynd` daemon. It holds **no**
state and does **no** networking itself — it talks to the daemon's local control
channel and speaks the JSON request/response protocol defined in
[`crates/daemon/src/ipc.rs`](../crates/daemon/src/ipc.rs) (`UiRequest` / `UiEvent`).

> Built with **[iced](https://iced.rs)** — pure Rust, **self-contained binary**,
> **no system webview** (no WebView2/webkit) and no node. This is its own
> workspace root so iced/winit/wgpu stay out of the portable `deskoryn` workspace.

## Why iced (not Tauri)

A community, cross-platform tool shouldn't force an external runtime on users.
Tauri renders in the OS webview (WebView2 on Windows, webkit2gtk on Linux), which
must be installed/present. iced renders itself (wgpu, with a tiny-skia software
fallback) and links only to OS graphics/windowing that's always there — so the
binary just runs. MIT-licensed, matching the repo's `MIT OR Apache-2.0`.

## How it talks to the daemon

```
 iced UI  ──ipc::request──►  deskorynd          (Unix socket / Windows named pipe)
   UiRequest::Status ──────►   ipc::serve(handler)
                     ◄──────   UiEvent::Status { device_name, peers, active }
   UiRequest::SetFeature       UiEvent::Notice {..}
   UiRequest::Forget           ...
```

The same socket path as the daemon (`<state_dir>/deskorynd.sock`, overridable
with `DESKORYN_STATE_DIR`); on Windows it maps to `\\.\pipe\deskorynd.sock`.
Status is polled every 2 s (the channel is request/response, no push).

The UI can also **launch/stop the daemon itself** ([`src/daemon.rs`](src/daemon.rs)):
Server (`run`) vs Client (`run --connect <addr>`) role, with the `deskorynd`
binary auto-resolved (env → next to the UI binary → dev `target/` → `PATH`) and a
persisted override.

## Building & running

```bash
cd ui
cargo run                                          # Linux (native)
cargo build --release                              # Linux release
cargo build --release --target x86_64-pc-windows-gnu   # Windows cross (compiles clean)
```

No extra system packages beyond a desktop's usual graphics/windowing libs.

## Status (this increment)

Done and runnable on Linux (cross-compiles to Windows):

- Sidebar nav + connection indicator; **Status**, **Connection** (daemon
  start/stop + role + binary path), **Devices** (list + forget), **Settings**
  (feature toggles), **Transfers** (live list). Status/lifecycle polled on a timer.

Follow-ups (ported from the old Tauri scaffold, not yet in iced):

- **Monitor arranger** — the draggable canvas (an iced `canvas` widget).
- **Pairing SAS flow** — needs an iced subscription bridging the `deskorynd pair`
  subprocess's stdout/stdin (the daemon ignores `Pair` over the socket by design;
  see [`docs/UI.md`] and the daemon ipc notes).
- **Tray icon + close-to-tray** — via the `tray-icon` crate, integrated with the
  winit event loop (cross-platform tray is the fiddly part).
- Live layout read-back + push events (daemon-side IPC gaps).
