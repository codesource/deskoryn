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
- ✅ mDNS advertise/browse (`net::discovery::mdns`): TXT records (id/name/fp),
  resolve → `PeerHint`, auto-dial trusted peers with a single-dialer + active-set
  dedup in the supervisor. Verified live between two daemons on localhost.
- ✅ SAS pairing flow wired to `deskorynd pair` (dial or `--listen`); verified
  live (matching 6-digit code on both processes, trust persisted).
- ✅ `Control::Ping`/`Pong` heartbeat in `control::run_control` (also graceful
  `Goodbye`).

**M1 is functionally complete:** two daemons discover each other, pair once, then
reconnect automatically. No input sharing yet (that's M2).

---

## M2 — Seamless input (the core feature) ✅ (live across Linux↔Windows — first daily-usable build)

- ✅ Input pump wired (`daemon::input`): capture → `Controller` → inject/forward,
  with `release_all` on `Leave`/disconnect (no stuck keys). Tested over the
  loopback session.
- ✅ Hotkeys (switch / lock) + edge resistance in the `Controller` (unit-tested);
  focus-follows-mouse is the model (the machine under the cursor is active).
- ✅ Linux backend (`input::platform`): evdev capture + uinput injection (works
  under X11 and Wayland at the kernel level), with live modifier tracking.
  **Hardware-validated** on the 3-monitor Linux PC: `input-test` captured live
  evdev pointer events and a uinput cursor wiggle injected correctly.
- ✅ Windows backend: `WH_MOUSE_LL`/`WH_KEYBOARD_LL` capture + suppression +
  cursor-recenter + injected-event filtering; `SendInput` injection; evdev↔VK
  keymap (numpad/media/system, round-trip tested). **Validated live** on the
  Windows PC: capture, monitor detect, and `SendInput` injection all confirmed
  in a real Linux↔Windows session.
- ✅ Stopgap monitor arranger (`deskorynd arrange`): add/remove/clear/show,
  JSON import/export, relative placement, and `detect` (X11/`xrandr` —
  hardware-validated, 3 monitors; Windows `EnumDisplayMonitors` — validated).
- ✅ Live Linux↔Windows bring-up: paired, the union desktop composes across both
  machines (Linux row 0→5760, Windows placed right of it via
  `arrange detect --offset-x`), and the cursor + keyboard cross the boundary onto
  the Windows displays.
- ⬜ Tuning + polish (recenter feel, keymap gaps, reverse-direction sweep, UAC /
  secure-desktop elevation for injecting into elevated windows).
- ⬜ libei (Wayland portal) + X11/XTest capture backends; Wayland monitor
  detect; absolute-axis uinput device for exact cursor entry on handoff.

**Exit reached:** cursor and keyboard move across the machine boundary onto the
Windows displays, focus follows the mouse, `release_all` clears keys on
leave/disconnect. **First daily-usable build is live.** Remaining M2 items are
tuning/polish and the optional alt backends, not blockers.

---

> **Note:** the protocol + pump *logic* for M3/M4/M5 has already landed and is
> tested over the loopback session (ahead of M2). What remains for each is the
> OS-native backend (real clipboard / filesystem triggers / audio devices),
> which needs a graphical session and hardware to validate.

## M3 — Global clipboard

- ✅ Clipboard sync pump (`daemon::clipboard`): inline small text, delayed
  rendering (`Pull`) for other formats, echo suppression. Tested both directions.
- ⬜ OS backends — Linux: `wlr-data-control`/portal + X11 selections (`INCR`);
  Windows: Clipboard API + `WM_RENDERFORMAT`; image + file-list formats.

**Exit:** copy text/image on one machine, paste on the other, both directions.

---

## M4 — File transfer

- ✅ Transfer pump (`daemon::transfer`): manifest → accept → `Chunk` stream →
  complete, with hashing, conflict policy, resume offsets, path-traversal guard,
  progress. Tested transferring a directory tree over the session.
- ⬜ Drag-and-drop + file-clipboard paste (`CF_HDROP` ⇄ file list) triggers;
  tray progress UI; dedicated per-transfer streams for parallelism.

**Exit:** drag a folder across the boundary; it lands with names/metadata intact,
resumes after an interruption, shows progress.

---

## M5 — Audio forwarding

- ✅ Audio pipeline (`daemon::audio`): capture → codec → datagrams → jitter
  buffer (profile-sized, gap-concealing) → playback. Tested with a passthrough
  codec over the loopback session.
- ⬜ OS backends — WASAPI loopback (Windows) / PipeWire monitor + virtual sink
  (Linux); real Opus codec (`opus` feature); device pickers.

**Exit:** Windows audio plays on Linux speakers with selectable latency; optional
reverse direction.

---

## M6 — Tray UI + autostart + polish

- ✅ Local control socket (`ipc::serve`/`request`) + `deskorynd status` CLI
  client over it; UI scaffold in `ui/` describing the Tauri app's use of the
  same protocol.
- ✅ Autostart artifacts: systemd user service + udev rule (`packaging/`).
- ⬜ The Tauri GUI itself (status, monitor arranger, pairing dialog, device
  list, transfer progress) — scaffold present; needs node + webkit to build.
- ⬜ Live connection state in the status handler; Windows Service registration.

**Exit:** install → log in → it just works, configured entirely from the tray.

---

## M7 — Optional shared folders & hardening

- ✅ Shared-folder sync planning (`filexfer::sync::plan`): push/pull/conflict/
  in-sync resolution by hash + mtime, one-way or bidirectional. Unit-tested.
- ✅ Wire-decoder hardening: a stable "never panic on garbage" sweep plus a
  cargo-fuzz target (`fuzz/`).
- ✅ Packaging/autostart scaffolds (`packaging/`).
- ⬜ Sync *execution* wired to the transfer pump; resource limits; `.deb`/MSI
  packaging + signing.

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
