# Deskoryn — Configuration & State Format

Deskoryn separates **human-editable configuration** (TOML) from **mutable runtime
state** (JSON / key files). Config is something you'd hand-edit or commit to a
dotfiles repo; state is machine-managed and security-sensitive.

The schemas are the Rust types in `deskoryn-core` (`config.rs`, `trust.rs`) — this
document is the human-facing companion.

---

## 1. Locations

Resolved by `core::config::Paths` (XDG on Linux, `%APPDATA%` on Windows):

| File              | Linux                                   | Windows                                  | Kind   |
|-------------------|-----------------------------------------|------------------------------------------|--------|
| `config.toml`     | `~/.config/deskoryn/config.toml`        | `%APPDATA%\Deskoryn\config\config.toml`  | config |
| `trusted.json`    | `~/.local/share/deskoryn/trusted.json`  | `%APPDATA%\Deskoryn\data\trusted.json`   | state  |
| `device.key`/`.crt`| `~/.local/share/deskoryn/`             | `%APPDATA%\Deskoryn\data\`               | state  |

---

## 2. Why TOML + JSON, not a database

A SQL/embedded DB (SQLite) was considered for transfer history and device lists.
Rejected for the core config because:

- The data is tiny (a handful of devices, one layout) and read-mostly.
- Human-editability and dotfile-friendliness matter more than query power.
- Fewer native dependencies keeps the cross-platform build lean.

A future **transfer-history / metrics** store (append-heavy, queryable) is the one
place a small embedded DB (SQLite via `rusqlite`, or `redb`) would be justified;
it would live alongside the JSON state, not replace the config.

---

## 3. `config.toml` (full example)

```toml
# === Identity =============================================================
[device]
# Generated once on first run; stable across IP/hostname changes.
id = "a4e5212bfe96bb2060b83598250fb0e3"
name = "matthias-linux"

# === Networking ===========================================================
[network]
listen_port = 0              # 0 = OS-assigned, advertised over mDNS
discovery_enabled = true     # mDNS advertise + browse on the LAN
static_peers = []            # e.g. ["192.168.1.42:7423"] when mDNS is blocked

# === Input sharing ========================================================
[input]
focus_follows_mouse = true
edge_resistance_px  = 0       # soft wall: px to push past a monitor edge before
                              # the cursor hands off to the other PC (0 = off).
                              # Also editable live from the GUI monitor arranger.
switch_hotkey = "Ctrl+Alt+S"  # force cursor to the other machine
lock_hotkey   = "Ctrl+Alt+L"  # lock cursor to current machine

# === Clipboard ============================================================
[clipboard]
sync_text   = true
sync_images = true
sync_files  = true
inline_max_bytes = 262144     # inline payloads up to 256 KiB; larger = pulled

# === Audio ================================================================
[audio]
forward_enabled = false
profile = "low_latency"       # "low_latency" | "high_quality"
source_device = "default"     # capture device id on the source machine
sink_device   = "default"     # playback device id on the destination

# === File transfer ========================================================
[file_transfer]
download_dir = "/home/matthias/Deskoryn"   # omit to use the platform default
conflict_policy = "rename"    # "rename" | "overwrite" | "skip" | "ask"
shared_folders = []           # optional folder-sync pairs

# [[file_transfer.shared_folders]]
# local_path = "/home/matthias/Shared"
# name = "Shared"
# bidirectional = true

# === Virtual desktop layout ===============================================
# Each machine contributes its own monitors; the union is the virtual desktop.
# `bounds` are in virtual-desktop pixels (one shared, signed coordinate space).
[[layout.monitors]]
id = { device = "a4e5212bfe96bb2060b83598250fb0e3", index = 0 }
label  = "Lin-L"
bounds = { x = 0,    y = 0, w = 1920, h = 1080 }
native = { w = 1920, h = 1080 }
scale_pct = 100

[[layout.monitors]]
id = { device = "a4e5212bfe96bb2060b83598250fb0e3", index = 1 }
label  = "Lin-C"
bounds = { x = 1920, y = 0, w = 1920, h = 1080 }
native = { w = 1920, h = 1080 }
scale_pct = 100

[[layout.monitors]]
id = { device = "a4e5212bfe96bb2060b83598250fb0e3", index = 2 }
label  = "Lin-R"
bounds = { x = 3840, y = 0, w = 1920, h = 1080 }
native = { w = 1920, h = 1080 }
scale_pct = 100

# Windows machine's two monitors (placed to the right of the Linux row).
[[layout.monitors]]
id = { device = "ee10c0ffee10c0ffee10c0ffee10c0ff", index = 0 }
label  = "Win-L"
bounds = { x = 5760, y = 0, w = 2560, h = 1440 }
native = { w = 2560, h = 1440 }
scale_pct = 100

[[layout.monitors]]
id = { device = "ee10c0ffee10c0ffee10c0ffee10c0ff", index = 1 }
label  = "Win-R"
bounds = { x = 8320, y = 0, w = 2560, h = 1440 }
native = { w = 2560, h = 1440 }
scale_pct = 100
```

Notes:

- `scale_pct` lets a HiDPI monitor map virtual pixels onto the owning OS's
  logical pointer space.
- The layout is normally arranged visually in the tray UI (drag the monitor
  tiles); editing TOML by hand is the power-user path.

---

## 4. `trusted.json` (state, machine-managed)

```json
{
  "devices": [
    {
      "id": "ee10c0ffee10c0ffee10c0ffee10c0ff",
      "name": "matthias-windows",
      "fingerprint": [79, 42, 156, 16, 0, 1, "...32 bytes total..."],
      "paired_at": 1750000000,
      "last_address": "192.168.1.42:7423"
    }
  ]
}
```

- `fingerprint` is the 32-byte BLAKE3 of the peer's DER certificate.
- Edited only by the daemon (on pair / forget). Mode `0600`.

---

## 5. Precedence & overrides

1. `--config <path>` CLI flag (highest).
2. `config.toml` at the resolved path.
3. Built-in defaults (`AppConfig::bootstrap`) — written out on first run so the
   file always exists and is discoverable.

Environment: `RUST_LOG` / `DESKORYN_LOG` controls log verbosity (via
`tracing_subscriber` `EnvFilter`).
