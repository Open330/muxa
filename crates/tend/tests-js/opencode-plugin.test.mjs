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
// The plugin spawns `tend` itself; the harness stands in for node's spawn
// and records each payload written to the child's stdin.
const spawnImport = 'import { spawn } from "node:child_process";';
assert.ok(body.includes(spawnImport), "the plugin spawns tend through node:child_process");
writeFileSync(file, body.replace(spawnImport, "const spawn = (...args) => globalThis.__tendSpawn(...args);"));
const plugin = await import(pathToFileURL(file).href);
const { TendPlugin } = plugin;

function fakeSpawn(sent) {
  return (command, args) => {
    assert.equal(command, "tend");
    assert.deepEqual(args, ["hook", "opencode", "--event", "event"]);
    const handlers = {};
    const child = {
      on: (name, fn) => {
        handlers[name] = fn;
        return child;
      },
      stdin: {
        on: () => {},
        end: (text) => {
          sent.push(JSON.parse(text));
          setTimeout(() => handlers.close && handlers.close(0), 0);
        },
      },
    };
    return child;
  };
}

test("one file loads under both plugin APIs", () => {
  // 1.x prefers `default.server`; 2.x requires `default.{id, setup}`.
  assert.equal(plugin.default.server, TendPlugin);
  assert.equal(plugin.default.id, "tend");
  assert.equal(typeof plugin.default.setup, "function");
  assert.equal(plugin.default.setup(), undefined);
});

async function harness() {
  const sent = [];
  globalThis.__tendSpawn = fakeSpawn(sent);
  const hooks = await TendPlugin({ client: { app: { log: async () => {} } }, directory: "/w" });
  const emit = (type, properties) => hooks.event({ event: { type, properties } });
  const settle = () => new Promise((resolve) => setTimeout(resolve, 20));
  return { sent, hooks, emit, settle };
}

const bind = (hooks, sessionID) =>
  hooks["chat.message"]({ sessionID }, { message: {}, parts: [{ type: "text", text: "go" }] });

const tool = (sessionID, callID, status) => ({
  part: { id: callID, sessionID, messageID: "m", type: "tool", tool: "bash", callID, state: { status, input: { command: "ls" } } },
});

test("a prompt binds its session and drops synthetic parts", async () => {
  const { sent, hooks, emit, settle } = await harness();
  await emit("session.created", { info: { id: "ses_1", directory: "/w" } });
  await hooks["chat.message"]({ sessionID: "ses_1" }, { message: {}, parts: [
    { type: "text", text: "fix it" },
    { type: "text", text: "<file contents>", synthetic: true },
  ] });
  await settle();
  // Creation alone binds nothing: another client may have created it.
  assert.deepEqual(sent.map((p) => p.type), ["tend.session.active", "tend.prompt"]);
  assert.equal(sent[1].properties.text, "fix it");
});

test("child sessions never reach tend", async () => {
  const { sent, hooks, emit, settle } = await harness();
  await emit("session.created", { info: { id: "ses_1" } });
  await bind(hooks, "ses_1");
  await emit("session.created", { info: { id: "ses_kid", parentID: "ses_1" } });
  await emit("message.part.updated", tool("ses_kid", "c9", "running"));
  await emit("session.idle", { sessionID: "ses_kid" });
  await settle();
  assert.ok(sent.every((p) => p.properties.sessionID !== "ses_kid"), JSON.stringify(sent));
});

test("tool parts are forwarded once per status change, never pending", async () => {
  const { sent, hooks, emit, settle } = await harness();
  await bind(hooks, "ses_1");
  for (const status of ["pending", "running", "running", "completed"]) {
    await emit("message.part.updated", tool("ses_1", "c1", status));
  }
  await settle();
  const tools = sent.filter((p) => p.type === "message.part.updated");
  assert.deepEqual(tools.map((p) => p.properties.part.state.status), ["running", "completed"]);
  assert.equal(tools[0].properties.part.state.command, "ls");
});

test("idle carries the last assistant message's text", async () => {
  const { sent, hooks, emit, settle } = await harness();
  await bind(hooks, "ses_1");
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
  const { sent, hooks, emit, settle } = await harness();
  await bind(hooks, "ses_1");
  await emit("session.status", { sessionID: "ses_1", status: { type: "busy" } });
  await emit("session.status", { sessionID: "ses_1", status: { type: "retry", attempt: 1, message: "rate limited", next: 1 } });
  await settle();
  assert.deepEqual(sent.map((p) => p.type).slice(2), ["session.status"]);
});

const prompt = (hooks, sessionID, text) =>
  hooks["chat.message"]({ sessionID }, { message: {}, parts: [{ type: "text", text }] });
const assistant = async (emit, sessionID, id, text) => {
  await emit("message.updated", { info: { id, sessionID, role: "assistant", time: { created: 1 } } });
  await emit("message.part.updated", { part: { id: `${id}-p`, sessionID, messageID: id, type: "text", text } });
};

test("only a prompt that changes the owner rebinds; other sessions' events never do", async () => {
  const { sent, hooks, emit, settle } = await harness();
  await prompt(hooks, "A", "first");
  await emit("permission.asked", { id: "p", sessionID: "A", permission: "bash", patterns: [] });
  await prompt(hooks, "B", "second");
  await emit("permission.replied", { sessionID: "A", requestID: "p", reply: "once" });
  await prompt(hooks, "A", "again");
  await prompt(hooks, "A", "and again");
  await settle();
  const binds = sent.filter((p) => p.type === "tend.session.active").map((p) => p.properties.sessionID);
  assert.deepEqual(binds, ["A", "B", "A"]);
});

test("a reply from an earlier turn never reaches a later idle", async () => {
  const { sent, hooks, emit, settle } = await harness();
  await prompt(hooks, "s", "one");
  await assistant(emit, "s", "m1", "OLD ANSWER");
  await emit("session.idle", { sessionID: "s" });
  await prompt(hooks, "s", "two");
  await emit("session.idle", { sessionID: "s" });
  await settle();
  const idles = sent.filter((p) => p.type === "session.idle").map((p) => p.tend.response);
  assert.deepEqual(idles, ["OLD ANSWER", undefined]);
});

test("a failed or aborted turn's partial text is not reported", async () => {
  const { sent, hooks, emit, settle } = await harness();
  for (const name of ["APIError", "MessageAbortedError"]) {
    await prompt(hooks, "s", "go");
    await assistant(emit, "s", `m-${name}`, "PARTIAL");
    await emit("session.error", { sessionID: "s", error: { name, data: { message: "x" } } });
    await emit("session.idle", { sessionID: "s" });
  }
  await prompt(hooks, "s", "go");
  await assistant(emit, "s", "m-ok", "FULL");
  await emit("session.idle", { sessionID: "s" });
  await settle();
  const idles = sent.filter((p) => p.type === "session.idle").map((p) => p.tend.response);
  assert.deepEqual(idles, [undefined, undefined, "FULL"]);
});

test("a session the pane no longer drives cannot revive its row", async () => {
  const { sent, hooks, emit, settle } = await harness();
  await prompt(hooks, "A", "one");
  await prompt(hooks, "B", "two");
  await prompt(hooks, "A", "three");
  await settle();
  const before = sent.length;
  await emit("session.idle", { sessionID: "B" });
  await emit("message.part.updated", tool("B", "cB", "running"));
  await emit("permission.asked", { id: "p", sessionID: "B", permission: "bash", patterns: [] });
  await emit("session.error", { sessionID: "B", error: { name: "APIError", data: { message: "x" } } });
  // Nothing about an unbound session reaches tend either.
  await emit("session.idle", { sessionID: "C" });
  await emit("session.idle", { sessionID: "A" });
  await settle();
  const after = sent.slice(before);
  assert.deepEqual(after.map((p) => [p.type, p.properties.sessionID]), [["session.idle", "A"]]);
});
