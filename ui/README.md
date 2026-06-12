# Deskoryn tray UI (Tauri)

A thin tray application that controls the local `deskorynd` daemon. It holds **no**
state and does **no** networking itself — it connects to the daemon's local
control socket and speaks the JSON request/response protocol defined in
[`crates/daemon/src/ipc.rs`](../crates/daemon/src/ipc.rs) (`UiRequest` / `UiEvent`).

> Status: **scaffold.** The protocol and the daemon-side server are implemented
> and tested (`deskorynd status` is a working CLI client over the same socket);
> this directory is the structure for the Tauri GUI, which needs `node` +
> `webkit2gtk` (Linux) / WebView2 (Windows) to build and is therefore not part of
> the Rust workspace's `cargo build`.

## How it talks to the daemon

```
 Tray UI (webview)  ──IPC──►  deskorynd            (Unix socket / named pipe)
   UiRequest::Status ──────►   ipc::serve(handler)
                     ◄──────   UiEvent::Status { device_name, peers, active }
   UiRequest::Pair { addr }    UiEvent::PairingPrompt { sas }
   UiRequest::SetLayout {..}    UiEvent::TransferProgress {..}
   ...                          UiEvent::Notice {..}
```

The same vocabulary backs both this GUI and the `deskorynd status` CLI, so the
daemon is the single source of truth and can be driven headless.

## Screens

See [`../docs/UI.md`](../docs/UI.md) for the mockups: tray menu, the monitor
arranger (the signature screen), the SAS pairing dialog, devices, transfers, and
settings.

## Building (once the toolchain is present)

```bash
cd ui
npm install
npm run tauri dev      # or: npm run tauri build
```

The Tauri Rust shell (`src-tauri/`) connects to `deskorynd`'s socket via the same
length-prefixed JSON framing as `ipc::request`.
