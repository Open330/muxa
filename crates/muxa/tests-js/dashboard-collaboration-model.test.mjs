import test from "node:test";
import assert from "node:assert/strict";

import {
  collaborationSequence,
  edgeIdentity,
  normalizeCollaborationPayload,
  participantIdentity,
  projectCollaboration,
  requestRoomKey,
} from "../src/dashboard/web/collaboration-model.mjs";

const room = { host: "tmux", socket: "/tmp/tmux-1000/default", window_id: "@9" };
const participant = (session, alias, pane) => ({
  agent_kind: "codex",
  agent_session_id: session,
  pane,
  room,
  alias,
  roles: alias === "reviewer" ? ["review"] : ["implementation"],
});
const implementer = participant("session-impl", "implementer", "%1");
const reviewer = participant("session-review", "reviewer", "%2");

const request = (id, createdAt, extra = {}) => ({
  id,
  from: implementer,
  to: reviewer,
  kind: "review",
  status: "completed",
  body: `review ${id}`,
  created_at: createdAt,
  thread_id: "CAL-7345/review",
  work_id: "CAL-7345",
  ...extra,
});

test("accepts both the paged API envelope and a legacy bare array", () => {
  const bare = normalizeCollaborationPayload([request("one", "2026-08-30T01:00:00Z")]);
  assert.equal(bare.requests.length, 1);
  assert.equal(bare.pagination.has_more, false);

  const paged = normalizeCollaborationPayload({
    requests: [],
    pagination: { total: 12, limit: 5, has_more: true, next_cursor: "opaque" },
    generated_at: "2026-08-30T02:00:00Z",
  });
  assert.equal(paged.pagination.total, 12);
  assert.equal(paged.pagination.next_cursor, "opaque");
});

test("aggregates directional requests and reverse replies without losing roles", () => {
  const projection = projectCollaboration([
    request("one", "2026-08-30T01:00:00Z", {
      reply: { status: "completed", body: "approved", at: "2026-08-30T01:01:00Z" },
    }),
    request("two", "2026-08-30T02:00:00Z"),
  ]);
  assert.equal(projection.nodes.length, 2);
  assert.equal(projection.nodes.find((node) => node.label === "reviewer").subtitle, "review");
  assert.equal(projection.edges.length, 1);
  assert.equal(projection.edges[0].count, 2);
  assert.equal(projection.edges[0].replyCount, 1);
  assert.deepEqual(projection.edges[0].kinds, { review: 2 });
  assert.deepEqual(projection.works, ["CAL-7345"]);
});

test("durable participant identity namespaces a session by host and socket", () => {
  const sameSessionOtherSocket = {
    ...implementer,
    socket: "/tmp/tmux-1000/other",
    room: { ...room, socket: "/tmp/tmux-1000/other" },
  };
  assert.notEqual(participantIdentity(implementer), participantIdentity(sameSessionOtherSocket));
  assert.equal(participantIdentity(implementer).includes(implementer.pane), false);
});

test("qualifies work ids with workspace to avoid cross-workspace collisions", () => {
  const projection = projectCollaboration([
    request("one", "2026-08-30T01:00:00Z", { workspace_id: "callabo" }),
    request("two", "2026-08-30T02:00:00Z", { workspace_id: "muxa" }),
  ]);
  assert.deepEqual(projection.works, ["callabo/CAL-7345", "muxa/CAL-7345"]);
});

test("drill-down filters by exact room and directed edge, then sorts chronologically", () => {
  const reverse = {
    ...request("reverse", "2026-08-30T00:30:00Z"),
    from: reviewer,
    to: implementer,
  };
  const requests = [request("late", "2026-08-30T02:00:00Z"), reverse, request("early", "2026-08-30T01:00:00Z")];
  const edge = edgeIdentity(participantIdentity(implementer), participantIdentity(reviewer));
  const sequence = collaborationSequence(requests, { edgeKey: edge, room: requestRoomKey(requests[0]) });
  assert.deepEqual(sequence.map((item) => item.id), ["early", "late"]);
});

test("anchors console-origin messages on the recipient room", () => {
  const consoleRoom = { host: "dashboard", window_id: "console" };
  const console = {
    agent_kind: "unknown",
    agent_session_id: "console",
    pane: "console",
    room: consoleRoom,
    console: true,
  };
  const dispatched = { ...request("dispatch", "2026-08-30T03:00:00Z"), from: console };
  assert.equal(requestRoomKey(dispatched), requestRoomKey(request("peer", "2026-08-30T03:01:00Z")));
  assert.notEqual(requestRoomKey(dispatched), "dashboard\u001fdefault\u001fconsole");
});
