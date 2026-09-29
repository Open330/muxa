// Landing view for visitors who can read nothing here: no operator session
// and no dashboard token. It explains muxa up front (a problem-first hero,
// a short film of the product, how it works, real screens from a demo
// fleet) and keeps the ways in close by: Sign in when the daemon has a
// login provider, and the dashboard token.
//
// Everything is local: a self-hosted daemon should not load fonts, images
// or scripts from elsewhere. Screens are regenerated with
// scripts/landing-shots/capture.mjs from made-up data.

import { mountFilm } from "./landing-film.mjs";

const LANG_KEY = "muxa.landing.lang";
const GITHUB = "https://github.com/Open330/muxa";

const COPY = {
  en: {
    badge: "tmux · Claude Code · Codex · Gemini CLI",
    title: "Six agents running.<br>Which one is <em>waiting on you</em>?",
    lead: "Muxa watches the coding agents you already run in tmux, tells you the moment one stops for input, a choice, or an error, and takes you straight to its pane. No wrapper, no new terminal.",
    how: "How it works",
    private: "<b>{host}</b> is a private muxa dashboard.",
    privateSignIn: "Sign in, or use its access token.",
    privateToken: "Open it with its access token.",
    signedInNoAccess: "Signed in as <b>{email}</b>, but this account has no access here.",
    tokenRejected: "The saved access token was not accepted.",
    signIn: "Sign in",
    signOut: "Sign out",
    useToken: "Use access token",
    tokenPrompt: "Muxa dashboard token",
    film: {
      steps: ["Working", "Someone waits", "Jump there", "See it all"],
      captions: [
        "Six agents, six panes. You're busy in one of them.",
        "Two stop and wait. The status line and a notification say who, and for how long.",
        "`muxa attend` jumps to the agent that has waited longest.",
        "The dashboard groups the fleet into Work, with what needs you on top.",
      ],
      states: { working: "working", input: "waiting", choice: "waiting" },
      statusWorking: "● 6 working",
      statusWaiting: "⚠ 2 waiting · planner 4m",
      toast: "planner is waiting for input",
      boardTitle: "Work board",
      boardMeta: "acme · 5 works · 2 need you",
      attention: "needs you",
      stages: { queued: "Queued", in_progress: "In progress", review: "Review", done: "Done" },
      play: "Play",
      pause: "Pause",
    },
    howTitle: "How it works",
    howLede: "Muxa doesn't launch or wrap your agents. It reads the state they already report and maps it onto the panes you already have.",
    flow: [
      ["Observe", "Claude Code, Codex, and Gemini CLI hooks report state; screen detection covers agents without hooks."],
      ["Model", "One daemon keeps every agent's state: working, waiting for input or a choice, error, idle."],
      ["Notify", "The tmux status line, desktop notifications, this dashboard, and the Mac app show who is waiting."],
      ["Jump", "<code>muxa attend</code> focuses the pane that has waited longest; <code>--cycle</code> goes through the rest."],
      ["Group", "Panes and runs roll up into Work, linked to GitHub or Linear issues, on one board."],
      ["Delegate", "<code>muxa mcp</code> and <code>muxa msg</code> let one agent prompt its peers and wait for replies."],
    ],
    shotsTitle: "The dashboard",
    shotsLede: "Real screens with a made-up fleet: three services, ten agents, eight Works.",
    shots: [
      ["board", "Work board", "Work grouped by stage. Cards that need you carry a signal; each lists its agents and their state."],
      ["collaboration", "Collaboration", "Who asked whom for what: requests and replies between agents, as a graph and a timeline."],
      ["agents", "Agents", "Every agent with its state, model, context, limits, last prompt, and prompt or abort controls."],
    ],
    principlesTitle: "Three principles",
    principles: [
      ["Your setup, unchanged", "No wrapper, no new terminal, no new multiplexer. tmux, rmux, and herdr can be watched at the same time."],
      ["Local first", "State lives in a daemon on your machine. The dashboard is served by that daemon and loads nothing from elsewhere."],
      ["Control stays explicit", "Prompting or aborting an agent takes an operator token or sign-in; automation rules run inside guards they can't opt out of."],
    ],
    surfacesTitle: "One state, everywhere",
    surfaces: [
      ["muxa attend", "Jump to the agent that needs you"],
      ["muxa watch", "The whole fleet in one TUI"],
      ["muxa peek", "Overlay on the current tmux window"],
      ["status line", "Your pane's agent, in tmux"],
      ["dashboard", "Work board, timeline, collaboration"],
      ["Muxa for Mac", "Native app with notifications"],
      ["muxa mcp", "Let an agent orchestrate the rest"],
      ["muxa automation", "Rules on agent state, with guards"],
    ],
    supportTitle: "Works with",
    agentsLabel: "Agents",
    hostsLabel: "Multiplexers",
    installTitle: "Run your own",
    installBody: "Needs tmux 3.x (or herdr) on macOS or Linux. <code>muxa init</code> wires tmux and the agent hooks and starts the daemon, dashboard included.",
    installC1: "# wires tmux and agent hooks, starts the daemon",
    installC2: "# jump to the agent that has waited longest",
    github: "View on GitHub",
    docs: "Docs",
    ctaTitle: "Is this your dashboard?",
    footer: "Part of <a href=\"https://github.com/Open330\">Open330</a> · open source tools for AI-agent workflows",
    site: {
      install: "Install",
      watchShot: ["watch", "muxa watch", "The TUI: the whole fleet, the inspector, the swarm view, and muxa attend jumping to who needs you."],
      installTitle: "Install",
      installBody: "Needs tmux 3.x (or herdr) on macOS or Linux. Homebrew is the main path; the others are for the Mac app, a look before installing, and building from source.",
      ways: [
        ["Homebrew", "brew install open330/tap/muxa\nmuxa init\nmuxa doctor", "<code>muxa init</code> wires tmux and the agent hooks and starts the daemon; <code>muxa doctor</code> checks the result."],
        ["Muxa for Mac", "brew install --cask open330/tap/muxa-app", "Notarized. Updates through Homebrew; the app has no built-in updater."],
        ["Try it without installing", "curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh", "Runs the real <code>muxa onboard</code> in a throwaway tmux server and deletes it on exit. Your tmux server is never touched."],
        ["From source", "git clone https://github.com/Open330/muxa.git\ncd muxa && scripts/install.sh", "Rust 1.89 or newer. Builds and installs <code>muxad</code> and <code>muxa</code>, then runs <code>muxa init</code>."],
      ],
      ctaTitle: "Stop hunting through tmux windows.",
      ctaBody: "<strong>Beta.</strong> The daemon, CLI, TUIs, notifications, dashboard, stats, and reports work end to end; APIs may still change before 1.0.",
      changelog: "Changelog",
      releases: "Releases",
    },
  },
  ko: {
    badge: "tmux · Claude Code · Codex · Gemini CLI",
    title: "에이전트는 여섯 개.<br><em>누가 나를 기다리는지</em> 아시나요?",
    lead: "muxa는 tmux에서 이미 돌리고 있는 코딩 에이전트를 지켜보다가, 입력이나 선택을 기다리거나 오류로 멈추는 순간 알려 주고 그 pane으로 바로 데려다 줍니다. 래퍼도, 새 터미널도 필요 없습니다.",
    how: "어떻게 동작하나",
    private: "<b>{host}</b>는 비공개 muxa 대시보드입니다.",
    privateSignIn: "로그인하거나 접근 토큰을 사용하세요.",
    privateToken: "접근 토큰으로 열 수 있습니다.",
    signedInNoAccess: "<b>{email}</b>(으)로 로그인했지만 이 계정에는 접근 권한이 없습니다.",
    tokenRejected: "저장된 접근 토큰이 거부되었습니다.",
    signIn: "로그인",
    signOut: "로그아웃",
    useToken: "접근 토큰 사용",
    tokenPrompt: "Muxa 대시보드 토큰",
    film: {
      steps: ["작업 중", "누군가 멈춤", "바로 이동", "한눈에 보기"],
      captions: [
        "에이전트 여섯, pane 여섯. 나는 그중 하나에서 일하는 중입니다.",
        "둘이 멈춰서 기다립니다. status line과 알림이 누가, 얼마나 기다렸는지 알려 줍니다.",
        "`muxa attend` 한 번이면 가장 오래 기다린 에이전트로 이동합니다.",
        "대시보드는 전체를 Work 단위로 묶고, 내가 봐야 할 것을 위에 올립니다.",
      ],
      states: { working: "작업 중", input: "대기 중", choice: "대기 중" },
      statusWorking: "● 6 작업 중",
      statusWaiting: "⚠ 2 대기 · planner 4분",
      toast: "planner가 입력을 기다립니다",
      boardTitle: "Work 보드",
      boardMeta: "acme · Work 5개 · 2개 확인 필요",
      attention: "확인 필요",
      stages: { queued: "대기", in_progress: "진행 중", review: "리뷰", done: "완료" },
      play: "재생",
      pause: "일시정지",
    },
    howTitle: "어떻게 동작하나",
    howLede: "muxa는 에이전트를 띄우거나 감싸지 않습니다. 에이전트가 이미 내보내는 상태를 읽어서, 이미 쓰고 있는 pane 위에 얹습니다.",
    flow: [
      ["관찰", "Claude Code, Codex, Gemini CLI의 hook이 상태를 보내고, hook이 없는 에이전트는 화면 감지로 읽습니다."],
      ["상태 모델", "데몬 하나가 모든 에이전트의 상태를 들고 있습니다. 작업 중, 입력·선택 대기, 오류, 유휴."],
      ["알림", "tmux status line, 데스크톱 알림, 이 대시보드, Mac 앱이 누가 기다리는지 보여 줍니다."],
      ["이동", "<code>muxa attend</code>는 가장 오래 기다린 pane으로 포커스를 옮기고, <code>--cycle</code>은 나머지를 차례로 돕니다."],
      ["묶기", "pane과 실행은 Work로 묶이고, GitHub·Linear 이슈와 연결되어 한 보드에 모입니다."],
      ["위임", "<code>muxa mcp</code>와 <code>muxa msg</code>로 에이전트 하나가 동료에게 지시하고 답을 기다립니다."],
    ],
    shotsTitle: "대시보드",
    shotsLede: "가상의 팀(서비스 셋, 에이전트 열, Work 여덟)으로 채운 실제 화면입니다.",
    shots: [
      ["board", "Work 보드", "단계별로 모인 Work. 내가 봐야 할 카드에는 신호가 붙고, 각 카드에 에이전트와 상태가 보입니다."],
      ["collaboration", "협업", "누가 누구에게 무엇을 요청했는지. 에이전트 사이의 요청과 답장을 그래프와 시간순으로 봅니다."],
      ["agents", "에이전트", "모든 에이전트의 상태, 모델, 컨텍스트, 사용량 한도, 마지막 프롬프트와 프롬프트·중단 버튼."],
    ],
    principlesTitle: "세 가지 원칙",
    principles: [
      ["지금 환경 그대로", "래퍼도, 새 터미널도, 새 멀티플렉서도 없습니다. tmux, rmux, herdr를 동시에 볼 수 있습니다."],
      ["로컬 우선", "상태는 내 머신의 데몬에 있습니다. 대시보드도 그 데몬이 제공하고, 외부에서 아무것도 불러오지 않습니다."],
      ["제어는 명시적으로", "에이전트에게 프롬프트를 보내거나 중단하려면 운영자 토큰이나 로그인이 필요하고, 자동화 규칙은 끌 수 없는 안전장치 안에서 돕니다."],
    ],
    surfacesTitle: "어디서 보든 같은 상태",
    surfaces: [
      ["muxa attend", "나를 기다리는 에이전트로 이동"],
      ["muxa watch", "전체 에이전트를 한 TUI에서"],
      ["muxa peek", "현재 tmux 창 위 오버레이"],
      ["status line", "지금 pane의 에이전트를 tmux에"],
      ["대시보드", "Work 보드, 타임라인, 협업"],
      ["Muxa for Mac", "알림이 있는 네이티브 앱"],
      ["muxa mcp", "에이전트 하나가 나머지를 지휘"],
      ["muxa automation", "안전장치가 있는 상태 규칙"],
    ],
    supportTitle: "지원",
    agentsLabel: "에이전트",
    hostsLabel: "멀티플렉서",
    installTitle: "직접 운영하기",
    installBody: "macOS나 Linux에서 tmux 3.x(또는 herdr)가 필요합니다. <code>muxa init</code>이 tmux와 에이전트 hook을 연결하고 대시보드를 포함한 데몬을 시작합니다.",
    installC1: "# tmux와 에이전트 hook 연결, 데몬 시작",
    installC2: "# 가장 오래 기다린 에이전트로 이동",
    github: "GitHub에서 보기",
    docs: "문서",
    ctaTitle: "내 대시보드인가요?",
    footer: "<a href=\"https://github.com/Open330\">Open330</a>의 프로젝트 · AI 에이전트 워크플로를 위한 오픈소스 도구",
    site: {
      install: "설치",
      watchShot: ["watch", "muxa watch", "TUI 화면: 전체 에이전트, inspector, swarm 뷰, 그리고 나를 기다리는 곳으로 가는 muxa attend."],
      installTitle: "설치",
      installBody: "macOS나 Linux에서 tmux 3.x(또는 herdr)가 필요합니다. Homebrew가 기본 경로이고, 나머지는 Mac 앱, 설치 전 체험, 소스 빌드용입니다.",
      ways: [
        ["Homebrew", "brew install open330/tap/muxa\nmuxa init\nmuxa doctor", "<code>muxa init</code>이 tmux와 에이전트 hook을 연결하고 데몬을 시작하며, <code>muxa doctor</code>가 결과를 점검합니다."],
        ["Muxa for Mac", "brew install --cask open330/tap/muxa-app", "공증된 앱입니다. 앱 안에 업데이트 기능이 없으므로 Homebrew로 갱신합니다."],
        ["설치 없이 체험", "curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh", "일회용 tmux 서버에서 진짜 <code>muxa onboard</code>를 실행하고 끝나면 지웁니다. 기존 tmux 서버는 건드리지 않습니다."],
        ["소스에서", "git clone https://github.com/Open330/muxa.git\ncd muxa && scripts/install.sh", "Rust 1.89 이상이 필요합니다. <code>muxad</code>와 <code>muxa</code>를 빌드해 설치하고 <code>muxa init</code>까지 실행합니다."],
      ],
      ctaTitle: "tmux 창을 뒤지는 일은 이제 그만.",
      ctaBody: "<strong>베타.</strong> 데몬, CLI, TUI, 알림, 대시보드, 통계, 리포트는 끝까지 동작하지만 1.0 전에는 API가 바뀔 수 있습니다.",
      changelog: "변경 기록",
      releases: "릴리스",
    },
  },
};

const AGENTS = ["Claude Code", "Codex", "Gemini CLI", "Antigravity", "opencode"];
const HOSTS = ["tmux", "rmux", "herdr"];

export const LANDING_LANGUAGES = Object.keys(COPY);

/** Saved choice first, then the browser's preferred languages, then English. */
export function pickLanguage(saved, preferred = []) {
  if (LANDING_LANGUAGES.includes(saved)) return saved;
  for (const tag of preferred) {
    const base = String(tag || "").toLowerCase().split("-")[0];
    if (LANDING_LANGUAGES.includes(base)) return base;
  }
  return "en";
}

/**
 * Which ways in to offer. `login` is the dashboard's normalized login state;
 * `tokenRejected` is true when a stored token was sent and refused.
 */
export function landingAccess(login, { tokenRejected = false } = {}) {
  const signedIn = Boolean(login?.signedIn);
  return {
    signIn: Boolean(login?.available) && !signedIn,
    signOut: signedIn,
    token: true,
    // With a provider, sign-in is the main door and the token the fallback.
    tokenPrimary: !login?.available,
    signedInNoAccess: signedIn && login?.role === "none",
    tokenRejected,
  };
}

function escapeText(value) {
  return String(value ?? "").replace(/[&<>"']/g, (ch) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  })[ch]);
}

function fill(template, values) {
  return template.replace(/\{(\w+)\}/g, (_, key) => escapeText(values[key]));
}

const GITHUB_ICON = `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0 0 16 8c0-4.42-3.58-8-8-8z"/></svg>`;
const LOCK_ICON = `<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4 7V5a4 4 0 1 1 8 0v2h.5A1.5 1.5 0 0 1 14 8.5v5a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 2 13.5v-5A1.5 1.5 0 0 1 3.5 7H4zm1.5 0h5V5a2.5 2.5 0 0 0-5 0v2z"/></svg>`;

// Backticks in film captions mark inline code.
const codeSpans = (text) => escapeText(text).replace(/`([^`]+)`/g, "<code>$1</code>");

function accessButtons(t, access, size = "") {
  return [
    access.signIn ? `<button class="l-btn primary ${size}" type="button" data-landing-action="sign-in">${escapeText(t.signIn)}</button>` : "",
    `<button class="l-btn ${access.tokenPrimary ? "primary" : "ghost"} ${size}" type="button" data-landing-action="token">${escapeText(t.useToken)}</button>`,
    access.signOut ? `<button class="l-btn ghost ${size}" type="button" data-landing-action="sign-out">${escapeText(t.signOut)}</button>` : "",
  ].join("");
}

// Where icon.svg, landing/*.webp and demo.gif live: the daemon serves them
// under /static/, the GitHub Pages site next to its index.html.
const asset = (path) => `${view.base}${path}`;

function shotPicture(name, alt) {
  // The TUI recording (site only) is one GIF for both color schemes.
  if (name === "watch") {
    return `<a class="l-shot-link" href="${asset("demo.gif")}" target="_blank" rel="noopener"><img src="${asset("demo.gif")}" alt="${escapeText(alt)}" loading="lazy" decoding="async"></a>`;
  }
  // The shot follows the page's color scheme; on a phone it is small, so it
  // links to the full-size image.
  const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  return `<a class="l-shot-link" href="${asset(`landing/${name}-${dark ? "dark" : "light"}.webp`)}" target="_blank" rel="noopener"><picture>
    <source srcset="${asset(`landing/${name}-dark.webp`)}" media="(prefers-color-scheme: dark)">
    <img src="${asset(`landing/${name}-light.webp`)}" alt="${escapeText(alt)}" loading="lazy" decoding="async">
  </picture></a>`;
}

/** The screens tabs: the public site leads with the TUI recording. */
const shotsFor = (t) => (view.mode === "site" ? [t.site.watchShot, ...t.shots] : t.shots);

function markup(t, access, { host, email, shot }) {
  const notices = [
    access.signedInNoAccess ? `<p class="l-notice">${fill(t.signedInNoAccess, { email: email || "" })}</p>` : "",
    access.tokenRejected ? `<p class="l-notice">${escapeText(t.tokenRejected)}</p>` : "",
  ].join("");
  const langs = LANDING_LANGUAGES.map((lang) =>
    `<button type="button" data-landing-lang="${lang}" aria-pressed="${t === COPY[lang]}">${lang === "ko" ? "한국어" : "EN"}</button>`
  ).join("");
  const flow = t.flow.map(([heading, body], i) =>
    `<li class="l-step" style="--i:${i}"><b><span>${i + 1}</span>${escapeText(heading)}</b><p>${body}</p></li>`
  ).join("");
  const site = view.mode === "site";
  const shots = shotsFor(t);
  const shotTabs = shots.map(([name, label], i) =>
    `<button type="button" role="tab" data-landing-shot="${i}" aria-selected="${i === shot}">${escapeText(label)}</button>`
  ).join("");
  const [shotName, shotLabel, shotCaption] = shots[shot];
  const principles = t.principles.map(([heading, body]) =>
    `<div class="l-card"><h3>${escapeText(heading)}</h3><p>${escapeText(body)}</p></div>`
  ).join("");
  const surfaces = t.surfaces.map(([name, body]) =>
    `<div class="l-surface"><code>${escapeText(name)}</code><span>${escapeText(body)}</span></div>`
  ).join("");
  const chips = (items) => items.map((item) => `<span class="l-chip">${escapeText(item)}</span>`).join("");

  return `
    <header class="l-nav">
      <div class="l-wrap">
        <span class="l-brand"><img src="${asset("icon.svg")}" width="26" height="26" alt="">muxa</span>
        ${site ? "" : `<span class="l-host" title="${escapeText(host)}">${LOCK_ICON}${escapeText(host)}</span>`}
        <nav class="l-nav-actions">
          <div class="l-lang" role="group" aria-label="Language">${langs}</div>
          <a class="l-btn ghost sm l-hide-sm" href="${GITHUB}" rel="noopener">GitHub</a>
          ${site
            ? `<a class="l-btn dark sm" href="#install">${escapeText(t.site.install)}</a>`
            : access.signIn
              ? `<button class="l-btn dark sm" type="button" data-landing-action="sign-in">${escapeText(t.signIn)}</button>`
              : `<button class="l-btn dark sm" type="button" data-landing-action="token">${escapeText(t.useToken)}</button>`}
        </nav>
      </div>
    </header>

    <main>
      <section class="l-hero l-wrap">
        <div class="l-hero-copy">
          <span class="l-badge">${escapeText(t.badge)}</span>
          <h1>${t.title}</h1>
          <p class="l-lead">${escapeText(t.lead)}</p>
          <div class="l-actions">
            ${site
              ? `<a class="l-btn primary lg" href="#install">${escapeText(t.site.install)}</a>
                 <a class="l-btn ghost lg" href="${GITHUB}" rel="noopener">${GITHUB_ICON}<span>${escapeText(t.github)}</span></a>`
              : accessButtons(t, access, "lg")}
            <a class="l-btn ghost lg" href="#how">${escapeText(t.how)} ↓</a>
          </div>
          ${site ? "" : `<div class="l-private">
            ${LOCK_ICON}
            <p>${fill(t.private, { host })} ${escapeText(access.signIn || access.signOut ? t.privateSignIn : t.privateToken)}</p>
          </div>
          ${notices}`}
        </div>
        <div class="l-film" data-landing-film></div>
      </section>

      <section class="l-section l-wrap" id="how" data-reveal>
        <h2>${escapeText(t.howTitle)}</h2>
        <p class="l-lede">${escapeText(t.howLede)}</p>
        <ol class="l-flow">${flow}</ol>
      </section>

      <section class="l-section l-wrap" id="screens" data-reveal>
        <h2>${escapeText(t.shotsTitle)}</h2>
        <p class="l-lede">${escapeText(t.shotsLede)}</p>
        <div class="l-shot">
          <div class="l-shot-bar">
            <span class="l-shot-lights" aria-hidden="true"><i></i><i></i><i></i></span>
            <div class="l-shot-tabs" role="tablist" aria-label="${escapeText(t.shotsTitle)}">${shotTabs}</div>
          </div>
          <figure>
            ${shotPicture(shotName, `${shotLabel}: ${shotCaption}`)}
            <figcaption>${escapeText(shotCaption)}</figcaption>
          </figure>
        </div>
      </section>

      <section class="l-section l-wrap" data-reveal>
        <h2>${escapeText(t.principlesTitle)}</h2>
        <div class="l-three">${principles}</div>
      </section>

      <section class="l-section l-wrap" data-reveal>
        <h2>${escapeText(t.surfacesTitle)}</h2>
        <div class="l-surfaces">${surfaces}</div>
        <div class="l-support">
          <span>${escapeText(t.agentsLabel)}</span><div>${chips(AGENTS)}</div>
          <span>${escapeText(t.hostsLabel)}</span><div>${chips(HOSTS)}</div>
        </div>
      </section>

      <section class="l-section l-wrap l-install" id="install" data-reveal>
        <div>
          <h2>${escapeText(site ? t.site.installTitle : t.installTitle)}</h2>
          <p>${site ? t.site.installBody : t.installBody}</p>
          <div class="l-actions">
            <a class="l-btn ghost" href="${GITHUB}" rel="noopener">${GITHUB_ICON}<span>${escapeText(t.github)}</span></a>
            <a class="l-btn ghost" href="${GITHUB}/tree/main/docs" rel="noopener">${escapeText(t.docs)}</a>
          </div>
        </div>
<pre class="l-code"><code><span class="p">$</span> brew install open330/tap/muxa
<span class="p">$</span> muxa init      <span class="c">${escapeText(t.installC1)}</span>
<span class="p">$</span> muxa attend    <span class="c">${escapeText(t.installC2)}</span></code></pre>
      </section>
      ${site ? `<div class="l-wrap l-ways" data-reveal>${t.site.ways.map(([heading, command, body]) => `
        <div class="l-card"><h3>${escapeText(heading)}</h3><pre class="l-code sm"><code>${escapeText(command)}</code></pre><p>${body}</p></div>`).join("")}
      </div>` : ""}

      <section class="l-cta l-wrap" data-reveal>
        ${site ? `<div>
          <h2>${escapeText(t.site.ctaTitle)}</h2>
          <p>${t.site.ctaBody}</p>
        </div>
        <div class="l-actions">
          <a class="l-btn primary" href="${GITHUB}" rel="noopener">${GITHUB_ICON}<span>${escapeText(t.github)}</span></a>
          <a class="l-btn ghost" href="${GITHUB}/tree/main/docs" rel="noopener">${escapeText(t.docs)}</a>
        </div>` : `<div>
          <h2>${escapeText(t.ctaTitle)}</h2>
          <p>${fill(t.private, { host })}</p>
        </div>
        <div class="l-actions">${accessButtons(t, access)}</div>`}
      </section>
    </main>

    <footer class="l-foot"><div class="l-wrap">
      <span class="l-brand small"><img src="${asset("icon.svg")}" width="18" height="18" alt="">muxa</span>
      <span>${t.footer}${site ? ` · <a href="${GITHUB}/blob/main/CHANGELOG.md" rel="noopener">${escapeText(t.site.changelog)}</a> · <a href="${GITHUB}/releases" rel="noopener">${escapeText(t.site.releases)}</a>` : ""}</span>
    </div></footer>`;
}

// One landing per page; the click handler reads whatever the latest render
// set, so re-rendering with new login state never leaves stale callbacks.
const view = { root: null, mode: "dashboard", base: "/static/", lang: "en", shot: 0, access: null, host: "", email: null, actions: null, film: null, reveal: null };

function revealOnScroll(root) {
  const items = [...root.querySelectorAll("[data-reveal]")];
  if (!("IntersectionObserver" in window) || window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
    items.forEach((el) => el.classList.add("in"));
    return null;
  }
  const observer = new IntersectionObserver((entries) => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      entry.target.classList.add("in");
      observer.unobserve(entry.target);
    }
  }, { rootMargin: "0px 0px -8% 0px", threshold: 0.12 });
  items.forEach((el) => { el.classList.add("pending"); observer.observe(el); });
  return observer;
}

function draw() {
  const t = COPY[view.lang];
  view.film?.destroy();
  view.reveal?.disconnect();
  document.documentElement.lang = view.lang;
  view.root.innerHTML = markup(t, view.access, { host: view.host, email: view.email, shot: view.shot });
  view.film = mountFilm(view.root.querySelector("[data-landing-film]"), {
    ...t.film,
    captions: t.film.captions.map(codeSpans),
  });
  view.reveal = revealOnScroll(view.root);
}

function showShot(index) {
  view.shot = index;
  const t = COPY[view.lang];
  const [name, label, caption] = shotsFor(t)[index];
  const figure = view.root.querySelector(".l-shot figure");
  figure.innerHTML = `${shotPicture(name, `${label}: ${caption}`)}<figcaption>${escapeText(caption)}</figcaption>`;
  view.root.querySelectorAll("[data-landing-shot]").forEach((tab) => {
    tab.setAttribute("aria-selected", String(Number(tab.dataset.landingShot) === index));
  });
}

function onLandingClick(event) {
  const langButton = event.target.closest("[data-landing-lang]");
  if (langButton) {
    view.lang = langButton.getAttribute("data-landing-lang");
    try { localStorage.setItem(LANG_KEY, view.lang); } catch (_) { /* storage blocked */ }
    draw();
    view.root.querySelectorAll("[data-reveal]").forEach((el) => el.classList.add("in"));
    return;
  }
  const shotTab = event.target.closest("[data-landing-shot]");
  if (shotTab) {
    showShot(Number(shotTab.dataset.landingShot));
    return;
  }
  const action = event.target.closest("[data-landing-action]")?.getAttribute("data-landing-action");
  if (action === "sign-in") view.actions.signIn();
  else if (action === "sign-out") view.actions.signOut();
  else if (action === "token") {
    const token = window.prompt(COPY[view.lang].tokenPrompt);
    if (token && token.trim()) view.actions.useToken(token.trim());
  }
}

/**
 * Render the landing view into `root` and wire its buttons. `actions` holds
 * `signIn`, `signOut` and `useToken(token)` callbacks owned by the dashboard.
 * `mode: "site"` is the public project page (site/index.html): no access
 * prompts, install and GitHub instead; `assetBase` is where its files live.
 */
export function renderLanding(root, { login, tokenRejected = false, actions, mode = "dashboard", assetBase = "/static/" } = {}) {
  view.mode = mode;
  view.base = assetBase;
  let saved = null;
  try { saved = localStorage.getItem(LANG_KEY); } catch (_) { /* storage blocked */ }
  if (view.root !== root) {
    view.root = root;
    view.lang = pickLanguage(saved, navigator.languages || [navigator.language]);
    root.addEventListener("click", onLandingClick);
  }
  view.access = landingAccess(login, { tokenRejected });
  view.host = window.location.host;
  view.email = login?.email || null;
  view.actions = actions;
  root.hidden = false;
  draw();
}
