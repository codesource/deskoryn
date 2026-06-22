// Thin wrapper over the Tauri commands exposed by the Rust shell (src-tauri).
// Each call forwards one UiRequest to the daemon and returns its UiEvents as a
// plain array of objects (the daemon's `serde` tag is on the `event` field).
import { invoke } from "@tauri-apps/api/core";

/** @returns {Promise<Array<object>>} the daemon's response events */
export async function status() {
  return invoke("daemon_status");
}

export async function pair(addr) {
  return invoke("daemon_pair", { addr });
}

export async function pairConfirm(accept) {
  return invoke("daemon_pair_confirm", { accept });
}

export async function forget(device) {
  return invoke("daemon_forget", { device });
}

export async function setFeature(feature, enabled) {
  // `feature` is one of "clipboard_sync" | "audio_forward" | "input_sharing".
  return invoke("daemon_set_feature", { feature, enabled });
}

export async function setLayout(layout) {
  return invoke("daemon_set_layout", { layout });
}

/** Pull the first Status event out of a response, or null. */
export function pickStatus(events) {
  return (events || []).find((e) => e.event === "status") || null;
}

// --- Daemon process management (the Rust shell spawns/stops deskorynd) -------

export async function binInfo() {
  return invoke("daemon_bin_info");
}

export async function setBin(path) {
  return invoke("set_daemon_bin", { path });
}

export async function lifecycle() {
  return invoke("daemon_lifecycle");
}

export async function daemonStart(connect) {
  // connect: optional "host:port" → "client" role (run --connect); omit for the
  // symmetric "server/auto" role.
  return invoke("daemon_start", { connect: connect || null });
}

export async function daemonStop() {
  return invoke("daemon_stop");
}

export async function pairStart(listen, addr) {
  return invoke("pair_start", { listen, addr: addr || null });
}

export async function pairRespond(accept) {
  return invoke("pair_respond", { accept });
}

export async function pairCancel() {
  return invoke("pair_cancel");
}

export async function pairReap() {
  return invoke("pair_reap");
}
