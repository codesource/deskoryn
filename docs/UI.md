# Deskoryn — UI Mockup Description

The UI is a **tray-first** Tauri app. There is no main window you keep open — the
daemon does the work; the UI is a thin control surface (`daemon::ipc`). Everything
is reachable from the tray icon; a settings window opens on demand.

Design tone: calm, native, status-at-a-glance. The UI must reinforce the core
illusion — it talks about "this workspace" and named monitors, never "the Linux
box vs the Windows box."

---

## 1. Tray icon + menu

The tray icon encodes connection state at a glance:

- **● solid** — peer connected, sharing active.
- **◐ half** — connected, input sharing paused/locked.
- **○ hollow** — searching / disconnected.
- **▲ badge** — a transfer is in progress or a pairing needs confirmation.

Left-click → status popover; right-click → menu:

```
┌─────────────────────────────────────────┐
│  Deskoryn — Connected                    │
│  ● matthias-windows   12 ms   ▲ 1 file   │
├─────────────────────────────────────────┤
│  Input sharing            ▣ on           │
│  Clipboard sync           ▣ on           │
│  Audio forwarding         ▢ off          │
├─────────────────────────────────────────┤
│  Arrange monitors…                       │
│  Devices…                                │
│  Transfers…                              │
│  Settings…                               │
├─────────────────────────────────────────┤
│  Lock cursor here     Ctrl+Alt+L         │
│  Switch machine       Ctrl+Alt+S         │
│  Quit                                    │
└─────────────────────────────────────────┘
```

---

## 2. Monitor arranger (the signature screen)

A direct-manipulation canvas where the **five monitors are draggable tiles** in
one shared space — the visual embodiment of the virtual desktop. The user drags
tiles to match the physical arrangement on their desk; edges that touch become
cursor-crossing boundaries.

```
   Arrange monitors — drag to match your desk
  ┌──────────────────────────────────────────────────────────────┐
  │  ┌────────┐┌────────┐┌────────┐      ┌──────────┐┌──────────┐  │
  │  │ Lin-L  ││ Lin-C  ││ Lin-R  │      │  Win-L   ││  Win-R   │  │
  │  │1920×1080││1920×1080││1920×1080│    │2560×1440 ││2560×1440 │  │
  │  └────────┘└────────┘└────────┘      └──────────┘└──────────┘  │
  │     this workspace · 5 displays · 12 800 × 1440 virtual        │
  └──────────────────────────────────────────────────────────────┘
   [ Auto-align tops ]  [ Snap to grid ]      [ Revert ]  [ Apply ]
```

- Tiles snap edge-to-edge; misaligned heights are allowed (the transition math
  projects across them).
- Tile color/label, **not** machine name, identifies a display. A subtle owner
  glyph appears only on hover, so the boundary stays de-emphasized.
- "Apply" pushes a `LayoutUpdate`; both daemons recompute live. Hotplugging a
  monitor animates a new tile in.

---

## 3. Pairing dialog (SAS verification)

Appears on both machines simultaneously during first contact:

```
   Pair with “matthias-windows”?
   ┌──────────────────────────────────────────┐
   │   Confirm this code matches on BOTH        │
   │                                            │
   │                0 4 2   1 3 7                │
   │                                            │
   │     [ QR ]   or scan with the other app    │
   │                                            │
   │   They don’t match → someone may be         │
   │   intercepting. Do not continue.            │
   └──────────────────────────────────────────┘
        [ They don’t match ]      [ Confirm ]
```

Big, legible digits; an explicit "don't match → abort" affordance (security UX).

---

## 4. Devices

```
   Devices
   ┌──────────────────────────────────────────────┐
   │ ● matthias-windows   paired 12 Jun   192.168… │  [ Forget ]
   │ ○ office-nuc         paired 03 May            │  [ Forget ]
   ├──────────────────────────────────────────────┤
   │ [ + Pair a new device ]   [ Enter IP manually]│
   └──────────────────────────────────────────────┘
```

---

## 5. Transfers

A live list with per-item progress, throughput, and ETA (from `ProgressTracker`):

```
   Transfers
   ┌──────────────────────────────────────────────┐
   │ ⬇ project/                42%  18 MB/s  ~22 s │  [ Cancel ]
   │   1 230 / 2 940 files · 5.1 / 12.0 GB          │
   ├──────────────────────────────────────────────┤
   │ ✓ screenshot.png          done                │
   └──────────────────────────────────────────────┘
```

Drag-and-drop: dropping files onto the tray icon (or a configured hot-corner)
sends them to the peer; conflicts raise an inline prompt when policy = `ask`.

---

## 6. Settings

Grouped to mirror `config.toml`: Input (hotkeys, focus-follows-mouse, edge
resistance), Clipboard (text/images/files), Audio (profile + device pickers),
Files (download dir, conflict policy, shared folders), Network (port, discovery,
static peers), Startup (launch at login).

Audio device pickers list sources/sinks from `audio::platform::{capture,playback}_devices`.

---

## 7. Notifications

Native OS notifications for: pairing requests, connection lost/restored, transfer
complete, and errors needing attention. Levels map to `ipc::NoticeLevel`. Kept
sparse — steady-state operation is silent.

---

## Implementation notes

- Tauri (Rust backend + web frontend) reuses the workspace's Rust stack and ships
  a small native binary. The frontend talks only to the local daemon via the IPC
  messages in `daemon::ipc` (`UiRequest`/`UiEvent`), not the network.
- The arranger is the one bespoke component; everything else is standard lists,
  toggles, and dialogs.
