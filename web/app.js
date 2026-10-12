// UI for the patcher. All firmware processing happens in worker.js.

// Shown in the footer once the repository is public.
const REPO_URL = "";

const $ = (id) => document.getElementById(id);
const worker = new Worker(new URL("worker.js", import.meta.url), { type: "module" });

let catalog = [];
let firmware = null;
let outputUrl = null;

const handlers = {
  catalog(msg) {
    catalog = msg.catalog;
    const list = $("versions");
    list.replaceChildren(
      ...catalog.map((f) => {
        const li = el("li");
        li.append(el("strong", f.name));
        if (f.description) li.append(" — " + f.description + " ");
        if (f.download_url) {
          const a = el("a", "Download from vendor");
          a.href = f.download_url;
          a.rel = "noreferrer";
          li.append(a);
        }
        return li;
      }),
    );
  },
  identified({ result }) {
    firmware = result.firmware;
    const box = $("ident");
    box.hidden = false;
    box.className = "ident " + (firmware ? "ok" : "bad");
    if (firmware) {
      box.replaceChildren(el("strong", "Supported: " + firmware.name), el("div", "SHA-256 " + result.sha256, "mono muted"));
      renderPatches();
    } else {
      box.replaceChildren(
        el("strong", "This file is not a supported stock firmware."),
        el("div", "Use the original, unmodified GEMINI_PACK.BIN of a supported version (a file that was already patched is not accepted)."),
        el("div", "SHA-256 " + result.sha256, "mono muted"),
      );
      $("step-patches").hidden = $("step-apply").hidden = true;
    }
  },
  progress({ stage, fraction }) {
    $("stage").textContent = stage;
    $("bar-fill").style.width = Math.round(fraction * 100) + "%";
  },
  applied({ report, buffer }) {
    busy(false);
    $("progress").hidden = true;
    if (outputUrl) URL.revokeObjectURL(outputUrl);
    outputUrl = URL.createObjectURL(new Blob([buffer], { type: "application/octet-stream" }));
    const dl = $("download");
    dl.href = outputUrl;
    dl.hidden = !report.ok;
    const titles = report.applied.map((id) => firmware.patch.find((p) => p.id === id)?.title ?? id);
    $("summary").textContent = report.ok
      ? `Built with: ${titles.join("; ")}. All checks passed.`
      : "Verification failed. Do not flash this file.";
    $("checks").replaceChildren(
      ...report.checks.map((c) => {
        const li = el("li", null, c.ok ? "pass" : "fail");
        li.append(el("span", c.ok ? "✓" : "✗", "mark"), el("strong", c.name), el("span", c.detail, "muted"));
        return li;
      }),
    );
    $("step-result").hidden = false;
    $("step-result").scrollIntoView({ behavior: "smooth", block: "start" });
  },
  error({ message }) {
    busy(false);
    $("progress").hidden = true;
    showError(message);
  },
};
worker.onmessage = ({ data }) => handlers[data.type]?.(data);
worker.postMessage({ type: "catalog" });

function el(tag, text, className) {
  const e = document.createElement(tag);
  if (text != null) e.textContent = text;
  if (className) e.className = className;
  return e;
}

function showError(message) {
  const box = $("error");
  box.textContent = message;
  box.hidden = !message;
}

function busy(on) {
  $("apply").disabled = on || !selectionValid();
  $("file").disabled = on;
  for (const i of $("patches").querySelectorAll("input")) i.disabled = on;
}

// --- File input --------------------------------------------------------------

async function load(file) {
  if (!file) return;
  reset();
  const box = $("ident");
  box.hidden = false;
  box.className = "ident";
  box.textContent = `Checking ${file.name}…`;
  worker.postMessage({ type: "identify", buffer: await file.arrayBuffer() });
}

function reset() {
  firmware = null;
  showError("");
  for (const id of ["step-patches", "step-apply", "step-result"]) $(id).hidden = true;
}

$("file").addEventListener("change", (e) => load(e.target.files[0]));
const drop = $("drop");
drop.addEventListener("dragover", (e) => {
  e.preventDefault();
  drop.classList.add("over");
});
drop.addEventListener("dragleave", () => drop.classList.remove("over"));
drop.addEventListener("drop", (e) => {
  e.preventDefault();
  drop.classList.remove("over");
  load(e.dataTransfer.files[0]);
});

// --- Patch selection ----------------------------------------------------------

function renderPatches() {
  const form = $("patches");
  form.replaceChildren(
    ...firmware.patch.map((p) => {
      const card = el("fieldset", null, "patch");
      const head = el("label", null, "patch-head");
      const box = el("input");
      box.type = "checkbox";
      box.name = p.id;
      head.append(box, el("strong", p.title));
      card.append(head, el("p", p.description, "muted"));
      if (p.warning) card.append(el("p", "⚠ " + p.warning, "warning"));
      const params = el("div", null, "params");
      params.hidden = true;
      for (const q of p.param) {
        const label = el("label", null, "param");
        const input = el("input");
        input.type = "text";
        input.dataset.patch = p.id;
        input.dataset.param = q.id;
        input.placeholder = q.placeholder;
        input.maxLength = q.max_bytes;
        input.autocomplete = "off";
        input.spellcheck = false;
        const err = el("span", null, "param-error");
        input.addEventListener("input", () => {
          err.textContent = validate(q, input.value) ?? "";
          update();
        });
        label.append(el("span", q.label), input, el("span", q.help, "muted small"), err);
        params.append(label);
      }
      box.addEventListener("change", () => {
        params.hidden = !box.checked;
        update();
      });
      card.append(params);
      return card;
    }),
  );
  $("step-patches").hidden = $("step-apply").hidden = false;
  update();
}

// Mirrors ParamDef::validate in the Rust core (which re-checks everything).
function validate(q, value) {
  const bytes = new TextEncoder().encode(value).length;
  if (bytes < q.min_bytes) return bytes === 0 ? "Required" : `At least ${q.min_bytes} characters`;
  if (bytes > q.max_bytes) return `At most ${q.max_bytes} bytes`;
  if (value.trim() !== value) return "No spaces at the start or end";
  for (const c of value) {
    if (/\p{Cc}/u.test(c) || q.forbidden_chars.includes(c)) return `Character “${c}” is not allowed`;
    if (q.ascii_only && c.charCodeAt(0) > 0x7e) return `Character “${c}” is not allowed (ASCII only)`;
  }
  return null;
}

function selection() {
  const patches = [];
  for (const p of firmware.patch) {
    const box = $("patches").querySelector(`input[type=checkbox][name="${p.id}"]`);
    if (!box.checked) continue;
    const params = {};
    for (const q of p.param) {
      params[q.id] = $("patches").querySelector(`input[data-patch="${p.id}"][data-param="${q.id}"]`).value;
    }
    patches.push({ id: p.id, params });
  }
  return patches;
}

function selectionValid() {
  if (!firmware) return false;
  const sel = selection();
  return (
    sel.length > 0 &&
    sel.every(({ id, params }) => firmware.patch.find((p) => p.id === id).param.every((q) => !validate(q, params[q.id])))
  );
}

function update() {
  $("apply").disabled = !selectionValid();
  $("step-result").hidden = true;
}

$("apply").addEventListener("click", () => {
  showError("");
  busy(true);
  $("progress").hidden = false;
  handlers.progress({ stage: "Starting…", fraction: 0 });
  worker.postMessage({ type: "apply", selection: { patches: selection() } });
});

if (REPO_URL) {
  $("repo-link").href = $("repo-link").textContent = REPO_URL;
  $("repo-line").hidden = false;
}
