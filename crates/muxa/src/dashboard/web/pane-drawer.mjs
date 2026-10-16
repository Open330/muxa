// Pane drawer: talk to one agent without leaving the dashboard.
//
// Clicking an agent (or pane) row opens a right-side drawer with the pane's
// live output and a composer. Operators see output (streamed from the
// operator-only /api/panes/{pane}/output/stream, or polled from
// /api/panes/{pane}/output on a daemon without the stream route) and can send
// a message, abort or share; everyone else sees the pane's metadata and a
// "view only" note, and the output endpoints are never called for them.
//
// The pure helpers at the top carry the rules worth testing without a DOM
// (tests-js/dashboard-pane-drawer.test.mjs); createPaneDrawer wires them to
// the elements in index.html.

export const OUTPUT_REFRESH_MS = 1500;
export const OUTPUT_LINES = 200;
const NEAR_BOTTOM_PX = 24;
const MAX_MESSAGE_BYTES = 16384;

/// What a keydown in the composer should do: "send" or "default" (let the
/// textarea handle it). Enter sends and Shift+Enter inserts a newline. While
/// an IME is composing (Korean, Japanese, Chinese input), Enter confirms the
/// candidate and must never send; `keyCode === 229` covers browsers that
/// report the confirming keydown after `isComposing` has flipped back.
export function composerKeyAction(event) {
  if (!event || event.key !== "Enter") return "default";
  if (event.isComposing || event.keyCode === 229) return "default";
  if (event.shiftKey || event.altKey) return "default";
  return "send";
}

/// `#pane=%5&socket=default` → `{ pane, socket }`, or null. Other fragment
/// parameters (e.g. a not-yet-scrubbed token) are ignored.
export function parsePaneHash(hash) {
  const params = new URLSearchParams(String(hash || "").replace(/^#/, ""));
  const pane = params.get("pane");
  if (!pane) return null;
  return { pane, socket: params.get("socket") || null };
}

/// Return `hash` with the pane deep link set (or removed when `target` is
/// null), keeping any unrelated fragment parameters.
export function withPaneHash(hash, target) {
  const params = new URLSearchParams(String(hash || "").replace(/^#/, ""));
  params.delete("pane");
  // A navigator selection (#session/#window) shares the socket parameter.
  if (!params.get("session") && !params.get("window")) params.delete("socket");
  if (target?.pane) {
    params.set("pane", target.pane);
    if (target.socket) params.set("socket", target.socket);
  }
  const rest = params.toString();
  return rest ? `#${rest}` : "";
}

/// Whether a scroll box is (close enough to) its bottom that new output
/// should keep it pinned there.
export function isNearBottom({ scrollTop, scrollHeight, clientHeight }) {
  return scrollHeight - scrollTop - clientHeight <= NEAR_BOTTOM_PX;
}

/// "updated 3s ago" style label for the last successful refresh. With a
/// live stream the output only arrives when it changes, so the label says
/// the stream is live and how long ago the pane last changed.
export function updatedLabel(lastMs, nowMs, live = false) {
  if (!lastMs) return live ? "live" : "";
  const secs = Math.max(0, Math.floor((nowMs - lastMs) / 1000));
  const ago = secs < 60 ? `${secs}s` : `${Math.floor(secs / 60)}m`;
  return live ? `live · changed ${ago} ago` : `updated ${ago} ago`;
}

/// Wire the drawer. `deps` supplies everything that lives in main.js:
///   elements           — the drawer's DOM nodes (see index.html)
///   isOperator()       — may this browser read output and send?
///   canShare(target)   — show the share action for this pane?
///   describe(target)   — { title, kind, state, model, cwd, activity, command }
///   fetchOutput(url)   — GET returning a Response (auth headers attached)
///   openStream(url, handlers) — open a live output stream (pane-stream.mjs
///                        openPaneStream with auth headers); optional, the
///                        drawer polls without it
///   controlFetch(url, options) — the dashboard's control fetch (CSRF + auth)
///   openShare(target)  — open the existing share dialog
///   showToast(msg)
export function createPaneDrawer(deps) {
  const el = deps.elements;
  let target = null;
  let opener = null;
  let timer = null;
  let ticker = null;
  let inFlight = false;
  let generation = 0;
  let lastUpdated = 0;
  let sending = false;
  let firstPaint = true;
  // The output endpoint refused this browser: stop polling until reopened.
  let denied = false;
  let wasOperator = false;
  // Live stream for the open pane, and whether the daemon lacks the route
  // (then the drawer polls, as before streams existed).
  let stream = null;
  let live = false;
  let streamUnsupported = !deps.openStream;

  const isOpen = () => !el.drawer.hidden;

  function renderMeta() {
    if (!target) return;
    const info = deps.describe(target) || {};
    el.title.textContent = info.title || target.pane;
    el.meta.replaceChildren();
    const add = (text, cls = "") => {
      if (!text) return;
      const span = document.createElement("span");
      if (cls) span.className = cls;
      span.textContent = text;
      el.meta.append(span);
    };
    add(info.kind || info.command || "pane", "pane-drawer-kind");
    if (info.state) {
      const pill = document.createElement("span");
      pill.className = `state-pill ${info.state}`;
      pill.textContent = info.state;
      el.meta.append(pill);
    }
    add(info.model);
    add(info.activity ? `active ${info.activity}` : "");
    el.cwd.textContent = info.cwd || "";
    el.cwd.title = info.cwd || "";
    el.cwd.hidden = !info.cwd;
  }

  function renderAccess() {
    const operator = deps.isOperator();
    el.viewOnly.hidden = operator;
    el.outputWrap.hidden = !operator;
    el.composer.hidden = !operator;
    el.abort.hidden = !operator;
    el.share.hidden = !(operator && deps.canShare(target));
    el.updated.hidden = !operator;
  }

  function setOutput(text) {
    const box = el.output;
    const pinned = firstPaint || isNearBottom(box);
    if (box.textContent !== text) {
      box.textContent = text;
      if (pinned) box.scrollTop = box.scrollHeight;
      else el.jump.hidden = false;
    }
    firstPaint = false;
  }

  function setStatus(message) {
    el.status.textContent = message || "";
    el.status.hidden = !message;
  }

  function tick() {
    el.updated.textContent = updatedLabel(lastUpdated, Date.now(), live);
  }

  const wantsOutput = () => Boolean(target) && isOpen() && deps.isOperator() && !document.hidden && !denied;

  function schedule() {
    clearTimeout(timer);
    timer = null;
    if (!wantsOutput() || !streamUnsupported) return;
    timer = setTimeout(refresh, OUTPUT_REFRESH_MS);
  }

  function stopStream() {
    stream?.close();
    stream = null;
    live = false;
  }

  function stopOutput() {
    stopStream();
    clearTimeout(timer);
    timer = null;
  }

  // Start (or keep) whichever output source applies: the live stream, or
  // polling when the daemon has no stream route.
  function startOutput() {
    if (!wantsOutput()) {
      stopOutput();
      return;
    }
    if (streamUnsupported) {
      stopStream();
      if (!timer && !inFlight) refresh();
      return;
    }
    if (stream) return;
    const current = generation;
    const { pane, socket } = target;
    const params = new URLSearchParams({ lines: String(OUTPUT_LINES) });
    if (socket) params.set("socket", socket);
    const handle = deps.openStream(`/api/panes/${encodeURIComponent(pane)}/output/stream?${params}`, {
      onOutput(payload) {
        if (current !== generation || stream !== handle) return;
        setStatus("");
        setOutput(typeof payload?.text === "string" ? payload.text : "");
        lastUpdated = Date.now();
        renderMeta();
        tick();
      },
      onGone() {
        if (current !== generation || stream !== handle) return;
        stream = null;
        live = false;
        setStatus("This pane has closed.");
        tick();
      },
      onState(state, info) {
        if (current !== generation || stream !== handle) return;
        live = state === "live";
        if (state === "live") {
          if (el.status.textContent.startsWith("Connection")) setStatus("");
        } else if (state === "retrying") {
          setStatus(info?.status === 429
            ? "Too many live viewers right now; retrying…"
            : "Connection interrupted; reconnecting…");
        } else if (state === "denied") {
          stream = null;
          denied = true;
          setStatus("Output needs operator access. Sign in or unlock edit.");
        } else if (state === "unsupported") {
          // An older daemon: poll /output instead.
          stream = null;
          streamUnsupported = true;
          refreshNow();
        }
        tick();
      },
    });
    stream = handle;
  }

  // After a send or abort: a live stream shows the result on its own.
  function nudge() {
    if (streamUnsupported) refreshNow();
  }

  async function refresh() {
    if (!target || !isOpen() || !deps.isOperator() || document.hidden) return;
    if (inFlight || !streamUnsupported) return;
    inFlight = true;
    const current = generation;
    const { pane, socket } = target;
    const params = new URLSearchParams({ lines: String(OUTPUT_LINES) });
    if (socket) params.set("socket", socket);
    try {
      const resp = await deps.fetchOutput(`/api/panes/${encodeURIComponent(pane)}/output?${params}`);
      if (current !== generation) return;
      let payload = null;
      try { payload = await resp.json(); } catch (_) { /* status-only rejection */ }
      if (resp.status === 401 || resp.status === 403) {
        denied = true;
        setStatus("Output needs operator access. Sign in or unlock edit.");
        return;
      }
      if (!resp.ok) {
        setStatus(payload?.error || `output unavailable (${resp.status})`);
        return;
      }
      setStatus("");
      setOutput(typeof payload?.text === "string" ? payload.text : "");
      lastUpdated = Date.now();
      tick();
    } catch (_) {
      if (current === generation) setStatus("Connection interrupted; retrying…");
    } finally {
      inFlight = false;
      renderMeta();
      if (current === generation) schedule();
    }
  }

  function refreshNow() {
    clearTimeout(timer);
    timer = null;
    refresh();
  }

  function open(next, from = null) {
    if (!next?.pane) return;
    const same = target && target.pane === next.pane && (target.socket || null) === (next.socket || null);
    target = { pane: next.pane, socket: next.socket || null };
    if (!same) {
      generation++;
      stopOutput();
      firstPaint = true;
      lastUpdated = 0;
      el.output.textContent = "";
      el.jump.hidden = true;
      el.error.textContent = "";
      setStatus("");
      el.updated.textContent = "";
    }
    denied = false;
    wasOperator = deps.isOperator();
    if (!isOpen()) opener = from || document.activeElement;
    el.drawer.hidden = false;
    el.backdrop.hidden = false;
    document.body.classList.add("drawer-open");
    renderMeta();
    renderAccess();
    deps.onHashChange?.(target);
    clearInterval(ticker);
    ticker = setInterval(tick, 1000);
    (deps.isOperator() ? el.text : el.close).focus();
    if (deps.isOperator()) {
      if (!same) setStatus("Loading output…");
      startOutput();
    }
  }

  function close() {
    if (!isOpen()) return;
    generation++;
    stopOutput();
    clearInterval(ticker);
    ticker = null;
    const closed = target;
    target = null;
    el.drawer.hidden = true;
    el.backdrop.hidden = true;
    document.body.classList.remove("drawer-open");
    deps.onHashChange?.(null);
    const back = opener && opener.isConnected ? opener : deps.findOpener?.(closed);
    opener = null;
    back?.focus?.();
  }

  async function send() {
    if (!target || sending || !deps.isOperator()) return;
    const text = el.text.value;
    if (!text.trim()) return;
    if (new TextEncoder().encode(text).length > MAX_MESSAGE_BYTES) {
      el.error.textContent = "Message is too long (16 KiB max).";
      return;
    }
    const { pane, socket } = target;
    sending = true;
    el.send.disabled = true;
    el.text.disabled = true;
    el.send.textContent = "Sending…";
    el.error.textContent = "";
    try {
      await deps.controlFetch(`/api/panes/${encodeURIComponent(pane)}/prompt`, {
        method: "POST",
        body: JSON.stringify({ text, submit: true, socket }),
      });
      if (target?.pane === pane) {
        el.text.value = "";
        el.output.scrollTop = el.output.scrollHeight;
        el.jump.hidden = true;
        nudge();
      }
    } catch (error) {
      el.error.textContent = error?.message || "send failed";
    } finally {
      sending = false;
      el.send.disabled = false;
      el.text.disabled = false;
      el.send.textContent = "Send";
      if (isOpen()) el.text.focus();
    }
  }

  async function abort() {
    if (!target) return;
    const { pane, socket } = target;
    if (!window.confirm(`Send Ctrl-C to ${el.title.textContent || pane}?`)) return;
    try {
      await deps.controlFetch(`/api/panes/${encodeURIComponent(pane)}/abort`, {
        method: "POST",
        body: JSON.stringify({ socket }),
      });
      deps.showToast(`abort sent to ${pane}`);
      nudge();
    } catch (error) {
      el.error.textContent = error?.message || "abort failed";
    }
  }

  // Keep Tab inside the open drawer (it is a modal dialog).
  function trapFocus(event) {
    if (event.key !== "Tab") return;
    const focusable = [...el.drawer.querySelectorAll(
      "button:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex='-1'])"
    )].filter((node) => !node.closest("[hidden]"));
    if (focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  el.close.addEventListener("click", close);
  el.backdrop.addEventListener("click", close);
  el.drawer.addEventListener("keydown", trapFocus);
  el.composer.addEventListener("submit", (event) => {
    event.preventDefault();
    send();
  });
  el.text.addEventListener("keydown", (event) => {
    if (composerKeyAction(event) !== "send") return;
    event.preventDefault();
    send();
  });
  el.abort.addEventListener("click", abort);
  el.share.addEventListener("click", () => {
    if (target) deps.openShare({ ...target });
  });
  el.output.addEventListener("scroll", () => {
    if (isNearBottom(el.output)) el.jump.hidden = true;
  });
  el.jump.addEventListener("click", () => {
    el.output.scrollTop = el.output.scrollHeight;
    el.jump.hidden = true;
  });
  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape" || !isOpen()) return;
    // A share dialog on top owns Escape.
    if (document.querySelector("dialog[open]")) return;
    event.preventDefault();
    close();
  });
  document.addEventListener("visibilitychange", () => {
    if (!isOpen()) return;
    // Hidden tab: close the stream (or stop polling); reopen when visible.
    if (document.hidden) stopOutput();
    else startOutput();
  });

  return {
    open,
    close,
    isOpen,
    target: () => (target ? { ...target } : null),
    /// Re-read access and metadata (after a sign-in change or a data poll).
    refreshAccess() {
      if (!isOpen()) return;
      const operator = deps.isOperator();
      if (operator !== wasOperator) {
        // Unlocked or signed in while open: start (or stop) reading output.
        wasOperator = operator;
        denied = false;
        if (!operator) stopOutput();
      }
      renderMeta();
      renderAccess();
      if (operator && !denied) startOutput();
    },
  };
}
