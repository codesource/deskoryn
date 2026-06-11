# Deskoryn — Network Protocol Design

Transport-level design for the secure peer-to-peer link. Message *types* live in
`deskoryn-proto` (`crates/proto/src/message.rs`); this document explains how they
map onto the transport and the rules of each exchange.

---

## 1. Transport: QUIC + TLS 1.3

One **QUIC connection** per peer carries everything, chosen for:

- **Stream multiplexing without head-of-line blocking** — input, clipboard, and
  file bytes ride independent streams; a stalled 4 GB file transfer never delays
  a mouse move.
- **Unreliable datagrams** for audio (`RFC 9221`) — a lost Opus packet is
  concealed, never retransmitted.
- **TLS 1.3 built in** — mutual authentication and encryption with one handshake.
- **Connection migration** — survives the client IP/port changing (Wi-Fi↔Ethernet,
  DHCP lease change) without a full reconnect.

> Alternative considered: WebRTC DataChannels (also QUIC/DTLS-SCTP under the
> hood). Rejected as the primary transport — heavier stack, designed around
> browser/NAT traversal we don't need on a LAN — but kept as a possible optional
> transport for future WAN use.

### Channel → transport mapping

| Logical channel (`proto::Channel`) | QUIC mapping                         | Reliability        |
|------------------------------------|--------------------------------------|--------------------|
| `Control`                          | 1 bidirectional stream               | reliable, ordered  |
| `Input`                            | 1 bidirectional stream               | reliable, ordered  |
| `Clipboard`                        | 1 bidi stream + on-demand uni streams| reliable, ordered  |
| `FileXfer`                         | 1 uni stream **per transfer**        | reliable, ordered  |
| `Audio`                            | QUIC **datagrams**                   | unreliable         |

Input gets its own stream (not shared with Control) so heartbeat/layout chatter
never sits ahead of a keystroke in the same ordered stream.

### Framing

Reliable streams carry length-prefixed messages: `[u32 big-endian length][postcard body]`
(`proto::framing`). `postcard` is compact and schema-is-the-Rust-types. Datagrams
are self-delimiting, so a single `AudioFrame` is encoded directly with no prefix.

Max frame size is capped (`MAX_FRAME_LEN`, 16 MiB) to bound memory against a
hostile/buggy peer; payloads larger than that always stream as chunks.

---

## 2. Session lifecycle

```
  TCP/UDP? no — QUIC/UDP
  1. Discover (mDNS) or static host:port
  2. QUIC + mutual TLS handshake (cert pinned per trust store)
  3. Control: Hello  ⇄  Hello        (version check, exchange monitors + caps)
  4. (first time only) SAS pairing — see docs/SECURITY.md
  5. Steady state: Input / Clipboard / FileXfer / Audio as needed
  6. Heartbeat: Ping ⇄ Pong every ~1s
  7. Goodbye on clean shutdown, else timeout → reconnect with backoff
```

### Handshake (`Control::Hello`)

Both peers send `Hello { version, device, name, monitors, capabilities }`
immediately after the secure channel is up. Rules:

- **Version**: connection is refused if `version.major` differs (the schema is
  not forward-compatible across majors).
- **Monitors**: each peer contributes its monitors already placed in virtual
  space; the union forms the combined `VirtualDesktop`. Conflicts (overlapping
  rects) are resolved by the saved layout / the UI arranger.
- **Capabilities**: features are the *intersection* of both peers' advertised
  capabilities, so an older/limited peer degrades gracefully.

---

## 3. Channel protocols

### 3.1 Input (`proto::Input`)

| Message               | Direction        | Meaning |
|-----------------------|------------------|---------|
| `Enter { entry, mods }` | active → peer  | "You now own the cursor; warp to `entry`, sync modifiers, start injecting." |
| `Leave`               | active → peer    | "I'm taking the cursor back." |
| `Events { seq, events }` | active → peer | A batch of `InputEvent`s to inject. |
| `Ack { seq }`         | peer → active    | Highest processed seq (loss/RTT stats). |

- Pointer motion is sent as **relative** deltas during normal movement (immune to
  scaling rounding), and as an absolute `PointerPosition` on `Enter` and
  periodically to correct drift.
- Events are **batched** (flushed every event or every few ms) to amortize
  framing while keeping latency minimal.
- Key events carry the full `Modifiers` snapshot so a handoff mid-chord stays
  consistent.

### 3.2 Clipboard (`proto::Clipboard`)

Delayed-rendering model (like RDP clipboard redirection):

```
A copies → A: Offer{seq, formats, inline?}  ───────────►  B caches the offer
B pastes, needs PNG → B: Pull{seq, Png, tag} ───────────►  A renders on demand
                       A: DataStream{tag, Png, len}  or  Data{tag, payload}
                       (bytes follow on a dedicated stream if large)
```

- Small UTF-8 text is **inlined** in the `Offer` (≤ `clipboard.inline_max_bytes`,
  default 256 KiB) so the common case is one round trip.
- `FileList` offers carry only metadata; the actual files move via §3.3 when the
  user pastes into a file manager.
- **Echo suppression**: each write records the originating `(device, seq)`;
  the resulting local change is recognized and *not* re-offered, breaking the
  A→B→A loop.

### 3.3 File transfer (`proto::FileXfer`)

```
Sender: Offer{tag, manifest}  ──────────────►  Receiver
Receiver: Accept{tag, resume:[{file_index, offset}]}  (or Reject)
Sender: <open uni stream tagged `tag`>
        chunks: [u32 file_index][u64 offset][u32 len][bytes] …
        Progress{tag, file_index, bytes_done}  (heartbeat for the UI)
Sender: Complete{tag}        — all files delivered
either: Cancel{tag, reason}  — abort
```

- **Manifest** lists files/folders with size, mtime, POSIX mode, and (lazily) a
  BLAKE3 hash. Directories are listed so empty folders survive.
- **Resume**: the receiver may request a per-file `offset`; the sender seeks and
  continues. The receiver hashes as it writes and validates against the manifest
  hash on `finish`.
- **Conflicts**: applied by the receiver per `ConflictPolicy`
  (`rename`/`overwrite`/`skip`/`ask`); see `filexfer::resolve_conflict`. Peer-
  supplied relative paths are validated to stay under the download root (no `..`
  traversal).
- **Progress**: derived from bytes received and the `Progress` heartbeats →
  overall fraction + throughput + ETA (`ProgressTracker`).

### 3.4 Audio (`proto::AudioControl` + `AudioFrame`)

```
Source: AudioControl::Start{tag, profile, sample_rate, channels, frame_us}  (Control-side)
Source: datagram( AudioFrame{tag, seq, opus} )  ×N   (Audio datagram channel)
Source: AudioControl::Stop{tag}
```

- Each `AudioFrame` is one Opus packet in a single datagram (no fragmentation —
  Opus packets fit comfortably under the QUIC datagram MTU).
- The receiver feeds frames into a `JitterBuffer` keyed by `seq`; gaps are filled
  by Opus PLC (`conceal`). Buffer depth comes from the `AudioProfile`
  (low-latency vs high-quality).
- Bidirectional audio is two independent one-way streams with distinct `tag`s.

---

## 4. Heartbeat, timeouts, reconnect

- `Control::Ping{nonce}` every ~1 s; peer echoes `Pong{nonce}`. RTT feeds the UI
  latency indicator.
- Missing N consecutive pongs (default 5 s) → declare the session dead, tear
  down, and let `supervisor` reconnect with backoff (500 ms → 30 s cap).
- On reconnect the pinned certificate is checked silently; no user interaction.

---

## 5. Versioning & extensibility

- `PROTOCOL_VERSION { major, minor }`. Major bumps are breaking and refused
  cross-major. Minor bumps add optional messages/fields (postcard tolerates added
  enum variants only at the tail — additions must append).
- New capabilities are negotiated through the `Capabilities` struct, never
  assumed.
