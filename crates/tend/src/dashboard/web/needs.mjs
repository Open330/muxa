// "Needs you": the decision queue, phone-first. Reads /api/decisions and
// /api/decisions/history with the operator's credential: the dashboard
// token kept by the main page, or the OIDC operator session cookie.
// Everything an agent wrote is rendered with textContent, never as HTML.

import { decidedCard, pendingCard } from "./needs-model.mjs";

const TOKEN_KEY = "tend.token";
const REFRESH_MS = 10000;

const $ = (id) => document.getElementById(id);
const state = { tab: "queue", timer: null };

function authHeaders() {
  let token = null;
  try {
    token = localStorage.getItem(TOKEN_KEY);
  } catch {
    // Storage blocked: the session cookie may still authorize.
  }
  return token ? { Authorization: `Bearer ${token}` } : {};
}

async function read(path) {
  const resp = await fetch(path, { credentials: "same-origin", headers: authHeaders() });
  if (resp.status === 401 || resp.status === 403) {
    const error = new Error(resp.status === 403 ? "signed in without operator access" : "sign in to see what needs you");
    error.auth = true;
    throw error;
  }
  if (!resp.ok) throw new Error(`${path} → ${resp.status}`);
  return resp.json();
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}

function renderCard(card) {
  const li = el("li", "card");
  li.dataset.id = card.id;
  li.append(el("div", `kind ${card.kind}`, card.label));
  li.append(el("h2", null, card.title));
  li.append(el("div", "meta", card.meta));
  if (card.needed) li.append(el("p", "needed", card.needed));
  if (card.options.length) {
    const list = el("ol", "options");
    for (const option of card.options) list.append(el("li", null, option));
    li.append(list);
  }
  if (card.recommendation) li.append(el("p", "rec", card.recommendation));
  if (card.evidence.length) {
    const list = el("ul", "evidence");
    for (const line of card.evidence) {
      const item = el("li", line.cls || null);
      if (line.href) {
        const link = el("a", null, line.text);
        link.href = line.href;
        link.rel = "noopener noreferrer";
        link.target = "_blank";
        item.append(link);
      } else {
        item.textContent = line.text;
      }
      if (line.tail) item.append(el("pre", null, line.tail));
      list.append(item);
    }
    li.append(list);
  }
  if (card.pane) {
    const link = el("a", "meta", "open the pane");
    link.href = `/#pane=${encodeURIComponent(card.pane)}`;
    li.append(link);
  }
  if (card.outcome) li.append(el("p", "outcome", card.outcome));
  return li;
}

function renderList(list, cards, empty) {
  list.replaceChildren(...cards.map(renderCard));
  if (!cards.length) list.replaceChildren(el("li", "card meta", empty));
}

function unavailableNote(unavailable, truncated) {
  const notes = [];
  if (truncated) notes.push("more than shown");
  for (const source of unavailable || []) notes.push(`unavailable: ${source}`);
  return notes.join(" · ");
}

async function refresh() {
  const status = $("status");
  try {
    if (state.tab === "queue") {
      const data = await read("/api/decisions");
      const cards = (data.queue?.items || []).map((item) => pendingCard(item)).filter(Boolean);
      renderList($("queue"), cards, "Nothing is waiting on you.");
      $("count").textContent = cards.length ? `${cards.length} waiting` : "";
      status.textContent = unavailableNote(data.queue?.unavailable, data.queue?.truncated);
      $("covers").textContent = `Covers ${data.covers || "this host"}.`;
    } else {
      const data = await read("/api/decisions/history");
      const cards = (data.history?.items || []).map((item) => decidedCard(item)).filter(Boolean);
      renderList($("history"), cards, "No decisions recorded yet.");
      status.textContent = unavailableNote(data.history?.unavailable, data.history?.truncated);
      $("covers").textContent = `Covers ${data.covers || "this host"}.`;
    }
    $("login").hidden = true;
  } catch (error) {
    status.textContent = error.message;
    if (error.auth) {
      const login = $("login");
      login.href = `/auth/login?return_to=${encodeURIComponent("/needs")}`;
      login.hidden = false;
    }
  }
}

function select(tab) {
  state.tab = tab;
  $("tab-queue").setAttribute("aria-selected", String(tab === "queue"));
  $("tab-history").setAttribute("aria-selected", String(tab === "history"));
  $("queue").hidden = tab !== "queue";
  $("history").hidden = tab !== "history";
  refresh();
}

$("tab-queue").addEventListener("click", () => select("queue"));
$("tab-history").addEventListener("click", () => select("history"));
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) refresh();
});
state.timer = setInterval(() => {
  if (!document.hidden) refresh();
}, REFRESH_MS);
refresh();
