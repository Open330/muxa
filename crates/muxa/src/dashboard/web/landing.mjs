// Landing view for visitors who can read nothing here: no operator session
// and no dashboard token. It says what muxa is and offers the two ways in
// (sign-in when the daemon has a login provider, and the dashboard token),
// instead of an empty dashboard stuck on "sign in required".
//
// Copy follows the project landing page (site/index.html). Everything is
// local: a self-hosted daemon should not load fonts or images from elsewhere.

const LANG_KEY = "muxa.landing.lang";

const COPY = {
  en: {
    badge: "tmux · Claude Code · Codex · Gemini CLI",
    title: "Know which coding agent is <em>waiting on you</em> — and jump to it.",
    lead: "No wrapper, no new terminal. Muxa watches the Claude Code, Codex, and Gemini CLI sessions you already run in tmux, tells you which one needs you, and takes you there.",
    accessTitle: "This dashboard is private",
    accessBody: "It shows the live agents, panes, and Work on <b>{host}</b>. Sign in, or use this dashboard's access token.",
    accessBodyToken: "It shows the live agents, panes, and Work on <b>{host}</b>. Open it with this dashboard's access token.",
    signedInNoAccess: "Signed in as <b>{email}</b>, but this account has no access to this dashboard.",
    tokenRejected: "The saved access token was not accepted.",
    signIn: "Sign in",
    signOut: "Sign out",
    useToken: "Use access token",
    tokenPrompt: "Muxa dashboard token",
    tokenHint: "Operators find the token under <code>[dashboard]</code> in muxa's <code>config.toml</code> on that machine.",
    previewLabel: "What an operator sees",
    previewWaiting: "waiting for input · 4m",
    previewChoice: "asking a choice · 1m",
    previewWorking: "working",
    previewError: "error · 12m",
    whyTitle: "Why muxa",
    why: [
      ["Keeps your setup", "Agent state comes from Claude Code, Codex, and Gemini CLI hooks, with screen detection for hook-less agents. You don't launch agents through muxa and you don't switch multiplexers."],
      ["Tells you who is waiting", "The tmux status line, the <code>muxa watch</code> TUI, desktop notifications, this dashboard, and the Mac app all show which agent is blocked on input, a choice, or an error."],
      ["Takes you there", "<code>muxa attend</code> focuses the pane blocked longest; <code>--cycle</code> tabs through every agent that needs you."],
      ["Lets one agent drive the rest", "<code>muxa mcp</code> gives a coding agent the same view, plus send-prompt and wait-for-change tools, so an orchestrator can prompt peers and wait on them."],
    ],
    installTitle: "Run your own",
    installBody: "Requires tmux 3.x (or herdr) and a Unix-like OS. <code>muxa init</code> wires tmux and the agent hooks and starts the daemon, dashboard included.",
    installC1: "# wires tmux and agent hooks, starts the daemon",
    installC2: "# jump to the agent that has waited longest",
    github: "View on GitHub",
    docs: "Docs",
    footer: "Part of <a href=\"https://github.com/Open330\">Open330</a> · open source tools for AI-agent workflows",
  },
  ko: {
    badge: "tmux · Claude Code · Codex · Gemini CLI",
    title: "지금 <em>나를 기다리는</em> 코딩 에이전트를 알려 주고, 바로 데려다 줍니다.",
    lead: "래퍼도, 새 터미널도 필요 없습니다. 이미 tmux에서 돌리고 있는 Claude Code, Codex, Gemini CLI 세션을 지켜보다가 어느 에이전트가 나를 기다리는지 알려 주고 그 pane으로 옮겨 줍니다.",
    accessTitle: "비공개 대시보드입니다",
    accessBody: "<b>{host}</b>의 실시간 에이전트, pane, Work를 보여 줍니다. 로그인하거나 이 대시보드의 접근 토큰을 사용하세요.",
    accessBodyToken: "<b>{host}</b>의 실시간 에이전트, pane, Work를 보여 줍니다. 이 대시보드의 접근 토큰으로 열 수 있습니다.",
    signedInNoAccess: "<b>{email}</b>(으)로 로그인했지만 이 계정에는 대시보드 접근 권한이 없습니다.",
    tokenRejected: "저장된 접근 토큰이 거부되었습니다.",
    signIn: "로그인",
    signOut: "로그아웃",
    useToken: "접근 토큰 사용",
    tokenPrompt: "Muxa 대시보드 토큰",
    tokenHint: "토큰은 해당 머신의 muxa <code>config.toml</code> 중 <code>[dashboard]</code>에 있습니다.",
    previewLabel: "운영자에게 보이는 화면",
    previewWaiting: "입력 대기 · 4분",
    previewChoice: "선택 요청 · 1분",
    previewWorking: "작업 중",
    previewError: "오류 · 12분",
    whyTitle: "왜 muxa인가",
    why: [
      ["지금 쓰는 환경을 그대로", "에이전트 상태는 Claude Code, Codex, Gemini CLI의 hook에서 오고, hook이 없는 에이전트는 화면 감지로 읽습니다. muxa로 에이전트를 띄우지도, 멀티플렉서를 바꾸지도 않습니다."],
      ["누가 기다리는지 알려 줍니다", "tmux status line, <code>muxa watch</code> TUI, 데스크톱 알림, 이 대시보드, Mac 앱 모두 어느 에이전트가 입력, 선택, 오류로 멈춰 있는지 보여 줍니다."],
      ["그 자리로 데려다 줍니다", "<code>muxa attend</code>는 가장 오래 멈춰 있는 pane으로 포커스를 옮기고, <code>--cycle</code>은 나를 기다리는 에이전트를 차례로 돕니다."],
      ["에이전트 하나가 나머지를 지휘", "<code>muxa mcp</code>는 코딩 에이전트에게 같은 시야와 프롬프트 전송, 상태 변화 대기 도구를 줍니다. 오케스트레이터가 동료 에이전트에게 지시하고 결과를 기다릴 수 있습니다."],
    ],
    installTitle: "직접 운영하기",
    installBody: "tmux 3.x(또는 herdr)와 Unix-like OS가 필요합니다. <code>muxa init</code>이 tmux와 에이전트 hook을 연결하고 대시보드를 포함한 데몬을 시작합니다.",
    installC1: "# tmux와 에이전트 hook을 연결하고 데몬을 시작합니다",
    installC2: "# 가장 오래 기다린 에이전트로 이동합니다",
    github: "GitHub에서 보기",
    docs: "문서",
    footer: "<a href=\"https://github.com/Open330\">Open330</a>의 프로젝트 · AI 에이전트 워크플로를 위한 오픈소스 도구",
  },
};

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

function markup(t, access, { host, email }) {
  const notices = [
    access.signedInNoAccess ? `<p class="landing-notice">${fill(t.signedInNoAccess, { email: email || "" })}</p>` : "",
    access.tokenRejected ? `<p class="landing-notice">${escapeText(t.tokenRejected)}</p>` : "",
  ].join("");
  const buttons = [
    access.signIn ? `<button class="landing-btn primary" type="button" data-landing-action="sign-in">${escapeText(t.signIn)}</button>` : "",
    `<button class="landing-btn${access.tokenPrimary ? " primary" : ""}" type="button" data-landing-action="token">${escapeText(t.useToken)}</button>`,
    access.signOut ? `<button class="landing-btn" type="button" data-landing-action="sign-out">${escapeText(t.signOut)}</button>` : "",
  ].join("");
  const preview = [
    ["waiting_input", "claude", t.previewWaiting],
    ["waiting_choice", "codex", t.previewChoice],
    ["working", "gemini", t.previewWorking],
    ["error", "claude", t.previewError],
  ].map(([state, agent, label]) => `<li><span class="state-dot ${state}"></span><b>${agent}</b><span>${escapeText(label)}</span></li>`).join("");
  const why = t.why.map(([heading, body]) => `<div class="landing-card"><h3>${escapeText(heading)}</h3><p>${body}</p></div>`).join("");
  const langs = LANDING_LANGUAGES.map((lang) =>
    `<button type="button" data-landing-lang="${lang}" aria-pressed="${t === COPY[lang]}">${lang === "ko" ? "한국어" : "EN"}</button>`
  ).join("");

  return `
    <header class="landing-nav">
      <div class="landing-wrap">
        <span class="landing-brand"><img src="/static/icon.svg" width="24" height="24" alt="">muxa</span>
        <span class="landing-host" title="${escapeText(host)}">${escapeText(host)}</span>
        <nav class="landing-links">
          <a href="https://github.com/Open330/muxa" rel="noopener">GitHub</a>
          <div class="landing-lang" role="group" aria-label="Language">${langs}</div>
        </nav>
      </div>
    </header>
    <main class="landing-main">
      <section class="landing-hero landing-wrap">
        <div class="landing-intro">
          <span class="landing-badge">${escapeText(t.badge)}</span>
          <h1>${t.title}</h1>
          <p class="landing-lead">${escapeText(t.lead)}</p>
        </div>
        <aside class="landing-access" aria-labelledby="landing-access-title">
          <h2 id="landing-access-title">${escapeText(t.accessTitle)}</h2>
          <p>${fill(access.signIn || access.signOut ? t.accessBody : t.accessBodyToken, { host })}</p>
          ${notices}
          <div class="landing-actions">${buttons}</div>
          <p class="landing-hint">${t.tokenHint}</p>
          <figure class="landing-preview" aria-label="${escapeText(t.previewLabel)}">
            <figcaption>${escapeText(t.previewLabel)}</figcaption>
            <ul>${preview}</ul>
          </figure>
        </aside>
      </section>
      <section class="landing-section landing-wrap">
        <h2>${escapeText(t.whyTitle)}</h2>
        <div class="landing-grid">${why}</div>
      </section>
      <section class="landing-section landing-wrap landing-install">
        <div>
          <h2>${escapeText(t.installTitle)}</h2>
          <p>${t.installBody}</p>
          <div class="landing-actions">
            <a class="landing-btn" href="https://github.com/Open330/muxa" rel="noopener">${GITHUB_ICON}<span>${escapeText(t.github)}</span></a>
            <a class="landing-btn" href="https://github.com/Open330/muxa/tree/main/docs" rel="noopener">${escapeText(t.docs)}</a>
          </div>
        </div>
<pre><code>brew install open330/tap/muxa
muxa init      <span class="c">${escapeText(t.installC1)}</span>
muxa attend    <span class="c">${escapeText(t.installC2)}</span></code></pre>
      </section>
    </main>
    <footer class="landing-footer"><div class="landing-wrap">${t.footer}</div></footer>`;
}

// One landing per page; the click handler reads whatever the latest render
// set, so re-rendering with new login state never leaves stale callbacks.
const view = { root: null, lang: "en", access: null, host: "", email: null, actions: null };

function draw() {
  document.documentElement.lang = view.lang;
  view.root.innerHTML = markup(COPY[view.lang], view.access, { host: view.host, email: view.email });
}

function onLandingClick(event) {
  const langButton = event.target.closest("[data-landing-lang]");
  if (langButton) {
    view.lang = langButton.getAttribute("data-landing-lang");
    try { localStorage.setItem(LANG_KEY, view.lang); } catch (_) { /* storage blocked */ }
    draw();
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
 */
export function renderLanding(root, { login, tokenRejected = false, actions }) {
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
  draw();
  root.hidden = false;
}
