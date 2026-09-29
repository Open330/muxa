// Landing hero film, made of real screens.
//
// The terminal frames are captures of a real muxa in a sandbox
// (scripts/landing-shots/film.sh → landing-frames.mjs): tmux, the muxa
// status line, `muxa attend` moving the client, and the `muxa peek` popup.
// The last scene is the dashboard's Work board screenshot. This module only
// sequences them, types the command, and adds the desktop notification.
// It pauses off screen or in a hidden tab; with reduced motion it does not
// autoplay and rests on the attend scene.

import { FILM_FRAMES, FILM_SIZE } from "./landing-frames.mjs";

// [frame, duration ms, step, zoom, focus, how]. Steps are what the controls
// show. A whole terminal is small at hero size, so a scene either zooms in
// ("zoom": centre the focus element, or the top-left corner without one) or
// keeps the whole screen and spotlights the focus element ("spot") when it
// sits at an edge where zooming would frame mostly empty space.
const SCENES = [
  ["working", 1800, 0, 1, null, "zoom"],
  ["waiting", 2400, 1, 1, ".film-alert", "spot"],
  ["typed", 1300, 2, 1.7, ".film-type", "zoom"],
  ["attended", 2200, 2, 1.5, null, "zoom"],
  ["peek", 2400, 3, 1, null, "zoom"],
  ["board", 2600, 4, 1, null, "zoom"],
];
const STARTS = SCENES.reduce((acc, [, ms]) => [...acc, acc[acc.length - 1] + ms], [0]);
const TOTAL = STARTS[STARTS.length - 1];
const REST_SCENE = 3;
const COMMAND = "muxa attend";

const esc = (value) => String(value).replace(/[&<>"']/g, (ch) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[ch]);

/** Wrap the typed command so CSS can type it out; mark the status-line alert. */
function decorate(name, html) {
  if (name === "typed") {
    return html.replace(COMMAND, `<span class="film-type">${COMMAND}</span>`);
  }
  if (name === "waiting") {
    // "⚠ 2 need you": the glyph is pinned to one cell by ansi2html (<i class="c">).
    return html.replace(/(<i class="c">⚠<\/i>|⚠)([^<|]*)/, (alert) => `<span class="film-alert">${alert.trimEnd()}</span>${alert.slice(alert.trimEnd().length)}`);
  }
  return html;
}

function markup(t) {
  const layers = SCENES.map(([name], i) => name === "board"
    ? `<div class="film-layer film-shot" data-scene="${i}"><img src="${esc(t.boardSrc)}" alt="" decoding="async"></div>`
    : `<div class="film-layer" data-scene="${i}"><pre class="film-term">${decorate(name, FILM_FRAMES[name])}</pre></div>`
  ).join("");
  const steps = t.steps.map((label, i) => `
    <button type="button" class="film-step" data-film-step="${i}" aria-label="${esc(label)}">
      <span class="film-bar"><span></span></span><span class="film-step-label">${esc(label)}</span>
    </button>`).join("");
  return `
    <div class="film-head">
      <span class="film-lights" aria-hidden="true"><i></i><i></i><i></i></span>
      <span class="film-title">~/acme — tmux</span>
      <span class="film-live"><i></i>muxa</span>
    </div>
    <div class="film-stage" aria-hidden="true" style="--cols:${FILM_SIZE.cols};--rows:${FILM_SIZE.rows}">
      ${layers}
      <div class="film-spot"></div>
      <div class="film-callout"></div>
      <div class="film-toast"><b>muxa</b><span>${esc(t.toast)}</span></div>
    </div>
    <div class="film-caption"></div>
    <div class="film-controls">
      <button type="button" class="film-play" aria-label="${esc(t.pause)}"></button>
      <div class="film-steps">${steps}</div>
    </div>`;
}

/**
 * Point the camera for one scene: scale by `zoom` and move the focus
 * element to the middle of the stage, without showing past the frame's
 * edges. Measured from the live layout, so it holds at any width and after
 * the frames are recaptured.
 */
function aim(layer, zoom, focus, how) {
  const term = layer.querySelector(".film-term");
  if (!term) return;
  const stage = layer.parentElement;
  if (how === "spot") {
    spotlight(stage, term.querySelector(focus));
    term.style.setProperty("--cam", "none");
    return;
  }
  let x = 0;
  let y = 0;
  const target = focus && term.querySelector(focus);
  if (target) {
    // The typed command starts zero-width; aim at where it will end up.
    const cell = term.offsetWidth / FILM_SIZE.cols;
    const width = Math.max(target.offsetWidth, target.textContent.length * cell);
    x = stage.clientWidth / 2 - zoom * (target.offsetLeft + width / 2);
    y = stage.clientHeight / 2 - zoom * (target.offsetTop + target.offsetHeight / 2);
  }
  x = Math.min(0, Math.max(stage.clientWidth - zoom * term.offsetWidth, x));
  y = Math.min(0, Math.max(stage.clientHeight - zoom * term.offsetHeight, y));
  term.style.setProperty("--cam", `translate(${x}px, ${y}px) scale(${zoom})`);
}

/** Place the spotlight ring and the enlarged callout over `target`. */
function spotlight(stage, target) {
  if (!target) return;
  // Unscaled layer, so offsets inside the term are stage coordinates.
  const pad = 4;
  const x = target.offsetLeft - pad;
  const y = target.offsetTop - pad;
  const w = target.offsetWidth + pad * 2;
  const h = target.offsetHeight + pad * 2;
  stage.style.setProperty("--spot-x", `${x}px`);
  stage.style.setProperty("--spot-y", `${y}px`);
  stage.style.setProperty("--spot-w", `${w}px`);
  stage.style.setProperty("--spot-h", `${h}px`);
  const callout = stage.querySelector(".film-callout");
  callout.textContent = target.textContent.trim();
  // Right-align the callout with the target, clear of the stage edge.
  stage.style.setProperty("--callout-right", `${Math.max(8, stage.clientWidth - (x + w))}px`);
  stage.style.setProperty("--callout-bottom", `${stage.clientHeight - y + 10}px`);
}

const PLAY_ICON = `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M5 3.5v9l7-4.5z"/></svg>`;
const PAUSE_ICON = `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4.5 3h2.5v10H4.5zM9 3h2.5v10H9z"/></svg>`;

/**
 * Mount the film into `root`. `t` holds the translated chrome (`steps`,
 * `captions` as trusted HTML, `toast`, `play`, `pause`) and `boardSrc`.
 * Returns `{ destroy }`.
 */
export function mountFilm(root, t) {
  root.classList.add("film");
  root.innerHTML = markup(t);
  const $ = (sel) => root.querySelector(sel);
  const $$ = (sel) => [...root.querySelectorAll(sel)];
  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const stepStart = (step) => STARTS[SCENES.findIndex(([, , s]) => s === step)];
  const stepEnd = (step) => STARTS[SCENES.findLastIndex(([, , s]) => s === step) + 1];

  let elapsed = reduced ? STARTS[REST_SCENE] + 1 : 0;
  let playing = !reduced;
  let visible = true;
  let last = 0;
  let frame = 0;
  let shownScene = -1;

  function draw() {
    const scene = Math.max(0, STARTS.findIndex((start) => elapsed < start) - 1);
    const step = SCENES[scene][2];
    if (scene !== shownScene) {
      shownScene = scene;
      root.dataset.scene = SCENES[scene][0];
      $$(".film-layer").forEach((layer) => {
        const on = Number(layer.dataset.scene) === scene;
        if (on) aim(layer, SCENES[scene][3], SCENES[scene][4], SCENES[scene][5]);
        layer.classList.toggle("on", on);
      });
      $(".film-caption").innerHTML = t.captions[step];
    }
    $$(".film-step").forEach((button, i) => {
      const p = Math.max(0, Math.min(1, (elapsed - stepStart(i)) / (stepEnd(i) - stepStart(i))));
      button.querySelector(".film-bar span").style.transform = `scaleX(${p})`;
      button.classList.toggle("on", i === step);
      button.setAttribute("aria-current", i === step ? "step" : "false");
    });
    const play = $(".film-play");
    const icon = playing ? PAUSE_ICON : PLAY_ICON;
    if (play.dataset.icon !== String(playing)) {
      play.dataset.icon = String(playing);
      play.innerHTML = icon;
    }
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
      // Paused: land on the step's last scene, fully played; playing: its start.
      elapsed = playing ? stepStart(i) : stepEnd(i) - 1;
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
  // Camera and spotlight are measured in pixels; re-aim when the stage
  // resizes (a phone rotating, a window narrowing) mid-scene.
  const resized = "ResizeObserver" in window
    ? new ResizeObserver(() => { shownScene = -1; draw(); })
    : null;
  resized?.observe($(".film-stage"));

  draw();
  resume();
  return {
    destroy() {
      if (frame) cancelAnimationFrame(frame);
      observer?.disconnect();
      resized?.disconnect();
      document.removeEventListener("visibilitychange", onVisibility);
    },
  };
}
