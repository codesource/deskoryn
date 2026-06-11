# Deskoryn — Hard OS-Specific Problems & Proposed Solutions

The architecture is the easy part; these platform realities are where software
KVMs actually live or die. Each entry states the problem, why it's hard, and the
chosen approach (with fallbacks).

---

## A. Linux input capture & injection

### A1. Wayland forbids global input capture/injection by design
**Problem.** Unlike X11, a Wayland client cannot read other windows' input or
synthesize global events — that's the whole security model. Classic KVMs
(Synergy/Barrier) only worked on X11.

**Solution.** Use **libei** + the XDG desktop portals:
- `org.freedesktop.portal.InputCapture` to grab pointer/keyboard at screen edges
  (the compositor mediates and shows the user they granted it).
- `org.freedesktop.portal.RemoteDesktop` + libei **emulation** to inject.
This is the only sanctioned path and is supported by modern GNOME/KDE/wlroots.
The `reis` crate provides Rust libei bindings.

**Fallbacks.** If portals/libei are unavailable: **evdev + uinput** (read
`/dev/input/event*`, write a virtual device via `/dev/uinput`). Works under any
session but needs device permissions (A2). On X11 sessions, **XInput2 + XTest**.

`input::platform::detect()` picks: libei → X11 → evdev, in that order.

### A2. uinput/evdev permissions
**Problem.** `/dev/uinput` and `/dev/input/*` are root-only by default.

**Solution.** Ship a udev rule granting the `input` group access and add the user
at install time (documented, opt-in). Prefer the portal path which needs no such
privilege. Never run the daemon as root.

### A3. Wayland gives no absolute pointer warp
**Problem.** You can't "set the cursor to (x,y)" globally on Wayland.

**Solution.** Drive the pointer with **relative** motion (libei supports it), and
treat `Enter` as "begin at this edge" rather than an absolute warp. The
`FocusMachine` already prefers relative deltas; absolute positions are advisory
and used for drift correction on backends that support them (X11/Windows).

### A4. Per-display fractional scaling
**Problem.** Mixed DPI/scale means virtual pixels ≠ device pixels.

**Solution.** Store `scale_pct` per monitor; map virtual↔native at the injection
edge. Relative motion sidesteps most rounding; absolute warps are scaled.

---

## B. Windows input

### B1. Suppressing local delivery while the cursor is "away"
**Problem.** When the cursor has crossed to Linux, the Windows machine must still
*capture* keyboard/mouse but **not** deliver them to local apps.

**Solution.** Install low-level hooks (`WH_MOUSE_LL`, `WH_KEYBOARD_LL`) and return
non-zero to swallow events while grabbed; use **Raw Input** (`WM_INPUT`) for
high-resolution relative mouse data. Combine: Raw Input for fidelity, hooks for
suppression.

### B2. UAC / secure desktop / elevated windows
**Problem.** `SendInput` can't inject into elevated processes or the secure
desktop (UAC prompt, lock screen, Ctrl+Alt+Del) unless the injector is at least
as privileged.

**Solution.** Run the input portion as a service with appropriate rights for the
common case; **accept** that the UAC secure desktop and the logon screen are
out of reach (documented limitation — by OS design). Surface a brief "controlled
locally" indicator when focus can't be forwarded.

### B3. Injection timing / `SendInput` coalescing
**Problem.** Bursty injection can be dropped or reordered.

**Solution.** Inject with scancodes (not VKs) for layout-independence, pace via
the event batch cadence, and always issue matching key-up on `Leave`/disconnect
(`release_all`).

---

## C. Keyboard semantics across OSes

### C1. Keycode spaces differ (evdev vs VK/scancode)
**Problem.** A canonical wire keycode is needed so a Linux keypress lands right in
Windows.

**Solution.** Standardize the wire on the **evdev** code space
(`core::input::KeyCode`); each backend maps to/from its native space at the edge
(evdev native on Linux; VK+scancode tables on Windows).

### C2. Modifier mismatch (Super vs Windows vs Meta)
**Problem.** The "Meta/Super/Windows/Command" key and Alt/AltGr behave differently
per OS and layout.

**Solution.** Carry an explicit `Modifiers` snapshot with key events and re-sync
on every handoff so chords can't desync; map `META` to Super on Linux / Win key
on Windows. Offer an optional Alt/Meta swap in settings.

---

## D. Clipboard

### D1. Wayland clipboard access without a focused window
**Problem.** Wayland restricts clipboard reads to the focused client.

**Solution.** Use the **`wlr-data-control`** protocol (wlroots) or the clipboard
portal, which exist precisely for clipboard managers/sync daemons. X11: own a
`CLIPBOARD` selection and use `INCR` for large payloads.

### D2. Large payloads & delayed rendering
**Problem.** Eagerly serializing a 50 MB image on every copy is wasteful and laggy.

**Solution.** Advertise **formats** on copy; render the actual bytes only when the
peer pastes (`Pull`), streaming large ones. Mirrors RDP clipboard redirection and
Windows `WM_RENDERFORMAT`.

### D3. File clipboard (`CF_HDROP` ⇄ file list)
**Problem.** "Copy file in Explorer, paste in Nautilus" must move real bytes.

**Solution.** Translate `CF_HDROP` / `text/uri-list` into a `FileList` offer; on
paste, drive the file-transfer subsystem and present a local temp path. Metadata
(names, mtime) preserved via the manifest.

### D4. Clipboard echo loops
**Problem.** Syncing A→B re-triggers B's monitor → B→A → storm.

**Solution.** `EchoGuard`: tag each write with its origin sequence and suppress
the resulting local change (content-hash compare as a backstop).

---

## E. Audio

### E1. Capturing "what's playing" (loopback)
**Problem.** You need the system *output* mix, not a microphone.

**Solution.** Windows: **WASAPI loopback** capture in shared mode. Linux:
**PipeWire** monitor source, or create a virtual sink named "Deskoryn" that the
user can route apps to. Expose both as selectable sources.

### E2. Clock drift between machines
**Problem.** Two independent sound cards run at slightly different rates;
buffers slowly under/overflow.

**Solution.** Adaptive resampling at the sink driven by jitter-buffer fill level;
drop/insert a sample-frame occasionally to track drift. Opus's flexible frame
sizing helps.

### E3. Latency vs robustness
**Problem.** Calls/gaming want minimal latency; music wants no glitches.

**Solution.** `AudioProfile` selects Opus settings + `JitterBuffer::depth_for`
(2 frames low-latency, 8 high-quality). Audio rides **QUIC datagrams** so loss is
concealed, never retransmitted, and never stalls input.

---

## F. Discovery, networking, lifecycle

### F1. mDNS blocked / multiple interfaces / VPNs
**Problem.** Some networks block multicast; VPN virtual adapters confuse browsing.

**Solution.** mDNS as the happy path; **manual `host:port`** always available;
remember `last_address` for a fast reconnect that skips discovery. Bind/advertise
per-interface and prefer the subnet shared with a trusted peer.

### F2. Reconnect after sleep/reboot/roaming
**Problem.** The link drops constantly in real life.

**Solution.** Treat drops as normal: heartbeat detection, capped-backoff redial,
silent re-auth via pinned certs, QUIC connection migration for IP changes.

### F3. Autostart with the right session/permissions
**Problem.** Input/clipboard need the user's graphical session, not a bare system
context.

**Solution.** Linux: a **systemd *user* service** (`systemctl --user`) so it runs
inside the graphical session with portal access. Windows: a **service** for the
network/transfer parts plus a per-user auto-start (Run key / Task Scheduler at
logon) for the input/clipboard parts that need the desktop.

### F4. Monitor hotplug / resolution change
**Problem.** Plugging a monitor or changing resolution invalidates the layout.

**Solution.** Subscribe to display-change events (Wayland output events / X RandR
/ Windows `WM_DISPLAYCHANGE`), rebuild the local monitor set, and send a
`LayoutUpdate`; the arranger animates the change.

---

## G. Cross-cutting safety

### G1. Stuck keys on disconnect
A dropped connection mid-keypress must not leave a key down. **Solution:**
`Injector::release_all` on `Leave` and on any session teardown.

### G2. Hostile/malformed peer input
Even a paired peer might send garbage. **Solution:** frame-size caps, path-
traversal checks, hash validation, keycode range checks (see `docs/SECURITY.md`).
