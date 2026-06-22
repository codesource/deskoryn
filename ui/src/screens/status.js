import { h, section, toast } from "../ui.js";

// Status screen: the at-a-glance dashboard mirroring the tray popover — the
// active peer, feature toggles, and quick lock/switch hints.
export function renderStatus(view, { api, store }) {
  const root = h("div", { class: "screen" });
  view.append(root);

  const draw = () => {
    root.innerHTML = "";
    const s = store.status;

    if (!store.reachable) {
      root.append(
        section(
          "Status",
          h("p", { class: "empty" }, "The deskorynd daemon isn't reachable."),
          h("p", { class: "hint" }, "Start it with: deskorynd run"),
        ),
      );
      return;
    }

    const peers = (s && s.peers) || [];
    const connected = peers.filter((p) => p.connected);

    const peerRows = peers.length
      ? peers.map((p) =>
          h("div", { class: "row peer" }, [
            h("span", { class: `dot ${p.connected ? "on" : "off"}` }),
            h("span", { class: "peer-name" }, p.name),
            h(
              "span",
              { class: "peer-meta" },
              p.connected
                ? p.latency_ms != null
                  ? `${p.latency_ms} ms`
                  : "connected"
                : "offline",
            ),
          ]),
        )
      : [h("p", { class: "empty" }, "No paired devices yet.")];

    root.append(
      section(
        s ? `This workspace — ${s.device_name}` : "This workspace",
        h("div", { class: "stat-line" }, [
          h(
            "span",
            { class: `badge ${connected.length ? "good" : "idle"}` },
            connected.length ? "Sharing active" : "Idle",
          ),
          h(
            "span",
            { class: "muted" },
            `${connected.length} of ${peers.length} devices connected`,
          ),
        ]),
        ...peerRows,
      ),
    );

    root.append(featurePanel(api, store));

    root.append(
      section(
        "Quick controls",
        h("div", { class: "row" }, [
          h("kbd", {}, "Ctrl+Alt+L"),
          h("span", { class: "muted" }, "Lock cursor to this machine"),
        ]),
        h("div", { class: "row" }, [
          h("kbd", {}, "Ctrl+Alt+S"),
          h("span", { class: "muted" }, "Switch active machine"),
        ]),
      ),
    );
  };

  draw();
  const unsub = store.subscribe(draw);
  // Screens are torn down by clearing #view; drop the subscription when the
  // root node leaves the DOM.
  const obs = new MutationObserver(() => {
    if (!document.body.contains(root)) {
      unsub();
      obs.disconnect();
    }
  });
  obs.observe(view, { childList: true });
}

const FEATURES = [
  { key: "input_sharing", label: "Input sharing" },
  { key: "clipboard_sync", label: "Clipboard sync" },
  { key: "audio_forward", label: "Audio forwarding" },
];

function featurePanel(api) {
  // The daemon doesn't yet report per-feature enabled state in Status, so these
  // are write-through toggles: flipping one sends SetFeature. They default to
  // on (input/clipboard) to match the daemon's defaults.
  const rows = FEATURES.map((f) => {
    const input = h("input", { type: "checkbox", checked: f.key !== "audio_forward" });
    input.addEventListener("change", async () => {
      try {
        await api.setFeature(f.key, input.checked);
        toast(`${f.label} ${input.checked ? "on" : "off"}`);
      } catch (e) {
        input.checked = !input.checked;
        toast(String(e), "error");
      }
    });
    return h("label", { class: "row toggle" }, [
      h("span", {}, f.label),
      h("span", { class: "switch" }, [input, h("span", { class: "track" })]),
    ]);
  });
  return section("Features", ...rows);
}
