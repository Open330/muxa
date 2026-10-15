// Shared landing/onboarding design and navigation regression checks.
// MUXA_PLAYWRIGHT_PACKAGE=/path/to/playwright-core node scripts/landing-shots/pages-smoke.mjs http://127.0.0.1:8766 /tmp/muxa-pages-check
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
const require=createRequire(import.meta.url);
const {chromium}=require(process.env.MUXA_PLAYWRIGHT_PACKAGE||"playwright");
const origin=process.argv[2],output=process.argv[3]||"/tmp/muxa-pages-check";
if(!origin)throw new Error("Pass the assembled site's preview URL");
mkdirSync(output,{recursive:true});
const browser=await chromium.launch({executablePath:process.env.MUXA_TEST_CHROMIUM||"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",headless:true});
try {
  for(const locale of ["ko-KR","en-US"])for(const colorScheme of ["light","dark"])for(const mobile of [false,true]){
    const context=await browser.newContext({locale,colorScheme,reducedMotion:"reduce",viewport:mobile?{width:390,height:844}:{width:1280,height:1000}});
    const page=await context.newPage(),errors=[];
    page.on("pageerror",error=>errors.push(error.message));
    page.on("response",response=>{if(response.status()>=400)errors.push(`${response.status()} ${response.url()}`);});
    const label=`${locale}-${colorScheme}-${mobile?"mobile":"desktop"}`;
    await page.goto(`${origin}/`);
    await page.locator('.l-learning-path').first().waitFor();
    assert.equal(await page.locator('h1').count(),1);
    assert.equal(await page.locator('.xterm').count(),0,"landing should link to the course instead of mounting ten terminals");
    assert.equal(await page.locator('.l-learning-path').count(),2);
    assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
    const tokens=await page.locator('.landing').evaluate(el=>Object.fromEntries(["--l-bg","--l-ink","--l-accent","--l-sans","--l-max"].map(name=>[name,getComputedStyle(el).getPropertyValue(name)])));
    await page.locator('img[loading="lazy"]').evaluateAll(async images=>{for(const image of images)image.loading="eager";await Promise.all(images.map(image=>image.decode()));});
    await page.screenshot({path:join(output,`${label}-landing-fold.png`)});
    await page.screenshot({path:join(output,`${label}-landing.png`),fullPage:true});
    await page.locator('.l-learning-path[href*="track=basics"]').click();
    await page.locator('.xterm-helper-textarea').first().waitFor({state:"attached"});
    assert.equal(await page.locator('h1').count(),1);
    assert.equal(await page.locator('.lp-workspace').getAttribute('data-tmux'),"outside");
    assert.equal(await page.locator('.lp-exercises [data-exercise="basics"]').getAttribute('aria-pressed'),"true");
    const learnTokens=await page.locator('.landing').evaluate(el=>Object.fromEntries(["--l-bg","--l-ink","--l-accent","--l-sans","--l-max"].map(name=>[name,getComputedStyle(el).getPropertyValue(name)])));
    assert.deepEqual(learnTokens,tokens,"both pages use one theme");
    assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
    await page.screenshot({path:join(output,`${label}-learn.png`),fullPage:true});
    await page.keyboard.type("echo persistent-language-draft");
    const currentLang=await page.evaluate(()=>document.documentElement.lang),nextLang=currentLang==="ko"?"en":"ko";
    await page.locator(`[data-landing-lang="${nextLang}"]`).click();
    assert.equal(await page.evaluate(()=>document.documentElement.lang),nextLang);
    await page.waitForFunction(()=>document.querySelector('.lp-pane.is-active .xterm-accessibility-tree')?.textContent.includes('echo persistent-language-draft'));
    await page.locator('.lp-pane.is-active .xterm-helper-textarea').focus();
    await page.keyboard.press("Control+u");await page.keyboard.type("tmux attach -t acme");await page.keyboard.press("Enter");
    await page.keyboard.press("Control+b");await page.keyboard.press("s");await page.keyboard.press("/");await page.keyboard.type("codex");await page.keyboard.press("Enter");await page.keyboard.press("m");
    await page.keyboard.type("Keep this review draft");
    await page.locator(`[data-landing-lang="${currentLang}"]`).click();
    assert.equal(await page.locator('[data-mode]').textContent(),"compose");
    await page.waitForFunction(()=>document.querySelector('[data-overlay] .xterm-accessibility-tree')?.textContent.includes('Keep this review draft'));
    await page.goto(`${origin}/learn/?track=attention`);
    await page.locator('.xterm-helper-textarea').first().waitFor({state:"attached"});
    assert.equal(await page.locator('.lp-workspace').getAttribute('data-tmux'),"inside");
    assert.equal(await page.locator('.lp-exercises [data-exercise="attention"]').getAttribute('aria-pressed'),"true");
    await page.locator('.lp-exercises [data-exercise="workspace"]').click();
    assert.equal(new URL(page.url()).searchParams.get('track'),"workspace");
    await page.locator('.l-nav a[href*="#install"]').click();
    await page.locator('#install').waitFor();
    assert.match(page.url(),/#install$/);
    assert.deepEqual(errors,[]);
    console.log(`${label}: shared theme, entry paths, language continuity, links and viewport passed`);
    await context.close();
  }
} finally { await browser.close(); }
