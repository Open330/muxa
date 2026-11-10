// Live pane output over SSE (GET /api/panes/{pane}/output/stream).
//
// EventSource cannot send an Authorization header, so this uses fetch() and
// parses the frames itself, the same way main.js reads /api/events: bearer
// tokens travel in the header and an operator session cookie rides along
// with `credentials: "same-origin"`.
//
// The pure helpers (reconnect delay, status classification, the tile stream
// budget, the frame parser) are tested in tests-js/dashboard-pane-stream.test.mjs;
// openPaneStream wires them to fetch.

export const RECONNECT_BASE_MS = 500;
export const RECONNECT_MAX_MS = 15000;
/// Window tiles that may hold a live stream at once; the rest poll.
export const TILE_STREAM_BUDGET = 6;

/// Delay before reconnect attempt `attempt` (0 = first retry): exponential
/// from `baseMs`, capped at `maxMs`, with "equal jitter" (half fixed, half
/// random) so many tabs reconnecting after a daemon restart spread out.
export function reconnectDelay(attempt, { baseMs = RECONNECT_BASE_MS, maxMs = RECONNECT_MAX_MS, random = Math.random } = {}) {
  const step = Math.max(0, Math.min(30, Math.floor(Number(attempt) || 0)));
  const ceiling = Math.min(maxMs, baseMs * 2 ** step);
  const r = Math.min(1, Math.max(0, Number(random()) || 0));
  return Math.round(ceiling / 2 + (ceiling / 2) * r);
}

/// What a stream response's status means for the client:
///   "stream"      — 200: read events
///   "denied"      — 401/403: not an operator; stop, do not retry
///   "unsupported" — 404/405/400/501: an older daemon without the stream
///                   route (or a request it will never accept); fall back
///                   to polling /output, which reports the real error
///   "retry"       — anything else (429 over capacity, 5xx, proxies): back off
export function classifyStreamStatus(status) {
  if (status === 200) return "stream";
  if (status === 401 || status === 403) return "denied";
  if ([400, 404, 405, 501].includes(status)) return "unsupported";
  return "retry";
}

/// Split the panes a window shows into those that get a live stream and
/// those that poll. Panes already streaming keep their slot so a re-plan
/// does not churn connections; free slots go to the first remaining panes
/// in `keys` order (callers list agent panes first).
export function planTileStreams(keys, budget = TILE_STREAM_BUDGET, streaming = new Set()) {
  const wanted = [...new Set(keys)];
  const max = Math.max(0, Math.floor(budget));
  const keep = wanted.filter((key) => streaming.has(key)).slice(0, max);
  const fill = wanted.filter((key) => !streaming.has(key)).slice(0, max - keep.length);
  const chosen = new Set([...keep, ...fill]);
  return {
    stream: wanted.filter((key) => chosen.has(key)),
    poll: wanted.filter((key) => !chosen.has(key)),
  };
}

/// Incremental SSE parser. `push(chunk)` feeds decoded text; `onEvent(name,
/// data)` fires for each complete event that has a name. Comments
/// (keep-alives), `id` and `retry` fields are ignored; multi-line `data`
/// is joined with "\n" as the spec says.
export function createSseParser(onEvent) {
  let buf = "";
  let event = "";
  let data = [];
  return {
    push(chunk) {
      buf += chunk;
      let nl;
      while ((nl = buf.indexOf("\n")) !== -1) {
        const line = buf.slice(0, nl).replace(/\r$/, "");
        buf = buf.slice(nl + 1);
        if (line === "") {
          if (event && data.length) onEvent(event, data.join("\n"));
          event = "";
          data = [];
        } else if (line.startsWith(":")) {
          // comment / keep-alive
        } else if (line.startsWith("event:")) {
          event = line.slice(6).replace(/^ /, "");
        } else if (line.startsWith("data:")) {
          data.push(line.slice(5).replace(/^ /, ""));
        }
      }
    },
  };
}

/// Open a live output stream and keep it open until `close()`.
///
///   url              — /api/panes/{pane}/output/stream?…
///   headers()        — auth headers for each (re)connect
///   onOutput(data)   — `{ pane, text, captured_at }` on connect and on change
///   onGone(data)     — the pane closed; the stream stops for good
///   onState(state, info) — "connecting" | "live" | "retrying" ({ delay, status })
///                      | "denied" | "unsupported" ({ status }); the last two are final
///
/// A stream the server ends normally (its lifetime cap) reconnects right
/// away; errors back off with `reconnectDelay`.
export function openPaneStream({
  url,
  headers = () => ({}),
  onOutput = () => {},
  onGone = () => {},
  onState = () => {},
  fetchImpl = (...args) => fetch(...args),
  random = Math.random,
}) {
  let closed = false;
  let controller = null;
  let timer = null;
  let wake = null;

  const sleep = (ms) => new Promise((resolve) => {
    wake = resolve;
    timer = setTimeout(resolve, ms);
  });

  async function run() {
    let attempt = 0;
    while (!closed) {
      controller = new AbortController();
      let status = 0;
      let received = false;
      onState("connecting");
      try {
        const resp = await fetchImpl(url, {
          credentials: "same-origin",
          cache: "no-store",
          headers: { ...headers(), Accept: "text/event-stream" },
          signal: controller.signal,
        });
        status = resp.status;
        const kind = classifyStreamStatus(resp.status);
        if (kind === "denied" || kind === "unsupported") {
          if (resp.body) resp.body.cancel().catch(() => {});
          if (!closed) onState(kind, { status });
          return;
        }
        if (kind !== "stream" || !resp.body) throw new Error(`stream: ${resp.status}`);
        if (closed) return;
        onState("live");
        let gone = false;
        const parser = createSseParser((name, raw) => {
          if (closed || gone) return;
          let payload = null;
          try { payload = JSON.parse(raw); } catch (_) { return; }
          if (name === "output") {
            received = true;
            onOutput(payload);
          } else if (name === "gone") {
            gone = true;
            onGone(payload);
          }
        });
        const reader = resp.body.getReader();
        const decoder = new TextDecoder();
        while (!closed && !gone) {
          const { value, done } = await reader.read();
          if (done) break;
          parser.push(decoder.decode(value, { stream: true }));
        }
        if (gone) {
          reader.cancel().catch(() => {});
          return;
        }
      } catch (_) {
        // Network error, abort, or a retryable status: fall through.
      }
      if (closed) return;
      // A stream that delivered output and then ended (lifetime cap, daemon
      // restart) starts over from the shortest delay.
      if (received) attempt = 0;
      const delay = reconnectDelay(attempt, { random });
      attempt += 1;
      onState("retrying", { delay, status });
      await sleep(delay);
    }
  }

  // Start on a microtask so no callback fires before the caller has stored
  // the returned handle (callers compare against it).
  Promise.resolve().then(() => (closed ? undefined : run()));

  return {
    close() {
      if (closed) return;
      closed = true;
      controller?.abort();
      clearTimeout(timer);
      wake?.();
    },
    get closed() {
      return closed;
    },
  };
}
