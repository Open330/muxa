// Shared public-page language settings and navigation.
export const LANDING_LANG_KEY = "muxa.landing.lang";
export const LANDING_LANGUAGES = ["en", "ko"];
const escape = value => String(value).replace(/[&<>"']/g, ch => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"})[ch]);

export function pickLanguage(saved, preferred=[]) {
  if (LANDING_LANGUAGES.includes(saved)) return saved;
  for (const tag of preferred) {
    const base=String(tag||"").toLowerCase().split("-")[0];
    if (LANDING_LANGUAGES.includes(base)) return base;
  }
  return "en";
}

export function languageControls(lang) {
  return `<div class="l-lang" role="group" aria-label="${lang==="ko"?"언어":"Language"}">${LANDING_LANGUAGES.map(code=>`<button type="button" data-landing-lang="${code}" aria-pressed="${code===lang}" lang="${code}">${code==="ko"?"한국어":"EN"}</button>`).join("")}</div>`;
}

export function publicHeader({lang="en",homeHref="#",iconSrc="icon.svg",section="",context="",links=""}={}) {
  return `<header class="l-nav"><div class="l-wrap"><a class="l-brand" href="${escape(homeHref)}"><img src="${escape(iconSrc)}" width="24" height="24" alt="">muxa${section?`<span class="l-brand-section">/ ${escape(section)}</span>`:""}</a>${context}<nav class="l-nav-actions" aria-label="${lang==="ko"?"주 메뉴":"Main navigation"}">${languageControls(lang)}${links}</nav></div></header>`;
}
