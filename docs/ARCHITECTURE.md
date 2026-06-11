# Deskoryn — System Architecture

Deskoryn turns a **Linux PC (3 monitors)** and a **Windows PC (2 monitors)** into
one *virtual single workstation*. Each monitor stays physically attached to its
own machine; software makes the boundary between the two computers effectively
invisible.

---

## 1. Design philosophy

> **The user must never think in terms of "the Linux computer" and "the Windows
> computer."**

Everything in this document serves that one principle. Concretely, the system
presents four unified abstractions:

| Abstraction        | What the user perceives                                   | Backed by                         |
|--------------------|-----------------------------------------------------------|-----------------------------------|
| **Virtual desktop**| One cursor roaming a single 5-monitor desktop             | `core::layout` + `daemon::focus`  |
| **Global clipboard**| Copy anywhere, paste anywhere                            | `clipboard` crate                 |
| **Shared audio**   | Either machine's sound on either machine's speakers       | `audio` crate                     |
| **Local-feel files**| Drag/drop & paste files as if on one PC                  | `filexfer` crate                  |

### Non-goals (explicitly out of scope)

- **Not** a remote-desktop replacement (no primary screen streaming).
- **Not** a VM or emulation solution.
- **Not** a screen-mirroring tool.
- **No** requirement to physically connect all monitors to one computer.
- **No** cloud dependency — strictly local-first, LAN-only.

Optional, secondary screen streaming (à la Sunshine/Moonlight) may be added later
behind a feature flag, but it is never the main path.

---

## 2. Topology

```
        LINUX PC (deskorynd)                        WINDOWS PC (deskorynd)
   ┌───────┬───────┬───────┐                       ┌─────────┬─────────┐
   │ Lin-L │ Lin-C │ Lin-R │                       │  Win-L  │  Win-R  │
   └───────┴───────┴───────┘                       └─────────┴─────────┘
        │  capture/inject                                │ capture/inject
        ▼                                                ▼
   ┌──────────────────┐      QUIC + TLS 1.3 / mDNS  ┌──────────────────┐
   │  deskorynd        │◄────────  LAN  ───────────►│  deskorynd        │
   │  (background svc) │      one secure session    │  (background svc) │
   └──────────────────┘                             └──────────────────┘
        ▲                                                ▲
        │ local IPC (UDS)                                │ local IPC (named pipe)
   ┌──────────────────┐                             ┌──────────────────┐
   │  tray UI / CLI    │                             │  tray UI / CLI    │
   └──────────────────┘                             └──────────────────┘
```

Both machines run the **same daemon** (`deskorynd`); there is no fixed
client/server. The peer that currently owns the cursor is *active*; ownership
moves across the wire as the cursor crosses monitor edges. The relationship is a
symmetric peer-to-peer mesh keyed on a stable `DeviceId`, which generalizes
cleanly beyond two machines.

---

## 3. Process model

Per machine:

- **`deskorynd`** — a long-running background service (systemd user service on
  Linux, Windows Service on Windows). Owns *all* state and network connections.
- **Tray UI** (Tauri) and **`deskorynctl`** (CLI) — thin clients that connect to
  the daemon over a local-only IPC socket (`daemon::ipc`). They never touch the
  network or OS input APIs directly.

Keeping the daemon authoritative means the UI can crash/restart without dropping
the session, and the same control surface serves both the GUI and scripts.

---

## 4. Crate / module breakdown

The repository is a Cargo workspace. Each concern is an independent crate with a
**platform-neutral trait surface** plus **OS-specific backends** behind
`cfg`/feature gates. This is what lets the whole thing compile and unit-test on
any host while the native integrations are a feature flag away.

| Crate                | Responsibility                                                            | Key types |
|----------------------|---------------------------------------------------------------------------|-----------|
| `deskoryn-core`      | Domain model: ids, geometry, **virtual-desktop layout + transition math**, input events, config, trust store. No I/O. | `VirtualDesktop`, `Monitor`, `Transition`, `InputEvent`, `AppConfig`, `TrustStore` |
| `deskoryn-proto`     | Wire protocol: versioned messages per channel + length-prefixed framing.  | `Control`, `Input`, `Clipboard`, `FileXfer`, `AudioFrame`, `Channel` |
| `deskoryn-net`       | Secure session transport (QUIC/TLS), LAN discovery (mDNS), pairing (SAS).  | `Session`, `Discovery`, `PairingSession`, `QuicEndpoint` |
| `deskoryn-input`     | Input capture + injection. Linux (libei/X11/evdev-uinput), Windows (Raw Input/`SendInput`). | `Capture`, `Injector`, `Hotkey` |
| `deskoryn-clipboard` | Global clipboard: watch/read/write, formats, echo suppression.            | `ClipboardMonitor`, `LocalClip`, `EchoGuard` |
| `deskoryn-filexfer`  | Manifests, chunked streaming, resume, conflict handling, progress.        | `Manifest`, `FileSink`, `ProgressTracker` |
| `deskoryn-audio`     | Capture → Opus → datagrams → jitter buffer → playback.                    | `Capture`, `Playback`, `Codec`, `JitterBuffer` |
| `deskoryn-daemon`    | The `deskorynd` binary: orchestration, **focus state machine**, IPC.      | `FocusMachine`, `supervisor`, `session`, `ipc` |

Dependency direction (acyclic):

```
core  ◄── proto ◄── net
  ▲        ▲         ▲
  └──── input / clipboard / filexfer / audio ──┐
                              ▲                 │
                              └──── daemon ─────┘
```

---

## 5. The unified virtual desktop

`core::layout::VirtualDesktop` is the linchpin. All monitors from all machines
are placed as rectangles into **one signed, global coordinate space** (virtual-
desktop pixels). Higher layers never ask "which machine?" — they ask "which
monitor is under this point, and who owns it?" (`owner_at`).

The cursor has a single global position. On each motion the active daemon calls
`resolve_move(from, to)`:

- stays on the same monitor → keep moving locally;
- crosses to a monitor owned by **this** machine → move locally to the new
  monitor (handles per-machine multi-monitor layouts);
- crosses to a monitor owned by the **other** machine → return a `Transition`,
  which the `FocusMachine` turns into a hand-off.

`resolve_move` also handles mismatched resolutions/positions by projecting the
crossing so that, e.g., the vertical position carries across a left/right hop
between a 1080p row and a 1440p row. See `crates/core/src/layout.rs` (with tests).

### Focus state machine (`daemon::focus`)

Exactly one machine is `Active` at a time. The machine is pure logic (no I/O) so
it is fully unit-tested:

```
            cursor crosses to peer's monitor / switch hotkey
   ┌──────────┐ ───────────────────────────────────────────►  ┌────────┐
   │  Active  │                                                │  Idle  │
   │ (owns    │  ◄───────────────────────────────────────────  │(awaits │
   │  cursor) │            Input::Enter from peer               │ Enter) │
   └──────────┘                                                └────────┘
```

Edge resistance (hysteresis) and a *lock* hotkey prevent accidental crossings.
On hand-off the active side stops grabbing input and emits `Input::Enter{entry}`;
the peer warps its local cursor and starts injecting. Keyboard focus follows the
cursor because the active machine is by definition the one receiving keystrokes.

---

## 6. Data & control flow per feature

- **Input** (latency-critical): capture → `FocusMachine` → inject locally *or*
  forward `Input::Events` + hand off. Runs on its own reliable, ordered QUIC
  stream so a big file transfer can never add cursor lag.
- **Clipboard**: local change → `Offer` (small text inlined, else format list) →
  peer `Pull`s on paste → delayed rendering streams large payloads. File lists
  defer to the file-transfer machinery.
- **Files**: build `Manifest` → `Offer`/`Accept` → chunk stream with BLAKE3
  hashing → resume + conflict policy + progress to the tray.
- **Audio**: WASAPI loopback / PipeWire monitor → Opus → **QUIC datagrams**
  (drop-tolerant) → jitter buffer → playback. Profile picks codec + buffer depth.

See `docs/PROTOCOL.md` for the exact channel/stream mapping.

---

## 7. Reliability

- **Auto-reconnect**: a dropped session is normal; `supervisor` re-dials with
  capped exponential backoff and silently re-pairs using the pinned certificate
  (no user prompt). Survives sleep, reboot, Wi-Fi blips.
- **Liveness**: `Control::Ping`/`Pong` heartbeats detect a half-open connection
  faster than TCP/QUIC idle timeouts.
- **Stuck-key safety**: on hand-off *and* on disconnect the injector calls
  `release_all`, so a lost connection can never leave a key/button held down.
- **Layout changes**: hotplugging a monitor triggers a `Control::LayoutUpdate`;
  both daemons recompute the virtual desktop live.
- **Graceful offline**: when a peer goes away the surviving machine reclaims its
  own cursor immediately and the tray shows the disconnected state.
- **Observability**: structured `tracing` logs + an in-UI connection/status line.

---

## 8. Why these technologies

| Choice | Rationale | Prior art studied |
|--------|-----------|-------------------|
| Rust core | memory safety + predictable latency for an always-on input-path service | RustDesk |
| QUIC + TLS 1.3 | one encrypted session, multiplexed streams (no head-of-line blocking between input/clipboard/files), unreliable datagrams for audio, fast handshake & migration across network changes | QUIC, WireGuard (design ideas) |
| mDNS/DNS-SD | zero-config LAN discovery | KDE Connect |
| SAS pairing + cert pinning (TOFU) | MITM-resistant first contact with no central authority | Noise/Signal safety numbers, Bluetooth SSP |
| libei (Wayland) / evdev-uinput / X11 XTest | only sanctioned paths for capture+inject on each Linux session type | Input Leap, Deskflow |
| Raw Input + `SendInput` (Windows) | high-resolution capture + reliable injection | Barrier, Deskflow |
| Opus over datagrams | low-latency, loss-concealing audio that never stalls input | Scream, VBAN |
| PipeWire / WASAPI | native low-latency capture + virtual devices | PipeWire, Scream |
| Tauri tray UI | small, native-feeling cross-platform UI sharing the Rust stack | — |

See `docs/OS_PROBLEMS.md` for the hard platform-specific issues and how each is
handled, and `docs/REFERENCES.md` for the full prior-art list.
