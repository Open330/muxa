// Small compatibility helpers for the dashboard wire model.
//
// Work projection belongs to the Rust domain (`muxa::work`) and is delivered
// by /api/works. The browser must not infer Work or external issues from tmux
// session/window names because doing so makes execution topology the product
// model again.

export const WORK_STAGES = ["auto", "queued", "in_progress", "review", "done"];

// `agent_session_id` is the current public wire name. Keep the older local
// alias because older daemons and cached fixtures can still send `session_id`.
export function normalizeAgent(agent) {
  if (!agent || agent.session_id || !agent.agent_session_id) return agent;
  return { ...agent, session_id: agent.agent_session_id };
}

// The key travels through data-* attributes, so it must survive HTML
// parsing: a NUL separator is rewritten to U+FFFD by the parser and the
// clicked card then matches no Work. JSON keeps the two parts unambiguous
// with printable characters only.
export function logicalWorkKey(identity) {
  return JSON.stringify([identity?.workspace_id || "", identity?.work_id || ""]);
}

export function validateWorkSnapshot(snapshot) {
  if (!snapshot || snapshot.schema_version !== 2) {
    throw new Error("unsupported Work snapshot");
  }
  for (const work of snapshot.works || []) {
    if (!work.identity?.workspace_id || !work.identity?.work_id) {
      throw new Error("Work snapshot contains an invalid logical identity");
    }
  }
  for (const run of snapshot.unlinked_executions || []) {
    if (run.linked || run.work) {
      throw new Error("unlinked execution unexpectedly references Work");
    }
  }
  return snapshot;
}
