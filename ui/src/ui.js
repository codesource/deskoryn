// Minimal DOM helpers — the UI is small enough that a framework would be more
// weight than the whole app. `h` builds elements; `toast` shows a transient
// notice; `modal` opens a centered dialog.

export function h(tag, attrs = {}, children = []) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") el.className = v;
    else if (k === "text") el.textContent = v;
    else if (k === "html") el.innerHTML = v;
    else if (k.startsWith("on") && typeof v === "function") {
      el.addEventListener(k.slice(2).toLowerCase(), v);
    } else if (v !== false && v != null) {
      el.setAttribute(k, v === true ? "" : v);
    }
  }
  const kids = Array.isArray(children) ? children : [children];
  for (const c of kids) {
    if (c == null || c === false) continue;
    el.append(c.nodeType ? c : document.createTextNode(String(c)));
  }
  return el;
}

let toastTimer = null;
export function toast(text, level = "info") {
  const host = document.getElementById("toast-host");
  if (!host) return;
  const t = h("div", { class: `toast ${level}` }, text);
  host.append(t);
  requestAnimationFrame(() => t.classList.add("in"));
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    t.classList.remove("in");
    setTimeout(() => t.remove(), 300);
  }, 4000);
}

export function modal(node) {
  const overlay = h("div", { class: "overlay" }, [node]);
  const close = () => overlay.remove();
  overlay.addEventListener("click", (e) => {
    if (e.target === overlay) close();
  });
  document.body.append(overlay);
  return close;
}

export function section(title, ...children) {
  return h("section", { class: "panel" }, [
    h("h2", { class: "panel-title" }, title),
    ...children,
  ]);
}
