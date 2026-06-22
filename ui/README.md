# Deskoryn tray UI (Tauri)

A thin tray application that controls the local `deskorynd` daemon. It holds **no**
state and does **no** networking itself — it connects to the daemon's local
control socket and speaks the JSON request/response protocol defined in
[`crates/daemon/src/ipc.rs`](../crates/daemon/src/ipc.rs) (`UiRequest` / `UiEvent`).

> Status: **implemented scaffold.** The Tauri v2 app is complete — Rust shell
> ([`src-tauri/`](src-tauri/)) and a vanilla-JS frontend ([`src/`](src/)) with all
> six screens (Status, Monitor arranger, Devices, Transfers, Settings, plus the
> SAS pairing dialog). It is **not** part of the Rust workspace's `cargo build`:
> it pulls in Tauri + webkit2gtk (Linux) / WebView2 (Windows), kept out of the
> portable default build (it is its own workspace root, also `exclude`d in the
> root manifest).
>
> Verified: the frontend builds (`npm run build` → `dist/`); the Rust shell's
> manifest and dependency graph resolve and compile up to the webkit link step
> (`cargo check` fails only on the missing `javascriptcoregtk-4.1` /
> `libsoup-3.0` system libraries — see prerequisites below).

## How it talks to the daemon

```
 Tray UI (webview)  ──invoke──►  src-tauri ──IPC──►  deskorynd   (Unix socket / named pipe)
   api.status()      daemon_status     UiRequest::Status ─►   ipc::serve(handler)
                                                        ◄──   UiEvent::Status {..}
   api.pair(addr)    daemon_pair       UiRequest::Pair        UiEvent::PairingPrompt { sas }
   api.setLayout(..) daemon_set_layout UiRequest::SetLayout   UiEvent::Notice {..}
   ...
```

The frontend calls Tauri commands in [`src-tauri/src/lib.rs`](src-tauri/src/lib.rs);
each forwards one `UiRequest` over the daemon's control socket
([`src-tauri/src/ipc.rs`](src-tauri/src/ipc.rs), length-prefixed JSON, mirroring
the daemon's vocabulary) and returns the `UiEvent`s to the webview. The same
vocabulary backs `deskorynd status`, so the daemon is the single source of truth.

The socket path mirrors the daemon's `Paths::socket_file`
(`<state_dir>/deskorynd.sock`); set `DESKORYN_STATE_DIR` to point the UI at a
non-standard install.

## Tray behaviour

- **Left-click** the tray icon → menu: **Open Deskoryn** / **Quit**.
- **Right-click** does nothing (the menu is bound to left-click only).
- **Closing the window hides it to the tray** (it does not quit). Quitting is the
  explicit tray-menu action. See `on_window_event` / the tray setup in
  [`src-tauri/src/lib.rs`](src-tauri/src/lib.rs).

## Screens

See [`../docs/UI.md`](../docs/UI.md) for the mockups. The monitor arranger is the
one bespoke component: draggable, edge-snapping monitor tiles in one shared
space, serialized to a `VirtualDesktop` and pushed via `SetLayout`.

The **Connection** screen launches and manages the daemon itself, so the user
never needs a terminal:

- **Daemon lifecycle** — Start / Stop `deskorynd run`. The Rust shell
  ([`src-tauri/src/daemon.rs`](src-tauri/src/daemon.rs)) owns the child process,
  streams its stdout/stderr to the UI as `daemon-log` events, and reports
  liveness. **Server** role runs plain `run` (listen + mDNS discovery + dial
  remembered peers); **Client** role runs `run --connect <addr>` to also
  proactively dial a known address.
- **Pairing** — spawns `deskorynd pair --listen` (server, waits) or
  `deskorynd pair <addr>` (client, dials), parses the 6-digit SAS from the
  process's stdout into the confirmation dialog, and writes the user's
  yes/no back to its stdin. Pairing binds the listen port, so the UI requires
  the daemon to be stopped first.
- **Binary resolution** — auto-resolved in order: `DESKORYN_BIN` env → next to
  the UI binary → dev `target/{debug,release}/deskorynd` → `PATH`; with a user
  override that persists in the UI config dir (`ui.json`).

The daemon's new `run --connect <HOST:PORT>` flag (repeatable) is what the
Client role uses; a plain `run` stays the symmetric peer-to-peer role.

## Known protocol gaps (need daemon-side additions)

These are noted inline in the screens and tracked here so the GUI and daemon stay
in step:

1. **Live layout read-back.** `Status` reports peer *names* but neither device
   ids nor the current `VirtualDesktop`, so the arranger edits a working model
   (seeded from the bring-up rig) rather than the live layout, and pushes with
   placeholder device ids. Needs `UiRequest::Layout` → `UiEvent::Layout { desktop }`.
2. **Push events.** The control socket is one-request/one-response, so
   `TransferProgress` isn't streamed continuously. A subscription/streaming
   request would light up live transfer progress. (Pairing does **not** rely on
   this — the running daemon's IPC handler ignores `Pair`/`PairConfirm`, so the
   Connection screen drives a separate `deskorynd pair` subprocess instead, with
   the SAS surfaced over the shell's `pair-sas`/`pair-result` events.)
3. **Live feature state.** `Status` doesn't report which features are enabled, so
   the toggles are write-through (`SetFeature`) defaulting to the daemon's
   defaults.
4. **Config get/set** over IPC (Settings groups beyond feature toggles are
   read-only descriptions of `config.toml` today).
5. **Windows named-pipe transport.** The client has the named-pipe path; the
   daemon side is still `#[cfg(unix)]` only (TODO in `daemon::ipc`).

## Building

Prerequisites: `node`/`npm` (frontend) and the platform webview toolchain.

```bash
# Linux (Debian/Ubuntu) — the system libs the cargo check above is missing:
sudo apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev libgtk-3-dev \
     librsvg2-dev libayatana-appindicator3-dev build-essential

cd ui
npm install
npm run tauri dev      # dev server + native shell
npm run tauri build    # bundled app
```

On Windows the shell uses the bundled WebView2 runtime; on macOS, the system
WKWebView (no extra libs).
