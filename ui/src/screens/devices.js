import { h, section, toast } from "../ui.js";
import { navigate } from "../main.js";

// Devices: the trusted peer list, plus pairing (manual IP entry → SAS
// verification dialog).
export function renderDevices(view, { api, store }) {
  const root = h("div", { class: "screen" });
  view.append(root);

  const draw = () => {
    root.innerHTML = "";
    const peers = (store.status && store.status.peers) || [];

    const rows = peers.length
      ? peers.map((p) =>
          h("div", { class: "row device" }, [
            h("span", { class: `dot ${p.connected ? "on" : "off"}` }),
            h("div", { class: "device-id" }, [
              h("div", { class: "device-name" }, p.name),
              h(
                "div",
                { class: "device-addr muted" },
                p.address || (p.connected ? "connected" : "offline"),
              ),
            ]),
            forgetBtn(api, p.name),
          ]),
        )
      : [h("p", { class: "empty" }, "No trusted devices yet.")];

    const pairBtn = h("button", { class: "btn primary" }, "Pair a new device");
    pairBtn.addEventListener("click", () => navigate("connection"));

    root.append(
      section("Devices", ...rows),
      section(
        "Pair a new device",
        h("p", { class: "hint" }, "Both machines must confirm the same 6-digit code."),
        h("div", { class: "row" }, [pairBtn]),
      ),
    );
  };

  draw();
  store.subscribe(draw);
}

function forgetBtn(api, name) {
  const b = h("button", { class: "btn danger small" }, "Forget");
  b.addEventListener("click", async () => {
    try {
      await api.forget(name);
      toast(`Forgot ${name}`);
    } catch (e) {
      toast(String(e), "error");
    }
  });
  return b;
}
