// Landing hero film: what muxa does, in four scenes on a pretend tmux window.
//   1 six agents are working in their panes
//   2 two of them stop and wait; the status line and a notification say so
//   3 `muxa attend` jumps to the one that has waited longest
//   4 the dashboard's Work board shows the whole fleet
// One clock drives everything (no video file). It pauses off screen or in a
// hidden tab; with reduced motion it does not autoplay and rests on scene 3.

const STEPS = [
  { start: 0, end: 4200 },
  { start: 4200, end: 8400 },
  { start: 8400, end: 13600 },
  { start: 13600, end: 18000 },
];
const TOTAL = 18000;
const REST_STEP = 2;

// Pane content stays in English, like a real terminal; only the chrome is
// translated.
const PANES = [
  { alias: "planner", kind: "claude", lines: ["Read docs/rate-limit.md", "Read src/routes/orders.ts", "Plan: token bucket, 60/min", "Drafting the 429 response…"], waits: "input", ask: "Retry-After: seconds or HTTP date?" },
  { alias: "impl", kind: "codex", lines: ["Edit src/middleware/limit.ts", "Edit src/routes/orders.ts", "Run npm test", "✓ 212 passed · 0 failed"] },
  { alias: "reviewer", kind: "claude", lines: ["Reviewing diff (+184 −12)", "limit.ts:41 refill + read", "Simulating 500 req burst", "Checking bucket races…"], waits: "choice", ask: "Lua script, or accept ±1 drift?" },
  { alias: "e2e", kind: "claude", lines: ["checkout.spec.ts", "Payment iframe: 8.2s", "Raise wait, add retry", "Re-running 50× on CI…"] },
  { alias: "a11y", kind: "gemini", lines: ["Auditing checkout form", "✗ CVC input has no label", "✗ focus lost on coupon", "Checking error messages…"] },
  { alias: "deploy", kind: "codex", lines: ["pipeline.yml: canary 10%", "Rollback on 5xx > 1%", "Dry run on staging", "Watching 5xx rate…"] },
];

const BOARD = [
  ["queued", [["API-160", "Idempotency keys"]]],
  ["in_progress", [["API-142", "Rate limit orders API", "attention"], ["OPS-19", "Canary deploys"]]],
  ["review", [["WEB-91", "Settings page tabs"]]],
  ["done", [["OPS-23", "Backfill order_totals"]]],
];

const esc = (value) => String(value).replace(/[&<>"']/g, (ch) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[ch]);
const clamp01 = (x) => Math.max(0, Math.min(1, x));
const ease = (x) => 1 - (1 - clamp01(x)) ** 3;

function markup(t) {
  const panes = PANES.map((pane, i) => `
    <div class="film-pane" data-pane="${i}"${pane.waits ? ` data-waits="${pane.waits}"` : ""}>
      <div class="film-pane-head"><span class="film-dot"></span><b>${esc(pane.alias)}</b><small>${esc(pane.kind)}</small><em class="film-state"></em></div>
      <div class="film-pane-body">
        ${pane.lines.map((line, i) => `<span${i === pane.lines.length - 1 ? ' class="film-now"' : ""}>${esc(line)}</span>`).join("")}
        ${pane.ask ? `<span class="film-ask">? ${esc(pane.ask)}</span>` : ""}
      </div>
    </div>`).join("");
  const board = BOARD.map(([stage, cards]) => `
    <div class="film-lane">
      <span>${esc(t.stages[stage])}</span>
      ${cards.map(([id, title, signal]) => `<div class="film-card${signal ? " is-attention" : ""}"><b>${esc(id)}</b>${esc(title)}${signal ? `<i>${esc(t.attention)}</i>` : ""}</div>`).join("")}
    </div>`).join("");
  const steps = t.steps.map((label, i) => `
    <button type="button" class="film-step" data-film-step="${i}" aria-label="${esc(label)}">
      <span class="film-bar"><span></span></span><span class="film-step-label">${esc(label)}</span>
    </button>`).join("");
  return `
    <div class="film-head">
      <span class="film-lights" aria-hidden="true"><i></i><i></i><i></i></span>
      <span class="film-title">tmux · api:orders</span>
      <span class="film-live"><i></i>muxa</span>
    </div>
    <div class="film-stage" aria-hidden="true">
      <div class="film-layer film-tmux">
        <div class="film-grid">${panes}</div>
        <div class="film-cmd"><span class="film-prompt">$</span> <span class="film-typed"></span><span class="film-caret"></span></div>
        <div class="film-status">
          <span class="film-status-left">[api] 0:orders*</span>
          <span class="film-status-right"></span>
        </div>
        <div class="film-toast"><b>muxa</b><span>${esc(t.toast)}</span></div>
      </div>
      <div class="film-layer film-board">
        <div class="film-board-head"><b>${esc(t.boardTitle)}</b><span>${esc(t.boardMeta)}</span></div>
        <div class="film-lanes">${board}</div>
      </div>
    </div>
    <div class="film-caption" aria-live="polite"></div>
    <div class="film-controls">
      <button type="button" class="film-play" aria-label="${esc(t.pause)}"></button>
      <div class="film-steps">${steps}</div>
    </div>`;
}

const PLAY_ICON = `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M5 3.5v9l7-4.5z"/></svg>`;
const PAUSE_ICON = `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4.5 3h2.5v10H4.5zM9 3h2.5v10H9z"/></svg>`;

/**
 * Mount the film into `root`. `t` holds the translated chrome (steps,
 * labels) and `captions` as trusted HTML. Returns `{ destroy }`.
 */
export function mountFilm(root, t) {
  root.classList.add("film");
  root.innerHTML = markup(t);
  const $ = (sel) => root.querySelector(sel);
  const $$ = (sel) => [...root.querySelectorAll(sel)];
  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  let elapsed = reduced ? STEPS[REST_STEP].end - 1 : 0;
  let playing = !reduced;
  let visible = true;
  let last = 0;
  let frame = 0;
  let shownStep = -1;
  const command = "muxa attend";

  function draw() {
    const step = STEPS.findIndex((s) => elapsed < s.end);
    const current = step === -1 ? STEPS.length - 1 : step;
    const { start, end } = STEPS[current];
    const p = clamp01((elapsed - start) / (end - start));
    root.dataset.step = String(current);

    // Scene 2+: two panes wait. Scene 3: the longest waiter gets focus.
    const waiting = current >= 1;
    const focused = current >= 2 && p > (current === 2 ? 0.45 : 0);
    $$(".film-pane").forEach((pane) => {
      const waits = pane.dataset.waits;
      const isWaiting = waiting && waits;
      pane.classList.toggle("is-waiting", Boolean(isWaiting));
      pane.classList.toggle("is-focused", focused && pane.dataset.pane === "0");
      pane.classList.toggle("is-dim", focused && pane.dataset.pane !== "0");
      pane.querySelector(".film-state").textContent = isWaiting ? t.states[waits] : t.states.working;
    });
    root.style.setProperty("--toast", String(current === 1 ? ease((p - 0.35) * 4) : 0));
    $(".film-status-right").textContent = waiting ? t.statusWaiting : t.statusWorking;
    $(".film-status-right").classList.toggle("is-waiting", waiting && !focused);

    // Scene 3 types the command, then "presses enter" at 45%.
    const typed = current === 2 ? Math.round(clamp01(p / 0.4) * command.length) : current > 2 ? command.length : 0;
    $(".film-typed").textContent = command.slice(0, typed);
    $(".film-cmd").classList.toggle("is-on", current === 2);
    root.style.setProperty("--board", String(current === 3 ? ease(p * 2.5) : 0));

    if (shownStep !== current) {
      shownStep = current;
      $(".film-caption").innerHTML = t.captions[current];
    }
    $$(".film-step").forEach((button, i) => {
      const fill = i < current ? 1 : i === current ? p : 0;
      button.querySelector(".film-bar span").style.transform = `scaleX(${fill})`;
      button.classList.toggle("on", i === current);
      button.setAttribute("aria-current", i === current ? "step" : "false");
    });
    const play = $(".film-play");
    play.innerHTML = playing ? PAUSE_ICON : PLAY_ICON;
    play.setAttribute("aria-label", playing ? t.pause : t.play);
  }

  function tick(now) {
    frame = 0;
    if (!playing || !visible || document.hidden) { last = 0; return; }
    if (last) elapsed = (elapsed + (now - last)) % TOTAL;
    last = now;
    draw();
    frame = requestAnimationFrame(tick);
  }
  const resume = () => { if (!frame && playing && visible && !document.hidden) frame = requestAnimationFrame(tick); };

  root.addEventListener("click", (event) => {
    const stepButton = event.target.closest("[data-film-step]");
    if (stepButton) {
      const i = Number(stepButton.dataset.filmStep);
      // Jump to the point where the scene has fully played out.
      elapsed = STEPS[i].start + (STEPS[i].end - STEPS[i].start) * (playing ? 0 : 0.99);
      draw();
      return;
    }
    if (event.target.closest(".film-play")) {
      playing = !playing;
      last = 0;
      draw();
      resume();
    }
  });

  const observer = "IntersectionObserver" in window
    ? new IntersectionObserver(([entry]) => { visible = entry.isIntersecting; resume(); })
    : null;
  observer?.observe(root);
  const onVisibility = () => resume();
  document.addEventListener("visibilitychange", onVisibility);

  draw();
  resume();
  return {
    destroy() {
      if (frame) cancelAnimationFrame(frame);
      observer?.disconnect();
      document.removeEventListener("visibilitychange", onVisibility);
    },
  };
}
