# Deskoryn — MVP Roadmap

Phased plan that front-loads the hardest, most differentiating piece (seamless
input across the machine boundary) and reaches a daily-usable tool fast, then
layers the remaining features.

Each milestone is shippable and independently testable. The workspace already
contains the M0 foundation (types, protocol, focus state machine, loopback,
tests, `--dry-run`).

---

## M0 — Foundation ✅ (in this skeleton)

- Cargo workspace, crate boundaries, CI-friendly default build.
- `core`: virtual-desktop layout + transition math (tested), config, trust store.
- `proto`: message schema + framing (tested).
- `net`: `Session`/`Discovery`/pairing trait surface + in-memory loopback.
- `daemon`: focus state machine (tested), handshake, `--dry-run` end-to-end.

**Exit:** `cargo test` green; `deskorynd run --dry-run` performs a full handshake
and focus hand-off in-process.

---

## M1 — Real secure transport + discovery 🚧 (in progress)

- ✅ `net::quic` (quinn + rustls): mutual-TLS endpoint, per-channel streams,
  audio datagrams, cert-pinning verifier (`PinVerifier`), self-signed identity
  generation/persistence (`DeviceIdentity`). Covered by an integration test that
  mutually authenticates two endpoints on localhost, exchanges a framed `Hello`,
  passes an audio datagram, and rejects an unpaired peer
  (`cargo test -p deskoryn-net --features quic`).
- ✅ `supervisor` real path: bind endpoint, accept loop, per-peer dial loop with
  capped-backoff auto-reconnect (`--features linux`/`windows`).
- ⬜ mDNS advertise/browse (currently: static peers + remembered `last_address`).
- ⬜ SAS pairing flow wired to a CLI/tray prompt (`deskorynd pair`) — the crypto
  (`net::pairing`) and trust store exist; the interactive accept path is pending.
- ⬜ `Control::Ping`/`Pong` heartbeat task in `session`.

**Exit:** two daemons on the LAN pair once, then reconnect automatically across a
network blip. No input yet — verified with the handshake + an echo channel.

---

## M2 — Seamless input (the core feature)

- Linux backend: libei (Wayland) with evdev/uinput fallback; X11 via XTest.
- Windows backend: Raw Input capture + low-level hooks for suppression;
  `SendInput` injection.
- Wire the input pump: capture → `FocusMachine` → inject/forward.
- Visual layout arranger in a stopgap CLI/JSON until the GUI lands.
- Hotkeys (switch / lock), focus-follows-mouse, edge resistance.

**Exit:** cursor and keyboard move seamlessly across all 5 monitors; keyboard
focus follows the mouse; no stuck keys on disconnect. **This is the first
daily-usable build.**

---

## M3 — Global clipboard

- Text + image sync with delayed rendering and echo suppression.
- Linux: `wlr-data-control`/portal + X11 selections (`INCR`). Windows: Clipboard
  API + `WM_RENDERFORMAT`.

**Exit:** copy text/image on one machine, paste on the other, both directions.

---

## M4 — File transfer

- Drag-and-drop between machines; file-clipboard paste (`CF_HDROP` ⇄ file list).
- Background transfer service: manifests, chunk streams, resume, conflict policy.
- Tray progress for large transfers.

**Exit:** drag a folder across the boundary; it lands with names/metadata intact,
resumes after an interruption, shows progress.

---

## M5 — Audio forwarding

- WASAPI loopback (Windows) / PipeWire monitor + virtual sink (Linux).
- Opus encode/decode (`opus` feature), datagram transport, jitter buffer.
- Profile switch (low-latency vs high-quality); source/sink device pickers.

**Exit:** Windows audio plays on Linux speakers with selectable latency; optional
reverse direction.

---

## M6 — Tray UI + autostart + polish

- Tauri tray app: status, monitor arranger, pairing dialog, device list,
  per-feature toggles, transfer progress (see `docs/UI.md`).
- Autostart: systemd user service (Linux), Windows Service + auto-login start.
- Reconnect/offline UX, structured logs, crash recovery.

**Exit:** install → log in → it just works, configured entirely from the tray.

---

## M7 — Optional shared folders & hardening

- Two-way folder sync (Syncthing-style block reuse) with conflict handling.
- Resource limits, fuzzing of the wire decoder, packaging (`.deb`/MSI), signing.

---

## Post-MVP / optional

- Secondary **screen streaming** (Sunshine/Moonlight-style) behind a flag — never
  the primary experience.
- N-machine (>2) meshes (the model already generalizes).
- WAN mode with an optional relay / NAT traversal (WebRTC transport).

---

## Cross-cutting throughout

- Keep the default `cargo check`/`cargo test` pure-Rust and OS-portable.
- Every wire-facing decoder gets bounds checks + a fuzz target.
- Each milestone adds integration tests over the loopback session before the real
  backends, so logic regressions are caught without hardware.
