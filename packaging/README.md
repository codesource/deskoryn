# Packaging & autostart

How to install `deskorynd` so it starts on login and has the permissions it needs.

## Linux

1. **Build & install the binary**
   ```bash
   cargo build --release -p deskoryn-daemon --features linux
   install -Dm755 target/release/deskorynd ~/.local/bin/deskorynd
   ```
2. **Input permissions** — install the udev rule and join the `input` group:
   see [`udev/99-deskoryn.rules`](udev/99-deskoryn.rules).
3. **Autostart** — install the user service:
   see [`systemd/deskoryn.service`](systemd/deskoryn.service).

> Optional real Opus audio: add `--features audio-opus` (needs `libopus-dev` /
> `pkg-config`).

## Windows

The Windows agent runs as a combination of:
- a **Windows Service** for the network/transfer/discovery parts, and
- a **per-user auto-start** entry (Run key or Task Scheduler "at logon") for the
  input/clipboard parts, which need the interactive desktop session.

Build (from Linux or Windows) with the MSVC target and the `windows` feature:
```bash
rustup target add x86_64-pc-windows-msvc
cargo build --release -p deskoryn-daemon --features windows --target x86_64-pc-windows-msvc
```
Packaging as an MSI (e.g. via WiX/`cargo-wix`) and the service registration are
tracked for a later iteration. See `docs/OS_PROBLEMS.md` (B2, F3) for the UAC /
secure-desktop and session caveats.

## Verifying a running daemon

```bash
deskorynd status        # queries the local control socket
deskorynd devices       # lists paired devices
```
