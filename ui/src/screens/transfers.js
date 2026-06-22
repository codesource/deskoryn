import { h, section } from "../ui.js";

// Transfers: a live list of in-progress file transfers with throughput and ETA.
//
// PROTOCOL GAP: `TransferProgress` events are defined in the protocol but the
// daemon's request/response control socket has no push channel, so progress
// isn't streamed continuously yet. This screen renders any progress events that
// arrive on a Status response and otherwise shows the empty state; a daemon-side
// streaming/subscription request (tracked in docs/UI.md) will light it up.
export function renderTransfers(view, { store }) {
  const root = h("div", { class: "screen" });
  view.append(root);

  const draw = () => {
    root.innerHTML = "";
    const xfers = store.transfers || [];

    if (!xfers.length) {
      root.append(
        section(
          "Transfers",
          h("p", { class: "empty" }, "No active transfers."),
          h(
            "p",
            { class: "hint" },
            "Copy files on one machine and paste on the other, or drop them on the tray icon.",
          ),
        ),
      );
      return;
    }

    root.append(
      section(
        "Transfers",
        ...xfers.map((t) => transferRow(t)),
      ),
    );
  };

  draw();
  store.subscribe(draw);
}

function transferRow(t) {
  const pct = Math.round((t.fraction || 0) * 100);
  const rate = humanRate(t.bytes_per_sec || 0);
  return h("div", { class: "row xfer" }, [
    h("div", { class: "xfer-head" }, [
      h("span", { class: "xfer-name" }, t.name),
      h("span", { class: "muted" }, `${pct}% · ${rate}`),
    ]),
    h("div", { class: "progress" }, [
      h("div", { class: "progress-fill", style: `width:${pct}%` }),
    ]),
  ]);
}

function humanRate(bps) {
  if (bps <= 0) return "—";
  const units = ["B/s", "KB/s", "MB/s", "GB/s"];
  let v = bps;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}
