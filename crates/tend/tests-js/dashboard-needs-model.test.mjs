import test from "node:test";
import assert from "node:assert/strict";

import { ago, decidedCard, evidenceLine, pendingCard, safeHref } from "../src/dashboard/web/needs-model.mjs";

const now = Date.parse("2026-11-12T12:00:00Z");

test("a human request card carries the question, options and recommendation", () => {
  const card = pendingCard(
    {
      kind: "human_request",
      request_id: "rq_1",
      action: "choice",
      body: "Merge #12?",
      from_session_id: "s",
      from_pane: "%3",
      from_alias: "impl",
      workspace_id: "tend",
      work_id: "ENG-1",
      created_at: "2026-11-12T11:00:00Z",
      decision: {
        options: ["merge", "hold"],
        recommendation: "merge",
        evidence: [{ kind: "pull_request", url: "https://github.com/o/r/pull/12" }],
      },
    },
    now,
  );
  assert.equal(card.kind, "ask");
  assert.equal(card.title, "Merge #12?");
  assert.equal(card.meta, "@impl · tend/ENG-1 · 1h");
  assert.deepEqual(card.options, ["merge", "hold"]);
  assert.equal(card.recommendation, "merge");
  assert.equal(card.evidence[0].href, "https://github.com/o/r/pull/12");
});

test("review and waiting-agent cards", () => {
  const review = pendingCard(
    {
      kind: "review",
      id: "rv_1",
      summary: "Login fixed",
      target: { kind: "work", workspace: "web", work: "ENG-1", alias: "impl" },
      requested_at: "2026-11-12T11:58:00Z",
      decision: { needed: "Ship it?", evidence: [{ kind: "test", command: "cargo test", passed: false, tail: "boom" }] },
    },
    now,
  );
  assert.equal(review.meta, "web/ENG-1 @impl · 2m");
  assert.equal(review.needed, "Ship it?");
  assert.deepEqual(review.evidence[0], { text: "test failed: cargo test", cls: "fail", tail: "boom" });
  const agent = pendingCard(
    { kind: "agent_waiting", session_id: "s", agent_kind: "codex", pane: "%4", state: "waiting_input", message: "Proceed?", since: "2026-11-12T11:59:30Z" },
    now,
  );
  assert.equal(agent.label, "waiting for input");
  assert.equal(agent.pane, "%4");
  assert.equal(pendingCard({ kind: "mystery" }), null);
});

test("decided cards show the outcome", () => {
  const review = decidedCard(
    {
      kind: "review",
      id: "rv_1",
      summary: "Login fixed",
      target: { kind: "session", session_id: "abcdef123456" },
      requested_at: "2026-11-12T10:00:00Z",
      disposition: { kind: "returned", note: "tests missing\nmore", at: "2026-11-12T11:00:00Z", by: { kind: "user", label: "web:owner@example.com" } },
    },
    now,
  );
  assert.equal(review.outcome, "returned: tests missing");
  assert.match(review.meta, /web:owner@example.com/);
  const answer = decidedCard({ kind: "human_answer", request_id: "rq_1", body: "Merge?", status: "completed", answer: "yes", answered_at: "2026-11-12T11:00:00Z", from_session_id: "s" }, now);
  assert.equal(answer.label, "answered");
  assert.equal(answer.outcome, "yes");
});

test("only web links are clickable", () => {
  assert.equal(safeHref("javascript:alert(1)"), null);
  assert.equal(safeHref("file:///etc/passwd"), null);
  assert.equal(safeHref("https://example.com/x"), "https://example.com/x");
  assert.equal(evidenceLine({ kind: "pull_request", url: "javascript:alert(1)" }).href, null);
  assert.equal(evidenceLine({ kind: "commit", sha: "abc", repo: "/r" }).text, "commit abc in /r");
  assert.equal(ago("not a date"), "");
});
