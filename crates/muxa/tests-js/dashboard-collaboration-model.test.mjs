import test from "node:test";
import assert from "node:assert/strict";

import {
  collaborationSequence,
  edgeIdentity,
  normalizeCollaborationPayload,
  participantIdentity,
  participantSession,
  projectCollaboration,
  requestRoomKey,
  scopeCollaborationRequests,
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

test("a session scope keeps messages touching a pane in that session", () => {
  const livePanes = new Map([
    ["default %1", { session: "youtube", socket: "default" }],
    ["default %2", { session: "youtube", socket: "default" }],
    ["default %3", { session: "somun", socket: "default" }],
  ]);
  const resolvePane = (pane, socket) => livePanes.get(`${String(socket || "default").split("/").pop()} ${pane}`) || null;
  const at = (pane, alias) => ({ ...participant(`s-${alias}`, alias, pane), socket: "default" });
  const editor = at("%1", "editor");
  const uploader = at("%2", "uploader");
  const writer = at("%3", "writer");
  const console = { console: true };
  const artist = { agent_kind: "codex", agent_session_id: "arena-art", alias: "implementer" }; // no pane
  const closed = { ...at("%99", "old"), tmux_session_name: "youtube" }; // pane gone from the scan
  const requests = [
    request("r1", "2026-08-30T01:00:00Z", { from: editor, to: uploader }),
    request("r2", "2026-08-30T02:00:00Z", { from: console, to: editor }),
    request("r3", "2026-08-30T03:00:00Z", { from: writer, to: artist }),
    request("r4", "2026-08-30T04:00:00Z", { from: artist, to: uploader }),
    request("r5", "2026-08-30T05:00:00Z", { from: console, to: writer }),
    request("r6", "2026-08-30T06:00:00Z", { from: writer, to: editor }),
    request("r7", "2026-08-30T07:00:00Z", { from: closed, to: console }),
  ];

  assert.equal(scopeCollaborationRequests(requests, null, resolvePane), requests);
  const youtube = scopeCollaborationRequests(requests, { session: "youtube", socket: "default", workspace: "" }, resolvePane);
  assert.deepEqual(youtube.map((r) => r.id), ["r1", "r2", "r4", "r6", "r7"]);
  const projection = projectCollaboration(youtube);
  // console and the pane-less implementer stay because they exchanged a
  // message with youtube; somun's writer stays as r6's sender.
  assert.deepEqual(projection.nodes.map((node) => node.label).sort(),
    ["console", "editor", "implementer", "old", "uploader", "writer"]);
  assert.equal(projection.edges.reduce((sum, edge) => sum + edge.count, 0), 5);

  const somun = scopeCollaborationRequests(requests, { session: "somun", socket: "", workspace: "" }, resolvePane);
  assert.deepEqual(somun.map((r) => r.id), ["r3", "r5", "r6"]);
  // The same session name on another server is out of scope.
  assert.deepEqual(scopeCollaborationRequests(requests, { session: "somun", socket: "work" }, resolvePane), []);
});

test("participantSession prefers the live pane scan and ignores the console", () => {
  const resolvePane = (pane) => (pane === "%1" ? { session: "live", socket: "default" } : null);
  assert.deepEqual(participantSession({ ...implementer, tmux_session_name: "stale" }, resolvePane),
    { session: "live", socket: "default" });
  assert.deepEqual(participantSession({ ...reviewer, tmux_session_name: "recorded" }, resolvePane),
    { session: "recorded", socket: "default" });
  assert.equal(participantSession(reviewer, resolvePane), null);
  assert.equal(participantSession({ console: true }, resolvePane), null);
  assert.equal(participantSession({ agent_session_id: "x", tmux_session_name: "s" }, resolvePane), null);
});

test("a managed-only workspace scope keeps the requests filed under its Work", () => {
  const requests = [
    request("a", "2026-08-30T01:00:00Z", { workspace_id: "billing" }),
    request("b", "2026-08-30T02:00:00Z", { work: { workspace_id: "billing", work_id: "B-1" } }),
    request("c", "2026-08-30T03:00:00Z", { workspace_id: "other" }),
  ];
  assert.deepEqual(
    scopeCollaborationRequests(requests, { session: "", socket: "", workspace: "billing" }).map((r) => r.id),
    ["a", "b"],
  );
  assert.deepEqual(scopeCollaborationRequests(requests, { session: "", socket: "", workspace: "" }), []);
});
