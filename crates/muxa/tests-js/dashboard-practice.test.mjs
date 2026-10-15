import assert from "node:assert/strict";
import test from "node:test";
import {EXERCISES,resolvePracticeTrack,practiceProgress,recordPractice,exerciseGuidance} from "../src/dashboard/web/landing-practice.mjs";

test("each guided exercise advances only after the expected action",()=>{
  for(const track of ["basics","attention","workspace","inspect"]){
    let progress=practiceProgress(track);
    assert.equal(recordPractice(progress,"unrelated"),progress);
    for(const event of EXERCISES[track].events) progress=recordPractice(progress,event);
    assert.equal(progress.index,EXERCISES[track].events.length);
    assert.match(exerciseGuidance(progress,"en"),/complete/);
    assert.equal(recordPractice(progress,EXERCISES[track].events[0]),progress);
  }
});
test("free play has no forced step progression",()=>{
  const progress=practiceProgress("free");
  assert.equal(recordPractice(progress,"review"),progress);
  assert.match(exerciseGuidance(progress,"ko"),/자유롭게/);
});

test("review guidance reveals only the current key or text entry",async()=>{
  const {nextPracticeAction}=await import("../src/dashboard/web/landing-practice.mjs");
  const progress={track:"attention",index:4};
  const guide=context=>nextPracticeAction(progress,{mode:"agent",...context},"ko");
  assert.equal(guide({}).key,"Ctrl+b");
  assert.equal(guide({prefix:true}).key,"s");
  assert.equal(guide({mode:"watch",selection:"window:acme/retry"}).key,"/");
  assert.equal(guide({mode:"watch",filterOpen:true,filter:"co"}).text,"codex");
  assert.equal(guide({mode:"watch",filterOpen:true,filter:"codex"}).key,"Enter");
  assert.equal(guide({mode:"watch",selection:"codex"}).key,"m");
  assert.equal(guide({mode:"compose",kind:"QUESTION"}).key,"Tab");
  assert.equal(guide({mode:"compose",kind:"REVIEW"}).example.text,"src/retry.ts를 리뷰해주세요.");
  const composing=guide({mode:"compose",kind:"REVIEW",editor:"R"});
  assert.equal(composing.key,"Enter");
  assert.equal(composing.example.text,guide({mode:"compose",kind:"REVIEW"}).example.text);
  assert.match(composing.example.note,/그대로 입력하지/);
});

test("workspace and inspection guides separate prefix, return, and action",async()=>{
  const {nextPracticeAction}=await import("../src/dashboard/web/landing-practice.mjs");
  assert.equal(nextPracticeAction({track:"workspace",index:4},{mode:"shell"}).key,"Ctrl+b");
  assert.equal(nextPracticeAction({track:"workspace",index:4},{mode:"shell",prefix:true}).key,"s");
  assert.equal(nextPracticeAction({track:"inspect",index:2},{mode:"preview"}).key,"Esc");
  assert.equal(nextPracticeAction({track:"inspect",index:2},{mode:"watch"}).key,"|");
  const waiting=nextPracticeAction({track:"attention",index:5},{mode:"watch",hasReply:false},"ko");
  assert.equal(waiting.key,null);
  assert.match(waiting.instruction,/검토/);
});

test("the first watch visit leads directly into the waiting pane",async()=>{
  const {nextPracticeAction}=await import("../src/dashboard/web/landing-practice.mjs");
  const progress={track:"attention",index:1};
  const windowGuide=nextPracticeAction(progress,{mode:"watch",selection:"window:acme/retry",activePane:"shell"},"ko");
  assert.equal(windowGuide.key,"l");
  assert.equal(windowGuide.text,null);
  const paneGuide=nextPracticeAction(progress,{mode:"watch",selection:"claude",activePane:"shell"},"ko");
  assert.equal(paneGuide.key,"Enter");
  assert.match(paneGuide.instruction,/Claude/);
  const closed=nextPracticeAction(progress,{mode:"shell",selection:"shell"});
  assert.equal(closed.key,"Ctrl+b");
  assert.equal(closed.text,null);
});


test("tmux basics is first and reveals one key at a time",async()=>{
  const {nextPracticeAction}=await import("../src/dashboard/web/landing-practice.mjs");
  assert.equal(Object.keys(EXERCISES)[0],"basics");
  assert.equal(practiceProgress().track,"basics");
  assert.equal(nextPracticeAction(practiceProgress(),{mode:"shell"}).key,null);
  for(const [index,key] of ["o","z","z","n","p"].entries()){
    const progress={track:"basics",index:index+2};
    assert.equal(nextPracticeAction(progress,{mode:"shell"}).key,"Ctrl+b");
    assert.equal(nextPracticeAction(progress,{mode:"shell",prefix:true}).key,key);
  }
});


test("outside tmux the guide starts with a real attach command",async()=>{
  const {nextPracticeAction}=await import("../src/dashboard/web/landing-practice.mjs");
  const state=practiceProgress();
  assert.equal(nextPracticeAction(state,{mode:"shell",tmuxAttached:false}).text,"tmux attach -t acme");
  assert.equal(nextPracticeAction(state,{mode:"shell",tmuxAttached:false,editor:"tmux attach -t acme"}).key,"Enter");
  assert.equal(nextPracticeAction({track:"attention",index:0},{mode:"shell",tmuxAttached:false}).text,"tmux attach -t acme");
});

test("workspace practice uses watch browsing and search instead of tmux window shortcuts",async()=>{
  const {nextPracticeAction,practiceAlternatives}=await import("../src/dashboard/web/landing-practice.mjs");
  let progress={track:"workspace",index:4};
  assert.equal(recordPractice(progress,"next-window"),progress);
  assert.equal(recordPractice(progress,"next-session"),progress);
  assert.equal(nextPracticeAction(progress,{mode:"watch"}).key,"/");
  assert.equal(nextPracticeAction(progress,{mode:"watch",filterOpen:true,filter:"d"}).text,"docs");
  assert.equal(nextPracticeAction(progress,{mode:"watch",filterOpen:true,filter:"docs"}).key,"Enter");
  progress=recordPractice(progress,"workspace-filter");
  assert.equal(nextPracticeAction(progress,{mode:"watch"}).key,"Enter");
  assert.equal(recordPractice(progress,"attach-search").index,6);
  assert.ok(!JSON.stringify(practiceAlternatives(progress,{mode:"watch"})).includes("n / p"));
  assert.equal(nextPracticeAction({track:"inspect",index:0},{mode:"watch",selection:"codex"}).key,"o");
});

test("entry links select only known practice tracks",()=>{
  for(const track of Object.keys(EXERCISES))assert.equal(resolvePracticeTrack(track),track);
  for(const track of [null,undefined,"unknown","constructor","__proto__"])assert.equal(resolvePracticeTrack(track),"basics");
});
