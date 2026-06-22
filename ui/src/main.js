import * as api from "./api.js";
import { renderStatus } from "./screens/status.js";
import { renderConnection } from "./screens/connection.js";
import { renderArranger } from "./screens/arranger.js";
import { renderDevices } from "./screens/devices.js";
import { renderTransfers } from "./screens/transfers.js";
import { renderSettings } from "./screens/settings.js";
import { toast } from "./ui.js";

const SCREENS = {
  status: renderStatus,
  connection: renderConnection,
  arranger: renderArranger,
  devices: renderDevices,
  transfers: renderTransfers,
  settings: renderSettings,
};

// Shared, reactive snapshot of the daemon state. Screens read from here and
// re-render when `refresh()` updates it; they never poll the daemon directly.
export const store = {
  status: null, // last UiEvent::Status object, or null when unreachable
  transfers: [], // last seen TransferProgress events (see transfers.js gap note)
  reachable: false,
  listeners: new Set(),
  subscribe(fn) {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  },
  emit() {
    for (const fn of this.listeners) fn(this);
  },
};

let current = "status";

export function navigate(screen) {
  show(screen);
}

function show(screen) {
  current = screen;
  for (const btn of document.querySelectorAll(".nav")) {
    btn.classList.toggle("active", btn.dataset.screen === screen);
  }
  const view = document.getElementById("view");
  view.innerHTML = "";
  SCREENS[screen](view, { api, store });
}

function paintConnection() {
  const dot = document.getElementById("brand-dot");
  const line = document.getElementById("conn-line");
  const s = store.status;
  if (!store.reachable) {
    dot.className = "brand-dot off";
    line.textContent = "daemon not running";
    return;
  }
  const peer = s && s.peers && s.peers.find((p) => p.connected);
  if (peer) {
    dot.className = s.active ? "brand-dot on" : "brand-dot half";
    const lat = peer.latency_ms != null ? ` · ${peer.latency_ms} ms` : "";
    line.textContent = `${peer.name}${lat}`;
  } else {
    dot.className = "brand-dot searching";
    line.textContent = s ? "searching…" : "connected";
  }
}

// Poll the daemon for a status snapshot. The control socket is request/response
// (no server push), so the GUI refreshes on a timer; commands also refresh
// immediately on completion.
export async function refresh() {
  try {
    const events = await api.status();
    store.status = api.pickStatus(events);
    store.reachable = true;
    const xfers = events.filter((e) => e.event === "transfer_progress");
    if (xfers.length) store.transfers = xfers;
    for (const ev of events) {
      if (ev.event === "notice") toast(ev.text, ev.level);
    }
  } catch (_e) {
    store.reachable = false;
    store.status = null;
  }
  paintConnection();
  store.emit();
}

function init() {
  // Suppress the webview's native right-click context menu (its "Inspect
  // Element" entry) in production. Kept in dev (`vite dev` / `tauri dev`) so the
  // inspector stays reachable while developing.
  if (!import.meta.env.DEV) {
    window.addEventListener("contextmenu", (e) => e.preventDefault());
  }

  for (const btn of document.querySelectorAll(".nav")) {
    btn.addEventListener("click", () => show(btn.dataset.screen));
  }
  show(current);
  refresh();
  setInterval(refresh, 2000);
}

window.addEventListener("DOMContentLoaded", init);
