// Runs the opencode plugin `tend init` installs against a fake opencode, so
// the filtering the Rust adapter relies on is checked where it happens.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const source = readFileSync(join(here, "../../tend-cli/src/init/files/opencode.rs"), "utf8");
const body = source.slice(source.indexOf('const BODY: &str = r#"') + 'const BODY: &str = r#"'.length, source.indexOf('"#;'));
const file = join(mkdtempSync(join(tmpdir(), "tend-opencode-")), "tend.mjs");
writeFileSync(file, body);
const { TendPlugin } = await import(pathToFileURL(file).href);

async function harness() {
  const sent = [];
  const $ = () => ({ stdin: (text) => ({ quiet: async () => { sent.push(JSON.parse(text)); } }) });
  const hooks = await TendPlugin({ $, client: { app: { log: async () => {} } }, directory: "/w" });
  const emit = (type, properties) => hooks.event({ event: { type, properties } });
  const settle = () => new Promise((resolve) => setTimeout(resolve, 20));
  return { sent, hooks, emit, settle };
}

const tool = (sessionID, callID, status) => ({
  part: { id: callID, sessionID, messageID: "m", type: "tool", tool: "bash", callID, state: { status, input: { command: "ls" } } },
});

test("a session announces itself, then its prompt without synthetic parts", async () => {
  const { sent, hooks, emit, settle } = await harness();
  await emit("session.created", { info: { id: "ses_1", directory: "/w" } });
  await hooks["chat.message"]({ sessionID: "ses_1" }, { message: {}, parts: [
    { type: "text", text: "fix it" },
    { type: "text", text: "<file contents>", synthetic: true },
  ] });
  await settle();
  assert.deepEqual(sent.map((p) => p.type), ["tend.session.active", "session.created", "tend.prompt"]);
  assert.equal(sent[2].properties.text, "fix it");
});

test("child sessions never reach tend", async () => {
  const { sent, emit, settle } = await harness();
  await emit("session.created", { info: { id: "ses_1" } });
  await emit("session.created", { info: { id: "ses_kid", parentID: "ses_1" } });
  await emit("message.part.updated", tool("ses_kid", "c9", "running"));
  await emit("session.idle", { sessionID: "ses_kid" });
  await settle();
  assert.ok(sent.every((p) => p.properties.sessionID !== "ses_kid"), JSON.stringify(sent));
});

test("tool parts are forwarded once per status change, never pending", async () => {
  const { sent, emit, settle } = await harness();
  for (const status of ["pending", "running", "running", "completed"]) {
    await emit("message.part.updated", tool("ses_1", "c1", status));
  }
  await settle();
  const tools = sent.filter((p) => p.type === "message.part.updated");
  assert.deepEqual(tools.map((p) => p.properties.part.state.status), ["running", "completed"]);
  assert.equal(tools[0].properties.part.state.command, "ls");
});

test("idle carries the last assistant message's text", async () => {
  const { sent, emit, settle } = await harness();
  await emit("message.updated", { info: { id: "m1", sessionID: "ses_1", role: "assistant", time: { created: 1 } } });
  await emit("message.part.updated", { part: { id: "p1", sessionID: "ses_1", messageID: "m1", type: "text", text: "Done." } });
  await emit("message.updated", { info: { id: "m1", sessionID: "ses_1", role: "assistant", time: { created: 1, completed: 2 }, modelID: "x", cost: 0.1 } });
  await emit("message.updated", { info: { id: "m1", sessionID: "ses_1", role: "assistant", time: { created: 1, completed: 2 }, modelID: "x", cost: 0.1 } });
  await emit("session.idle", { sessionID: "ses_1" });
  await settle();
  assert.equal(sent.filter((p) => p.type === "message.updated").length, 1, "completion reported once");
  assert.equal(sent.at(-1).type, "session.idle");
  assert.equal(sent.at(-1).tend.response, "Done.");
});

test("busy and idle statuses are left to prompts and session.idle", async () => {
  const { sent, emit, settle } = await harness();
  await emit("session.status", { sessionID: "ses_1", status: { type: "busy" } });
  await emit("session.status", { sessionID: "ses_1", status: { type: "retry", attempt: 1, message: "rate limited", next: 1 } });
  await settle();
  assert.deepEqual(sent.map((p) => p.type), ["tend.session.active", "session.status"]);
});
