// The harness: file in, worker, numbers out.
//
// Its whole job is to show that the four pieces line up — the browser's file
// reader, the worker, the wasm engine's two entry points, and the JSON they
// speak. It is not the interface, so it does the least it can while still
// failing visibly when any of those four is wrong.

const worker = new Worker(new URL("./worker.js", import.meta.url), {
  type: "module",
  name: "gridwright-engine",
});

// One promise per outstanding request, keyed by the id the worker echoes back.
const pending = new Map();
let nextId = 1;

worker.onmessage = ({ data }) => {
  const entry = pending.get(data.id);
  if (!entry) return;
  pending.delete(data.id);
  if (data.ok) entry.resolve({ result: data.result, ms: data.ms });
  else entry.reject(new Error(data.error));
};

// A worker that dies takes every outstanding request with it, and a promise
// nobody settles is a spinner that never stops.
worker.onerror = (event) => {
  const err = new Error(event.message || "the worker failed to start");
  for (const [, entry] of pending) entry.reject(err);
  pending.clear();
  fail(err);
};

function call(message, transfer = []) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    worker.postMessage({ ...message, id }, transfer);
  });
}

// --- rendering ---------------------------------------------------------------

const el = (id) => document.getElementById(id);
const statusEl = el("status");

function say(text) {
  statusEl.className = "";
  statusEl.textContent = text;
}

function fail(err) {
  statusEl.className = "err";
  statusEl.textContent = String(err.message ?? err);
}

function rows(dl, pairs) {
  dl.replaceChildren();
  for (const [label, value] of pairs) {
    const dt = document.createElement("dt");
    dt.textContent = label;
    const dd = document.createElement("dd");
    dd.textContent = value;
    dl.append(dt, dd);
  }
}

const count = (n) => n.toLocaleString();
const round = (x) =>
  x.toLocaleString(undefined, { maximumFractionDigits: 2 });

// --- the round trip ----------------------------------------------------------

async function run(file) {
  el("loaded").hidden = true;
  el("notes").hidden = true;
  el("solved").hidden = true;

  say(`Reading ${file.name} (${count(file.size)} bytes)…`);
  const bytes = await file.arrayBuffer();

  // The buffer is transferred rather than copied, so a 100 MB case does not
  // become 200 MB while it crosses. It is unusable here afterwards, which is
  // fine — the worker owns it now.
  say("Loading in the worker…");
  const loaded = await call(
    { op: "load", name: file.name, bytes },
    [bytes],
  );
  const net = loaded.result.network;

  rows(el("summary"), [
    ["Name", loaded.result.name],
    ["Snapshots", count(net.snapshots.weights.length)],
    ["Buses", count(net.buses.length)],
    ["Generators", count(net.generators.length)],
    ["Lines", count(net.lines.length)],
    ["Links", count(net.links.length)],
    ["Loads", count(net.loads.length)],
    ["Storage", count(net.storage.length)],
    ["Read in", `${round(loaded.ms)} ms`],
  ]);
  el("loaded").hidden = false;

  // Shown whether or not there are any, because "this reader dropped nothing"
  // is also information, and a panel that only appears on bad news trains
  // people not to look for it.
  const list = el("notes-list");
  list.replaceChildren();
  if (loaded.result.notes.length === 0) {
    const li = document.createElement("li");
    li.className = "none";
    li.textContent = "None. Everything in the file reached the model.";
    list.append(li);
  } else {
    for (const note of loaded.result.notes) {
      const li = document.createElement("li");
      li.textContent = note;
      list.append(li);
    }
  }
  el("notes").hidden = false;

  say("Solving…");
  // The network is sent back explicitly, which is the protocol as written and
  // the thing worth proving here. A real interface should instead omit
  // `network` and let the worker solve the copy it already holds: once the
  // model is large, re-serialising it on the main thread is exactly the stall
  // the worker exists to prevent.
  const solved = await call({ op: "solve", network: net });
  const s = solved.result;

  rows(el("result"), [
    ["Status", s.status],
    [
      "Objective",
      s.objective === null || s.objective === undefined
        ? "— (only optimal solves have one)"
        : round(s.objective),
    ],
    ["Unserved energy", `${round(s.total_shed)} MWh`],
    ["Capacity built", s.built.length === 0 ? "nothing extendable" : count(s.built.length)],
    ["Solved in", `${round(solved.ms)} ms`],
  ]);
  el("solved").hidden = false;

  say(`Done in ${round(loaded.ms + solved.ms)} ms of engine time.`);
}

function start(file) {
  if (!file) return;
  run(file).catch(fail);
}

// --- input -------------------------------------------------------------------

const drop = el("drop");
const input = el("file");

el("pick").addEventListener("click", () => input.click());
input.addEventListener("change", () => {
  start(input.files[0]);
  // Cleared so that picking the same file twice fires `change` both times.
  input.value = "";
});

for (const type of ["dragenter", "dragover"]) {
  drop.addEventListener(type, (e) => {
    e.preventDefault();
    drop.classList.add("hot");
  });
}
for (const type of ["dragleave", "drop"]) {
  drop.addEventListener(type, () => drop.classList.remove("hot"));
}
drop.addEventListener("drop", (e) => {
  e.preventDefault();
  start(e.dataTransfer.files[0]);
});
// Without this the browser navigates away to the dropped file, which looks
// exactly like the app crashing.
window.addEventListener("dragover", (e) => e.preventDefault());
window.addEventListener("drop", (e) => e.preventDefault());

say("Ready. The engine loads on the first file.");
