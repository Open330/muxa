// First-paint regression: slow assets, failed modules and JavaScript disabled.
// MUXA_PLAYWRIGHT_PACKAGE=/path/to/playwright-core node scripts/landing-shots/loading-smoke.mjs http://127.0.0.1:8768
import assert from "node:assert/strict";
import {createRequire} from "node:module";
const {chromium}=createRequire(import.meta.url)(process.env.MUXA_PLAYWRIGHT_PACKAGE||"playwright");
const origin=process.argv[2];
if(!origin)throw new Error("Pass the site's preview URL");
const browser=await chromium.launch({executablePath:process.env.MUXA_TEST_CHROMIUM||"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",headless:true});
try{
  for(const path of ["/","/learn/"])for(const colorScheme of ["light","dark"]){
    const context=await browser.newContext({locale:"ko-KR",colorScheme});
    const page=await context.newPage(),requests=[],errors=[];
    let release;
    const gate=new Promise(resolve=>release=resolve);
    page.on("request",r=>requests.push(r.url()));
    page.on("pageerror",e=>errors.push(e.message));
    await page.route(/\.(mjs|css)(\?|$)/,async route=>{await gate;await route.continue();});
    await page.goto(`${origin}${path}`,{waitUntil:"commit"});
    await page.locator('[data-page-fallback]').waitFor({state:"attached"});
    assert.equal(await page.locator('html').getAttribute('data-page-loading'),"");
    assert.equal(await page.locator('#landing').evaluate(el=>getComputedStyle(el).visibility),"hidden","fallback must not flash while CSS/modules load");
    const initialBackground=await page.locator('html').evaluate(el=>getComputedStyle(el).backgroundColor);
    assert.equal(initialBackground,colorScheme==="dark"?"rgb(17, 19, 21)":"rgb(247, 247, 245)");
    release();
    await page.waitForFunction(()=>!document.documentElement.hasAttribute('data-page-loading'));
    assert.equal(await page.locator('#landing').evaluate(el=>getComputedStyle(el).visibility),"visible");
    assert.equal(await page.locator('[data-page-fallback]').count(),0);
    assert.equal(await page.locator('html').getAttribute('lang'),"ko");
    assert.equal(await page.locator('html').evaluate(el=>getComputedStyle(el).backgroundColor),initialBackground);
    if(path==="/learn/"){
      assert.equal(await page.locator('.xterm').count(),10);
      assert.match(await page.evaluate(()=>document.activeElement.className),/xterm-helper-textarea/);
      await page.keyboard.type("echo ready");
      await page.waitForFunction(()=>document.querySelector('.lp-pane.is-active .xterm-accessibility-tree')?.textContent.includes('echo ready'));
    }else assert.equal(requests.some(url=>url.includes('landing-terminal/')),false,"landing does not load terminal-only CSS");
    assert.deepEqual(errors,[]);
    await context.close();
    console.log(`${path} ${colorScheme}: slow assets preserve background and show the localized page only when ready`);
  }
  for(const path of ["/","/learn/"]){
    const noJS=await browser.newContext({javaScriptEnabled:false}),page=await noJS.newPage();
    await page.goto(`${origin}${path}`);
    assert.equal(await page.locator('[data-page-fallback]').isVisible(),true);
    await noJS.close();
    const failed=await browser.newContext(),failedPage=await failed.newPage();
    await failedPage.route('**/*.mjs',route=>route.abort());
    await failedPage.goto(`${origin}${path}`);
    await failedPage.waitForFunction(()=>!document.documentElement.hasAttribute('data-page-loading'));
    assert.equal(await failedPage.locator('[data-page-fallback]').isVisible(),true);
    await failed.close();
    console.log(`${path}: fallback remains available when JavaScript is disabled or modules fail`);
    if(path==="/learn/"){
      const terminalFailure=await browser.newContext(),terminalPage=await terminalFailure.newPage();
      await terminalPage.route('**/landing-terminal/*.mjs',route=>route.abort());
      await terminalPage.goto(`${origin}${path}`);
      await terminalPage.waitForFunction(()=>!document.documentElement.hasAttribute('data-page-loading'));
      assert.equal(await terminalPage.locator('[data-page-fallback]').isVisible(),true);
      await terminalFailure.close();
      console.log(`${path}: terminal dependency failure restores the readable guide`);
    }
  }
}finally{await browser.close();}
