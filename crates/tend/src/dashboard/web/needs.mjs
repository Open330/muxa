// "Needs you": the decision queue, phone-first. Reads /api/decisions and
// /api/decisions/history with the operator's credential: the dashboard
// token kept by the main page, or the OIDC operator session cookie.
// Everything an agent wrote is rendered with textContent, never as HTML.

import { decidedCard, draftKey, hostSections, linkedItemOf, needsReturnPath } from "./needs-model.mjs";
import { withPaneHash } from "./pane-drawer.mjs";

const TOKEN_KEY = "tend.token";
const REFRESH_MS = 10000;

const $ = (id) => document.getElementById(id);
const state = { tab: "queue", timer: null, generation: 0, revealed: false };
/// Unsent answers and notes by card, kept across re-renders.
const drafts = new Map();


function authHeaders() {
  let token = null;
  try {
    token = localStorage.getItem(TOKEN_KEY);
  } catch {
    // Storage blocked: the session cookie may still authorize.
  }
  return token ? { Authorization: `Bearer ${token}` } : {};
}

/// A state change: the operator's credential plus the header the server
/// requires before a session cookie may change anything.
async function write(path, body) {
  const resp = await fetch(path, {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json", "X-Tend-Operator": "1", ...authHeaders() },
    body: JSON.stringify(body),
  });
  const payload = await resp.json().catch(() => null);
  // The auth gate answers with a bare status; a handler's own refusal
  // ("not addressed to you") carries a JSON error and keeps the page.
  if (resp.status === 401 || (resp.status === 403 && !payload?.error)) {
    const error = new Error("signed in without operator access");
    error.auth = true;
    throw error;
  }
  if (!resp.ok) throw new Error(payload?.error || `${resp.status}`);
  return payload;
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
  if (card.host) li.dataset.host = card.host;
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
    // Pane ids repeat across tmux servers: the socket picks the right one.
    link.href = `/${withPaneHash("", { pane: card.pane, socket: card.socket })}`;
    li.append(link);
  }
  if (card.outcome) li.append(el("p", "outcome", card.outcome));
  if (state.tab === "queue" && card.actionable !== false) li.append(actions(card));
  return li;
}

function button(label, onClick, className) {
  const node = el("button", className || null, label);
  node.type = "button";
  node.addEventListener("click", onClick);
  return node;
}

/// What the person can do from the card: answer a question (an option
/// fills the answer), or accept / send back / withdraw a result.
function actions(card) {
  const box = el("form", "actions");
  box.addEventListener("submit", (event) => event.preventDefault());
  const note = el("textarea", "note");
  note.rows = 2;
  note.value = drafts.get(draftKey(card)) || "";
  note.addEventListener("input", () => {
    if (note.value) drafts.set(draftKey(card), note.value);
    else drafts.delete(draftKey(card));
  });
  note.maxLength = card.kind === "ask" ? 16384 : 1024;
  const message = el("p", "result");
  message.setAttribute("role", "status");
  const busy = (on) => box.querySelectorAll("button").forEach((b) => (b.disabled = on));
  const send = async (path, body, done) => {
    busy(true);
    message.textContent = "Sending…";
    try {
      await write(path, body);
      message.textContent = done;
      drafts.delete(draftKey(card));
      setTimeout(refresh, 800);
    } catch (error) {
      if (error.auth) return lostAccess(error);
      message.textContent = `Not recorded: ${error.message}`;
      busy(false);
    }
  };
  if (card.kind === "ask") {
    note.placeholder = "Your answer";
    const options = el("div", "choices");
    for (const option of card.options) options.append(button(option, () => {
      note.value = option;
      drafts.set(draftKey(card), option);
    }));
    if (card.options.length) box.append(options);
    box.append(note);
    const path = `/api/decisions/requests/${encodeURIComponent(card.id)}`;
    // A question on another Fleet host is answered there, over Fleet.
    const withHost = (body) => (card.remote ? { ...body, host: card.host } : body);
    box.append(
      button("Send answer", () => {
        if (!note.value.trim()) return (message.textContent = "Write an answer first.");
        send(path, withHost({ answer: note.value }), "Answered.");
      }, "primary"),
      button("Decline", () => {
        if (!note.value.trim()) return (message.textContent = "Say why you decline.");
        send(path, withHost({ answer: note.value, declined: true }), "Declined.");
      }),
    );
  } else if (card.kind === "review") {
    note.placeholder = "Note (needed to send back)";
    const path = `/api/decisions/reviews/${encodeURIComponent(card.id)}`;
    const decide = (disposition, option, done) => {
      const body = { disposition };
      if (note.value.trim()) body.note = note.value;
      if (option) body.option = option;
      send(path, body, done);
    };
    const options = el("div", "choices");
    for (const option of card.options) {
      options.append(button(option, () => decide("accepted", option, `Accepted: ${option}.`)));
    }
    if (card.options.length) box.append(options);
    box.append(
      note,
      button("Looks good", () => decide("accepted", null, "Accepted."), "primary"),
      button("Send back", () => {
        if (!note.value.trim()) return (message.textContent = "Say what to change.");
        decide("returned", null, "Sent back.");
      }),
      button("Withdraw", () => decide("withdrawn", null, "Withdrawn.")),
    );
  } else {
    return el("span");
  }
  box.append(message);
  return box;
}

function renderList(list, cards, empty) {
  list.replaceChildren(...cards.map(renderCard));
  if (!cards.length) list.replaceChildren(el("li", "card meta", empty));
}

/// One heading per host (only when there is more than one), what it
/// covers and what could not be read, then its cards.
function renderSections(list, sections) {
  const nodes = [];
  const several = sections.length > 1;
  for (const section of sections) {
    if (several || section.notes.length) {
      const head = el("li", "host");
      if (several) head.append(el("h2", null, section.alias));
      if (several && section.covers.length) head.append(el("div", "meta", section.covers.join(" · ")));
      for (const note of section.notes) head.append(el("div", "meta", note));
      nodes.push(head);
    }
    nodes.push(...section.cards.map(renderCard));
  }
  const total = sections.reduce((sum, section) => sum + section.cards.length, 0);
  if (!total) nodes.push(el("li", "card meta", "Nothing is waiting on you."));
  list.replaceChildren(...nodes);
}

function unavailableNote(unavailable, truncated) {
  const notes = [];
  if (truncated) notes.push("more than shown");
  for (const source of unavailable || []) notes.push(`unavailable: ${source}`);
  return notes.join(" · ");
}

/// Drop everything private on the page: after a lost session nothing an
/// operator could read may stay on screen.
function forget() {
  $("queue").replaceChildren();
  $("history").replaceChildren();
  $("count").textContent = "";
  $("covers").textContent = "";
}

/// Signed out, expired or downgraded: nothing private stays on the page,
/// drafts included, and no older read may bring it back.
function lostAccess(error) {
  state.generation += 1;
  drafts.clear();
  forget();
  $("status").textContent = error.message;
  const login = $("login");
  // Back to the same card after signing in.
  login.href = `/auth/login?return_to=${encodeURIComponent(needsReturnPath(window.location.hash))}`;
  login.hidden = false;
}

function linkedItem() {
  return linkedItemOf(window.location.hash);
}

/// Bring the linked card into view once per visit: only on the queue tab,
/// never under someone typing, and only counted once the card was there
/// (an unauthorized or empty first read does not use it up).
function revealLinkedItem() {
  const id = linkedItem();
  if (!id || state.revealed || state.tab !== "queue" || composing()) return;
  const card = [...document.querySelectorAll("#queue .card")].find((node) => node.dataset.id === id);
  if (!card) return;
  state.revealed = true;
  card.classList.add("linked");
  card.scrollIntoView({ block: "center" });
}

async function refresh() {
  const status = $("status");
  // A later refresh (or an auth failure) wins over one still in flight.
  const generation = ++state.generation;
  const current = () => generation === state.generation;
  try {
    if (state.tab === "queue") {
      const data = await read("/api/decisions");
      if (!current()) return;
      // Fetched (so a lost session shows), but not re-rendered under the
      // cursor; drafts survive the next render either way.
      if (composing()) return;
      const sections = hostSections(data);
      renderSections($("queue"), sections);
      const total = sections.reduce((sum, section) => sum + section.cards.length, 0);
      $("count").textContent = total ? `${total} waiting` : "";
      status.textContent = "";
      $("covers").textContent = `Covers ${sections.map((s) => s.alias).join(", ")}.`;
    } else {
      const data = await read("/api/decisions/history");
      if (!current()) return;
      const cards = (data.history?.items || []).map((item) => decidedCard(item)).filter(Boolean);
      renderList($("history"), cards, "No decisions recorded yet.");
      status.textContent = unavailableNote(data.history?.unavailable, data.history?.truncated);
      $("covers").textContent = `Covers ${data.covers || "this host"}.`;
    }
    $("login").hidden = true;
  } catch (error) {
    if (!current()) return;
    if (error.auth) return lostAccess(error);
    status.textContent = error.message;
  }
}

function select(tab) {
  state.tab = tab;
  $("tab-queue").setAttribute("aria-selected", String(tab === "queue"));
  $("tab-history").setAttribute("aria-selected", String(tab === "history"));
  $("queue").hidden = tab !== "queue";
  $("history").hidden = tab !== "history";
  refresh().then(revealLinkedItem);
}

$("tab-queue").addEventListener("click", () => select("queue"));
$("tab-history").addEventListener("click", () => select("history"));
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) refresh().then(revealLinkedItem);
});
/// Typing in a card: a re-render now would move the cursor.
function composing() {
  return Boolean(document.activeElement?.closest?.(".actions"));
}

state.timer = setInterval(() => {
  if (!document.hidden) refresh();
}, REFRESH_MS);
refresh().then(revealLinkedItem);
