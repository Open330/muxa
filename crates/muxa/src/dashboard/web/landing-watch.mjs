// Browser approximation of the classic watch renderer. Vocabulary, borders,
// topology columns, width thresholds and keys follow muxa-cli/src/watch.rs.
export const DEMO_SESSIONS = [
  { name: "acme", windows: [{ name: "retry", panes: ["shell", "claude", "codex", "gemini"] }, { name: "docs", panes: ["docs-shell", "docs-claude"] }] },
  { name: "ops", windows: [{ name: "build", panes: ["build-shell", "build-codex"] }, { name: "logs", panes: ["logs-shell"] }] },
];
export const DEMO_PANES = DEMO_SESSIONS.flatMap(session => session.windows.flatMap(window => window.panes));
export const PANE_IDS = { shell: "%11", claude: "%12", codex: "%13", gemini: "%14", "docs-shell": "%21", "docs-claude": "%22", "build-shell": "%31", "build-codex": "%32", "logs-shell": "%41" };
export const PANE_META = Object.fromEntries(DEMO_SESSIONS.flatMap(session => session.windows.flatMap(window => window.panes.map((name,index) => [name, { session:session.name, window:window.name, role:name.split("-").at(-1), index, location:`${session.name}:${window.name}.${index}` }]))));
export function paneWindow(pane) { const meta=PANE_META[pane];return DEMO_SESSIONS.find(session=>session.name===meta.session).windows.find(window=>window.name===meta.window); }
export function watchPaneOrder(details, panes = Object.keys(details).filter(name => PANE_META[name])) {
  const rank = name => PANE_META[name]?.role === "shell" ? 9 : details[name]?.state?.startsWith("waiting") || details[name]?.state === "error" ? 0 : details[name]?.state === "working" ? 1 : 3;
  return [...panes].sort((a,b)=>rank(a)-rank(b)||a.localeCompare(b));
}
export function adjacentPane(pane, direction) {
  const panes=paneWindow(pane).panes,i=panes.indexOf(pane);
  const moves = { left: i % 2 ? i - 1 : i, right: i % 2 || i+1>=panes.length ? i : i + 1, up: i >= 2 ? i - 2 : i, down: i+2<panes.length ? i+2 : i };
  return panes[moves[direction] ?? i];
}
export function topologyNode(key) {
  if(PANE_META[key])return { key,type:"pane",pane:key,...PANE_META[key] };
  if(key.startsWith("session:"))return {key,type:"session",session:key.slice(8)};
  const [session,window]=key.slice(7).split("/");return {key,type:"window",session,window};
}
export function resolveNodePane(key) {
  const node=topologyNode(key);if(node.pane)return node.pane;
  const session=DEMO_SESSIONS.find(item=>item.name===node.session);
  return (session.windows.find(item=>item.name===node.window)||session.windows[0]).panes[0];
}
export function topologyRows(details, selected="shell", filter="") {
  const current=topologyNode(selected),rows=[],query=filter.toLowerCase();
  const matches=pane=>!query||`${PANE_META[pane].session} ${PANE_META[pane].window} ${PANE_META[pane].role} ${PANE_IDS[pane]} ${details[pane]?.summary||""}`.toLowerCase().includes(query);
  for(const session of DEMO_SESSIONS){
    const windows=session.windows.filter(window=>window.panes.some(matches));if(!windows.length)continue;
    const expanded=!!query||current.session===session.name;
    rows.push({key:`session:${session.name}`,type:"session",session:session.name,label:`${expanded?"▼":"▸"} ${session.name}`,summary:`${session.windows.length} windows`,level:0});
    if(!expanded)continue;
    for(const window of windows){
      const open=!!query||(current.type==="pane"&&current.window===window.name);
      rows.push({key:`window:${session.name}/${window.name}`,type:"window",session:session.name,window:window.name,label:`  ${window===windows.at(-1)?"└":"├"}─ ${open?"▼":"▸"} ${window.name}`,summary:`${window.panes.length} panes`,level:1});
      if(open){const ordered=watchPaneOrder(details,window.panes).filter(matches);for(const [index,pane] of ordered.entries())rows.push({key:pane,type:"pane",pane,session:session.name,window:window.name,label:`  ${window===windows.at(-1)?" ":"│"} ${index===ordered.length-1?"└":"├"}─   ${PANE_IDS[pane]}  ${PANE_META[pane].role}`,summary:details[pane]?.summary||"zsh",level:2});}
    }
  }
  return rows;
}
export function navigateTopology(selected, direction, details, filter="") {
  const node=topologyNode(selected);
  if(direction==="left")return node.type==="pane"?`window:${node.session}/${node.window}`:node.type==="window"?`session:${node.session}`:selected;
  if(direction==="right"){
    if(node.type==="session")return `window:${node.session}/${DEMO_SESSIONS.find(session=>session.name===node.session).windows[0].name}`;
    if(node.type==="window")return watchPaneOrder(details,paneWindow(resolveNodePane(selected)).panes)[0];
    return selected;
  }
  const siblings=filter?topologyRows(details,selected,filter).filter(row=>row.type==="pane").map(row=>row.key):node.type==="session"?DEMO_SESSIONS.map(session=>`session:${session.name}`):node.type==="window"?DEMO_SESSIONS.find(session=>session.name===node.session).windows.map(window=>`window:${node.session}/${window.name}`):watchPaneOrder(details,paneWindow(selected).panes);
  const index=siblings.indexOf(selected),delta=direction==="down"?1:siblings.length-1;
  return siblings.length?siblings[(Math.max(0,index)+delta)%siblings.length]:selected;
}
export function textWidth(text) {
  return Array.from(text).reduce((n, c) => n + (/\p{Mark}/u.test(c) ? 0 : /[\p{Script=Hangul}\p{Script=Han}\p{Script=Hiragana}\p{Script=Katakana}\p{Extended_Pictographic}]/u.test(c) ? 2 : 1), 0);
}
export function watchFrame({ cols, rows, selected = "shell", depth = 1, filter = "", filterOpen = false, split = 50, details = {}, view = "watch", request = null, tab = "incoming", input = "", cursor = 0, kind = "REVIEW", workMode = "READ-ONLY", previewLive = false, requests = null, mailboxIndex = 0, target = null }) {
  const cells = Array.from({ length: rows }, () => Array.from({ length: cols }, () => ({ ch: " ", fg: 7, bg: null })));
  function put(x, y, text, fg = 7, bg = null) {
    if (y < 0 || y >= rows) return;
    for (const ch of String(text).replace(/[\x00-\x1f\x7f-\x9f]/g, "")) {
      const size = textWidth(ch);
      if (x >= 0 && x + size <= cols) { cells[y][x] = { ch, fg, bg }; if (size === 2) cells[y][x + 1] = { ch: "", fg, bg }; }
      x += size;
    }
  }
  function clipped(text, width) {
    let out = "", n = 0;
    for (const ch of String(text)) { const size = textWidth(ch); if (n + size > width) break; out += ch; n += size; }
    return out;
  }
  function box(x, y, w, h, title, color = 8, titleBadge = true) {
    if (w < 3 || h < 3) return;
    put(x,y,"┌" + "─".repeat(w-2) + "┐",color);
    put(x,y+h-1,"└" + "─".repeat(w-2) + "┘",color);
    for(let line=y+1;line<y+h-1;line++){put(x,line,"│",color);put(x+w-1,line,"│",color);}
    put(x+1,y,clipped(` ${title} `,w-2),titleBadge?0:7,titleBadge?6:null);
  }
  function content(x, y, w, lines, max = rows-y-1) {
    let at = y;
    for (const line of lines.flatMap(value => String(value).split("\n"))) {
      let remaining = line;
      do {
        if (at >= y + max) return;
        const part = clipped(remaining,w); put(x,at++,part); remaining = remaining.slice(part.length);
      } while (remaining);
    }
  }
  const node=topologyNode(selected),pane=resolveNodePane(selected),recipient=target||pane;
  const current = details[pane] || {};
  const allRequests=requests||(request?[request]:[]);
  put(0,0," muxa watch ",0,6);
  put(13,0,clipped(`2 sessions · 4 windows · 9 panes   sort ST${allRequests.length ? `   mail 0/${allRequests.filter(item=>item.reply).length}` : ""}`,cols-13),8);
  if(cols>=120)put(cols-13,0,new Date().toISOString().slice(11,19)+" UTC",8);
  put(0,1,clipped(filterOpen ? `filter: ${filter || "▏"}` : "j/k move  ·  / filter  ·  ? help",cols),filterOpen ? 6 : 8);
  put(0,2,"─".repeat(cols),8);
  const footer = view === "compose" ? " Enter send   Tab kind   Ctrl-E read-only/execute/just-send   Esc cancel"
    : view === "inbox" ? " Tab incoming/sent   j/k select   M/Esc back"
    : view === "preview" ? " c live pane   Enter jump   q/Esc back"
    : view === "help" ? " ? / Esc back"
    : " j/k move  h/l tree  / filter  Enter jump  o preview  m message  M mailbox  ? help";
  put(0,rows-1,clipped(footer,cols),7);
  let cursorPosition = null;
  if (view === "inbox") {
    const applicable=allRequests.filter(item=>tab==="sent"||(node.type==="pane"?item.target===pane:PANE_META[item.target].session===node.session&&(node.type!=="window"||PANE_META[item.target].window===node.window))).slice().reverse();
    const picked=applicable[Math.min(mailboxIndex,Math.max(0,applicable.length-1))];
    box(0,3,cols,rows-4,`messages · ${PANE_META[pane].role}@${PANE_IDS[pane]} · ${tab === "sent" ? "incoming [SENT]" : "[INCOMING] sent"} · ${applicable.length}`);
    if (picked) {
      let at=5;const y=Math.max(10,rows-12);
      for(const [i,item] of applicable.entries()){
        if(at+1>=y)break;
        put(2,at++,clipped(`${item===picked?">":" "} ◆ ${item.kind||"REVIEW"} console → ${PANE_META[item.target].role}@${PANE_IDS[item.target]} · ${item.reply?"✓ completed":item.status||"pending"}`,cols-4),item.reply?2:3,item===picked?8:null);
        put(4,at++,clipped(item.body,cols-6));
      }
      put(1,y,"─".repeat(cols-2),8); put(2,y," conversation ",7);
      content(2,y+2,cols-4,[`console: ${picked.body}`,"",`${PANE_META[picked.target].role}: ${picked.reply || picked.progress || "Waiting for reply…"}`],rows-y-4);
    } else put(2,5,"No messages in this mailbox.",8);
  } else if (view === "help" || view === "preview") {
    box(0,3,cols,rows-4,view === "help" ? "Help" : `preview · ${PANE_META[pane].location} · ${previewLive ? "live pane" : "prompt"}`);
    content(2,5,cols-4,view === "help" ? ["j/k / ↑↓       move between panes", "h/l / ←→       parent / descend", "/               filter", "Enter           attach selected pane", "o               preview", "m / M           message / mailbox", "|               cycle inspector split", "q / Esc         close", "", "tmux: Ctrl+b then arrow = select pane", "tmux: Ctrl+b then z = zoom pane", "tmux: Ctrl+b then s = muxa watch", "tmux: Ctrl+b then n/p = next/previous window", "tmux: Ctrl+b then )/( = next/previous session"] : previewLive ? current.capture || [] : [`pane: ${PANE_META[pane].location}  kind: ${PANE_META[pane].role}  state: ${current.state}`, "", "Last prompt:", current.prompt || "—", "", "Last response:", current.response || "—"], rows-7);
  } else {
    const inspector = cols >= 120;
    const listWidth = inspector ? Math.floor(cols*split/100) : cols;
    box(0,3,listWidth,rows-4,"Topology");
    const nodeWidth = listWidth >= 70 ? Math.max(24,listWidth-36) : listWidth >=60 ? 28 : listWidth-15;
    put(3,4,"STATE",7);put(11,4,clipped("SESSION / WINDOW / PANE",nodeWidth),7);
    if(listWidth>=70)put(11+nodeWidth+1,4,"AGE",7);
    const summaryX=listWidth>=70 ? 11+nodeWidth+9 : 11+nodeWidth+1;
    if(listWidth>=60)put(summaryX,4,"SUMMARY",7);
    let y=6;
    const tree=topologyRows(details,selected,filter);
    function aggregate(row){
      const names=row.type==="pane"?[row.pane]:DEMO_PANES.filter(name=>PANE_META[name].session===row.session&&(row.type!=="window"||PANE_META[name].window===row.window));
      const glyphs=[...new Set(names.map(name=>details[name]?.glyph||"○"))];return {glyph:glyphs.join(" ").slice(0,6),age:row.type==="pane"?details[row.pane]?.age||"—":"—"};
    }
    for(const row of tree){
      if(y>=rows-2)break;
      const info=row.type==="pane"?details[row.pane]||{}:aggregate(row);const chosen=row.key===selected,bg=chosen?8:null;
      if(chosen)for(let x=1;x<listWidth-1;x++)put(x,y," ",7,bg);
      put(1,y,chosen?">":" ",7,bg);put(3,y,info.glyph||"○",info.state==="working"||info.state?.startsWith("waiting")?3:2,bg);
      put(11,y,clipped(row.label,nodeWidth),7,bg);
      if(listWidth>=70)put(11+nodeWidth+1,y,info.age||"—",8,bg);
      if(listWidth>=60)put(summaryX,y,clipped(row.summary,listWidth-summaryX-2),7,bg);
      y++;
    }
    if(inspector){
      const label=node.type==="pane"?`pane ${PANE_IDS[pane]}`:node.type==="window"?`window ${node.window}`:`session ${node.session}`;
      box(listWidth,3,cols-listWidth,rows-4,`Inspector · ${label}`);
      const w=cols-listWidth-4,meta=PANE_META[pane];
      const paneInfo=meta.role==="shell"?["status ○ untracked",`pane ${PANE_IDS[pane]} · zsh`,`endpoint tmux:default · ${meta.session}:${meta.window}`,"cwd /home/demo/acme"]:[`status ${current.glyph||""} ${current.state||"idle"}  ${current.age||"—"}`,`pane ${PANE_IDS[pane]} · ${meta.role}`,`room ${meta.session}:${meta.window}`,"cwd /home/demo/acme",`agent ${meta.role} · ${current.state||"idle"}`,`prompt ${current.prompt||"—"}`,`response ${current.response||"—"}`];
      const parentInfo=node.type==="session"?[`${node.session} · 2 windows`,...DEMO_SESSIONS.find(session=>session.name===node.session).windows.map(window=>`${window.name} · ${window.panes.length} panes`)] : [`${node.session}:${node.window}`,`${paneWindow(pane).panes.length} panes`,...paneWindow(pane).panes.flatMap(name=>[`${PANE_META[name].role} ${PANE_IDS[name]} · ${details[name]?.state||"shell"}`,details[name]?.summary||"zsh"])];
      if(node.type!=="window")content(listWidth+2,5,w,node.type==="pane"?[...paneInfo,"─".repeat(w),...current.capture||[]]:parentInfo,rows-7);
      else {
        const names=paneWindow(pane).panes,agents=names.filter(name=>PANE_META[name].role!=="shell"),waiting=agents.filter(name=>details[name]?.state?.startsWith("waiting")),working=agents.filter(name=>details[name]?.state==="working"),idle=agents.length-waiting.length-working.length;
        const states=[waiting.length?`waiting ${waiting.length}`:"",working.length?`working ${working.length}`:"",idle?`idle ${idle}`:""].filter(Boolean).join(" · ");
        const recent=agents.find(name=>details[name]?.state==="working")||agents[0];
        content(listWidth+2,5,w,[`status  ${states||"untracked"}`,`scope  ${names.length} panes · ${agents.length} agents`,"load  0 processes · 0 shells · 0 subagents",`room  tmux:default:${node.window} · ${agents.length} peers`,"cwd  /home/demo/acme",`attention  ${waiting.map(name=>`${PANE_IDS[name]} · ${details[name].summary}`).join(" · ")||"—"}`,`latest  ${recent?`${PANE_META[recent].role} · ${details[recent]?.summary||"—"}`:"—"}`,"─".repeat(w),"live layout  pane geometry · 1s refresh"],9);
        const columns=names.length===1?1:2,gridRows=Math.ceil(names.length/columns),top=14,height=rows-top-2;
        if(height>=gridRows*3)for(const [index,name] of names.entries()){
          const gx=listWidth+2+(index%columns)*Math.floor(w/columns),gy=top+Math.floor(index/columns)*Math.floor(height/gridRows),gw=index%columns===columns-1?w-Math.floor(w/columns)*(columns-1):Math.floor(w/columns),gh=Math.floor(height/gridRows);
          const item=details[name]||{},role=PANE_META[name].role;
          box(gx,gy,gw,gh,`${item.glyph||"○"} ${PANE_IDS[name]} · ${role}`,8,false);
          content(gx+1,gy+1,gw-2,(item.capture||[]).slice(-(gh-2)),gh-2);
        }
      }
    }
    if(view==="compose"){
      const w=Math.max(8,cols-8), x=Math.floor((cols-w)/2), y=Math.max(5,rows-7);
      for(let line=y;line<y+3;line++)for(let col=x;col<x+w;col++)put(col,line," ");
      box(x,y,w,3,`${kind==="REVIEW"?"◆":"?"} ${kind} · ○ ${workMode} → ${PANE_META[recipient].role}@${PANE_IDS[recipient]}`,kind==="REVIEW"?5:6);
      const badge = { QUESTION: ["?",6,0], REVIEW: ["◆",5,7], TASK: ["▶",3,0], NOTICE: ["!",4,7] }[kind] || ["?",6,0];
      const kindLabel = ` ${badge[0]} ${kind} `;
      put(x+1,y,clipped(kindLabel,w-2),badge[2],badge[1]);
      const modeLabel = workMode === "READ-ONLY" ? " ○ READ-ONLY " : workMode === "EXECUTE" ? " ● EXECUTE " : " ▷ SEND · keystrokes ";
      put(x+1+textWidth(kindLabel),y,clipped(modeLabel,w-textWidth(kindLabel)-2),workMode==="EXECUTE"?7:0,workMode==="EXECUTE"?1:2);
      const before=Array.from(input).slice(0,cursor).join("");
      const skip=Math.max(0,textWidth(before)-(w-6));
      let visible=input;while(skip>0&&textWidth(input)-textWidth(visible)<skip)visible=Array.from(visible).slice(1).join("");
      put(x+1,y+1,clipped(`> ${visible}`,w-2));
      cursorPosition={row:y+1,col:Math.min(x+w-2,x+3+Math.min(textWidth(before),w-6))};
    }
  }
  const lines=cells.map(row=>row.map(cell=>cell.ch).join(""));
  let ansi="\x1b[?25l\x1b[H\x1b[2J";
  for(let y=0;y<rows;y++){
    ansi+=`\x1b[${y+1};1H`;let previous="";
    for(const cell of cells[y]){const style=`\x1b[${cell.fg<8?30+cell.fg:90+cell.fg-8};${cell.bg===null?49:cell.bg<8?40+cell.bg:100+cell.bg-8}m`;if(style!==previous){ansi+=style;previous=style;}ansi+=cell.ch;}
  }
  ansi+="\x1b[0m";
  if(cursorPosition)ansi+=`\x1b[${cursorPosition.row+1};${cursorPosition.col+1}H\x1b[?25h`;
  return { lines, ansi, inspector: cols>=120, cursor:cursorPosition };
}
