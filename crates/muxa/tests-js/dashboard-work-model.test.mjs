import test from "node:test";
import assert from "node:assert/strict";

import {
  logicalWorkKey,
  normalizeAgent,
  validateWorkSnapshot,
  WORK_STAGES,
} from "../src/dashboard/web/work-model.mjs";

test("normalizes the current agent_session_id without overwriting legacy ids", () => {
  assert.equal(normalizeAgent({ agent_session_id: "current" }).session_id, "current");
  assert.equal(
    normalizeAgent({ agent_session_id: "current", session_id: "legacy" }).session_id,
    "legacy"
  );
});

test("uses a stable logical Work identity instead of a tmux binding", () => {
  const first = logicalWorkKey({ workspace_id: "payments", work_id: "settlement" });
  const second = logicalWorkKey({ workspace_id: "payments", work_id: "settlement" });
  assert.equal(first, second);
  assert.equal(first.includes("@42"), false);
});

test("keeps attention out of the board-stage vocabulary", () => {
  assert.deepEqual(WORK_STAGES, ["auto", "queued", "in_progress", "review", "done"]);
  assert.equal(WORK_STAGES.includes("attention"), false);
  assert.equal(WORK_STAGES.includes("blocked"), false);
});

test("accepts Work and unlinked executions as separate collections", () => {
  const snapshot = {
    schema_version: 2,
    workspaces: [{ id: "muxa", name: "muxa", work_count: 1 }],
    works: [{
      identity: { workspace_id: "muxa", work_id: "dashboard-v2" },
      stage: "in_progress",
      signals: ["attention"],
      external_items: [{ source: "linear", display_key: "CAL-7093", status: "started" }],
      runs: [],
    }],
    unlinked_executions: [{ id: "tmux/@9", linked: false, work: null }],
  };

  assert.equal(validateWorkSnapshot(snapshot), snapshot);
  assert.equal(snapshot.works[0].stage, "in_progress");
  assert.deepEqual(snapshot.works[0].signals, ["attention"]);
  assert.equal(snapshot.works[0].external_items[0].display_key, "CAL-7093");
});

test("rejects topology-only rows masquerading as Work", () => {
  assert.throws(() => validateWorkSnapshot({
    schema_version: 2,
    works: [{ identity: { session_id: "$1", window_id: "@2" } }],
    unlinked_executions: [],
  }), /invalid logical identity/);
});

test("rejects linked runs in the unlinked execution collection", () => {
  assert.throws(() => validateWorkSnapshot({
    schema_version: 2,
    works: [],
    unlinked_executions: [{
      linked: true,
      work: { workspace_id: "muxa", work_id: "oops" },
    }],
  }), /unexpectedly references Work/);
});
