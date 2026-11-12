"use strict";

const $ = (id) => document.getElementById(id);
const tbody = document.querySelector("#list tbody");

function say(text, kind) {
  $("message").textContent = text;
  $("message").className = kind || "";
}

function size(bytes) {
  if (!bytes) return "–";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (bytes >= 1024 && i < units.length - 1) { bytes /= 1024; i++; }
  return bytes.toFixed(i < 2 ? 0 : 1) + " " + units[i];
}

async function api(path, options) {
  const res = await fetch(path, options);
  if (!res.ok) {
    let detail = res.status + " " + res.statusText;
    try { detail = (await res.json()).error || detail; } catch (_) { /* not JSON */ }
    throw new Error(detail);
  }
  return res.status === 204 ? null : res.json();
}

async function loadCategories() {
  const cats = await api("/api/categories");
  const select = $("category");
  for (const name of Object.keys(cats)) {
    const opt = document.createElement("option");
    opt.value = opt.textContent = name;
    select.append(opt);
  }
  select.value = localStorage.getItem("category") || "other";
  if (!select.value) select.selectedIndex = 0;
}

function cell(row, text, className) {
  const td = row.insertCell();
  if (className) td.className = className;
  if (text !== undefined) td.textContent = text;
  return td;
}

function removeButton(item, label, deleteFiles) {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = label;
  b.onclick = async () => {
    const what = deleteFiles ? "Remove it AND delete its files from disk?" : "Remove it from the list (files stay on disk)?";
    if (!confirm(item.name + "\n\n" + what)) return;
    try {
      await api("/api/downloads/" + item.hash + "?delete_files=" + deleteFiles, { method: "DELETE" });
      refresh();
    } catch (e) { say("Could not remove: " + e.message, "error"); }
  };
  return b;
}

function render(items) {
  $("empty").hidden = items.length > 0;
  $("list").hidden = items.length === 0;
  tbody.replaceChildren();
  for (const item of items.slice().reverse()) {
    const row = tbody.insertRow();
    const name = cell(row, item.name, "name");
    name.title = item.path;
    if (item.error) {
      const err = document.createElement("small");
      err.textContent = item.error;
      name.append(err);
    }
    cell(row, item.category);
    cell(row, item.state, "state-" + item.state);
    const prog = cell(row, undefined, "num");
    const bar = document.createElement("progress");
    bar.max = 1;
    bar.value = item.progress;
    prog.append(bar, " " + Math.round(item.progress * 100) + "%");
    cell(row, size(item.size), "num");
    cell(row, undefined, "actions").append(
      removeButton(item, "Remove", false),
      removeButton(item, "Remove + files", true),
    );
  }
}

async function refresh() {
  try {
    render(await api("/api/downloads"));
  } catch (e) {
    say("Cannot reach the connector: " + e.message, "error");
  }
}

$("add").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const link = $("link").value.trim();
  const files = $("file").files;
  const category = $("category").value;
  if (!link && files.length === 0) {
    say("Paste a magnet or choose a .torrent file.", "error");
    return;
  }
  const button = ev.target.querySelector("button[type=submit]");
  button.disabled = true;
  try {
    if (link) {
      await api("/api/downloads", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ magnet: link, category }),
      });
    }
    if (files.length > 0) {
      const form = new FormData();
      form.append("category", category);
      for (const f of files) form.append("torrent", f);
      await api("/api/downloads/torrent", { method: "POST", body: form });
    }
    localStorage.setItem("category", category);
    $("link").value = "";
    $("file").value = "";
    say("Added to " + category + ".", "ok");
    refresh();
  } catch (e) {
    say("Could not add: " + e.message, "error");
  } finally {
    button.disabled = false;
  }
});

loadCategories().catch((e) => say("Cannot reach the connector: " + e.message, "error"));
refresh();
setInterval(refresh, 3000);
