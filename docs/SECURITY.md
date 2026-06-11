# Deskoryn — Security Model

Deskoryn forwards keystrokes, clipboard contents, files, and audio between two
computers. That link is a high-value target: anyone who can inject input owns
both machines. The security model is therefore strict, even though traffic stays
on the LAN.

---

## 1. Threat model

**In scope**

- A passive eavesdropper on the LAN (sniffing Wi-Fi / a mirrored switch port).
- An active man-in-the-middle during discovery/pairing (ARP/DNS/mDNS spoofing).
- A rogue host impersonating a trusted peer to inject input or exfiltrate the
  clipboard/files.
- A malicious/buggy *paired* peer sending malformed messages or hostile paths.

**Out of scope (documented assumptions)**

- A fully compromised endpoint (kernel-level malware on either machine). If the
  OS is owned, Deskoryn cannot help.
- Physical access to an unlocked machine.
- WAN/NAT traversal threats — Deskoryn is LAN-only by default; no relay servers.

---

## 2. Transport security

- **QUIC with TLS 1.3** for confidentiality, integrity, and forward secrecy on
  every byte (input, clipboard, files, audio).
- **Mutual authentication**: each device holds a self-signed certificate bound to
  its `DeviceId`, generated on first run and stored under the state dir
  (`device.key` / `device.crt`, private key `0600`). Both ends present a cert in
  the handshake.
- No reliance on the network being trusted: even on a hostile LAN, traffic is
  unreadable and unmodifiable, and an unknown cert is rejected.

---

## 3. Pairing — trust on first use with verification

There is no CA and no cloud. First contact is secured with a **short
authentication string (SAS)**, the same idea as Bluetooth Secure Simple Pairing
and Signal safety numbers:

1. Discover the peer (mDNS) or enter `host:port` manually.
2. Complete the TLS 1.3 handshake (each side presents its self-signed cert).
3. Each side computes a 6-digit SAS deterministically from **both** certificate
   fingerprints (order-independent) **and** the unique TLS channel-binding /
   exporter secret — see `net::pairing::ShortAuthString::derive`.
4. The user confirms the same 6 digits appear on **both** screens (also offered
   as a QR code / emoji set). A man-in-the-middle terminates two *different* TLS
   sessions, so its channel bindings differ and the codes cannot match.
5. On confirmation, each side pins the peer's `(DeviceId, cert fingerprint)` into
   the trust store (`trusted.json`).

After pairing, reconnection is **silent**: the pinned fingerprint is checked
during TLS and no user interaction occurs. Changing a device's certificate (e.g.
reinstall) requires re-pairing — surfaced clearly, never auto-trusted.

### Why SAS over a typed PIN

A user-typed PIN must feed a PAKE (e.g. SPAKE2/CPace) to be MITM-resistant.
SAS-over-an-already-authenticated-channel achieves the same guarantee with a
*compare two numbers* UX and no password entropy concerns. (A PAKE-based "type
the code shown on the other screen" flow is a drop-in alternative if a one-way
display is ever required; the trust-store interface is unchanged.)

---

## 4. Authorization & remembered devices

- The trust store (`core::trust::TrustStore`) is the single source of truth for
  who may connect. `verify(id, fingerprint)` gates every inbound session.
- Listed in the tray UI; the user can **forget** a device (removes the pin →
  forces re-pairing) at any time.
- `last_address` is a convenience hint for faster reconnect and is never used as
  an authentication factor.

---

## 5. Hardening against a paired-but-hostile peer

Pairing authenticates *who*, not *what they send*. Defenses:

- **Framing bounds**: `MAX_FRAME_LEN` rejects oversized frames before allocation.
- **Path traversal**: incoming file paths are normalized and validated to stay
  under the download root; `..` escapes are rejected (`filexfer::resolve_conflict`).
- **Hash validation**: received files are BLAKE3-checked against the manifest.
- **Input sanity**: injected key/button codes are range-checked at the backend
  edge; modifiers are re-synced on handoff so a malicious "key down, never up"
  is bounded by `release_all`.
- **Resource caps**: per-session limits on concurrent transfers and clipboard
  payload sizes; audio uses bounded datagram queues.

---

## 6. Local IPC

The daemon↔UI channel is local-only: a Unix domain socket (mode `0600`) on Linux
and a named pipe with a restrictive DACL on Windows. It is **not** exposed on the
network. Sensitive operations (pairing confirm, forget device) flow over it but
originate from the locally-authenticated desktop session.

---

## 7. Key & secret handling

| Secret                 | Location                         | Protection |
|------------------------|----------------------------------|------------|
| Device private key     | `<state>/device.key`             | `0600` (Linux) / per-user ACL (Windows) |
| Device certificate     | `<state>/device.crt`             | world-readable (public) |
| Trust store            | `<state>/trusted.json`           | `0600` |
| Session keys           | in-memory only (TLS 1.3)         | forward-secret, never persisted |

No secrets are written to logs; `DeviceId`/fingerprint `Debug` impls print only
short prefixes.

---

## 8. Summary of guarantees

- Eavesdropper: sees only encrypted QUIC. ✔
- MITM at pairing: defeated by the SAS comparison. ✔
- Imposter post-pairing: rejected by certificate pinning. ✔
- Hostile paired peer: contained by framing/path/hash/input validation. ✔
- Compromised endpoint: **out of scope** (documented).
