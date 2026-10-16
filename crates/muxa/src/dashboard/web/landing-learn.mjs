// Terminal input and local demo shell. No daemon or provider calls.
import { createLineEditor, editLine, createDemoShell, runDemoCommand, SHELL_COMPLETIONS } from "./landing-shell.mjs";
import { DEMO_PANES, PANE_IDS, adjacentPane, watchFrame, watchPaneOrder, DEMO_SESSIONS, PANE_META, paneWindow, topologyNode, topologyRows, navigateTopology, resolveNodePane } from "./landing-watch.mjs";
import { EXERCISES, practiceProgress, recordPractice, nextPracticeAction, practiceAlternatives, resolvePracticeTrack } from "./landing-practice.mjs";
const COPY = {
  en: {
    title: "A short session with muxa", lead: "Learn tmux basics, then move on to working with agents, one step at a time.",
    note: "Browser demo / about 2 minutes", steps: ["Open watch", "Find who needs you", "Answer the question", "Request a review", "Read the reply", "Done"],
    guidance: ["Click the shell. Hold Ctrl and press b, release both, then press s to open watch.", "Claude is waiting for an answer. Attend takes you to the longest-waiting pane.", "You are in Claude’s pane. Choose exponential backoff to continue.", "The change is ready. Send Codex a read-only review request.", "Codex has replied. Open the sent mailbox to read its findings.", "The reply stays with your request. You can return to it later."],
    actions: ["muxa watch", "muxa attend", "Use exponential backoff", "Send review", "Open mailbox"],
    reset: "Reset environment", install: "Install muxa", waiting: "waiting", working: "working", idle: "idle",
    question: "How should we retry a failed request?", implementation: "Added exponential backoff in src/retry.ts.\nTests: 12 passed. Ready for review.",
    prompt: "Review src/retry.ts. Check retry limits and edge cases. Report findings; do not edit files.",
    result: "Two findings:\n1. Cap the maximum delay at 30 seconds.\n2. Add a test for the final retry.\n\nNo files changed.",
    sent: "sent", completed: "completed", request: "request", reply: "reply", selectClaude: "Select Claude’s pane to answer the question.",
    fullPractice: "Open full demo", fromTmux: "Your session, windows, and panes stay as they are.",
    keyboard: "Demo keys", watchKey: "watch", attendKey: "attend", sendKey: "send", inboxKey: "mailbox", moveKey: "select agent", restartKey: "restart",
    real: "In your terminal", simulation: "Simulated agents. Nothing runs on your machine.",
    composer: "To", contract: "Read-only review", answerLabel: "Use exponential backoff", answerHint: "Enter sends this answer to Claude.",
    findings: "Review findings", commandHint: "Ctrl + b, then s. You can also type muxa watch.", commandLabel: "Demo command", invalidCommand: "Command not available in this demo. Try help.",
    installHint: "Start in your existing tmux session.",
    prefixHint: "Prefix received. Release Ctrl and b, then press the next key shown below.", prefixLabel: "tmux prefix", nextKey: "then", pressed: "Last key", focus: "Click the shell to use the keyboard", cancel: "Esc cancels prefix",
  },
  ko: {
    title: "muxa를 직접 다뤄보세요", lead: "tmux 기본 조작부터 에이전트 협업까지, 한 단계씩 익혀보세요.",
    note: "브라우저 데모 / 약 2분", steps: ["watch 열기", "기다리는 pane 찾기", "질문에 답하기", "리뷰 요청", "답장 읽기", "완료"],
    guidance: ["셸을 클릭하세요. Ctrl을 누른 채 b를 누르고, 두 키를 놓은 뒤 s를 눌러 watch를 여세요.", "Claude가 답을 기다립니다. attend는 가장 오래 기다린 pane으로 이동합니다.", "Claude pane으로 이동했습니다. Exponential backoff를 선택해 작업을 이어가세요.", "구현이 끝났습니다. Codex에게 읽기 전용 리뷰를 요청하세요.", "Codex의 답장이 도착했습니다. 보낸 요청의 mailbox를 열어 확인하세요.", "답장은 원래 요청에 남습니다. 나중에도 다시 읽을 수 있습니다."],
    actions: ["muxa watch", "muxa attend", "Exponential backoff 사용", "리뷰 보내기", "mailbox 열기"],
    reset: "환경 초기화", install: "muxa 설치", waiting: "답변 대기", working: "작업 중", idle: "유휴",
    question: "실패한 요청을 어떻게 재시도할까요?", implementation: "src/retry.ts에 exponential backoff를 추가했습니다.\n테스트 12개 통과. 리뷰를 받을 준비가 됐습니다.",
    prompt: "src/retry.ts의 재시도 한도와 예외 상황을 리뷰해주세요. 발견한 문제를 보고하고, 파일은 수정하지 마세요.",
    result: "발견한 문제 2개:\n1. 최대 지연 시간을 30초로 제한하세요.\n2. 마지막 재시도를 테스트에 추가하세요.\n\n파일은 변경하지 않았습니다.",
    sent: "보낸 요청", completed: "완료", request: "요청", reply: "응답", selectClaude: "Claude pane을 선택해 질문에 답하세요.",
    fullPractice: "전체 화면으로 체험", fromTmux: "기존 session·window·pane을 그대로 사용합니다.",
    keyboard: "체험 단축키", watchKey: "watch 열기", attendKey: "대기 pane 이동", sendKey: "보내기", inboxKey: "mailbox 열기", moveKey: "에이전트 선택", restartKey: "다시 시작",
    real: "실제 터미널에서는", simulation: "가상 에이전트입니다. 내 머신에서 명령을 실행하지 않습니다.",
    composer: "받는 사람", contract: "읽기 전용 리뷰", answerLabel: "Exponential backoff 사용", answerHint: "Enter로 Claude에게 답합니다.",
    findings: "리뷰 결과", commandHint: "Ctrl + b를 누른 뒤 s. muxa watch를 입력해도 됩니다.", commandLabel: "체험 명령", invalidCommand: "이 데모에 없는 명령입니다. help로 확인하세요.",
    installHint: "지금 쓰는 tmux 세션에서 시작하세요.",
    prefixHint: "Prefix를 받았습니다. Ctrl과 b를 놓고 아래에 표시된 다음 키를 누르세요.", prefixLabel: "tmux prefix", nextKey: "그다음", pressed: "누른 키", focus: "셸을 클릭하면 키보드로 조작합니다", cancel: "Esc로 prefix 취소",
  },
};

export function initialPractice() {
  return { step: 0, selected: "codex", mailbox: false, prefix: false };
}

// The learner must perform each action; navigation cannot skip a request.
export function practiceAction(state, action) {
  if (action === "reset") return initialPractice();
  if (action === "prefix") return state.step === 0 ? { ...state, prefix: true } : state;
  if (action === "cancel-prefix") return state.prefix ? { ...state, prefix: false } : state;
  if (action === "prefix-watch") return state.prefix && state.step === 0 ? practiceAction(state, "watch") : state;
  if (["claude", "codex", "gemini"].includes(action)) {
    return state.step > 0 ? { ...state, selected: action } : state;
  }
  const expected = ["watch", "attend", "answer", "review", "inbox"][state.step];
  if (action !== expected) return state;
  if (action === "answer" && state.selected !== "claude") return state;
  return { ...state, prefix: false, step: state.step + 1,
    selected: action === "attend" || action === "answer" ? "claude" : action === "review" ? "codex" : state.selected,
    mailbox: action === "inbox" };
}

const esc = (value) => String(value).replace(/[&<>"']/g, ch => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[ch]);

export function mountPractice(root, { lang = "en", installHref = "#install", standaloneHref = null, autofocus = false, initialTrack = "basics", onTrackChange = () => {} } = {}) {
  let t=COPY[lang]||COPY.en,ko=lang==="ko";
  initialTrack=resolvePracticeTrack(initialTrack);
  const exerciseProgress=Object.fromEntries(Object.keys(EXERCISES).map(track=>[track,practiceProgress(track)]));
  let tmuxAttached=initialTrack!=="basics",attachedPane="shell";
  const outerShell={editor:createLineEditor(),shell:createDemoShell(),transcript:[],log:[]};
  let progress=exerciseProgress[initialTrack],mode="shell",activePane="shell",selection="shell",prefix=false,lastKey="—",claudeAnswered=false;
  let filter="",filterOpen=false,split=50,zoomed=false,previewLive=false,mailboxTab="incoming",mailboxIndex=0;
  let composeKind="QUESTION",composeMode="READ-ONLY",composeTarget="codex",composer=createLineEditor();
  let requests=[],sequence=0,disposed=false,terminal,fit,observer;
  const records=new Map(),timers=new Set(),remembered=new Map(),sessionWindows=new Map();
  let structurePhase=-1,structureActive=false,structureTimers=[],keyFeedback=null,keyFeedbackTimer=null;
  const reduceMotion=globalThis.matchMedia?.("(prefers-reduced-motion: reduce)").matches||false;
  const overlays=new Set(["watch","compose","inbox","preview","help"]);
  const keyIcons={Alt:'<path d="M2 4h4l5 8h3M10 4h4"/>',Ctrl:'<path d="m4 10 4-5 4 5"/>',Shift:'<path d="m3 8 5-5 5 5h-3v5H6V8z"/>',Enter:'<path d="M12 3v6H3m3-3L3 9l3 3"/>',Tab:'<path d="M2 5h10m-3-3 3 3-3 3M14 2v6M14 11H4m3-3-3 3 3 3M2 8v6"/>',Esc:'<path d="M12 4H7a4 4 0 0 0 0 8h5M5 2 2 5l3 3"/>'};
  function key(value,scope=""){return `<span class="lp-key-chord" data-shortcut="${esc(String(value).replace(/\s/g,""))}"${scope?` data-key-scope="${scope}"`:""}>${String(value).split(/\s*\+\s*/).map((part,index)=>`${index?'<span class="lp-key-plus" aria-hidden="true">+</span>':''}<kbd>${keyIcons[part]?`<svg viewBox="0 0 16 16" aria-hidden="true">${keyIcons[part]}</svg>`:''}<span>${esc(part)}</span></kbd>`).join("")}</span>`;}
  const panes=Object.fromEntries(DEMO_PANES.map(name=>[name,{editor:createLineEditor(),shell:createDemoShell(),log:[],transcript:name==="shell"?["demo@local:~/acme$ git status --short"," M src/retry.ts"," M tests/retry.test.ts",""]:name==="logs-shell"?["[api] listening on :3000","[api] GET /health 200","[worker] queue empty",""]:[]} ]));
  function isShell(name){return PANE_META[name].role==="shell";}
  function emit(event){
    for(const track of Object.keys(EXERCISES))exerciseProgress[track]=recordPractice(exerciseProgress[track],event);
    progress=exerciseProgress[progress.track];
  }
  function selectExercise(track){
    if(track==="attention"){
      const index=requests.some(request=>request.kind==="REVIEW")?5:claudeAnswered?4:0;
      exerciseProgress.attention={...exerciseProgress.attention,index:Math.max(exerciseProgress.attention.index,index)};
    }
    progress=exerciseProgress[track];chrome();focusTerminal();onTrackChange(track);
  }
  function latest(name){return requests.filter(request=>request.target===name).at(-1);}
  function agentStatus(name){return isShell(name)?"shell":requests.some(request=>request.target===name&&!request.reply&&request.status!=="completed")?"working":name==="claude"&&!claudeAnswered?"waiting_input":name==="gemini"?"working":"idle";}
  function glyph(name){return agentStatus(name)==="working"?"●":agentStatus(name).startsWith("waiting")?"▶":"○";}
  function shellRecord(name){return !tmuxAttached&&name==="shell"?outerShell:panes[name];}
  function currentEditor(){return mode==="compose"?composer:shellRecord(activePane).editor;}
  function attachPane(name){if(name==="claude"&&!claudeAnswered){emit("find-waiting");emit("attend");}activePane=name;selection=name;mode=isShell(name)?"shell":"agent";zoomed=false;remembered.set(`${PANE_META[name].session}/${PANE_META[name].window}`,name);sessionWindows.set(PANE_META[name].session,PANE_META[name].window);}
  function focusTerminal(){(overlays.has(mode)?terminal:records.get(activePane)?.terminal)?.focus();}
  function nodeAgent(nodeKey){const node=topologyNode(nodeKey);if(node.type==="pane")return node.pane;const possible=DEMO_PANES.filter(name=>PANE_META[name].session===node.session&&(node.type!=="window"||PANE_META[name].window===node.window));return possible.find(name=>!isShell(name))||possible[0];}
  function attachNode(nodeKey){const node=topologyNode(nodeKey);const defaultPane=resolveNodePane(nodeKey);attachPane(node.type==="pane"?node.pane:remembered.get(`${PANE_META[defaultPane].session}/${PANE_META[defaultPane].window}`)||defaultPane);if(node.type==="window")emit("attach-window");if(filter.trim())emit("attach-search");}
  function switchWindow(delta){const meta=PANE_META[activePane],windows=DEMO_SESSIONS.find(session=>session.name===meta.session).windows;const window=windows[(windows.findIndex(item=>item.name===meta.window)+delta+windows.length)%windows.length];attachPane(remembered.get(`${meta.session}/${window.name}`)||window.panes[0]);emit(delta>0?"next-window":"previous-window");}
  function switchSession(delta){const index=DEMO_SESSIONS.findIndex(session=>session.name===PANE_META[activePane].session),session=DEMO_SESSIONS[(index+delta+DEMO_SESSIONS.length)%DEMO_SESSIONS.length];const window=session.windows.find(item=>item.name===sessionWindows.get(session.name))||session.windows[0];attachPane(remembered.get(`${session.name}/${window.name}`)||window.panes[0]);if(delta>0)emit("next-session");}
  root.innerHTML=`<div class="lp-intro"><div><p class="lp-eyebrow">${ko?"브라우저 실습":"Browser practice"}</p><${root.dataset.standalone?"h1":"h2"}>${esc(t.title)}</${root.dataset.standalone?"h1":"h2"}><p>${esc(t.lead)}</p></div>${standaloneHref?`<a class="lp-text-link" href="${esc(standaloneHref)}">${esc(t.fullPractice)} ↗</a>`:""}</div>
    <nav class="lp-exercises" aria-label="${ko?"실습 선택":"Exercises"}">${Object.entries(EXERCISES).map(([id,exercise])=>`<button type="button" data-exercise="${id}" aria-pressed="${id===progress.track}">${esc(exercise.title[ko?1:0])}</button>`).join("")}</nav>
    <div class="lp-layout"><aside class="lp-lesson" data-lesson></aside><div class="l-practice lp-live">
      <div class="lp-top"><span><b data-terminal-title>Terminal</b> <span class="lp-dim" data-mode>shell</span></span><span class="lp-dim" data-location></span></div>
      <div class="lp-session-bar"><nav data-sessions aria-label="Sessions"></nav><nav data-windows aria-label="Windows"></nav></div>
      <div class="lp-delivery" data-delivery hidden></div>
      <div class="lp-workspace" data-terminal data-active-pane="shell"><div class="lp-pane-grid">${DEMO_PANES.map(name=>`<section class="lp-pane" data-pane="${name}" aria-label="${PANE_META[name].role} pane ${PANE_IDS[name]}"><button type="button" class="lp-pane-label" data-focus-pane="${name}"><span>${PANE_META[name].role} ${PANE_IDS[name]}</span><span>${PANE_META[name].location}</span></button><div class="lp-pane-terminal" data-pane-terminal="${name}"></div></section>`).join("")}</div><div class="lp-overlay" data-overlay hidden><div class="lp-overlay-terminal" data-overlay-terminal></div></div></div>
      <div class="lp-status"><span data-tmux-status></span><span data-status></span></div><details class="lp-shortcuts"><summary>${ko?"전체 단축키":"All keyboard shortcuts"}</summary><div class="lp-keys" data-keys></div></details><div class="lp-key-feedback"><span>${esc(t.pressed)}</span><span data-last-key>—</span><span class="lp-focus-state" data-focus></span></div>
    </div></div><p class="lp-disclosure">${ko?"브라우저 안의 데모 환경입니다. 실습을 바꿔도 터미널과 기록은 유지됩니다. 내 컴퓨터의 셸이나 실제 agent에 연결하지 않습니다.":"A browser-local demo. Switching exercises keeps your terminal and history. Not connected to your computer's shell or a live agent."}</p>`;
  const workspace=root.querySelector(".lp-workspace"),host=root.querySelector("[data-overlay-terminal]");
  function alternativesMarkup(info,expanded=false){
    if(!info)return "";
    const items=list=>`<dl>${list.map(item=>`<div${item.selected?' class="is-selected"':''}><dt>${esc(item.label)}${item.selected?`<span>${ko?"선택됨":"selected"}</span>`:""}</dt><dd>${esc(item.description)}</dd></div>`).join("")}</dl>`;
    return `<aside class="lp-alternatives" data-alternatives aria-label="${ko?"함께 사용할 수 있는 기능":"Related features"}"><h4>${esc(info.title)}</h4>${items(info.items)}<p>${info.key?key(info.key)+" ":""}${esc(info.note)}</p>${info.extra?`<details${expanded?" open":""}><summary>${esc(info.extra.title)}</summary>${items(info.extra.items)}<p>${key(info.extra.key)} ${esc(info.extra.note)}</p></details>`:""}</aside>`;
  }
  function freeGuideMarkup(){
    const row=(keys,en,kr,context="tmux")=>`<li><span>${keys.map(value=>key(value,context)).join(" ")}</span><span>${esc(ko?kr:en)}</span></li>`;
    return `<div class="lp-free-guide" data-free-guide>
      <section><h4>tmux</h4><p class="lp-free-prefix">${key("Ctrl+b","prefix")} <span>${ko?"를 누르고 놓은 뒤, 아래 키를 누르세요.":"Release both keys, then press a key below."}</span></p><ul>
        ${row(["s"],"Open muxa watch","muxa watch 열기")}
        ${row(["←","↑","↓","→"],"Move between panes","pane 이동")}
        ${row(["o"],"Cycle through panes","다음 pane으로 이동")}
        ${row(["z"],"Zoom pane / restore layout","pane 확대 / 원래 배치")}
        ${row(["n","p"],"Next / previous window","다음 / 이전 window")}
        ${row([")","("],"Next / previous session","다음 / 이전 session")}
      </ul></section>
      <section><h4>muxa watch</h4><p>${ko?"watch 안에서는 prefix 없이 누릅니다.":"Inside watch, press these without a prefix."}</p><ul>
        ${row(["j","k"],"Select next / previous row","다음 / 이전 항목 선택","watch")}
        ${row(["h","l"],"Parent / child node","상위 / 하위 항목 이동","watch")}
        ${row(["Enter"],"Connect to selected target","선택한 대상으로 연결","watch")}
        ${row(["/"],"Filter agents and workspaces","에이전트·작업공간 검색","watch")}
        ${row(["o"],"Preview selected pane","선택한 pane 미리보기","watch")}
        ${row(["m"],"Write a message","메시지 작성","watch")}
        ${row(["M"],"Open mailbox","mailbox 열기","watch")}
        ${row(["|"],"Change inspector split","inspector 비율 변경","watch")}
        ${row(["?"],"Show help","도움말 보기","watch")}
        ${row(["q"],"Return to the original pane","원래 pane으로 돌아가기","watch")}
      </ul></section>
      <details><summary>${ko?"메시지·미리보기 키도 보기":"Message and preview keys"}</summary><ul>
        ${row(["Tab"],"Composer: change message type","메시지 작성: 종류 변경","compose")}
        ${row(["Ctrl+e"],"Composer: change delivery mode","메시지 작성: 전달 방식 변경","compose")}
        ${row(["Enter"],"Composer: send message","메시지 작성: 보내기","compose")}
        ${row(["Tab"],"Mailbox: incoming / sent","mailbox: 받은 / 보낸 보기","mailbox")}
        ${row(["c"],"Preview: prompt / live pane","미리보기: 기록 / 현재 화면","preview")}
        ${row(["Esc"],"Return to watch","watch로 돌아가기","overlay")}
      </ul></details>
    </div>`;
  }
  function basicsMarkup(){
    const meta=PANE_META[activePane],count=paneWindow(activePane).panes.length;
    const concepts=[
      ["session",ko?`작업공간 전체. 지금은 ${meta.session}입니다.`:`The whole workspace. Here it is ${meta.session}.`],
      ["window",ko?`session 안의 탭. 지금은 ${meta.window}입니다.`:`A tab inside a session. Here it is ${meta.window}.`],
      ["pane",ko?`window를 나눈 각각의 터미널. 지금은 ${count}칸입니다.`:`A terminal area inside a window. Here there are ${count}.`],
    ];
    return `<div class="lp-basics" data-basics><dl>${concepts.map(([name,description],index)=>`<div class="lp-concept" data-concept="${index}"><dt><span>${index+1}</span>${name}</dt><dd>${esc(description)}</dd></div>`).join("")}</dl><p class="lp-hierarchy">${concepts.map(([name],index)=>`${index?'<span aria-hidden="true">→</span>':""}<span data-hierarchy="${index}">${name}</span>`).join("")}</p><button class="lp-structure-replay" type="button" data-demo="structure-replay">${ko?"순서대로 다시 보기":"Replay the sequence"} <span aria-hidden="true">↺</span></button><button type="button" class="lp-continue" data-demo="basics-start" aria-keyshortcuts="Alt+Enter"><span>${ko?"직접 조작해보기":"Try the controls"}</span>${key("Alt+Enter","guide")}</button><button type="button" class="lp-reset" data-exercise="attention">${ko?"이미 익숙해요 · muxa로 건너뛰기":"Already familiar? Skip to muxa"}</button></div>`;
  }
  function paintStructure(){
    const names=["session","window","pane"],panel=root.querySelector("[data-basics]");
    root.querySelector(".lp-live").dataset.structure=structureActive?names[structurePhase]:"";
    if(!panel)return;
    panel.dataset.phase=String(structurePhase);
    panel.querySelectorAll("[data-concept]").forEach(item=>{
      const index=Number(item.dataset.concept),visible=reduceMotion||index<=structurePhase;
      item.classList.toggle("is-revealed",visible);item.classList.toggle("is-current",index===structurePhase);item.setAttribute("aria-hidden",String(!visible));
    });
    panel.querySelectorAll("[data-hierarchy]").forEach(item=>item.classList.toggle("is-current",Number(item.dataset.hierarchy)===structurePhase));
  }
  function stopStructure(){
    for(const timer of structureTimers){clearTimeout(timer);timers.delete(timer);}structureTimers=[];structureActive=false;structurePhase=-1;
  }
  function syncStructure(replay=false){
    const visible=tmuxAttached&&progress.track==="basics"&&progress.index===1;
    if(!visible){if(structureActive)stopStructure();paintStructure();return;}
    if(replay||!structureActive){
      stopStructure();structureActive=true;structurePhase=reduceMotion?2:0;
      if(!reduceMotion)for(const [phase,delay] of [[1,1600],[2,3200]])structureTimers.push(later(()=>{structurePhase=phase;paintStructure();},delay));
    }
    paintStructure();
  }
  function paintKeyFeedback(){
    root.querySelectorAll("[data-free-guide] .is-key-active").forEach(item=>item.classList.remove("is-key-active"));
    root.querySelectorAll("[data-free-guide] [data-shortcut]").forEach(item=>{
      const scope=item.dataset.keyScope,shared=scope==="overlay"&&["compose","mailbox","preview","help"].includes(keyFeedback?.scope);
      const pressed=keyFeedback&&item.dataset.shortcut===keyFeedback.key&&(scope===keyFeedback.scope||shared);
      item.classList.toggle("is-pressed",Boolean(pressed));item.classList.toggle("is-armed",scope==="prefix"&&prefix&&tmuxAttached);
      if(pressed)item.closest("li")?.classList.add("is-key-active");
    });
  }
  function rememberKey(event){
    if(["Control","Shift","Alt","Meta"].includes(event.key))return;
    const aliases={ArrowLeft:"←",ArrowRight:"→",ArrowUp:"↑",ArrowDown:"↓",Escape:"Esc"," ":"Space"};
    const name=aliases[event.key]||event.key;
    const scope=event.altKey?"guide":event.ctrlKey&&event.key.toLowerCase()==="b"&&tmuxAttached?"prefix":prefix?"tmux":mode==="watch"?(filterOpen?"filter":"watch"):mode==="inbox"?"mailbox":mode;
    keyFeedback={key:event.altKey?`Alt+${name}`:event.ctrlKey?`Ctrl+${name.toLowerCase()}`:name,scope};
    if(keyFeedbackTimer){clearTimeout(keyFeedbackTimer);timers.delete(keyFeedbackTimer);}
    keyFeedbackTimer=later(()=>{keyFeedback=null;keyFeedbackTimer=null;paintKeyFeedback();},550);
    paintKeyFeedback();
  }
  function chrome(){
    const meta=PANE_META[activePane],exercise=EXERCISES[progress.track],total=exercise.events.length,done=total&&progress.index>=total;
    const stepTitle=done?(ko?"완료":"Done"):total?(ko?exercise.ko:exercise.steps)[progress.index]:exercise.title[ko?1:0];
    const next=nextPracticeAction(progress,{mode,prefix,filterOpen,filter,selection,editor:currentEditor().text,kind:composeKind,previewLive,mailboxTab,tmuxAttached,hasReply:requests.some(item=>item.reply),activePane},lang);
    const alternativesExpanded=Boolean(root.querySelector("[data-alternatives] details")?.open);
    const alternatives=practiceAlternatives(progress,{mode,kind:composeKind,workMode:composeMode,mailboxTab,previewLive},lang);
    const freeGuideExpanded=Boolean(root.querySelector("[data-free-guide] details")?.open);
    const continuation={basics:["attention",ko?"muxa 실습 계속하기":"Continue with muxa"],attention:["workspace",ko?"작업공간 예제 계속하기":"Continue with workspaces"],workspace:["inspect",ko?"화면 살펴보기 계속하기":"Continue with inspection"],inspect:["free",ko?"자유 탐색 계속하기":"Continue exploring"]}[progress.track];
    root.querySelector("[data-lesson]").innerHTML=`<p class="lp-counter">${total?`${String(Math.min(progress.index+1,total)).padStart(2,"0")} <span>/ ${String(total).padStart(2,"0")}</span>`:"FREE"}</p><h3>${esc(stepTitle)}</h3><p role="status">${esc(next.instruction)}</p>${progress.track==="basics"&&tmuxAttached&&progress.index===1?basicsMarkup():""}${done&&continuation?`<button class="lp-continue" type="button" data-exercise="${continuation[0]}" aria-keyshortcuts="Alt+Enter"><span>${esc(continuation[1])}</span>${key("Alt+Enter","guide")}</button>`:""}${next.example?`<div class="lp-message-example" data-message-example><div><span>${esc(next.example.label||(ko?"메시지 예시":"Example message"))}</span><small>${esc(next.example.note)}</small></div><code>${esc(next.example.text)}</code></div>`:""}${next.key||next.text?`<div class="lp-next-action" data-next-action><span class="lp-action-label">${ko?next.text?"입력할 내용":"지금 누를 키":next.text?"Type this":"Press now"}</span>${next.key?key(next.key):`<code>${esc(next.text)}</code>`}${next.note?`<small>${esc(next.note)}</small>`:""}</div>`:""}${mode==="compose"&&!composer.text?`<button class="lp-reset" type="button" data-demo="example">${ko?"예시 입력":"Insert example"}</button>`:""}${progress.track==="basics"&&!tmuxAttached?`<button class="lp-reset" type="button" data-exercise="attention">${ko?"이미 익숙해요 · muxa로 건너뛰기":"Already familiar? Skip to muxa"}</button>`:""}${progress.track==="free"&&tmuxAttached?freeGuideMarkup():alternativesMarkup(alternatives,alternativesExpanded)}<button class="lp-reset" type="button" data-demo="reset">${esc(t.reset)}</button>${done?`<p><a class="lp-text-link" href="${esc(installHref)}">${esc(t.install)} →</a></p>`:""}`;
    if(freeGuideExpanded&&progress.track==="free"&&tmuxAttached)root.querySelector("[data-free-guide] details").open=true;
    root.querySelector("[data-lesson]").classList.toggle("is-complete",Boolean(done));
    root.querySelectorAll(".lp-exercises [data-exercise]").forEach(button=>button.setAttribute("aria-pressed",String(button.dataset.exercise===progress.track)));
    root.querySelector("[data-mode]").textContent=mode;root.querySelector("[data-location]").textContent=tmuxAttached?`${meta.location} · ${PANE_IDS[activePane]} ${meta.role}`:(ko?"일반 셸 · tmux 밖":"zsh · outside tmux");
    root.querySelector("[data-terminal-title]").textContent=tmuxAttached?"muxa":"Terminal";
    root.querySelector(".lp-session-bar").hidden=!tmuxAttached;root.querySelector(".lp-status").hidden=!tmuxAttached;root.querySelector(".lp-shortcuts").hidden=!tmuxAttached;
    workspace.dataset.activePane=activePane;workspace.dataset.tmux=tmuxAttached?"inside":"outside";workspace.dataset.session=tmuxAttached?meta.session:"";workspace.dataset.window=tmuxAttached?meta.window:"";workspace.dataset.zoomed=tmuxAttached&&zoomed?activePane:"";workspace.dataset.paneCount=tmuxAttached?String(paneWindow(activePane).panes.length):"1";
    root.querySelector("[data-overlay]").hidden=!overlays.has(mode);root.querySelector(".lp-pane-grid").inert=overlays.has(mode);
    for(const name of DEMO_PANES){const section=root.querySelector(`[data-pane="${name}"]`);section.hidden=tmuxAttached?!paneWindow(activePane).panes.includes(name):name!=="shell";section.setAttribute("aria-label",tmuxAttached?`${PANE_META[name].role} pane ${PANE_IDS[name]}`:(ko?"일반 터미널":"Terminal"));section.classList.toggle("is-active",name===activePane);section.classList.toggle("is-working",agentStatus(name)==="working");const receipt=latest(name);section.querySelector(".lp-pane-label span:last-child").textContent=receipt?`${receipt.id} · ${receipt.reply?"replied":receipt.status}`:PANE_META[name].location;}
    root.querySelector("[data-sessions]").innerHTML=DEMO_SESSIONS.map(session=>`<button type="button" data-session="${session.name}" aria-pressed="${session.name===meta.session}">${session.name}</button>`).join("");
    root.querySelector("[data-windows]").innerHTML=DEMO_SESSIONS.find(session=>session.name===meta.session).windows.map((window,index)=>`<button type="button" data-window="${window.name}" aria-pressed="${window.name===meta.window}">${index}:${window.name}${window.name===meta.window?"*":""}</button>`).join("");
    const peers=paneWindow(activePane).panes,waiting=peers.filter(name=>agentStatus(name).startsWith("waiting")).length,working=peers.filter(name=>agentStatus(name)==="working").length,replies=requests.filter(item=>item.reply).length;
    root.querySelector("[data-tmux-status]").textContent=`[${meta.session}] ${DEMO_SESSIONS.find(session=>session.name===meta.session).windows.findIndex(window=>window.name===meta.window)}:${meta.window}* · ${PANE_IDS[activePane]}`;
    root.querySelector("[data-status]").textContent=`${waiting} waiting · ${working} working${requests.length?` · ${replies} ${replies===1?"reply":"replies"}`:""}${prefix?" · PREFIX":""}`;
    root.querySelector("[data-last-key]").innerHTML=key(lastKey);
    const receipt=requests.at(-1),delivery=root.querySelector("[data-delivery]");delivery.hidden=!tmuxAttached||!receipt;
    if(receipt)delivery.innerHTML=`<span><b>${PANE_META[receipt.target].role} ${PANE_IDS[receipt.target]} · ${PANE_META[receipt.target].session}:${PANE_META[receipt.target].window}</b><small>${esc(receipt.id)} · ${esc(receipt.progress||receipt.status)}</small></span><button type="button" data-open-target="${receipt.target}">${ko?"대상 pane 보기":"Open recipient pane"} ↗</button>`;
    const shortcuts=mode==="watch"?[["↑",t.moveKey],["↓",t.moveKey],["h","parent"],["l","child"],["Enter","attach"],["m",t.request],["M",t.inboxKey],["q",ko?"닫기":"quit"]]:mode==="compose"?[["Tab","kind"],["Ctrl + e","contract"],["Enter",t.sendKey],["Ctrl + c",ko?"취소":"cancel"]]:mode==="inbox"?[["↑","select"],["↓","select"],["Tab","incoming/sent"],["Esc / q","watch"]]:mode==="preview"?[["c","live pane"],["Esc / q","watch"],["Enter","attach"]]:mode==="help"?[["Esc / q","watch"]]:[["Ctrl + b","prefix"],["s",t.watchKey],["n","next window"],["p","prev window"],[")","next session"],["z","zoom"],["←","cursor"],["→","cursor"],["↑","history"],["↓","history"],["Tab","complete"]];
    root.querySelector("[data-keys]").innerHTML=shortcuts.map(([k,label])=>`<span><button type="button" data-demo-key="${esc(k)}">${key(k)}</button> ${esc(label)}</span>`).join("");
    syncStructure();paintKeyFeedback();
  }
  const plain=text=>String(text).replace(/[\x00-\x08\x0b-\x1f\x7f-\x9f]/g,""),width=text=>Array.from(text).reduce((n,c)=>n+(/\p{Mark}/u.test(c)?0:/[\p{Script=Hangul}\p{Script=Han}\p{Script=Hiragana}\p{Script=Katakana}\p{Extended_Pictographic}]/u.test(c)?2:1),0);
  function wrap(text,cols){const lines=[];let line="",count=0;for(const c of plain(text)){const size=width(c);if(count+size>cols){lines.push(line);line="";count=0;}line+=c;count+=size;}lines.push(line);return lines;}
  function paneLines(name){const item=shellRecord(name),meta=PANE_META[name];if(isShell(name))return [...item.transcript,...item.log];const initial=name==="claude"?["╭ Claude Code ────────────────────","│ acme / retry","╰────────────────────────────────","","❯ Implement retry with backoff","Read src/retry.ts","",claudeAnswered?t.implementation:t.question,""]:name==="gemini"?["Gemini CLI · ~/acme","","❯ Run integration tests","PASS api.integration.test.ts","Running retry.integration.test.ts…",""]:meta.role==="codex"?[">_ OpenAI Codex",`window: ${meta.session}:${meta.window}`,"Ready for a review request.",""]:["Claude Code · docs","Draft README installation guide","Documentation draft ready.",""];return [...initial,...item.log];}
  function renderPane(name){const record=records.get(name);if(!record)return;const term=record.terminal,cols=Math.max(10,term.cols),rows=term.rows,item=shellRecord(name);let lines=paneLines(name).flatMap(line=>String(line).split("\n").flatMap(value=>wrap(value,cols)));const prompt=isShell(name)?`demo@local:${item.shell.cwd.replace("/home/demo","~")}$ `:"› ",inputLines=wrap(prompt+item.editor.text,cols),position=width(prompt+Array.from(item.editor.text).slice(0,item.editor.cursor).join(""));let cursorRow=lines.length+Math.floor(position/cols);if(position===cols*inputLines.length)inputLines.push("");lines.push(...inputLines);const trim=Math.max(0,lines.length-rows);lines=lines.slice(trim);cursorRow-=trim;const output=lines.map(line=>line.startsWith("demo@local:")?`\x1b[38;5;110m${line}\x1b[0m`:line.startsWith("[muxa]")?`\x1b[38;5;110m${line}\x1b[0m`:line).join("\r\n");const cursor=name===activePane&&!overlays.has(mode);term.write(`\x1b[?25l\x1b[H\x1b[2J${output}${cursor?`\x1b[${Math.max(1,cursorRow+1)};${position%cols+1}H\x1b[?25h`:""}`);}
  function details(){return Object.fromEntries(DEMO_PANES.map(name=>{const receipt=latest(name),meta=PANE_META[name],item=panes[name];return [name,{state:agentStatus(name),glyph:glyph(name),age:name==="claude"&&!claudeAnswered?"4m":"18s",summary:receipt?receipt.reply?"Reply ready":receipt.progress||"Request delivered":isShell(name)?"zsh":name==="claude"?claudeAnswered?"Backoff implemented":"Choose retry strategy":name==="gemini"?"Running integration tests":meta.window==="docs"?"Drafting documentation":"Ready for review",prompt:receipt?.body||(!isShell(name)?"Review project changes":"—"),response:receipt?.reply||"—",capture:[...paneLines(name),`${isShell(name)?`demo@local:${item.shell.cwd.replace("/home/demo","~")}$ `:"› "}${item.editor.text}`].flatMap(line=>String(line).split("\n").flatMap(value=>wrap(value,records.get(name)?.terminal.cols||50))).slice(-(records.get(name)?.terminal.rows||14))}];}));}
  function render(){chrome();if(disposed)return;for(const name of DEMO_PANES)renderPane(name);if(terminal&&overlays.has(mode))terminal.write(watchFrame({cols:terminal.cols,rows:terminal.rows,selected:selection,details:details(),filter,filterOpen,split,view:mode,requests,tab:mailboxTab,mailboxIndex,input:composer.text,cursor:composer.cursor,kind:composeKind,workMode:composeMode,previewLive,target:composeTarget}).ansi);}
  function fitAll(){if(disposed)return;for(const record of records.values())if(record.host.clientWidth>0)record.fit.fit();if(overlays.has(mode)&&host.clientWidth>0)fit?.fit();}
  function sync(){chrome();fitAll();render();focusTerminal();}
  function openWatch(){if(!tmuxAttached)return;mode="watch";selection=`window:${PANE_META[activePane].session}/${PANE_META[activePane].window}`;filter="";filterOpen=false;prefix=false;emit("watch");sync();}
  function later(callback,delay){const timer=setTimeout(()=>{timers.delete(timer);if(!disposed)callback();},delay);timers.add(timer);return timer;}
  function demoReply(request){
    if(request.kind==="NOTICE")return ko?"공지 확인했습니다.":"Notice received.";
    if(request.kind==="QUESTION")return ko?"실패한 요청만 재시도하고 최대 횟수를 제한하세요. 지연에는 exponential backoff와 jitter를 함께 적용하는 것이 좋겠습니다.":"Retry failed requests only, limit attempts, and combine exponential backoff with jitter.";
    if(request.kind==="TASK")return ko?"작업을 정리했습니다.\n1. 재시도 횟수와 최대 지연을 확인\n2. 마지막 시도 실패 테스트 확인\n"+(request.workMode==="READ-ONLY"?"READ-ONLY: 변경 제안을 남겼습니다.":"EXECUTE: 데모 작업 완료."):"Task checked.\n1. Check retry count and maximum delay\n2. Check final-attempt failure coverage\n"+(request.workMode==="READ-ONLY"?"READ-ONLY: proposed changes recorded.":"EXECUTE: demo task complete.");
    const window=PANE_META[request.target].window;
    if(window==="docs")return ko?"문서 리뷰 완료. 설치 명령 다음에 초기화 단계와 Ctrl+b → s 사용 예를 추가해주세요. 파일은 변경하지 않았습니다.":"Documentation review complete. Add initialization steps and a Ctrl+b → s example after installation. No files changed.";
    if(window==="build")return ko?"빌드 리뷰 완료. 배포 전에 테스트 실행과 버전 태그 검증을 추가해주세요. 파일은 변경하지 않았습니다.":"Build review complete. Add test execution and version-tag validation before publishing. No files changed.";
    return t.result;
  }
  function pump(target){if(requests.some(item=>item.target===target&&item.status==="working"))return;const request=requests.find(item=>item.target===target&&item.status==="queued");if(!request)return;request.status="working";request.progress=ko?"읽는 중":"reading";panes[target].log.push(`[muxa] ${request.id} · ${request.kind} · ${request.workMode}`,`› ${request.body}`,ko?"요청을 받았습니다. 내용을 확인합니다.":"Request received. Reading the change.");render();later(()=>{if(!requests.includes(request))return;request.progress=ko?"검토 중":"reviewing";panes[target].log.push(ko?"재시도 한도와 테스트를 확인하고 있습니다…":"Checking retry limits and tests…");render();},800);later(()=>{if(!requests.includes(request))return;request.status="completed";request.progress=ko?"답장 도착":"reply ready";request.reply=demoReply(request);if(mode==="inbox"&&mailboxTab==="sent")emit("inbox");panes[target].log.push(`[muxa] ${request.id} · completed`,...request.reply.split("\n"),"");render();pump(target);},2200);}
  function send(){const body=composer.text.trim();if(!body)return;if(composeMode==="JUST SEND"){if(isShell(composeTarget))runCommand(composeTarget,body);else panes[composeTarget].log.push(`› ${body}`);composer=createLineEditor();mode="watch";sync();return;}const request={id:`req_demo_${String(++sequence).padStart(2,"0")}`,target:composeTarget,kind:composeKind,workMode:composeMode,body,status:"queued",progress:ko?"요청 전달됨":"delivered",reply:null};requests.push(request);panes[composeTarget].log.push(`[muxa] received ${request.id}`,`console → ${PANE_META[composeTarget].role}: ${body}`);composer=createLineEditor();mode="watch";if(composeKind==="REVIEW")emit("review");pump(composeTarget);sync();}
  function mailboxRequests(){const node=topologyNode(selection);return requests.filter(item=>mailboxTab==="sent"||(node.type==="pane"?item.target===node.pane:PANE_META[item.target].session===node.session&&(node.type!=="window"||PANE_META[item.target].window===node.window))).slice().reverse();}
  function runCommand(name,command){const item=shellRecord(name),cwd=item.shell.cwd;const result=runDemoCommand(item.shell,command,{status:DEMO_PANES.filter(pane=>!isShell(pane)).map(pane=>`${PANE_META[pane].role.padEnd(7)} ${PANE_IDS[pane]} ${agentStatus(pane)}`).join("\n")});item.shell=result.shell;if(result.clear)item.transcript=[];else{item.transcript.push(`demo@local:${cwd.replace("/home/demo","~")}$ ${command}`);if(result.output)item.transcript.push(...result.output.split("\n"));}if(result.action==="tmux-attach"&&!tmuxAttached){tmuxAttached=true;attachPane(attachedPane);emit("attach-tmux");}
    if(result.action==="tmux-detach"){detachTmux();}
    if(result.action==="watch"){if(tmuxAttached)openWatch();else item.transcript.push("Connect to tmux first: tmux attach -t acme");}if(result.action==="attend"){if(!tmuxAttached)item.transcript.push("Connect to tmux first: tmux attach -t acme");else if(!claudeAnswered){attachPane("claude");emit("attend");}else item.transcript.push("No agents need attention.");}item.transcript=item.transcript.slice(-200);}
  function detachTmux(){if(!tmuxAttached)return;attachedPane=activePane;tmuxAttached=false;activePane="shell";selection="shell";mode="shell";zoomed=false;prefix=false;}
  function input(data){if(disposed)return;if(!tmuxAttached&&data==="\x02"){outerShell.editor=editLine(outerShell.editor,"\x1b[D").editor;render();return;}if(data==="\x02"){prefix=true;lastKey="Ctrl + b";render();return;}if(prefix){prefix=false;lastKey=({"\x1b":"Esc","\x1b[A":"↑","\x1b[B":"↓","\x1b[C":"→","\x1b[D":"←"})[data]||data;if(data==="s"){openWatch();return;}if(data==="d")detachTmux();else if(data==="n"||data==="p")switchWindow(data==="n"?1:-1);else if(data===")"||data==="(")switchSession(data===")"?1:-1);else if(data==="z"){zoomed=!zoomed;emit(zoomed?"zoom-pane":"restore-panes");}else if(data==="o"){const names=paneWindow(activePane).panes;attachPane(names[(names.indexOf(activePane)+1)%names.length]);emit("move-pane");}else if(["\x1b[A","\x1b[B","\x1b[C","\x1b[D"].includes(data)){const next=adjacentPane(activePane,{"\x1b[A":"up","\x1b[B":"down","\x1b[C":"right","\x1b[D":"left"}[data]);if(next!==activePane){attachPane(next);emit("move-pane");}}sync();return;}
    if(mode==="watch"){
      if(filterOpen){if(data==="\x1b"||data==="\r"){filterOpen=false;if(filter){emit("filter");if(data==="\r")emit("workspace-filter");}}else if(data==="\x7f")filter=Array.from(filter).slice(0,-1).join("");else if(!data.startsWith("\x1b")&&data>=" ")filter+=data;const matches=topologyRows(details(),selection,filter).filter(row=>row.type==="pane");if(matches.length&&!matches.some(row=>row.key===selection))selection=matches[0].key;}
      else if(["j","k","h","l","\x1b[A","\x1b[B","\x1b[C","\x1b[D"].includes(data)){selection=navigateTopology(selection,{j:"down",k:"up",h:"left",l:"right","\x1b[A":"up","\x1b[B":"down","\x1b[C":"right","\x1b[D":"left"}[data],details(),filter);const node=topologyNode(selection);if(node.type==="session"){emit("session-node");if(["j","k","\x1b[A","\x1b[B"].includes(data))emit("other-session");}if(node.type==="window")emit("window-node");}
      else if(["q","\x03","\x1b"].includes(data))attachPane(activePane);
      else if(data==="\r")attachNode(selection);
      else if(data==="m"){composeTarget=nodeAgent(selection);composeMode=isShell(composeTarget)?"JUST SEND":"READ-ONLY";composer=createLineEditor();mode="compose";}
      else if(data==="M"){mode="inbox";mailboxTab="incoming";mailboxIndex=0;}
      else if(data==="o"){previewLive=false;mode="preview";emit("preview");}
      else if(data==="?"){mode="help";emit("help");}
      else if(data==="/"){filterOpen=true;filter="";}
      else if(data==="|"){split=split===50?70:split===70?30:50;emit("split");}
      if(!filterOpen&&selection==="claude"&&!claudeAnswered)emit("find-waiting");
      sync();return;
    }
    if(["inbox","preview","help"].includes(mode)){if(["q","\x1b","\x03","M"].includes(data))mode="watch";else if(mode==="inbox"&&data==="\t"){mailboxTab=mailboxTab==="incoming"?"sent":"incoming";mailboxIndex=0;if(mailboxTab==="sent"&&requests.some(item=>item.reply))emit("inbox");}else if(mode==="inbox"&&["j","k","\x1b[A","\x1b[B"].includes(data)){const n=mailboxRequests().length;mailboxIndex=n?(mailboxIndex+(["j","\x1b[B"].includes(data)?1:n-1))%n:0;}else if(mode==="preview"&&data==="c"){previewLive=!previewLive;emit("live-preview");}else if(mode==="preview"&&data==="\r")attachNode(selection);sync();return;}
    if(mode==="compose"&&data==="\t"){composeKind=composeKind==="REVIEW"?"TASK":composeKind==="TASK"?"NOTICE":composeKind==="NOTICE"?"QUESTION":"REVIEW";render();return;}if(mode==="compose"&&data==="\x05"){composeMode=composeMode==="READ-ONLY"?"EXECUTE":composeMode==="EXECUTE"?"JUST SEND":"READ-ONLY";render();return;}if(mode==="compose"&&["\x1b","\x03"].includes(data)){composer=createLineEditor();mode="watch";sync();return;}if(mode==="compose"&&["\r","\n"].includes(data)){send();return;}if(data==="\x0c"){if(isShell(activePane))shellRecord(activePane).transcript=[];render();return;}
    if(data.length>1&&!data.startsWith("\x1b"))data=data.replace(/\r?\n|\r/g," ");const edited=editLine(currentEditor(),data,isShell(activePane)?SHELL_COMPLETIONS:[]);if(mode==="compose")composer=edited.editor;else shellRecord(activePane).editor=edited.editor;if(edited.interrupted&&isShell(activePane))shellRecord(activePane).transcript.push("^C");if(edited.suggestions?.length&&isShell(activePane))shellRecord(activePane).transcript.push(edited.suggestions.join("  "));if(edited.command!==undefined){if(isShell(activePane))runCommand(activePane,edited.command);else if(edited.command.trim()){panes[activePane].log.push(`› ${edited.command}`);if(activePane==="claude"){claudeAnswered=true;emit("answer");}}}sync();
  }
  function reset(track=progress.track){stopStructure();keyFeedback=null;keyFeedbackTimer=null;tmuxAttached=false;attachedPane="shell";outerShell.editor=createLineEditor();outerShell.shell=createDemoShell();outerShell.transcript=[];for(const timer of timers)clearTimeout(timer);timers.clear();for(const record of records.values())record.terminal.reset();terminal?.reset();requests=[];sequence=0;claudeAnswered=false;for(const name of Object.keys(EXERCISES))exerciseProgress[name]=practiceProgress(name);progress=exerciseProgress[track];mode="shell";activePane="shell";selection="shell";prefix=false;lastKey="—";filter="";filterOpen=false;split=50;zoomed=false;previewLive=false;mailboxTab="incoming";mailboxIndex=0;composeKind="QUESTION";composeMode="READ-ONLY";composer=createLineEditor();remembered.clear();sessionWindows.clear();for(const [name,pane] of Object.entries(panes)){pane.editor=createLineEditor();pane.shell=createDemoShell();pane.log=[];pane.transcript=name==="logs-shell"?["[api] listening on :3000","[api] GET /health 200","[worker] queue empty",""]:[];}sync();}
  function click(event){const exercise=event.target.closest("[data-exercise]")?.dataset.exercise;if(exercise){selectExercise(exercise);return;}const action=event.target.closest("[data-demo]")?.dataset.demo;if(action==="structure-replay"){syncStructure(true);focusTerminal();return;}if(action==="basics-start"){emit("basics-start");chrome();focusTerminal();return;}if(action==="reset")reset();else if(action==="example"){composer.text=t.prompt;composer.cursor=Array.from(t.prompt).length;render();}const target=event.target.closest("[data-open-target]")?.dataset.openTarget;if(target){attachPane(target);sync();return;}const session=event.target.closest("button[data-session]")?.dataset.session;if(session){const item=DEMO_SESSIONS.find(item=>item.name===session),window=item.windows.find(item=>item.name===sessionWindows.get(session))||item.windows[0];attachPane(remembered.get(`${session}/${window.name}`)||window.panes[0]);sync();return;}const window=event.target.closest("button[data-window]")?.dataset.window;if(window){const meta=PANE_META[activePane],item=DEMO_SESSIONS.find(item=>item.name===meta.session).windows.find(item=>item.name===window);attachPane(remembered.get(`${meta.session}/${window}`)||item.panes[0]);sync();return;}const virtual=event.target.closest("[data-demo-key]")?.dataset.demoKey;if(virtual)input({"Ctrl + b":"\x02",s:"s",n:"n",p:"p",")":")",z:"z",c:"c",h:"h",l:"l","Ctrl + e":"\x05","↑":"\x1b[A","↓":"\x1b[B","←":"\x1b[D","→":"\x1b[C",Enter:"\r",m:"m","M":"M",q:"q","Esc / q":"\x1b","Ctrl + c":"\x03",Tab:"\t"}[virtual]||"");const pane=event.target.closest("[data-pane]")?.dataset.pane;if(pane&&!overlays.has(mode)){attachPane(pane);render();}if(action||virtual||event.target.closest("[data-terminal]"))focusTerminal();}
  function continueWithKeyboard(event){
    if(event.key!=="Enter"||!event.altKey||event.ctrlKey||event.metaKey||event.shiftKey||event.isComposing)return;
    const button=root.querySelector('.lp-continue[aria-keyshortcuts="Alt+Enter"]');
    if(!button)return;
    event.preventDefault();event.stopPropagation();
    if(event.repeat)return;
    lastKey="Alt+Enter";rememberKey(event);button.click();
  }
  root.addEventListener("keydown",continueWithKeyboard,true);
  root.addEventListener("click",click);chrome();
  const ready=Promise.all([import("./landing-terminal/xterm.mjs"),import("./landing-terminal/fit.mjs")]).then(([{Terminal},{FitAddon}])=>{if(disposed)return;function create(container,name,fontSize){const term=new Terminal({cursorBlink:true,cursorStyle:"block",fontSize,lineHeight:1.35,fontFamily:'ui-monospace, "SFMono-Regular", Menlo, monospace',theme:{background:"#171a1e",foreground:"#d6d9df",cursor:"#d6d9df",selectionBackground:"#394758"},convertEol:true,scrollback:300,screenReaderMode:true});const addon=new FitAddon();term.loadAddon(addon);term.open(container);term.textarea.setAttribute("aria-label",`${name||"muxa watch"} terminal input`);term.attachCustomKeyEventHandler(event=>{if(event.type!=="keydown")return true;if(event.key==="Tab"&&event.shiftKey)return false;rememberKey(event);if(name&&!overlays.has(mode)&&activePane!==name){attachPane(name);render();}lastKey=["Control","Shift","Alt","Meta"].includes(event.key)?(event.key==="Control"?"Ctrl":event.key):keyFeedback.key;root.querySelector("[data-last-key]").innerHTML=key(lastKey);if(event.ctrlKey&&event.key.toLowerCase()==="b"){event.preventDefault();input("\x02");return false;}return true;});term.onData(input);term.onResize(render);term.textarea.addEventListener("focus",()=>{root.querySelector("[data-focus]").textContent=`${!tmuxAttached?"zsh":name?PANE_IDS[name]+" "+PANE_META[name].role:"muxa watch"} · ${ko?"입력 중":"keyboard active"}`;});return {terminal:term,fit:addon,host:container};}for(const name of DEMO_PANES)records.set(name,create(root.querySelector(`[data-pane-terminal="${name}"]`),name,11));const overlay=create(host,null,11);terminal=overlay.terminal;fit=overlay.fit;observer=new ResizeObserver(()=>{fitAll();render();});observer.observe(workspace);fitAll();render();if(autofocus)focusTerminal();return terminal;});
  function setLanguage(nextLang){
    if(!COPY[nextLang])return;
    lang=nextLang;t=COPY[lang];ko=lang==="ko";
    root.querySelector(".lp-eyebrow").textContent=ko?"브라우저 실습":"Browser practice";
    root.querySelector(".lp-intro h1, .lp-intro h2").textContent=t.title;
    root.querySelector(".lp-intro p:not(.lp-eyebrow)").textContent=t.lead;
    root.querySelector(".lp-exercises").setAttribute("aria-label",ko?"실습 선택":"Exercises");
    root.querySelectorAll(".lp-exercises [data-exercise]").forEach(button=>button.textContent=EXERCISES[button.dataset.exercise].title[ko?1:0]);
    root.querySelector(".lp-shortcuts summary").textContent=ko?"전체 단축키":"All keyboard shortcuts";
    root.querySelector(".lp-key-feedback > span:first-child").textContent=t.pressed;
    root.querySelector(".lp-disclosure").textContent=ko?"브라우저 안의 데모 환경입니다. 실습을 바꿔도 터미널과 기록은 유지됩니다. 내 컴퓨터의 셸이나 실제 agent에 연결하지 않습니다.":"A browser-local demo. Switching exercises keeps your terminal and history. Not connected to your computer's shell or a live agent.";
    chrome();fitAll();render();
  }
  return {ready,setLanguage,destroy(){disposed=true;for(const timer of timers)clearTimeout(timer);observer?.disconnect();terminal?.dispose();for(const record of records.values())record.terminal.dispose();root.removeEventListener("click",click);root.removeEventListener("keydown",continueWithKeyboard,true);}};
}
