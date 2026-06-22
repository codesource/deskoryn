import { h, section, modal, toast } from "../ui.js";
import { listen } from "@tauri-apps/api/event";

// Connection: launch and stop the deskorynd daemon, choose its client/server
// role, point the UI at the binary, and run first-time pairing — all without a
// terminal. The Rust shell (src-tauri/daemon.rs) owns the child processes; this
// screen drives them and reacts to the events they emit.
export function renderConnection(view, { api }) {
  const root = h("div", { class: "screen" });
  view.append(root);

  let life = { running: false, pairing: false };
  let bin = { path: null, source: "none", exists: false };

  // Daemon role state.
  let runRole = "server"; // "server" | "client"
  let runAddr = "";
  // Pairing role state.
  let pairRole = "server";
  let pairAddr = "";

  async function poll() {
    try {
      life = await api.lifecycle();
    } catch (_e) {}
    try {
      bin = await api.binInfo();
    } catch (_e) {}
    draw();
  }

  function draw() {
    root.innerHTML = "";
    root.append(daemonPanel(), pairingPanel(), binPanel());
  }

  function daemonPanel() {
    const roleSel = segmented(
      [
        ["server", "Server (listen / auto)"],
        ["client", "Client (connect to…)"],
      ],
      runRole,
      (v) => {
        runRole = v;
        draw();
      },
    );

    const addr = h("input", {
      type: "text",
      class: "text-input",
      placeholder: "192.168.1.42:7345",
      value: runAddr,
      oninput: (e) => (runAddr = e.target.value),
    });

    const startBtn = h(
      "button",
      { class: "btn primary", disabled: life.running || !bin.exists },
      life.running ? "Running…" : "Start daemon",
    );
    startBtn.addEventListener("click", async () => {
      try {
        await api.daemonStart(runRole === "client" ? runAddr : null);
        toast("Daemon started");
        poll();
      } catch (e) {
        toast(String(e), "error");
      }
    });

    const stopBtn = h("button", { class: "btn danger", disabled: !life.running }, "Stop");
    stopBtn.addEventListener("click", async () => {
      try {
        await api.daemonStop();
        toast("Daemon stopped");
        poll();
      } catch (e) {
        toast(String(e), "error");
      }
    });

    return section(
      "Daemon",
      h("div", { class: "row" }, [
        h("span", { class: `dot ${life.running ? "on" : "off"}` }),
        h("span", {}, life.running ? "Running" : "Stopped"),
        h("span", { class: "muted role-note" }, [
          "Server listens and auto-discovers; Client also dials a known address.",
        ]),
      ]),
      h("div", { class: "field" }, [h("label", {}, "Role"), roleSel]),
      runRole === "client"
        ? h("div", { class: "field" }, [h("label", {}, "Peer address"), addr])
        : null,
      h("div", { class: "toolbar" }, [stopBtn, startBtn]),
    );
  }

  function pairingPanel() {
    const roleSel = segmented(
      [
        ["server", "Server (wait)"],
        ["client", "Client (dial)"],
      ],
      pairRole,
      (v) => {
        pairRole = v;
        draw();
      },
    );
    const addr = h("input", {
      type: "text",
      class: "text-input",
      placeholder: "192.168.1.42:7345",
      value: pairAddr,
      oninput: (e) => (pairAddr = e.target.value),
    });
    const startBtn = h(
      "button",
      { class: "btn primary", disabled: life.pairing || !bin.exists },
      life.pairing ? "Pairing…" : "Start pairing",
    );
    startBtn.addEventListener("click", async () => {
      try {
        await api.pairStart(pairRole === "server", pairRole === "client" ? pairAddr : null);
        toast(pairRole === "server" ? "Waiting for a peer to pair…" : "Connecting to pair…");
        poll();
      } catch (e) {
        toast(String(e), "error");
      }
    });
    const cancelBtn = h("button", { class: "btn", disabled: !life.pairing }, "Cancel");
    cancelBtn.addEventListener("click", async () => {
      try {
        await api.pairCancel();
        await api.pairReap();
        toast("Pairing cancelled");
        poll();
      } catch (e) {
        toast(String(e), "error");
      }
    });

    return section(
      "Pair a device",
      h(
        "p",
        { class: "hint" },
        "Pairing uses the listen port, so stop the daemon first. One side waits (server), the other dials (client).",
      ),
      h("div", { class: "field" }, [h("label", {}, "Role"), roleSel]),
      pairRole === "client"
        ? h("div", { class: "field" }, [h("label", {}, "Peer address"), addr])
        : null,
      h("div", { class: "toolbar" }, [cancelBtn, startBtn]),
    );
  }

  function binPanel() {
    const input = h("input", {
      type: "text",
      class: "text-input",
      placeholder: "/usr/bin/deskorynd",
      value: bin.path || "",
    });
    const save = h("button", { class: "btn" }, "Set path");
    save.addEventListener("click", async () => {
      try {
        bin = await api.setBin(input.value);
        toast(bin.exists ? "Binary path set" : "Path set, but no file there", bin.exists ? "info" : "warning");
        draw();
      } catch (e) {
        toast(String(e), "error");
      }
    });
    const reset = h("button", { class: "btn" }, "Auto");
    reset.addEventListener("click", async () => {
      try {
        bin = await api.setBin(null);
        toast("Reverted to auto-resolved binary");
        draw();
      } catch (e) {
        toast(String(e), "error");
      }
    });
    return section(
      "deskorynd binary",
      h("div", { class: "row" }, [
        h("span", { class: `dot ${bin.exists ? "on" : "off"}` }),
        h("span", { class: "muted" }, bin.exists ? `resolved (${bin.source})` : "not found"),
      ]),
      h("div", { class: "row" }, [input, save, reset]),
    );
  }

  draw();
  poll();

  // React to the SAS prompt the pair subprocess emits.
  const unlisten = [];
  listen("pair-sas", (e) => showSas(api, e.payload, poll)).then((u) => unlisten.push(u));
  listen("pair-result", (e) => {
    toast(e.payload && e.payload.ok ? "Device paired ✓" : "Pairing aborted", e.payload && e.payload.ok ? "info" : "warning");
  }).then((u) => unlisten.push(u));
  listen("pair-ended", async () => {
    await api.pairReap();
    poll();
  }).then((u) => unlisten.push(u));

  const id = setInterval(poll, 2500);
  const obs = new MutationObserver(() => {
    if (!document.body.contains(root)) {
      clearInterval(id);
      unlisten.forEach((u) => u && u());
      obs.disconnect();
    }
  });
  obs.observe(view, { childList: true });
}

// A small segmented (radio) control.
function segmented(options, value, onChange) {
  return h(
    "div",
    { class: "segmented" },
    options.map(([v, label]) =>
      h(
        "button",
        {
          class: `seg ${v === value ? "active" : ""}`,
          onclick: () => onChange(v),
        },
        label,
      ),
    ),
  );
}

// SAS comparison dialog, wired to the pairing subprocess via pair_respond.
function showSas(api, payload, after) {
  const digits = (payload.sas || "").replace(/\s+/g, "").split("");
  const close = modal(
    h("div", { class: "dialog sas" }, [
      h("h2", {}, payload.device_name ? `Pair with “${payload.device_name}”?` : "Confirm pairing"),
      h("p", { class: "hint" }, "Confirm this code matches on BOTH machines."),
      h(
        "div",
        { class: "sas-digits" },
        digits.map((d) => h("span", { class: "sas-digit" }, d)),
      ),
      h("p", { class: "warn-text" }, "If they don't match, someone may be intercepting — do not continue."),
      h("div", { class: "dialog-actions" }, [
        h(
          "button",
          {
            class: "btn danger",
            onclick: async () => {
              try {
                await api.pairRespond(false);
              } catch (_e) {}
              close();
              after && after();
            },
          },
          "They don't match",
        ),
        h(
          "button",
          {
            class: "btn primary",
            onclick: async () => {
              try {
                await api.pairRespond(true);
              } catch (e) {
                toast(String(e), "error");
              }
              close();
              after && after();
            },
          },
          "Confirm",
        ),
      ]),
    ]),
  );
}
