# Deskoryn

**One workstation, five monitors, two computers.**

Deskoryn unifies a **Linux PC (3 monitors)** and a **Windows PC (2 monitors)**
into a single virtual desktop. Move one cursor across all five screens, share a
global clipboard, drag files between machines, and forward audio — while every
monitor stays physically plugged into its own computer.

It is **local-first** (LAN only, no cloud), **peer-to-peer**, and **encrypted**.
It is *not* a remote-desktop, VM, or screen-mirroring tool — it aims to feel like
*one computer with five monitors*, not a viewer onto another machine.

> Status: **early skeleton (M0).** The architecture, wire protocol, security
> model, virtual-desktop layout math, and the cursor focus state machine are
> implemented and unit-tested; the OS-native backends (input, clipboard, audio,
> QUIC, mDNS) are designed and stubbed behind feature flags. See
> [`docs/ROADMAP.md`](docs/ROADMAP.md).

## Features (target)

- **Seamless input** — one cursor across all monitors, keyboard focus follows the
  mouse, custom layout, switch/lock hotkeys.
- **Global clipboard** — text, images, and files/folders, both directions.
- **File transfer** — drag-and-drop, background service, resume, progress,
  conflict handling, optional shared folders.
- **Audio forwarding** — Windows↔Linux, Opus, low-latency or high-quality modes,
  selectable devices.
- **Secure pairing** — mDNS discovery, manual IP, 6-digit/QR verification, TLS
  1.3, remembered devices.
- **Reliable** — auto-reconnect after sleep/reboot/network loss, live layout
  changes, graceful offline.

## Quick start (developer)

```bash
cargo test --workspace                          # logic tests, no hardware needed
cargo run -p deskoryn-daemon -- run --dry-run   # full daemon over a loopback session
cargo run -p deskoryn-daemon -- info            # show resolved config + paths
```

The default build is pure-Rust and compiles on any OS. Real OS backends are
enabled per platform:

```bash
cargo build -p deskoryn-daemon --features linux     # on Linux
cargo build -p deskoryn-daemon --features windows   # on/for Windows
```

## How it works (one paragraph)

Both machines run the same background daemon, `deskorynd`. Every monitor from both
machines is placed into one global coordinate space — the *virtual desktop*. The
machine whose monitor currently holds the cursor is "active"; when the cursor
crosses an edge onto the other machine's monitor, control hands off over an
encrypted QUIC session. Clipboard, files, and audio ride the same session on
separate channels. A small tray app configures everything; the daemon does the
work.

## Documentation

| Doc | Contents |
|-----|----------|
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | System design, modules, focus state machine |
| [`docs/PROTOCOL.md`](docs/PROTOCOL.md) | QUIC channel mapping, message flows |
| [`docs/SECURITY.md`](docs/SECURITY.md) | Threat model, SAS pairing, cert pinning |
| [`docs/CONFIG.md`](docs/CONFIG.md) | Config (TOML) + trust store (JSON) with examples |
| [`docs/OS_PROBLEMS.md`](docs/OS_PROBLEMS.md) | Hard platform issues + solutions |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | MVP milestones |
| [`docs/UI.md`](docs/UI.md) | Tray UI + monitor-arranger mockups |
| [`docs/REFERENCES.md`](docs/REFERENCES.md) | Prior art studied |
| [`CLAUDE.md`](CLAUDE.md) | Orientation for the Claude Code agent |

## License

MIT OR Apache-2.0.
