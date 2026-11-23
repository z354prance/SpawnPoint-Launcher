"use strict";
// The window. Everything that touches files, downloads or Prism happens in src-tauri/src/lib.rs.
const T = window.__TAURI__;
const call = (cmd, args) => T.core.invoke(cmd, args);
const $ = (id) => document.getElementById(id);
const esc = (t) => String(t ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const LOADERS = { neoforge: "NeoForge", forge: "Forge", fabric: "Fabric", quilt: "Quilt" };

let servers = [];
let current = null;          // selected server id, or null for "add a server"
const info = {};             // id -> latest details from its page
const busy = {};             // id -> { label, done, total } while getting ready
let pollTimer = null;

function toast(msg, bad) {
  const t = $("toast");
  t.textContent = msg; t.className = "toast" + (bad ? " err" : ""); t.hidden = false;
  clearTimeout(toast.h); toast.h = setTimeout(() => (t.hidden = true), bad ? 20000 : 5000);
  t.onclick = () => (t.hidden = true);
}

// A yes/no question inside the window (the webview's own confirm() differs by system).
function ask(msg, yes) {
  return new Promise((done) => {
    const d = document.createElement("dialog");
    d.innerHTML = `<p>${esc(msg)}</p><form method="dialog" class="row"><button class="quiet" value="no">Cancel</button><button class="go" value="yes">${esc(yes)}</button></form>`;
    document.body.append(d);
    d.addEventListener("close", () => { done(d.returnValue === "yes"); d.remove(); });
    d.showModal(); d.querySelector(".go").focus();
  });
}

function drawList() {
  $("list").innerHTML = servers.map((s) => `<button data-id="${esc(s.id)}" aria-current="${s.id === current}">
    <span class="dot ${info[s.id]?.online ? "on" : ""}" aria-hidden="true"></span><span class="nm">${esc(s.name)}</span></button>`).join("");
  for (const b of $("list").querySelectorAll("button")) b.onclick = () => select(b.dataset.id);
}

function drawAdd() {
  $("main").innerHTML = `<div class="empty">
    <h1>${servers.length ? "Add a server" : "Welcome"}</h1>
    <p>Paste the link the server's owner gave you. It looks like <span class="addr">https://…/p/…</span></p>
    <form class="field" id="addform"><input id="link" placeholder="Server link" aria-label="Server link" autocomplete="off" spellcheck="false" autofocus>
    <button class="go" type="submit">Add</button></form>
    <p class="note" id="adderr"></p>
    <p class="note">The first time you play, SpawnPoint gets Prism Launcher (free and open source), which runs the game. You sign in with your Microsoft account there once.</p></div>`;
  $("addform").onsubmit = async (e) => {
    e.preventDefault();
    const link = $("link").value.trim(); if (!link) return;
    $("adderr").textContent = "Checking the link…"; $("adderr").className = "note";
    try {
      const s = await call("add_server", { link });
      servers = await call("list_servers");
      select(s.id);
    } catch (err) { $("adderr").textContent = String(err); $("adderr").className = "note err"; }
  };
}

function drawServer() {
  const s = servers.find((x) => x.id === current); if (!s) return drawAdd();
  const i = info[s.id], b = busy[s.id];
  const ld = i ? LOADERS[i.loader] || i.loader : "";
  const facts = i ? [
    [`<b>${i.online ? "Online" : "Offline"}</b>`, i.online ? "join any time" : "may wake up when you join", "status" + (i.online ? " on" : "")],
    [`<b>Minecraft ${esc(i.mc_version)}</b>`, ld ? `${esc(ld)} ${esc(i.loader_version || "")}` : "", ""],
    [`<b>${i.mods} mods</b>`, i.sync ? "kept up to date when you play or join" : "kept up to date when you play", ""],
    ...(i.pack?.title ? [[`<b>${esc(i.pack.title)}</b>`, i.pack.version ? `version ${esc(i.pack.version)}` : "modpack", ""]] : []),
  ] : [];
  const pct = b?.total ? Math.round((b.done / b.total) * 100) : null;
  $("main").innerHTML = `
    <h1>${esc(s.name)}</h1>
    <div class="addr">${esc(s.address)} <button class="link" id="copy">Copy</button></div>
    <div class="facts">${i ? facts.map(([a, c, k]) => `<div class="fact ${k}">${a}<span>${c}</span></div>`).join("")
      : `<div class="fact"><span id="infoerr">${info[s.id] === undefined ? "Checking the server…" : ""}</span></div>`}</div>
    <button class="play" id="play" ${b ? "disabled" : ""}>${b ? "Getting ready…" : "Play"}</button>
    <div class="prog" aria-live="polite">${b ? `<div class="lbl">${esc(b.label)}${pct !== null ? ` · ${pct}%` : ""}</div>
      <div class="bar ${pct === null ? "busy" : ""}"><i style="${pct !== null ? `width:${pct}%` : ""}"></i></div>` : `<div class="lbl" id="after"></div>`}</div>
    <div class="foot"><button class="link" id="reset">Get the server's setup again</button><button class="link" id="remove">Remove from this list</button></div>`;
  $("copy").onclick = () => navigator.clipboard.writeText(s.address).then(() => toast("Address copied"));
  $("play").onclick = () => play(s);
  $("reset").onclick = async () => {
    if (!(await ask("Get this server's setup again? Your game options and screenshots are kept.", "Get it again"))) return;
    try { await call("reset_instance", { id: s.id }); toast("Done. Press Play to get it again."); } catch (e) { toast(String(e), true); }
  };
  $("remove").onclick = async () => {
    if (!(await ask(`Remove ${s.name} from this list? You can add it again with its link.`, "Remove"))) return;
    await call("remove_server", { id: s.id });
    servers = await call("list_servers"); current = servers[0]?.id ?? null; render();
  };
}

function render() { drawList(); current ? drawServer() : drawAdd(); }

async function refresh(id) {
  try { info[id] = await call("server_info", { id }); }
  catch (e) { info[id] = null; if (id === current && !busy[id]) { render(); const el = $("infoerr"); if (el) { el.textContent = String(e); el.className = "err"; } return; } }
  if (!busy[id]) render(); else drawList();
}

function select(id) {
  current = id; render();
  refresh(id);
  clearInterval(pollTimer); pollTimer = setInterval(() => current && refresh(current), 20000);
}

async function play(s) {
  busy[s.id] = { label: "Getting ready", done: 0, total: null }; render();
  try {
    await call("play", { id: s.id });
    delete busy[s.id]; render();
    const a = $("after"); if (a) a.textContent = "Minecraft is starting. Mod updates are checked first, so after a big change it can take a minute.";
    setTimeout(() => { const a = $("after"); if (a) a.textContent = ""; }, 30000);
  } catch (e) { delete busy[s.id]; render(); toast(String(e), true); }
}

T.event.listen("progress", ({ payload: p }) => {
  if (!busy[p.id]) return;
  busy[p.id] = p;
  if (p.id === current) drawServer();
});

T.event.listen("warning", ({ payload }) => toast(String(payload), true));

$("addbtn").onclick = () => { current = null; render(); };
$("prismbtn").onclick = () => call("open_prism").catch((e) => toast(String(e), true));

(async () => {
  servers = await call("list_servers");
  current = servers[0]?.id ?? null;
  render();
  for (const s of servers) refresh(s.id);
  if (current) select(current);
})();
