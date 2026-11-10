export const EXERCISES = {
  basics: { title:["tmux basics","tmux 기본"], events:["attach-tmux","basics-start","move-pane","zoom-pane","restore-panes","next-window","previous-window"], steps:["Start in a normal terminal","Understand tmux","Move to another pane","Zoom a pane","Restore the layout","Switch windows","Return to your window"], ko:["일반 터미널에서 시작하기","tmux 구조 이해하기","다른 pane으로 이동","pane 확대하기","원래 배치로 돌아가기","window 바꾸기","이전 window로 돌아가기"] },
  attention: { title: ["Waiting & review","기다림·리뷰"], events:["watch","find-waiting","attend","answer","review","inbox"], steps:["Open watch","Find the waiting agent","Connect to Claude","Answer","Request review","Read reply"], ko:["watch 열기","기다리는 pane 찾기","Claude에 연결","질문에 답하기","리뷰 요청","답장 읽기"] },
  workspace: { title:["Navigate with watch","작업공간 이동"], events:["session-node","other-session","window-node","attach-window","workspace-filter","attach-search"], steps:["Select a session in watch","Find another session","Choose its window","Connect to the window","Search for a workspace","Connect to the search result"], ko:["watch에서 session 선택","다른 session 찾기","window 선택","window에 연결","작업공간 검색","검색 결과로 이동"] },
  inspect: { title:["Inspect panes","화면 살펴보기"], events:["preview","live-preview","split","help"], steps:["Open preview","Read the live pane","Resize inspector","Open help"], ko:["preview 열기","실제 pane 보기","inspector 비율 변경","도움말 열기"] },
  free: { title:["Free play","자유 탐색"], events:[], steps:[], ko:[] },
};
export function resolvePracticeTrack(value) { return Object.hasOwn(EXERCISES,value)?value:"basics"; }
export function practiceProgress(track="basics") { return {track,index:0}; }
export function recordPractice(progress,event) {
  const expected=EXERCISES[progress.track].events[progress.index];
  return expected===event?{...progress,index:progress.index+1}:progress;
}
export function exerciseGuidance(progress,lang,mode) {
  const ko=lang==="ko";
  if(progress.track==="free")return ko?"session·window·pane을 자유롭게 이동하고 메시지를 보내보세요. Ctrl+b → z로 pane을 확대합니다. watch에서 ?로 키 목록을 봅니다.":"Move between sessions, windows and panes, and send messages. Ctrl+b → z zooms a pane. ? in watch opens the key list.";
  if(progress.index>=EXERCISES[progress.track].events.length)return ko?"실습을 마쳤습니다. 계속 탐색하거나 다른 실습을 선택하세요.":"Practice complete. Keep exploring or choose another exercise.";
  const guides={
    workspace: [
      ["Open watch and use h to select the parent session.","watch를 열고 h로 상위 session을 선택하세요."],
      ["Use j to select another session.","j로 다른 session을 선택하세요."],
      ["Use l to select its first window.","l로 첫 window를 선택하세요."],
      ["Enter connects to the selected window.","Enter로 선택한 window에 연결하세요."],
      ["Search for docs with / in watch.","watch에서 /로 docs를 검색하세요."],
      ["Enter connects to the selected result.","Enter로 검색 결과에 연결하세요."],
    ],
    inspect:[
      ["Open a pane preview with o.","o로 선택한 pane의 preview를 여세요."],
      ["Press c to read the live pane.","c로 실제 pane 화면을 보세요."],
      ["Return to watch and use | to change the inspector split.","watch에 돌아가 |로 inspector 비율을 바꾸세요."],
      ["Press ? for help.","?로 도움말을 확인하세요."],
    ],
  };
  return guides[progress.track]?.[progress.index]?.[ko?1:0]||"";
}

// The guide describes the next action from the current terminal state.
// Selecting an exercise never executes this action or changes the terminal.
export function nextPracticeAction(progress, context, lang="en") {
  const ko=lang==="ko", {mode,prefix,filterOpen,filter="",selection="",editor="",kind="QUESTION",previewLive=false,mailboxTab="incoming",hasReply=false}=context;
  const action=(en,kr,key=null,text=null,note=null)=>({instruction:ko?kr:en,key,text,note});
  const openWatch=()=>prefix?action("Release both keys. Press s to open watch.","두 키를 놓고 s를 눌러 watch를 여세요.","s"):action("First, press Ctrl and b together.","먼저 Ctrl과 b를 함께 누르세요.","Ctrl+b",null,ko?"키를 누른 뒤 두 키를 놓으세요.":"Release both keys afterwards.");
  const search=(query,target)=>{
    if(mode!=="watch")return openWatch();
    if(filterOpen)return filter.toLowerCase()===query?action("Press Enter to finish searching.","Enter로 검색을 마치세요.","Enter"):action(`Type ${query} in the search field.`, `검색란에 ${query}를 입력하세요.`,null,query);
    if(selection===target)return null;
    return action("Press / to search for an agent.","/를 눌러 에이전트를 검색하세요.","/");
  };
  const inWatch=(next)=>mode==="watch"?next:mode==="preview"||mode==="help"||mode==="inbox"?action("Return to watch first.","먼저 watch로 돌아가세요.","Esc"):openWatch();
  const index=progress.index;
  if(context.tmuxAttached===false){
    return editor.trim()==="tmux attach -t acme"||/^tmux (?:a|attach|attach-session)$/.test(editor.trim())
      ?action("Press Enter to connect to the prepared acme session.","Enter로 준비된 acme 연습 session에 연결하세요.","Enter")
      :action("This is a normal shell, outside tmux. Connect to the prepared practice session.","지금은 tmux 밖의 일반 셸입니다. 미리 준비된 연습 session에 연결해보세요.",null,"tmux attach -t acme");
  }
  if(progress.track==="basics"&&index<EXERCISES.basics.events.length){
    if(index<=1)return action("tmux keeps several terminal workspaces in one place. Start with these three terms.","tmux는 여러 터미널 작업공간을 한곳에서 다룹니다. 세 가지 이름부터 살펴보세요.");
    if(mode!=="shell"&&mode!=="agent")return action("Return to the pane view first.","먼저 pane 화면으로 돌아가세요.",mode==="watch"?"q":"Esc");
    if(!prefix)return action("Press Ctrl and b together to start a tmux shortcut.","tmux 단축키를 시작하려면 Ctrl과 b를 함께 누르세요.","Ctrl+b",null,ko?"두 키를 놓으면 다음에 누를 키를 안내합니다.":"Release both keys to see the next key.");
    return [null,null,
      action("Press o to move to the next pane. Its border shows where you type.","o로 다음 pane에 이동하세요. 테두리가 입력할 위치를 알려줍니다.","o"),
      action("Press z to enlarge this pane.","z로 현재 pane을 크게 보세요.","z"),
      action("Press z again to restore the pane layout.","z를 다시 눌러 원래 pane 배치로 돌아가세요.","z"),
      action("Press n to move to the next window. A window holds its own panes.","n으로 다음 window에 이동하세요. window마다 별도의 pane들이 있습니다.","n"),
      action("Press p to return to the previous window. Your panes and text are still there.","p로 이전 window에 돌아가세요. pane과 입력 내용이 그대로 남아 있습니다.","p"),
    ][index];
  }
  if(prefix&&progress.track==="free")return action("Prefix received. Release both keys and choose a tmux key below.","prefix를 받았습니다. 두 키를 놓고 아래 tmux 목록에서 원하는 키를 누르세요.");
  if(prefix)return openWatch();
  if(progress.track==="free")return action("Try the shortcuts below in this terminal. Your workspace and history stay with you.","아래 단축키를 이 터미널에서 직접 써보세요. 작업공간과 기록은 그대로 유지됩니다.");
  if(index>=EXERCISES[progress.track].events.length)return action("Practice complete. Keep exploring or choose another guide.","실습을 마쳤습니다. 계속 탐색하거나 다른 안내를 선택하세요.");
  if(progress.track==="attention"){
    if(index===0)return openWatch();
    if(index===1){
      if(mode!=="watch")return openWatch();
      if(!filterOpen&&selection==="window:acme/retry")return action("Expand this window to find the waiting agent.","이 window를 펼쳐 기다리는 에이전트를 찾으세요.","l");
      return search("claude","claude")||action("Claude is waiting for you. Enter connects to its pane.","Claude가 답을 기다립니다. Enter로 이 pane에 연결하세요.","Enter");
    }
    if(index===2)return search("claude","claude")||action("The waiting pane is selected. Connect to Claude.","기다리는 pane을 찾았습니다. Claude에 연결하세요.","Enter");
    if(index===3){
      if(mode!=="agent"||context.activePane!=="claude")return search("claude","claude")||action("Go to Claude's pane.","Claude pane으로 이동하세요.","Enter");
      return {...action("Answer Claude's question. Press Enter when you are ready.","Claude의 질문에 답을 입력하세요. 작성을 마치면 Enter로 보내세요.",editor.trim()?"Enter":null),example:{label:ko?"답변 예시":"Example answer",text:"exponential backoff",note:ko?"그대로 입력하지 않아도 됩니다.":"You do not need to copy this exactly."}};
    }
    if(index===4){
      if(mode!=="compose")return search("codex","codex")||action("Open a message to Codex.","Codex에게 보낼 메시지를 여세요.","m");
      if(kind!=="REVIEW")return action("Select REVIEW with Tab.","Tab으로 REVIEW를 선택하세요.","Tab");
      return {...action("Write your review request. Press Enter when you are ready.","리뷰할 내용을 입력하세요. 작성을 마치면 Enter로 보내세요.",editor.trim()?"Enter":null),example:{text:ko?"src/retry.ts를 리뷰해주세요.":"Review src/retry.ts.",note:ko?"그대로 입력하지 않아도 됩니다.":"You do not need to copy this exactly."}};
    }
    if(mode==="inbox")return mailboxTab==="sent"?action("Read your request and its reply.","요청과 답장을 확인하세요."):action("Switch to sent messages.","보낸 요청을 선택하세요.","Tab");
    if(!hasReply)return action("The agent is reviewing your request. Its reply will appear here.","에이전트가 요청을 검토하고 있습니다. 곧 답장이 표시됩니다.");
    return inWatch(action("Open the mailbox.","mailbox를 여세요.","M"));
  }
  if(progress.track==="workspace"){
    if(index<4){const keys=["h","j","l","Enter"];
      return inWatch(action(["Select the parent session.","Select another session.","Select its first window.","Connect to this window."][index],["h로 상위 session을 선택하세요.","j로 다른 session을 선택하세요.","l로 첫 window를 선택하세요.","Enter로 이 window에 연결하세요."][index],keys[index]));
    }
    if(index===4){
      if(mode!=="watch")return inWatch(null)||openWatch();
      if(filterOpen)return filter.trim().toLowerCase()==="docs"?action("Finish searching for the docs workspace.","Enter로 docs 작업공간 검색을 마치세요.","Enter"):action("Type the workspace name. This search also finds sessions, windows and agents.","작업공간 이름을 입력하세요. session·window·에이전트 이름으로도 찾을 수 있습니다.",null,"docs");
      return action("Open workspace search in watch.","watch의 작업공간 검색을 여세요.","/");
    }
    return inWatch(action("Connect to the selected search result.","Enter로 선택한 검색 결과에 연결하세요.","Enter"));
  }
  if(index===0){
    if(mode==="watch"&&selection.startsWith("window:"))return action("Select a pane to inspect.","l로 살펴볼 pane을 선택하세요.","l");
    return inWatch(action("Open the selected pane's preview.","o로 선택한 pane의 preview를 여세요.","o"));
  }
  if(index===1)return mode==="preview"?action("Switch to the live pane.","실제 pane 화면으로 전환하세요.","c"):inWatch(action("Open preview first.","먼저 preview를 여세요.","o"));
  if(index===2)return inWatch(action("Change the inspector split.","inspector의 비율을 바꿔보세요.","|"));
  return inWatch(action("Open the key list.","키 목록을 확인하세요.","?"));
}

export function practiceAlternatives(progress, context, lang="en") {
  const ko=lang==="ko", {mode,kind="QUESTION",workMode="READ-ONLY",mailboxTab="incoming",previewLive=false}=context;
  if(mode==="compose")return {
    title:ko?"다른 메시지 종류도 있습니다":"Other message types are available",
    items:[
      {label:"QUESTION",description:ko?"질문하고 답변 받기":"Ask for an answer"},
      {label:"REVIEW",description:ko?"변경사항 검토 요청":"Ask for review findings"},
      {label:"TASK",description:ko?"구체적인 작업 위임":"Delegate a task"},
      {label:"NOTICE",description:ko?"답변이 필요 없는 공지":"Share information; no reply expected"},
    ].map(item=>({...item,selected:item.label===kind})),
    key:"Tab",note:ko?"메시지 종류를 순서대로 바꿉니다.":"Cycle through message types.",
    extra:{title:ko?"전달 방식도 선택할 수 있습니다":"Delivery modes are also available",key:"Ctrl+e",note:ko?"전달 방식을 순서대로 바꿉니다.":"Cycle through delivery modes.",items:[
      {label:"READ-ONLY",description:ko?"조사·답변만 허용":"Investigate and answer"},
      {label:"EXECUTE",description:ko?"작업 실행 허용":"Authorize changes"},
      {label:"JUST SEND",description:ko?"기록되는 요청 없이 터미널에 직접 전달":"Send directly without a durable request"},
    ].map(item=>({...item,selected:item.label===workMode}))},
  };
  if(mode==="inbox")return {title:ko?"mailbox에는 두 보기가 있습니다":"Two mailbox views are available",key:"Tab",note:ko?"받은 메시지와 보낸 요청을 전환합니다.":"Switch between received and sent messages.",items:[{label:"incoming",description:ko?"선택한 대상의 받은 메시지":"Messages received by this target",selected:mailboxTab==="incoming"},{label:"sent",description:ko?"내가 보낸 요청과 답장":"Your requests and their replies",selected:mailboxTab==="sent"}]};
  if(mode==="preview")return {title:ko?"preview에는 두 보기가 있습니다":"Two preview views are available",key:"c",note:ko?"두 화면을 전환합니다.":"Switch between the views.",items:[{label:"prompt",description:ko?"최근 프롬프트와 응답":"Recent prompt and response",selected:!previewLive},{label:"live pane",description:ko?"현재 터미널 화면":"Current terminal screen",selected:previewLive}]};
  if(progress.track==="workspace")return {title:ko?"watch에서 이동하는 두 가지 방법":"Two ways to navigate in watch",note:ko?"watch 안에서는 prefix 없이 누릅니다.":"Inside watch, these keys need no prefix.",items:[{label:"h / l · j / k",description:ko?"계층을 펼치며 작업공간 찾기":"Browse the workspace tree"},{label:"/",description:ko?"session·window·에이전트 검색":"Search sessions, windows and agents"}]};
  return null;
}
