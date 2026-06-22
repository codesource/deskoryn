import { h, section, toast } from "../ui.js";

// The signature screen: a direct-manipulation canvas where each monitor is a
// draggable tile in one shared virtual-desktop space. Dragging snaps tiles
// edge-to-edge; "Apply" serializes the arrangement into a `VirtualDesktop` and
// pushes it with `SetLayout`.
//
// PROTOCOL GAP: the daemon's `Status` event reports peer *names* but neither
// device ids nor the current `VirtualDesktop`, so this editor cannot yet read
// the live layout back or bind tiles to real `MonitorId`s. It operates on a
// working model seeded from a starter arrangement; wiring real monitors needs a
// daemon-side `UiRequest::Layout` → `UiEvent::Layout { desktop }` round-trip
// (tracked in docs/UI.md). The serialization below already matches the wire
// shape so only the read path is missing.

const SCALE = 0.06; // virtual px → screen px on the canvas
const SNAP = 40; // virtual px within which edges snap together

// A placeholder 16-byte device id (all-zero / all-one) per side. Replaced by
// real ids once the daemon exposes them.
const DEV_LOCAL = new Array(16).fill(0);
const DEV_PEER = new Array(16).fill(1);

function starterModel() {
  // Mirrors the bring-up rig: three 1080p displays on the local machine, two
  // 1440p displays placed to their right on the peer.
  const mons = [];
  for (let i = 0; i < 3; i++) {
    mons.push({
      dev: DEV_LOCAL,
      index: i,
      label: `Linux-${["L", "C", "R"][i]}`,
      x: i * 1920,
      y: 0,
      w: 1920,
      h: 1080,
    });
  }
  for (let i = 0; i < 2; i++) {
    mons.push({
      dev: DEV_PEER,
      index: i,
      label: `Win-${["L", "R"][i]}`,
      x: 5760 + i * 2560,
      y: 0,
      w: 2560,
      h: 1440,
    });
  }
  return mons;
}

export function renderArranger(view, { api }) {
  const model = starterModel();
  const root = h("div", { class: "screen arranger" });
  view.append(root);

  const note = h(
    "p",
    { class: "hint" },
    "Drag a display to match your physical desk. Touching edges become cursor-crossing boundaries.",
  );

  const canvas = h("div", { class: "arranger-canvas" });
  const stats = h("div", { class: "arranger-stats" });

  function bbox() {
    const l = Math.min(...model.map((m) => m.x));
    const t = Math.min(...model.map((m) => m.y));
    const r = Math.max(...model.map((m) => m.x + m.w));
    const b = Math.max(...model.map((m) => m.y + m.h));
    return { l, t, r, b, w: r - l, h: b - t };
  }

  function paint() {
    canvas.innerHTML = "";
    const bb = bbox();
    const pad = 200;
    const offX = -bb.l + pad;
    const offY = -bb.t + pad;
    canvas.style.height = `${(bb.h + pad * 2) * SCALE}px`;

    for (const m of model) {
      const tile = h(
        "div",
        {
          class: `tile ${arraysEqual(m.dev, DEV_LOCAL) ? "local" : "peer"}`,
          title: arraysEqual(m.dev, DEV_LOCAL) ? "this machine" : "peer",
        },
        [
          h("span", { class: "tile-label" }, m.label),
          h("span", { class: "tile-res" }, `${m.w}×${m.h}`),
        ],
      );
      tile.style.left = `${(m.x + offX) * SCALE}px`;
      tile.style.top = `${(m.y + offY) * SCALE}px`;
      tile.style.width = `${m.w * SCALE}px`;
      tile.style.height = `${m.h * SCALE}px`;
      attachDrag(tile, m, offX, offY, paint, model);
      canvas.append(tile);
    }

    stats.textContent = `this workspace · ${model.length} displays · ${bb.w} × ${bb.h} virtual`;
  }

  const apply = h("button", { class: "btn primary" }, "Apply");
  apply.addEventListener("click", async () => {
    try {
      await api.setLayout(toVirtualDesktop(model));
      toast("Layout pushed to the daemon");
    } catch (e) {
      toast(String(e), "error");
    }
  });
  const align = h("button", { class: "btn" }, "Auto-align tops");
  align.addEventListener("click", () => {
    const top = Math.min(...model.map((m) => m.y));
    for (const m of model) m.y = top;
    paint();
  });
  const revert = h("button", { class: "btn" }, "Revert");
  revert.addEventListener("click", () => {
    model.splice(0, model.length, ...starterModel());
    paint();
  });

  root.append(
    section(
      "Arrange monitors",
      note,
      canvas,
      stats,
      h("div", { class: "toolbar" }, [align, revert, apply]),
    ),
  );
  paint();
}

function attachDrag(tile, m, offX, offY, paint, model) {
  tile.addEventListener("pointerdown", (e) => {
    e.preventDefault();
    tile.setPointerCapture(e.pointerId);
    tile.classList.add("dragging");
    const startX = e.clientX;
    const startY = e.clientY;
    const origX = m.x;
    const origY = m.y;

    const move = (ev) => {
      m.x = origX + (ev.clientX - startX) / SCALE;
      m.y = origY + (ev.clientY - startY) / SCALE;
      snap(m, model);
      // paint() rebuilds the tiles, but the drag is driven by window-level
      // listeners (not the tile), so losing the captured element is harmless.
      paint();
    };
    const up = () => {
      tile.classList.remove("dragging");
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  });
}

// Snap the dragged monitor's edges to nearby edges of the others.
function snap(m, model) {
  for (const o of model) {
    if (o === m) continue;
    // horizontal: right-to-left and left-to-right
    if (Math.abs(m.x - (o.x + o.w)) < SNAP) m.x = o.x + o.w;
    if (Math.abs(m.x + m.w - o.x) < SNAP) m.x = o.x - m.w;
    if (Math.abs(m.x - o.x) < SNAP) m.x = o.x;
    // vertical
    if (Math.abs(m.y - (o.y + o.h)) < SNAP) m.y = o.y + o.h;
    if (Math.abs(m.y + m.h - o.y) < SNAP) m.y = o.y - m.h;
    if (Math.abs(m.y - o.y) < SNAP) m.y = o.y;
  }
  m.x = Math.round(m.x);
  m.y = Math.round(m.y);
}

// Serialize the working model into the `deskoryn_core::VirtualDesktop` JSON
// shape the daemon deserializes (see crates/core/src/layout.rs).
function toVirtualDesktop(model) {
  return {
    monitors: model.map((m) => ({
      id: { device: m.dev, index: m.index },
      label: m.label,
      bounds: { x: m.x, y: m.y, w: m.w, h: m.h },
      native: { w: m.w, h: m.h },
      scale_pct: 100,
    })),
  };
}

function arraysEqual(a, b) {
  return a.length === b.length && a.every((v, i) => v === b[i]);
}
