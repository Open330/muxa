import assert from "node:assert/strict";
import test from "node:test";

import {
  RECONNECT_BASE_MS,
  RECONNECT_MAX_MS,
  TILE_STREAM_BUDGET,
  classifyStreamStatus,
  createSseParser,
  openPaneStream,
  planTileStreams,
  reconnectDelay,
} from "../src/dashboard/web/pane-stream.mjs";
import { updatedLabel } from "../src/dashboard/web/pane-drawer.mjs";

test("reconnect delay doubles from the base and stops at the cap", () => {
  const noJitter = { random: () => 1 };
  assert.equal(reconnectDelay(0, noJitter), RECONNECT_BASE_MS);
  assert.equal(reconnectDelay(1, noJitter), RECONNECT_BASE_MS * 2);
  assert.equal(reconnectDelay(3, noJitter), RECONNECT_BASE_MS * 8);
  assert.equal(reconnectDelay(20, noJitter), RECONNECT_MAX_MS);
  assert.equal(reconnectDelay(10_000, noJitter), RECONNECT_MAX_MS);
  // Garbage attempts behave like the first one.
  assert.equal(reconnectDelay(-4, noJitter), RECONNECT_BASE_MS);
  assert.equal(reconnectDelay(undefined, noJitter), RECONNECT_BASE_MS);
});

test("reconnect jitter keeps at least half of the step", () => {
  assert.equal(reconnectDelay(2, { random: () => 0 }), RECONNECT_BASE_MS * 2);
  assert.equal(reconnectDelay(2, { random: () => 0.5 }), RECONNECT_BASE_MS * 3);
  assert.equal(reconnectDelay(2, { random: () => 7 }), RECONNECT_BASE_MS * 4);
  for (let attempt = 0; attempt < 12; attempt++) {
    const delay = reconnectDelay(attempt);
    assert.ok(delay >= RECONNECT_BASE_MS / 2 && delay <= RECONNECT_MAX_MS, `${attempt}: ${delay}`);
  }
  assert.equal(reconnectDelay(1, { baseMs: 100, maxMs: 150, random: () => 1 }), 150);
});

test("stream statuses map to stream, stop, poll or retry", () => {
  assert.equal(classifyStreamStatus(200), "stream");
  assert.equal(classifyStreamStatus(401), "denied");
  assert.equal(classifyStreamStatus(403), "denied");
  // An older daemon has no stream route: poll /output instead.
  assert.equal(classifyStreamStatus(404), "unsupported");
  assert.equal(classifyStreamStatus(405), "unsupported");
  assert.equal(classifyStreamStatus(400), "unsupported");
  assert.equal(classifyStreamStatus(501), "unsupported");
  // Over capacity or a flaky backend: back off and try again.
  assert.equal(classifyStreamStatus(429), "retry");
  assert.equal(classifyStreamStatus(502), "retry");
  assert.equal(classifyStreamStatus(503), "retry");
  assert.equal(classifyStreamStatus(0), "retry");
});

test("tile budget streams the first panes and polls the rest", () => {
  const keys = ["a", "b", "c", "d", "e", "f", "g", "h"];
  const plan = planTileStreams(keys, TILE_STREAM_BUDGET);
  assert.equal(TILE_STREAM_BUDGET, 6);
  assert.deepEqual(plan.stream, ["a", "b", "c", "d", "e", "f"]);
  assert.deepEqual(plan.poll, ["g", "h"]);
  assert.deepEqual(planTileStreams(["a", "b"], 6), { stream: ["a", "b"], poll: [] });
  assert.deepEqual(planTileStreams(["a", "b"], 0), { stream: [], poll: ["a", "b"] });
  assert.deepEqual(planTileStreams([], 6), { stream: [], poll: [] });
  // Duplicate keys count once.
  assert.deepEqual(planTileStreams(["a", "a", "b"], 1), { stream: ["a"], poll: ["b"] });
});

test("tile budget keeps panes that already stream instead of churning", () => {
  const plan = planTileStreams(["a", "b", "c", "d"], 2, new Set(["d", "gone"]));
  assert.deepEqual(plan.stream, ["a", "d"]);
  assert.deepEqual(plan.poll, ["b", "c"]);
  // More existing streams than slots: the earliest in order win.
  const shrunk = planTileStreams(["a", "b", "c"], 1, new Set(["c", "b"]));
  assert.deepEqual(shrunk, { stream: ["b"], poll: ["a", "c"] });
});

test("SSE parser handles split chunks, comments and CRLF", () => {
  const seen = [];
  const parser = createSseParser((name, data) => seen.push([name, data]));
  parser.push(": keep-alive\n\n");
  parser.push("event: output\r\ndata: {\"text\":");
  parser.push("\"hi\"}\r\n\r\nevent:gone\ndata:{}\n");
  assert.deepEqual(seen, [["output", "{\"text\":\"hi\"}"]]);
  parser.push("\n");
  assert.deepEqual(seen[1], ["gone", "{}"]);
  parser.push("data: a\ndata: b\nevent: x\n\n");
  assert.deepEqual(seen[2], ["x", "a\nb"]);
  // A nameless event is ignored.
  parser.push("data: orphan\n\n");
  assert.equal(seen.length, 3);
});

test("drawer label says the stream is live and when the pane last changed", () => {
  assert.equal(updatedLabel(0, 5000, true), "live");
  assert.equal(updatedLabel(1000, 4500, true), "live · changed 3s ago");
  assert.equal(updatedLabel(1000, 1000 + 125_000, true), "live · changed 2m ago");
  assert.equal(updatedLabel(1000, 4500), "updated 3s ago");
});

function sseResponse(chunks, status = 200) {
  const encoder = new TextEncoder();
  const body = new ReadableStream({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
      controller.close();
    },
  });
  return { status, ok: status === 200, body };
}

function waitFor(check) {
  return new Promise((resolve, reject) => {
    const started = Date.now();
    const poll = () => {
      if (check()) resolve();
      else if (Date.now() - started > 2000) reject(new Error("timed out"));
      else setTimeout(poll, 2);
    };
    poll();
  });
}

test("stream delivers output, sends auth headers, and stops at gone", async () => {
  const requests = [];
  const outputs = [];
  const gone = [];
  const states = [];
  const handle = openPaneStream({
    url: "/api/panes/%251/output/stream?lines=15",
    headers: () => ({ Authorization: "Bearer t" }),
    fetchImpl: async (url, init) => {
      requests.push([url, init]);
      return sseResponse([
        "event: output\ndata: {\"pane\":\"%1\",\"text\":\"one\"}\n\n",
        ": ping\n\n",
        "event: output\ndata: {\"pane\":\"%1\",\"text\":\"two\"}\n\nevent: gone\ndata: {\"pane\":\"%1\"}\n\n",
      ]);
    },
    onOutput: (payload) => outputs.push(payload.text),
    onGone: (payload) => gone.push(payload.pane),
    onState: (state) => states.push(state),
  });
  await waitFor(() => gone.length === 1);
  assert.deepEqual(outputs, ["one", "two"]);
  assert.deepEqual(gone, ["%1"]);
  assert.equal(requests.length, 1);
  assert.equal(requests[0][1].headers.Authorization, "Bearer t");
  assert.equal(requests[0][1].headers.Accept, "text/event-stream");
  assert.equal(requests[0][1].credentials, "same-origin");
  assert.deepEqual(states, ["connecting", "live"]);
  handle.close();
});

test("stream falls back on 404 and stops on 403 without retrying", async () => {
  for (const [status, expected] of [[404, "unsupported"], [403, "denied"]]) {
    let calls = 0;
    const states = [];
    openPaneStream({
      url: "/x",
      fetchImpl: async () => {
        calls += 1;
        return sseResponse([], status);
      },
      onState: (state) => states.push(state),
    });
    await waitFor(() => states.includes(expected));
    await new Promise((resolve) => setTimeout(resolve, 20));
    assert.equal(calls, 1, `${status}`);
    assert.deepEqual(states, ["connecting", expected]);
  }
});

test("stream retries with backoff after errors and a close stops it", async () => {
  let calls = 0;
  const delays = [];
  const handle = openPaneStream({
    url: "/x",
    random: () => 0,
    fetchImpl: async () => {
      calls += 1;
      if (calls === 1) throw new Error("network down");
      return sseResponse([], 429);
    },
    onState: (state, info) => {
      if (state === "retrying") delays.push(info.delay);
    },
  });
  await waitFor(() => delays.length === 2);
  // Equal jitter with random() = 0: half of 500, then half of 1000.
  assert.deepEqual(delays, [250, 500]);
  handle.close();
  assert.equal(handle.closed, true);
  const before = calls;
  await new Promise((resolve) => setTimeout(resolve, 700));
  assert.equal(calls, before, "no reconnect after close");
});

test("no callback fires before the caller holds the handle", async () => {
  const seen = [];
  const handle = openPaneStream({
    url: "/x",
    fetchImpl: async () => sseResponse([], 404),
    // Callers compare against the handle they stored; reading it here must
    // not hit the temporal dead zone.
    onState: (state) => seen.push([state, handle !== undefined]),
  });
  await waitFor(() => seen.length === 2);
  assert.deepEqual(seen, [["connecting", true], ["unsupported", true]]);
});

test("closing before the first tick never fetches", async () => {
  let calls = 0;
  const handle = openPaneStream({
    url: "/x",
    fetchImpl: async () => {
      calls += 1;
      return sseResponse([]);
    },
  });
  handle.close();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(calls, 0);
});
