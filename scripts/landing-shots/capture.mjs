// Regenerate the dashboard screenshots shown on the landing page.
//
//   MUXA_PLAYWRIGHT_PACKAGE=playwright-core node scripts/landing-shots/capture.mjs
//
// Serves crates/muxa/src/dashboard/web as-is, answers /api/* from the
// fictional fleet in fixtures.mjs (no daemon, no real data), captures each
// shot in light and dark, and writes WebP files next to the landing page.
// Needs Chrome (channel "chrome", or MUXA_TEST_CHROMIUM) and `cwebp`.

import { createRequire } from "node:module";
import { createServer } from "node:http";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import * as fx from "./fixtures.mjs";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.MUXA_PLAYWRIGHT_PACKAGE || "playwright");

const HERE = dirname(fileURLToPath(import.meta.url));
const WEB = resolve(HERE, "../../crates/muxa/src/dashboard/web");
const OUT = join(WEB, "landing");
const manifest = readFileSync(resolve(HERE, "../../Cargo.toml"), "utf8");
const version = manifest.match(/\[workspace\.package\][\s\S]*?^version = "([^"]+)"/m)?.[1];
if (!version) throw new Error("workspace version is missing from Cargo.toml");
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml", ".webp": "image/webp" };

function serve() {
  const now = Date.now();
  const api = {
    "/api/health": () => ({ ...fx.healthJson(), version }),
    "/api/access": fx.accessJson,
    "/api/agents": () => fx.agentsJson(now),
    "/api/panes": () => fx.panesJson(now),
    "/api/works": () => fx.worksJson(now),
    "/api/collaboration": () => fx.collaborationJson(now),
    "/api/timeline": () => fx.timelineJson(now),
    "/api/terminal-sessions": () => ({ sessions: [] }),
  };
  return createServer((req, res) => {
    const { pathname } = new URL(req.url, "http://x");
    if (pathname === "/api/events") {
      // Hold the stream open so the header reads "live".
      res.writeHead(200, { "Content-Type": "text/event-stream" });
      res.write(`event: snapshot\ndata: ${JSON.stringify(fx.agentsJson(now))}\n\n`);
      return;
    }
    if (api[pathname]) {
      res.writeHead(200, { "Content-Type": "application/json" });
      return res.end(JSON.stringify(api[pathname]()));
    }
    const file = pathname === "/" ? "index.html" : pathname.replace(/^\/static\//, "");
    try {
      res.writeHead(200, { "Content-Type": TYPES[extname(file)] || "application/octet-stream" });
      res.end(readFileSync(join(WEB, file)));
    } catch {
      res.writeHead(404).end();
    }
  });
}

// Each shot: viewport, what to open or scroll to, and what to capture.
const SHOTS = [
  { name: "board", viewport: { width: 1440, height: 900 }, prepare: async () => {}, target: null },
  {
    name: "collaboration",
    viewport: { width: 1440, height: 1100 },
    prepare: async (page) => {
      await page.locator("#collaboration-panel").scrollIntoViewIfNeeded();
      // One room (planner, impl, reviewer) reads as a conversation; all
      // nine participants make the sequence too wide to follow.
      await page.locator(".collaboration-room-chip").first().click();
      await page.waitForTimeout(400);
      await page.evaluate(() => { const s = document.querySelector("#collaboration-sequence"); s.scrollTop = 0; s.scrollLeft = 0; });
    },
    target: "#collaboration-panel",
  },
  {
    name: "agents",
    // Wide enough that every column, CONTROL included, fits.
    viewport: { width: 1760, height: 1100 },
    prepare: async (page) => {
      const panel = page.locator("#data-panel");
      if (await panel.evaluate((e) => e.classList.contains("collapsed"))) await panel.locator(".collapse-btn").click();
      await panel.scrollIntoViewIfNeeded();
    },
    target: "#data-panel",
  },
];

const server = serve();
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const origin = `http://127.0.0.1:${server.address().port}`;
const browser = await chromium.launch(process.env.MUXA_TEST_CHROMIUM ? { executablePath: process.env.MUXA_TEST_CHROMIUM } : { channel: "chrome" });
const tmp = mkdtempSync(join(tmpdir(), "muxa-shots-"));
try {
  for (const scheme of ["light", "dark"]) {
    for (const shot of SHOTS) {
      const context = await browser.newContext({ viewport: shot.viewport, deviceScaleFactor: 2, colorScheme: scheme, locale: "en-US" });
      // The dashboard remembers panel state per browser; start clean and expanded.
      await context.addInitScript(() => { try { localStorage.clear(); localStorage.setItem("muxa.token", "demo"); } catch {} });
      const page = await context.newPage();
      page.on("pageerror", (e) => { throw e; });
      await page.goto(origin);
      await page.waitForSelector(".board-work");
      await page.waitForTimeout(1200);
      await shot.prepare(page);
      await page.waitForTimeout(600);
      const png = join(tmp, `${shot.name}-${scheme}.png`);
      if (shot.target) await page.locator(shot.target).screenshot({ path: png });
      else await page.screenshot({ path: png });
      const webp = join(OUT, `${shot.name}-${scheme}.webp`);
      // 1600px wide is sharp on a 2x screen at the landing's display size.
      execFileSync("cwebp", ["-quiet", "-q", "78", "-resize", "1600", "0", png, "-o", webp]);
      console.log("wrote", webp);
      await context.close();
    }
  }
} finally {
  await browser.close();
  server.close();
  rmSync(tmp, { recursive: true, force: true });
}
