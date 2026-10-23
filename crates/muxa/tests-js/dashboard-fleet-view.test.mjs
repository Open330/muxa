import assert from "node:assert/strict";
import test from "node:test";

import { fleetTotals, needsAttention, orderAgents, orderHosts, renderFleet, summarizeHost } from "../src/dashboard/web/fleet-view.mjs";

const esc = (v) => String(v ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const relTime = () => "1s ago";

const local = {
  alias: "local", local: true, state: "online", muxa_version: "0.9.2", latency_ms: 0,
  remote: { agents: [{ state: "working" }, { state: "waiting_input" }], panes: [{}, {}, {}], sessions: [{}] },
};
const mbp = {
  alias: "jiun-mbp", local: false, state: "online", muxa_version: "0.9.2", latency_ms: 225,
  remote: {
    agents: [
      { state: "idle", kind: "codex", tmux_session: "web", pane: "%2", last_activity_at: "2026-10-23T07:00:00Z", ai_title: "old" },
      { state: "error", kind: "claude_code", tmux_session: "api", pane: "%1", last_activity_at: "2026-10-23T06:00:00Z", last_prompt: "<b>fix</b>" },
      { state: "working", kind: "codex", tmux_session: "db", pane: "%3", last_activity_at: "2026-10-23T07:05:00Z" },
    ],
    panes: [{}, {}],
    sessions: [{}, {}],
  },
};
const rtzr = { alias: "rtzr", local: false, state: "offline", error: "timed out waiting for the remote relay handshake" };

test("waiting on input or a choice, or an error, needs the operator", () => {
  assert.equal(needsAttention({ state: "waiting_input" }), true);
  assert.equal(needsAttention({ state: "waiting_choice" }), true);
  assert.equal(needsAttention({ state: "error" }), true);
  assert.equal(needsAttention({ state: "working" }), false);
  assert.equal(needsAttention(null), false);
});

test("a host summary counts its remote snapshot", () => {
  const s = summarizeHost(mbp);
  assert.equal(s.alias, "jiun-mbp");
  assert.equal(s.online, true);
  assert.equal(s.agents, 3);
  assert.equal(s.panes, 2);
  assert.equal(s.sessions, 2);
  assert.equal(s.attention, 1);
  assert.equal(s.latencyMs, 225);
});

test("an offline host keeps its error and has no counts", () => {
  const s = summarizeHost(rtzr);
  assert.equal(s.online, false);
  assert.equal(s.agents, 0);
  assert.match(s.error, /handshake/);
});

test("hosts: this node first, then online, then by name", () => {
  assert.deepEqual(orderHosts([rtzr, mbp, local]).map((h) => h.alias), ["local", "jiun-mbp", "rtzr"]);
});

test("agents: what needs the operator first, then busy, then most recent", () => {
  assert.deepEqual(orderAgents(mbp.remote.agents).map((a) => a.state), ["error", "working", "idle"]);
});

test("fleet totals add up across hosts", () => {
  assert.deepEqual(fleetTotals([local, mbp, rtzr]), { hosts: 3, online: 2, agents: 5, attention: 2 });
});

test("rendering lists every host and opens only the selected online one, escaped", () => {
  const html = renderFleet([local, mbp, rtzr], { selected: "jiun-mbp", esc, relTime });
  assert.match(html, /data-fleet-host="local"/);
  assert.match(html, /data-fleet-host="jiun-mbp"[^>]*aria-pressed="true"/);
  assert.match(html, /data-fleet-host="rtzr"[^>]*disabled/);
  assert.match(html, /handshake/);
  assert.match(html, /class="fleet-agents"/);
  assert.match(html, /&lt;b&gt;fix&lt;\/b&gt;/);
  assert.doesNotMatch(html, /<b>fix<\/b>/);
});

test("a host that is still connecting is not shown as an error", () => {
  const html = renderFleet([local, { alias: "rtzr", state: "connecting" }], { selected: null, esc, relTime });
  assert.match(html, /fleet-pending">connecting…/);
  assert.doesNotMatch(html, /fleet-error/);
});

test("an offline selection opens nothing; no hosts explains the config", () => {
  assert.doesNotMatch(renderFleet([local, rtzr], { selected: "rtzr", esc, relTime }), /fleet-agents"/);
  assert.match(renderFleet([], { selected: null, esc, relTime }), /\[fleet\.hosts\]/);
});
