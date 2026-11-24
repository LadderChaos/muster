"use strict";
// Muster web app: reads the snapshots `muster serve` streams and sends messages to open agents.
// Views are routed by the hash: #/chats, #/chat/<id>, #/agents, #/agent/<pane>.

const TOKEN_KEY = "muster.token";
const SUBAGENTS_KEY = "muster.subagents";
/// Identity hues, as in the notch; red is left out because it marks failed messages.
const HUES = [190, 28, 268, 140, 335, 52, 218, 300, 165, 12, 245, 88];
/// Without even a keepalive line for this long, the stream is taken as dead.
const STALE_MS = 40000;

let token = localStorage.getItem(TOKEN_KEY);
let snapshot = null;
let connection = "connecting";
let lastData = 0;
let controller = null;
let retryMs = 1000;
let retryTimer = null;
let renderedRoute = "";

const $ = (id) => document.getElementById(id);

// A token in the address (#token=…) is kept and removed from the address.
{
  const match = location.hash.match(/token=([0-9a-f]+)/i);
  if (match) {
    token = match[1];
    localStorage.setItem(TOKEN_KEY, token);
    history.replaceState(null, "", "/#/chats");
  }
}

function esc(text) {
  return String(text ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

function route() {
  const [, view = "chats", arg] = location.hash.match(/^#\/([a-z]+)(?:\/(.+))?/) || [];
  return { view, arg: arg && decodeURIComponent(arg) };
}

// ---- Model helpers ----

function agentByPane(pane) {
  return snapshot?.agents.find((a) => a.pane === pane);
}

function key(node) {
  return node.subagent ? `${node.pane}/${node.subagent}` : String(node.pane);
}

function name(node) {
  if (node.subagent) return node.subagent.split(".").pop();
  return agentByPane(node.pane)?.label || "External";
}

function hue(node) {
  let n = node.pane % HUES.length;
  if (node.subagent) for (const c of node.subagent) n = (n * 31 + c.charCodeAt(0)) % HUES.length;
  return `hsl(${HUES[n]} 94% 68%)`;
}

function stateWord(agent) {
  if (!agent.open) return "closed";
  return agent.status || (agent.working ? "working" : "idle");
}

function address(agent) {
  return agent.address || String(agent.pane);
}

function ago(ms) {
  if (ms == null) return "";
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

function quiet(ms) {
  return ago(Date.now() - ms);
}

function clock(ms) {
  if (ms == null) return "";
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function delivery(m) {
  if (m.failed) return ["failed", "✗ failed"];
  if (m.delivered_ms != null) return ["delivered", "✓ delivered"];
  if (!m.delivery_observable) return ["sent", "sent"];
  return ["awaiting", "… awaiting delivery"];
}

function showSubagents() {
  return snapshot?.backend === "tern" && localStorage.getItem(SUBAGENTS_KEY) === "1";
}

/// Conversations, newest first. Without subagents, only messages between distinct top-level panes
/// count, as in the notch. A repeated message is folded into its first copy.
function conversations() {
  if (!snapshot) return [];
  const subs = showSubagents();
  const repeats = new Map();
  for (const m of snapshot.messages) {
    if (m.repeat_of != null) repeats.set(m.repeat_of, (repeats.get(m.repeat_of) || 1) + 1);
  }
  const byId = new Map();
  snapshot.messages.forEach((m, index) => {
    if (m.repeat_of != null) return;
    if (!subs && (m.from.subagent || m.to.subagent || m.from.pane === m.to.pane)) return;
    const a = key(m.from), b = key(m.to);
    if (a === b) return;
    const id = a < b ? `${a}|${b}` : `${b}|${a}`;
    let c = byId.get(id);
    if (!c) {
      c = { id, left: m.from, right: m.to, messages: [], latest: 0 };
      byId.set(id, c);
    }
    c.messages.push({ ...m, copies: repeats.get(m.id) || 1 });
    c.latest = index;
  });
  return [...byId.values()].sort((x, y) => y.latest - x.latest);
}

// ---- Rendering ----

function render() {
  const r = route();
  const routeKey = `${r.view}/${r.arg ?? ""}`;
  const fresh = routeKey !== renderedRoute;
  renderedRoute = routeKey;

  document.querySelectorAll("#tabs a").forEach((a) => {
    const tab = a.dataset.tab;
    a.classList.toggle("on", tab === r.view || (tab === "chats" && r.view === "chat") || (tab === "agents" && r.view === "agent"));
  });
  renderHeader();

  const main = $("main");
  const atBottom = window.innerHeight + window.scrollY >= document.body.scrollHeight - 40;
  let composerFor = null;
  if (!token) {
    main.innerHTML = setupView();
    bindSetup();
  } else if (!snapshot) {
    main.innerHTML = `<div class="empty">${connection === "offline" ? "Cannot reach muster serve. Retrying…" : "Connecting…"}</div>`;
  } else if (snapshot.loading) {
    main.innerHTML = `<div class="empty">Loading ${esc(snapshot.backend)}…</div>`;
  } else if (r.view === "chat") {
    main.innerHTML = chatView(r.arg);
  } else if (r.view === "agents") {
    main.innerHTML = agentsView();
  } else if (r.view === "agent") {
    const agent = agentByPane(Number(r.arg));
    main.innerHTML = agentView(agent);
    if (agent?.open) composerFor = agent;
  } else {
    main.innerHTML = chatsView();
    const toggle = $("subs");
    if (toggle) toggle.onchange = () => {
      localStorage.setItem(SUBAGENTS_KEY, toggle.checked ? "1" : "0");
      render();
    };
  }
  setComposer(composerFor);
  if (r.view === "chat" && (fresh || atBottom)) window.scrollTo(0, document.body.scrollHeight);
  else if (fresh) window.scrollTo(0, 0);
}

/// The window after the shown one, in the notch's order.
const NEXT_BACKEND = { tern: "herdr", herdr: "orca", orca: "tern" };

$("backend").addEventListener("click", async () => {
  if (!snapshot || !token) return;
  const next = NEXT_BACKEND[snapshot.backend] || "tern";
  const button = $("backend");
  button.disabled = true;
  try {
    const res = await fetch("/api/backend", {
      method: "POST",
      headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
      body: JSON.stringify({ backend: next }),
    });
    const reply = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(reply.error || `HTTP ${res.status}`);
    // Conversations and agents belong to the window; start from its chats.
    if (route().view !== "chats") location.hash = "#/chats";
  } catch (error) {
    $("banner").hidden = false;
    $("banner").textContent = `Cannot switch to ${next}: ${error.message}`;
  } finally {
    button.disabled = false;
  }
});

function renderHeader() {
  $("backend").textContent = snapshot ? `${snapshot.backend.toUpperCase()} ▸` : "";
  $("backend").title = snapshot ? `Switch to ${NEXT_BACKEND[snapshot.backend]}` : "";
  const conn = $("conn");
  conn.className = `conn ${connection}`;
  conn.textContent = connection;
  if (!snapshot) {
    $("summary").textContent = "";
    $("banner").hidden = true;
    return;
  }
  if (snapshot.loading) {
    $("summary").textContent = "loading…";
    $("banner").hidden = true;
    return;
  }
  const agents = snapshot.agents.filter((a) => a.open && (a.cli || a.transcript || a.status));
  const count = (word) => agents.filter((a) => stateWord(a) === word).length;
  const parts = [`${count("working")} working`];
  if (count("blocked")) parts.push(`${count("blocked")} blocked`);
  parts.push(`${agents.length - count("working") - count("blocked")} idle`);
  $("summary").textContent = parts.join(" · ");
  $("banner").hidden = !snapshot.error;
  $("banner").textContent = snapshot.error || "";
}

function setupView() {
  return `<div class="setup">
    <p>Paste the token that <code>muster serve</code> printed on your Mac.</p>
    <input id="tokenInput" autocomplete="off" autocapitalize="off" spellcheck="false" placeholder="token">
    <button id="tokenSave" type="button">Connect</button>
  </div>`;
}

function bindSetup() {
  $("tokenSave").onclick = () => {
    const value = $("tokenInput").value.trim().replace(/^.*token=/, "");
    if (!value) return;
    token = value;
    localStorage.setItem(TOKEN_KEY, token);
    connect();
    render();
  };
}

function chatsView() {
  const list = conversations();
  const toggle = snapshot.backend === "tern"
    ? `<label class="toggle"><input type="checkbox" id="subs" ${showSubagents() ? "checked" : ""}> Subagents</label>`
    : "";
  if (!list.length) return `${toggle}<div class="empty">No messages between agents yet.</div>`;
  return toggle + list.map((c) => {
    const last = c.messages[c.messages.length - 1];
    const lastName = name(last.from);
    return `<a class="row" href="#/chat/${encodeURIComponent(c.id)}">
      <div class="head">
        <span style="color:${hue(c.left)}">${esc(name(c.left))}</span>
        <span class="count">${c.messages.length}</span>
        <span style="color:${hue(c.right)}">${esc(name(c.right))}</span>
        <span class="when">${ago(last.sent_ms ?? last.delivered_ms)}</span>
      </div>
      <div class="sub">${esc(lastName)}: ${esc(last.text)}</div>
    </a>`;
  }).join("");
}

function messageView(m, leftKey) {
  const [state, label] = delivery(m);
  const side = key(m.from) === leftKey ? "left" : "right";
  const tags = [];
  if (m.reply) tags.push("reply");
  if (m.kind === "spawn" || m.kind === "yield" || m.kind === "irc") tags.push(m.kind);
  if (m.copies > 1) tags.push(`×${m.copies}`);
  return `<div class="msg ${side} ${m.failed ? "failed" : ""}">
    <div class="meta">
      <span style="color:${hue(m.from)}">${esc(name(m.from))}</span>
      <span>→ ${esc(name(m.to))}</span>
      <span>${clock(m.sent_ms ?? m.delivered_ms)}</span>
      ${tags.map((t) => `<span class="tag">${esc(t)}</span>`).join("")}
      <span class="delivery ${state}">${label}${m.outcome ? ` · ${esc(m.outcome)}` : ""}</span>
    </div>
    <div class="body">${esc(m.text)}</div>
  </div>`;
}

function chatView(id) {
  const c = conversations().find((x) => x.id === id);
  if (!c) return `<a class="back" href="#/chats">‹ Chats</a><div class="empty">This conversation is gone.</div>`;
  const link = (node) => node.subagent
    ? `<span style="color:${hue(node)}">${esc(name(node))}</span>`
    : `<a href="#/agent/${node.pane}" style="color:${hue(node)}">${esc(name(node))}</a>`;
  return `<a class="back" href="#/chats">‹ Chats</a>
    <div class="title">${link(c.left)} <span class="count">${c.messages.length}</span> ${link(c.right)}</div>
    ${c.messages.map((m) => messageView(m, key(c.left))).join("")}`;
}

function agentsView() {
  // Orca mail sent from outside a terminal comes from `muster`: the person, not an agent.
  const agents = snapshot.agents.filter((a) => a.address !== "muster").sort((a, b) => {
    const rank = (x) => (!x.open ? 3 : stateWord(x) === "working" ? 0 : stateWord(x) === "blocked" ? 1 : 2);
    return rank(a) - rank(b) || a.label.localeCompare(b.label);
  });
  if (!agents.length) return `<div class="empty">No agents.</div>`;
  return agents.map((a) => {
    const word = stateWord(a);
    const doing = a.tool ? (a.tool.intent || a.tool.name) : a.description;
    return `<a class="row ${a.open ? "" : "closed"}" href="#/agent/${a.pane}">
      <div class="head">
        <span style="color:${hue({ pane: a.pane })}">${esc(a.label)}</span>
        <span class="status ${esc(word)}">${esc(word)}</span>
        <span class="when">${a.open && !a.working ? quiet(a.quiet_ms) : ""}</span>
      </div>
      <div class="sub">${esc([a.cli, a.session].filter(Boolean).join(" · "))}${doing ? ` — ${esc(doing)}` : ""}</div>
    </a>`;
  }).join("");
}

function agentView(agent) {
  if (!agent) return `<a class="back" href="#/agents">‹ Agents</a><div class="empty">This agent is gone.</div>`;
  const word = stateWord(agent);
  const facts = [
    [agent.cli, agent.model].filter(Boolean).join(" · "),
    agent.session && `workspace ${agent.session}`,
    agent.cwd,
    `address ${address(agent)}`,
    agent.open && !agent.working && `quiet ${quiet(agent.quiet_ms)}`,
    agent.tool && `now: ${agent.tool.intent || agent.tool.name}`,
    agent.todo && `todo ${agent.todo.done}/${agent.todo.total}`,
    agent.tokens ? `${agent.tokens.toLocaleString()} tokens` : "",
    agent.subagents.length && `subagents: ${agent.subagents.map((s) => `${s.name.split(".").pop()}${s.running ? " (running)" : ""}`).join(", ")}`,
  ].filter(Boolean);
  const mine = conversations().filter((c) => c.left.pane === agent.pane || c.right.pane === agent.pane);
  const recent = snapshot.messages
    .filter((m) => m.repeat_of == null && (m.from.pane === agent.pane || m.to.pane === agent.pane))
    .slice(-20);
  return `<a class="back" href="#/agents">‹ Agents</a>
    <div class="title"><span style="color:${hue({ pane: agent.pane })}">${esc(agent.label)}</span>
      <span class="status ${esc(word)}">${esc(word)}</span></div>
    <div class="facts">${facts.map((f) => `<div>${esc(f)}</div>`).join("")}</div>
    ${mine.length ? `<div class="section">CONVERSATIONS</div>` + mine.map((c) => {
      const other = c.left.pane === agent.pane && !c.left.subagent ? c.right : c.left;
      return `<a class="row" href="#/chat/${encodeURIComponent(c.id)}"><div class="head">
        <span style="color:${hue(other)}">${esc(name(other))}</span><span class="count">${c.messages.length}</span>
        <span class="when">${ago(c.messages[c.messages.length - 1].sent_ms)}</span></div></a>`;
    }).join("") : ""}
    ${recent.length ? `<div class="section">RECENT</div>` + recent.map((m) => messageView({ ...m, copies: 1 }, key({ pane: agent.pane }))).join("") : ""}
    ${agent.open ? "" : `<div class="empty">The ${snapshot.backend === "tern" ? "pane" : "terminal"} is closed.</div>`}`;
}

// ---- Composer ----

let composerTarget = null;

function setComposer(agent) {
  const form = $("composer");
  const next = agent ? address(agent) : null;
  if (next !== composerTarget) {
    composerTarget = next;
    $("sendStatus").textContent = "";
    $("sendStatus").className = "sendStatus";
  }
  form.hidden = !agent;
  document.body.classList.toggle("composing", !!agent);
  if (agent) $("text").placeholder = `Message ${agent.label}`;
}

$("text").addEventListener("input", (e) => {
  e.target.style.height = "auto";
  e.target.style.height = `${e.target.scrollHeight}px`;
});

$("composer").addEventListener("submit", async (e) => {
  e.preventDefault();
  const text = $("text").value.trim();
  if (!text || !composerTarget) return;
  const status = $("sendStatus");
  $("send").disabled = true;
  status.className = "sendStatus";
  status.textContent = "Sending…";
  try {
    const res = await fetch("/api/send", {
      method: "POST",
      headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
      body: JSON.stringify({ backend: snapshot.backend, address: composerTarget, text }),
    });
    const reply = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(reply.error || `HTTP ${res.status}`);
    $("text").value = "";
    $("text").style.height = "auto";
    status.textContent = "Sent";
  } catch (error) {
    status.className = "sendStatus error";
    status.textContent = `Not sent: ${error.message}`;
  } finally {
    $("send").disabled = false;
  }
});

// ---- Connection ----

function setConnection(state) {
  if (connection === state) return;
  connection = state;
  render();
}

function scheduleRetry() {
  clearTimeout(retryTimer);
  retryTimer = setTimeout(connect, retryMs);
  retryMs = Math.min(retryMs * 2, 15000);
}

async function connect() {
  clearTimeout(retryTimer);
  controller?.abort();
  if (!token) return;
  const own = new AbortController();
  controller = own;
  setConnection("connecting");
  try {
    const res = await fetch("/api/stream", {
      headers: { Authorization: `Bearer ${token}` },
      cache: "no-store",
      signal: own.signal,
    });
    if (res.status === 401) {
      token = null;
      localStorage.removeItem(TOKEN_KEY);
      snapshot = null;
      setConnection("offline");
      render();
      return;
    }
    if (!res.ok || !res.body) throw new Error(`HTTP ${res.status}`);
    lastData = Date.now();
    retryMs = 1000;
    const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
    let buffer = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      lastData = Date.now();
      buffer += value;
      let newline;
      let latest = null;
      while ((newline = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, newline).trim();
        buffer = buffer.slice(newline + 1);
        if (line) latest = line;
      }
      if (latest) {
        snapshot = JSON.parse(latest);
        connection = "live";
        render();
      } else {
        setConnection("live");
      }
    }
  } catch (error) {
    if (own.signal.aborted && controller !== own) return;
  }
  if (controller !== own) return;
  setConnection("offline");
  scheduleRetry();
}

// A stream that went quiet (a phone that slept, a dropped network) is restarted.
setInterval(() => {
  if (token && connection === "live" && Date.now() - lastData > STALE_MS) connect();
  if (snapshot && (route().view === "chats" || route().view === "agents")) renderHeader();
}, 5000);

document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible" && token && (connection !== "live" || Date.now() - lastData > 20000)) {
    retryMs = 1000;
    connect();
  }
});

window.addEventListener("hashchange", render);

if ("serviceWorker" in navigator) navigator.serviceWorker.register("/sw.js").catch(() => {});

render();
connect();
