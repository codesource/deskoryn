# Deskoryn — Prior Art & References

Projects and specs studied while designing Deskoryn, and what we take from each.

## Input sharing (software KVM)
- **Deskflow** — monitor topology, cursor edge-transition, keyboard/mouse
  forwarding. Direct model for `core::layout` + `daemon::focus`.
- **Input Leap** — modern open-source software-KVM; reference for cross-session
  Linux handling and the move toward libei.
- **Barrier** — network protocol shape and cross-platform input sharing patterns.

## Audio streaming
- **Scream** — low-latency network audio from Windows; informs the WASAPI
  loopback + send-PCM approach.
- **PipeWire** — Linux audio routing, monitor sources, and virtual devices.
- **VBAN** — audio-over-IP framing concepts.

## Clipboard synchronization
- **KDE Connect** — cross-platform clipboard sync + LAN device pairing UX.
- **Syncthing** — robust file-sync architecture (block model, conflict handling),
  scaled down for `filexfer` and shared folders.
- **RDP clipboard redirection** — delayed rendering and file/image clipboard
  transfer; the model for `proto::Clipboard`.

## Remote display (optional, post-MVP)
- **Sunshine** — efficient screen capture/streaming (host side).
- **Moonlight** — low-latency client.
- **RustDesk** — clipboard/file-transfer/remote-control architecture in Rust.
- **Parsec** — low-latency desktop streaming concepts.

## Networking
- **Tailscale** — peer discovery and (future) NAT-traversal concepts.
- **QUIC (RFC 9000) + datagrams (RFC 9221)** — the chosen low-latency transport.
- **WebRTC DataChannels** — considered as an optional/alternative transport.

## Security
- **WireGuard** — minimalist key/identity design inspiration.
- **Noise Protocol Framework** — alternative to TLS for the secure channel (we
  chose TLS 1.3 via QUIC; Noise remains a viable swap).
- **Mutual TLS** — the implemented authentication model, with cert pinning.
- **Bluetooth SSP / Signal safety numbers** — the SAS pairing UX.

## Platform APIs
- **Windows:** WASAPI, `SendInput`, Raw Input, Clipboard API, Windows Service
  framework, low-level hooks (`WH_*_LL`).
- **Linux:** PipeWire, libei (`reis`), uinput/evdev, Wayland protocols
  (`wlr-data-control`, input/remote-desktop portals), X11 XTest/XInput2/RandR.

## Rust crates (intended real dependencies)
- `quinn` (QUIC), `rustls` + `rcgen` (TLS / cert gen), `mdns-sd` (discovery).
- `reis` (libei), `evdev` (Linux input), `windows` (Win32 bindings).
- `audiopus`/`opus` (codec), `pipewire` (Linux audio).
- `postcard` (wire encoding), `blake3` (hashing), `tokio` (async runtime),
  `tauri` (tray UI).
