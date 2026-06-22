import { h, section, toast } from "../ui.js";

// Settings: groups mirroring config.toml. The control socket exposes feature
// toggles (SetFeature) today; the remaining groups are presented read-only as a
// map of what config.toml controls, until the daemon exposes a config get/set
// over IPC (tracked in docs/UI.md).
export function renderSettings(view, { api }) {
  const root = h("div", { class: "screen" });
  view.append(root);

  root.append(
    section(
      "Features",
      featureToggle(api, "input_sharing", "Input sharing", true),
      featureToggle(api, "clipboard_sync", "Clipboard sync", true),
      featureToggle(api, "audio_forward", "Audio forwarding", false),
    ),
    readonlyGroup("Input", [
      "Focus-follows-mouse (the machine under the cursor is active)",
      "Edge resistance, lock/switch hotkeys",
    ]),
    readonlyGroup("Clipboard", ["Text, images, and file lists", "Echo suppression"]),
    readonlyGroup("Files", [
      "Download directory and conflict policy",
      "Optional shared folders",
    ]),
    readonlyGroup("Network", ["Listen port", "mDNS discovery", "Static peers"]),
    readonlyGroup("Startup", ["Launch at login (systemd user service / Windows service)"]),
    h(
      "p",
      { class: "hint settings-foot" },
      "Editing these lives in config.toml until the daemon exposes config over the control socket.",
    ),
  );
}

function featureToggle(api, key, label, defaultOn) {
  const input = h("input", { type: "checkbox", checked: defaultOn });
  input.addEventListener("change", async () => {
    try {
      await api.setFeature(key, input.checked);
      toast(`${label} ${input.checked ? "on" : "off"}`);
    } catch (e) {
      input.checked = !input.checked;
      toast(String(e), "error");
    }
  });
  return h("label", { class: "row toggle" }, [
    h("span", {}, label),
    h("span", { class: "switch" }, [input, h("span", { class: "track" })]),
  ]);
}

function readonlyGroup(title, items) {
  return section(
    title,
    h(
      "ul",
      { class: "config-list" },
      items.map((i) => h("li", {}, i)),
    ),
  );
}
