# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Deskoryn makes a **Linux PC (3 monitors)** and a **Windows PC (2 monitors)** feel
like **one workstation with five monitors** — shared cursor/keyboard, global
clipboard, file transfer, and audio forwarding — while each monitor stays
physically attached to its own machine. It is local-first (LAN only, no cloud),
peer-to-peer, and **not** a remote-desktop/VM/screen-mirroring tool.

The guiding principle that should inform most design decisions: **the user must
never have to think "Linux machine vs Windows machine."** The code models one
unified virtual desktop, not two computers.

Read `docs/ARCHITECTURE.md` first; it is the map for everything below.

## Commands

```bash
# Build / check (default build is pure-Rust and OS-portable — no native deps)
cargo check --workspace
cargo build --workspace

# Tests (the real logic — layout transitions, focus machine, framing, jitter,
# pairing SAS — is unit-tested and runs without hardware)
cargo test --workspace
cargo test -p deskoryn-core layout         # a single crate / filter

# Run the daemon end-to-end in one process over an in-memory loopback session.
# Touches no real OS input/clipboard/audio or network — best for development.
cargo run -p deskoryn-daemon -- run --dry-run

# Inspect resolved config + paths, list trusted devices
cargo run -p deskoryn-daemon -- info
cargo run -p deskoryn-daemon -- devices

# Lint / format
cargo clippy --workspace --all-targets
cargo fmt

# Real builds turn on native backends via per-OS meta-features:
cargo build -p deskoryn-daemon --features linux      # on Linux
cargo build -p deskoryn-daemon --features windows    # on Windows (or cross)
```

`RUST_LOG=debug` (or `deskoryn=trace`) controls log verbosity.

## The most important architectural idea

`deskoryn-core::layout::VirtualDesktop` places **every monitor from both
machines into one signed global coordinate space**. Nothing above this layer
reasons about "which machine" — it asks "which monitor is under this point and
who owns it" (`owner_at`, `resolve_move`). The cursor has one global position;
the machine owning the monitor under it is *active*. This file
(`crates/core/src/layout.rs`) plus the focus state machine
(`crates/daemon/src/focus.rs`) are the heart of the product. Both are pure logic
with thorough tests — change them test-first.

## Workspace layout (crates/)

| Crate | Role |
|-------|------|
| `core` | Domain model: ids, geometry, **virtual-desktop layout + transition math**, input events, config (TOML), trust store. No I/O, no async. |
| `proto` | Wire protocol: per-channel message enums + length-prefixed `postcard` framing. Depends only on `core`. |
| `net` | Secure `Session` transport, `Discovery`, and `pairing` (SAS). Trait surface + in-memory loopback by default; real QUIC/mDNS behind features. |
| `input` | `Capture`/`Injector` traits + OS backends (libei/X11/evdev, Raw Input/`SendInput`). |
| `clipboard` | `ClipboardMonitor` (text/image/file-list, delayed rendering, echo suppression). |
| `filexfer` | Manifests, chunk streaming, resume, conflict policy, progress. |
| `audio` | `Capture`→Opus→datagrams→`JitterBuffer`→`Playback`. |
| `daemon` | The `deskorynd` binary: `supervisor` (reconnect), `session` (per-peer pumps), **`focus`** state machine, `ipc` (tray/CLI). |

Dependency direction is acyclic: `core ← proto ← net`; feature crates depend on
`core`/`proto`; `daemon` depends on everything.

## Conventions specific to this repo

- **Default build must stay pure-Rust and portable.** Heavy/native dependencies
  (quinn, rustls, mdns-sd, libei/reis, pipewire, the `windows` crate, opus) are
  **optional**, behind feature flags, with stub/loopback implementations compiled
  by default. This keeps `cargo check`/`cargo test` fast and runnable on any host
  (including CI and the "other" OS). Don't add a native dep to a crate's default
  features — gate it.
- **Trait surface + backends.** Each feature crate exposes OS-neutral traits in
  `lib.rs` and concrete backends in `platform.rs` (or `cfg`/feature modules).
  Selection happens at runtime via a `detect()`/`open_*()` function. Add new OS
  support as a new backend, not by branching in the daemon.
- **Wire types live in `proto`; domain types in `core`.** Don't define wire
  messages elsewhere. The canonical keycode space on the wire is **evdev**;
  backends translate at the edge.
- **Skeleton markers.** Real implementation gaps are marked `TODO(impl)` with a
  short plan. `#![allow(dead_code)]` appears only on intentionally-ahead-of-use
  API surfaces (e.g. `daemon::ipc`, some `focus` accessors) — prefer wiring code
  over widening these allows.
- **Safety invariants to preserve when editing input/transfer code:**
  `Injector::release_all` must run on `Leave` and on any disconnect (no stuck
  keys); incoming file paths must be validated under the download root (no `..`);
  decoders honor `MAX_FRAME_LEN`.
- Run new logic through the **loopback session** (`net::transport::loopback`) in
  tests before wiring a real backend.

## Where the deeper docs are

- `docs/ARCHITECTURE.md` — system + module breakdown, focus state machine.
- `docs/PROTOCOL.md` — channel↔QUIC mapping, per-channel message flows.
- `docs/SECURITY.md` — threat model, SAS pairing, cert pinning.
- `docs/CONFIG.md` — TOML config + JSON trust store, with a full example.
- `docs/OS_PROBLEMS.md` — the hard platform issues (Wayland capture, UAC,
  loopback audio, clock drift, autostart) and the chosen solutions.
- `docs/ROADMAP.md` — MVP milestones (M0 foundation exists; M2 input is the first
  daily-usable build).
- `docs/UI.md` — tray UI + monitor-arranger mockups.
