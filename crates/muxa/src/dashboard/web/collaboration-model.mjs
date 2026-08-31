// Pure collaboration projections used by the dashboard graph and sequence
// views. Keeping this module DOM-free makes the identity and aggregation rules
// independently testable without adding a frontend build step.

const SEP = "\u001f";

export function normalizeCollaborationPayload(payload) {
  if (Array.isArray(payload)) {
    return {
      requests: payload,
      pagination: {
        total: payload.length,
        limit: payload.length,
        offset: 0,
        has_more: false,
        next_offset: null,
        next_cursor: null,
      },
      generated_at: null,
      details_included: true,
    };
  }
  const requests = Array.isArray(payload?.requests) ? payload.requests : [];
  const page = payload?.pagination || {};
  return {
    requests,
    pagination: {
      total: finiteNumber(page.total, requests.length),
      limit: finiteNumber(page.limit, requests.length),
      offset: finiteNumber(page.offset, 0),
      has_more: Boolean(page.has_more),
      next_offset: page.next_offset != null && Number.isFinite(Number(page.next_offset)) ? Number(page.next_offset) : null,
      next_cursor: typeof page.next_cursor === "string" ? page.next_cursor : null,
    },
    generated_at: payload?.generated_at || null,
    details_included: payload?.details_included !== false,
  };
}

function finiteNumber(value, fallback) {
  const number = Number(value);
  return Number.isFinite(number) && number >= 0 ? number : fallback;
}

export function roomKey(room) {
  if (!room) return "unknown-room";
  return [room.host || "tmux", room.socket || "default", room.window_id || "unknown"].join(SEP);
}

export function roomLabel(request) {
  const participant = requestAnchor(request);
  const room = participant.room || {};
  const session = participant.tmux_session_name || participant.tmux_session_id || room.host || "host";
  const window = participant.window_name || room.window_id || "room";
  const socket = room.socket ? ` · ${shortSocket(room.socket)}` : "";
  return `${session}:${window}${socket}`;
}

function shortSocket(value) {
  const parts = String(value).split("/");
  return parts[parts.length - 1] || "default";
}

export function participantIdentity(participant) {
  if (!participant) return "unknown";
  if (participant.console) return "console";
  const host = participant.room?.host || "tmux";
  const socket = participant.socket || participant.room?.socket || "default";
  const session = participant.agent_session_id || `pane:${participant.socket || "default"}:${participant.pane || "?"}`;
  return `${host}${SEP}${socket}${SEP}${session}`;
}

export function participantLabel(participant) {
  if (!participant) return "unknown";
  if (participant.console) return "console";
  return participant.alias || participant.roles?.[0] || participant.agent_kind || participant.pane || "agent";
}

export function participantSubtitle(participant) {
  if (!participant || participant.console) return "operator";
  const roles = (participant.roles || []).filter((role) => role !== participant.alias);
  if (roles.length) return roles.join(", ");
  return `${participant.agent_kind || "agent"} · ${participant.pane || "pane"}`;
}

export function requestRoomKey(request) {
  return roomKey(requestAnchor(request)?.room);
}

function requestAnchor(request) {
  if (request?.from?.console && request?.to) return request.to;
  return request?.from || request?.to || {};
}

export function requestWorkId(request) {
  const work = request?.work_id || request?.work?.work_id || request?.work?.id || "";
  const workspace = request?.workspace_id || request?.work?.workspace_id || "";
  return workspace && work ? `${workspace}/${work}` : work;
}

export function requestThreadId(request) {
  return request?.thread_id || "";
}

export function edgeIdentity(fromId, toId) {
  return `${fromId}${SEP}→${SEP}${toId}`;
}

export function projectCollaboration(requests) {
  const nodes = new Map();
  const edges = new Map();
  const rooms = new Map();
  const works = new Set();
  const threads = new Set();

  for (const request of requests || []) {
    if (!request?.from || !request?.to) continue;
    const from = addNode(nodes, request.from, request.created_at || "");
    const to = addNode(nodes, request.to, request.created_at || "");
    const key = edgeIdentity(from.id, to.id);
    if (!edges.has(key)) {
      edges.set(key, {
        key,
        from: from.id,
        to: to.id,
        count: 0,
        replyCount: 0,
        replyStatuses: {},
        kinds: {},
        statuses: {},
        rooms: new Set(),
        latestAt: "",
      });
    }
    const edge = edges.get(key);
    edge.count += 1;
    increment(edge.kinds, request.kind || "unknown");
    increment(edge.statuses, request.status || "unknown");
    if (request.reply) {
      edge.replyCount += 1;
      increment(edge.replyStatuses, request.reply.status || request.status || "unknown");
    }
    const room = requestRoomKey(request);
    edge.rooms.add(room);
    if ((request.created_at || "") > edge.latestAt) edge.latestAt = request.created_at || "";
    if (!rooms.has(room)) {
      rooms.set(room, {
        key: room,
        room: requestAnchor(request)?.room || null,
        label: roomLabel(request),
        count: 0,
        latestAt: request.created_at || "",
      });
    }
    const roomEntry = rooms.get(room);
    roomEntry.count += 1;
    if ((request.created_at || "") > roomEntry.latestAt) {
      roomEntry.room = requestAnchor(request)?.room || null;
      roomEntry.label = roomLabel(request);
      roomEntry.latestAt = request.created_at || "";
    }
    const work = requestWorkId(request);
    const thread = requestThreadId(request);
    if (work) works.add(work);
    if (thread) threads.add(thread);
  }

  return {
    nodes: [...nodes.values()].sort((left, right) => left.label.localeCompare(right.label)),
    edges: [...edges.values()].sort((left, right) => right.count - left.count || left.key.localeCompare(right.key)),
    rooms: [...rooms.values()].sort((left, right) => right.count - left.count || left.label.localeCompare(right.label)),
    works: [...works].sort(),
    threads: [...threads].sort(),
  };
}

function addNode(nodes, participant, at) {
  const id = participantIdentity(participant);
  if (!nodes.has(id)) {
    nodes.set(id, {
      id,
      participant,
      label: participantLabel(participant),
      subtitle: participantSubtitle(participant),
      latestAt: at,
    });
  } else {
    // Requests can arrive newest-first from the API or oldest-first after
    // paging is merged, so choose labels by timestamp rather than input order.
    const current = nodes.get(id);
    if ((!current.participant.alias && participant.alias) || at > current.latestAt) {
      current.participant = participant;
      current.label = participantLabel(participant);
      current.subtitle = participantSubtitle(participant);
      current.latestAt = at;
    }
  }
  return nodes.get(id);
}

function increment(counts, key) {
  counts[key] = (counts[key] || 0) + 1;
}

export function collaborationSequence(requests, { edgeKey = "", room = "" } = {}) {
  let selected = [...(requests || [])];
  if (room) selected = selected.filter((request) => requestRoomKey(request) === room);
  if (edgeKey) {
    selected = selected.filter((request) => edgeIdentity(
      participantIdentity(request.from),
      participantIdentity(request.to),
    ) === edgeKey);
  }
  return selected.sort((left, right) => {
    const byTime = String(left.created_at || "").localeCompare(String(right.created_at || ""));
    return byTime || String(left.id || "").localeCompare(String(right.id || ""));
  });
}

export function sequenceParticipants(requests) {
  const participants = new Map();
  for (const request of requests || []) {
    for (const participant of [request?.from, request?.to]) {
      if (!participant) continue;
      const id = participantIdentity(participant);
      if (!participants.has(id)) participants.set(id, participant);
    }
  }
  return [...participants.entries()].map(([id, participant]) => ({
    id,
    participant,
    label: participantLabel(participant),
    subtitle: participantSubtitle(participant),
  }));
}

export function dominantCount(counts) {
  return Object.entries(counts || {}).sort((left, right) => right[1] - left[1] || left[0].localeCompare(right[0]))[0]?.[0] || "unknown";
}
