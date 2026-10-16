import assert from "node:assert/strict";
import test from "node:test";
import { baseSessionName, canonicalDashboardPanes, intervalPlacement } from "../src/dashboard/web/session-model.mjs";
import { buildTopology } from "../src/dashboard/web/topology-model.mjs";
import { buildRailEntries } from "../src/dashboard/web/workspace-rail.mjs";
import { projectCollaboration, scopeCollaborationRequests } from "../src/dashboard/web/collaboration-model.mjs";

test("numeric client views collapse to the base pane and do not inflate the session rail", () => {
  const base = { host: "rmux", socket: "/tmp/rmux/default", pane_id: "rmux:%1", window_id: "@1", session_id: "$1", session: "callabo" };
  const view = { ...base, session_id: "$9", session: "callabo~view~123" };
  const otherServer = { ...base, socket: "/tmp/other/control", session_id: "$2" };
  assert.deepEqual(canonicalDashboardPanes([view, base]), [base]);
  assert.equal(canonicalDashboardPanes([view])[0].session, "callabo");
  assert.equal(canonicalDashboardPanes([view, base, otherServer]).length, 2);
  const topology = buildTopology([view, base], [{ pane: "rmux:%1", tmux_socket: base.socket, state: "working" }]);
  const entries = buildRailEntries({ topology, unlinked: [{ session_name: view.session, execution: { socket: base.socket, window_id: "@1" } }] });
  assert.equal(entries.length, 1);
  assert.equal(entries[0].name, "callabo");
  assert.equal(entries[0].agents, 1);
  assert.equal(baseSessionName("callabo~view~12~view~34"), "callabo");
  assert.equal(baseSessionName("callabo~view~beta"), "callabo~view~beta");
});

test("retained collaboration rooms have a session group and client views share its scope", () => {
  const participant = (session, pane) => ({ tmux_session_name: session, pane, agent_session_id: pane, agent_kind: "codex", room: { host: "rmux", socket: "/tmp/rmux/default", window_id: "@1" } });
  const messages = [
    { id: "callabo", from: participant("callabo~view~123", "%1"), to: participant("callabo", "%2") },
    { id: "junia", from: participant("junia", "%3"), to: participant("junia", "%4") },
  ];
  assert.deepEqual(scopeCollaborationRequests(messages, { session: "callabo", socket: "default" }).map((request) => request.id), ["callabo"]);
  const projection = projectCollaboration(messages.slice(0, 1));
  assert.equal(projection.rooms[0].session, "callabo");
  assert.ok(!projection.rooms[0].label.includes("~view~"));
});

test("timeline placements use wall-clock positions and preserve short intervals and gaps", () => {
  const start = Date.parse("2026-10-12T00:00:00Z");
  const end = Date.parse("2026-10-13T00:00:00Z");
  const placement = intervalPlacement({ started_at: "2026-10-12T06:00:00Z", ended_at: "2026-10-12T12:00:00Z" }, start, end);
  assert.equal(placement.left, 25);
  assert.equal(placement.width, 25);
  const short = intervalPlacement({ started_at: "2026-10-12T18:00:00Z", ended_at: "2026-10-12T18:01:00Z" }, start, end);
  assert.equal(short.left, 75);
  assert.ok(short.width < 0.1, "a minute must not expand to a percentage of the whole day");
  assert.equal(intervalPlacement({ started_at: "2026-10-10T00:00:00Z", ended_at: "2026-10-11T00:00:00Z" }, start, end), null);
  assert.equal(intervalPlacement({ started_at: "2026-10-11T23:00:00Z", ended_at: "2026-10-12T06:00:00Z" }, start, end).left, 0);
});
