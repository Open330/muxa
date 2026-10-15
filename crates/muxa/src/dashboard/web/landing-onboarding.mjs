import { mountPractice } from "./landing-learn.mjs";
import { resolvePracticeTrack } from "./landing-practice.mjs";
import { LANDING_LANG_KEY, pickLanguage, publicHeader } from "./landing-shared.mjs";

const root=document.getElementById("landing");
const home=root.dataset.home||"../";
let saved;
try { saved=localStorage.getItem(LANDING_LANG_KEY); } catch {}
let lang=pickLanguage(saved,navigator.languages||[navigator.language]);
const initialTrack=resolvePracticeTrack(new URLSearchParams(location.search).get("track"));
const links=()=>`<a class="l-nav-link l-hide-sm" href="${home}#how">${lang==="ko"?"소개":"Overview"}</a><a class="l-btn dark sm" href="${home}#install">${lang==="ko"?"설치":"Install"}</a>`;
const header=()=>publicHeader({lang,homeHref:home,iconSrc:`${home}icon.svg`,section:"learn",links:links()});
const footer=()=>`<div class="l-wrap"><a class="l-brand small" href="${home}">muxa</a><span>${lang==="ko"?"직접 익힌 다음, 내 tmux에서 시작하세요.":"Learn it here, then start in your own tmux."} <a href="${home}#install">${lang==="ko"?"설치 안내":"Installation guide"} →</a></span><a href="https://github.com/Open330/muxa/tree/main/docs">${lang==="ko"?"문서":"Docs"}</a></div>`;
root.innerHTML=`${header()}<main><section class="l-section l-wrap" id="practice" data-standalone="true"></section></main><footer class="l-foot" data-learn-footer>${footer()}</footer>`;
function updateMetadata(){
  document.documentElement.lang=lang;
  document.title=lang==="ko"?"muxa 배우기 · tmux 기본부터 협업까지":"Learn muxa · From tmux basics to collaboration";
}
updateMetadata();
const practice=mountPractice(root.querySelector("#practice"),{lang,installHref:`${home}#install`,initialTrack,autofocus:true,onTrackChange(track){
  const url=new URL(location.href);url.searchParams.set("track",track);history.replaceState(null,"",url);
}});
root.addEventListener("click",event=>{
  const button=event.target.closest("[data-landing-lang]");
  if(!button||button.dataset.landingLang===lang)return;
  lang=button.dataset.landingLang;
  try { localStorage.setItem(LANDING_LANG_KEY,lang); } catch {}
  root.querySelector(".l-nav").outerHTML=header();
  root.querySelector("[data-learn-footer]").innerHTML=footer();
  updateMetadata();practice.setLanguage(lang);
  root.querySelector(`[data-landing-lang="${lang}"]`).focus();
});
window.addEventListener("pagehide",event=>{ if(!event.persisted)practice.destroy(); });
