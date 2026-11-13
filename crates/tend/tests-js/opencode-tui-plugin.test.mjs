// Runs the opencode 2.x TUI plugin `tend init` installs against a fake TUI
// context, so its session binding and translation are checked where they
// happen. The recorded real-binary run is replayed on the Rust side
// (`adapters/testdata/opencode-2.2.7-tui.jsonl`).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const source = readFileSync(join(here, "../../tend-cli/src/init/files/opencode.rs"), "utf8");
const open = 'const TUI_BODY: &str = r#"';
const start = source.indexOf(open) + open.length;
const body = source.slice(start, source.indexOf('"#;', start));
const spawnImport = 'import { spawn } from "node:child_process";';
assert.ok(body.includes(spawnImport), "the plugin spawns tend through node:child_process");
const file = join(mkdtempSync(join(tmpdir(), "tend-opencode-tui-")), "tui.mjs");
writeFileSync(file, body.replace(spawnImport, "const spawn = (...args) => globalThis.__tendSpawn(...args);"));
const { default: plugin } = await import(pathToFileURL(file).href);

async function harness(answer = () => "") {
  const sent = [];
  const prompted = [];
  globalThis.__tendSpawn = (command, args) => {
    assert.equal(command, "tend");
    assert.deepEqual(args, ["hook", "opencode", "--event", "event"]);
    const handlers = {};
    const stdout = {};
    const child = {
      on: (name, fn) => {
        handlers[name] = fn;
        return child;
      },
      stdout: { on: (name, fn) => (stdout[name] = fn) },
      stdin: {
        on: () => {},
        end: (text) => {
          const payload = JSON.parse(text);
          sent.push(payload);
          setTimeout(() => {
            const out = answer(payload);
            if (out && stdout.data) stdout.data(out);
            if (handlers.close) handlers.close(0);
          }, 0);
        },
      },
    };
    return child;
  };
  let route = { type: "home" };
  let listener;
  const ctx = {
    client: { session: { prompt: async (input) => prompted.push(input) } },
    location: { directory: "/w" },
    ui: { router: { current: () => route } },
    data: {
      listen: (fn) => {
        listener = fn;
        return () => {};
      },
    },
  };
  assert.equal(plugin.id, "tend");
  await plugin.setup(ctx);
  let here = "/w";
  const emit = (type, data) => listener({ details: { type, data, location: { directory: here } } });
  const changeDirectory = (directory) => {
    here = directory;
    ctx.location = { directory };
  };
  const show = (sessionID) => {
    route = { type: "session", sessionID };
  };
  const prompt = (sessionID, text = "go") =>
    emit("session.inbox.enqueued", { sessionID, item: { type: "user", payload: { text } } });
  // The plugin forwards on a timer, one child at a time: wait until nothing
  // new has been sent for a while rather than for a fixed delay.
  const settle = async () => {
    let seen = -1;
    while (seen !== sent.length + prompted.length) {
      seen = sent.length + prompted.length;
      await new Promise((resolve) => setTimeout(resolve, 40));
    }
  };
  return { sent, prompted, emit, show, prompt, settle, changeDirectory };
}

const types = (sent) => sent.map((payload) => payload.type);

test("a prompt into the shown session binds the pane and starts the turn", async () => {
  const { sent, show, prompt, settle } = await harness();
  show("ses_A");
  prompt("ses_A", "  fix it \n");
  await settle();
  assert.deepEqual(types(sent), ["tend.session.active", "tend.prompt"]);
  assert.equal(sent[1].properties.text, "fix it");
  assert.equal(sent[1].tend.directory, "/w");
});

test("a prompt into a session this TUI does not show binds nothing", async () => {
  const { sent, emit, show, prompt, settle } = await harness();
  show("ses_A");
  // Another TUI on the shared server prompts its own session.
  prompt("ses_B");
  emit("session.execution.succeeded", { sessionID: "ses_B" });
  await settle();
  assert.deepEqual(sent, []);
});

test("only the bound session's events are forwarded", async () => {
  const { sent, emit, show, prompt, settle } = await harness();
  show("ses_A");
  prompt("ses_A");
  emit("session.tool.input.started", { sessionID: "ses_B", id: "t", name: "shell" });
  emit("session.tool.called", { sessionID: "ses_B", id: "t", input: { command: "ls" } });
  emit("session.execution.succeeded", { sessionID: "ses_child" });
  await settle();
  assert.deepEqual(types(sent), ["tend.session.active", "tend.prompt"]);
});

test("viewing another session moves nothing until it is prompted", async () => {
  const { sent, emit, show, prompt, settle } = await harness();
  show("ses_A");
  prompt("ses_A");
  show("ses_B");
  emit("session.execution.succeeded", { sessionID: "ses_A" });
  prompt("ses_B");
  await settle();
  assert.deepEqual(types(sent), [
    "tend.session.active",
    "tend.prompt",
    "session.idle",
    "tend.session.active",
    "tend.prompt",
  ]);
  assert.equal(sent[3].properties.sessionID, "ses_B");
});

test("a clean turn reports tools, the model and the last message's text", async () => {
  const { sent, emit, show, prompt, settle } = await harness();
  show("s");
  prompt("s");
  emit("session.execution.started", { sessionID: "s" });
  emit("session.step.started", { sessionID: "s", assistantMessageID: "m1", model: { id: "m-x", providerID: "p" } });
  emit("session.text.ended", { sessionID: "s", assistantMessageID: "m1", text: "Let me look." });
  emit("session.tool.input.started", { sessionID: "s", id: "t1", name: "shell" });
  emit("session.tool.called", { sessionID: "s", id: "t1", input: { command: "ls" } });
  emit("session.tool.success", { sessionID: "s", id: "t1" });
  emit("session.step.ended", { sessionID: "s", assistantMessageID: "m1", cost: 0.01 });
  emit("session.text.ended", { sessionID: "s", assistantMessageID: "m2", text: "One file." });
  emit("session.execution.succeeded", { sessionID: "s" });
  await settle();
  const tools = sent.filter((payload) => payload.type === "message.part.updated").map((payload) => payload.properties.part);
  assert.deepEqual(
    tools.map((part) => [part.tool, part.state.status, part.state.command]),
    [
      ["shell", "running", "ls"],
      ["shell", "completed", undefined],
    ],
  );
  const heartbeat = sent.find((payload) => payload.type === "message.updated").properties.info;
  assert.deepEqual([heartbeat.modelID, heartbeat.cost], ["m-x", 0.01]);
  const idle = sent.at(-1);
  assert.equal(idle.type, "session.idle");
  assert.equal(idle.tend.response, "One file.");
});

test("a failed execution reports the error and no answer", async () => {
  const { sent, emit, show, prompt, settle } = await harness();
  show("s");
  prompt("s");
  emit("session.execution.started", { sessionID: "s" });
  emit("session.text.ended", { sessionID: "s", assistantMessageID: "m1", text: "partial" });
  emit("session.execution.failed", { sessionID: "s", error: { type: "api", message: "overloaded" } });
  await settle();
  assert.deepEqual(types(sent).slice(2), ["tend.execution.started", "session.error", "session.idle"]);
  assert.equal(sent[3].properties.error.data.message, "overloaded");
  assert.equal(sent[4].tend.response, undefined);
});

test("an interrupted execution is idle with no answer", async () => {
  const { sent, emit, show, prompt, settle } = await harness();
  show("s");
  prompt("s");
  emit("session.text.ended", { sessionID: "s", assistantMessageID: "m1", text: "partial" });
  emit("session.execution.interrupted", { sessionID: "s", reason: "user" });
  await settle();
  assert.equal(sent.at(-1).type, "session.idle");
  assert.equal(sent.at(-1).tend.response, undefined);
});

test("permissions and questions keep 2.x's ids and wording", async () => {
  const { sent, emit, show, prompt, settle } = await harness();
  show("s");
  prompt("s");
  emit("permission.asked", { id: "per_1", sessionID: "s", action: "shell", resources: ["ls"], save: ["ls *"] });
  emit("permission.replied", { sessionID: "s", requestID: "per_1", reply: "reject" });
  emit("form.created", {
    form: {
      id: "frm_1",
      sessionID: "s",
      metadata: { kind: "question" },
      fields: [{ key: "q0", title: "Color", description: "Which color?" }],
    },
  });
  emit("form.cancelled", { id: "frm_1", sessionID: "s" });
  await settle();
  assert.deepEqual(types(sent).slice(2), ["permission.asked", "permission.replied", "question.asked", "question.rejected"]);
  assert.deepEqual(sent[2].properties, { sessionID: "s", id: "per_1", action: "shell", resources: ["ls"] });
  assert.equal(sent[3].properties.reply, "reject");
  assert.deepEqual(sent[4].properties.questions, [{ question: "Which color?", header: "Color" }]);
});

test("a deleted bound session unbinds the pane", async () => {
  const { sent, emit, show, prompt, settle } = await harness();
  show("s");
  prompt("s");
  emit("session.deleted", { sessionID: "s" });
  emit("session.execution.succeeded", { sessionID: "s" });
  await settle();
  assert.deepEqual(types(sent), ["tend.session.active", "tend.prompt", "session.deleted"]);
});

test("a tend that is not on PATH never throws into opencode", async () => {
  const { show, prompt, settle } = await harness();
  globalThis.__tendSpawn = () => {
    throw new Error("spawn tend ENOENT");
  };
  show("s");
  prompt("s");
  await settle();
});

// From the #512 review (Codex), each against the committed plugin.
test("switching into an already-running session never reports the previous session's text", async () => {
  const { sent, emit, show, prompt, settle } = await harness();
  show("A");
  prompt("A");
  emit("session.execution.started", { sessionID: "A" });
  emit("session.text.ended", { sessionID: "A", assistantMessageID: "mA", text: "A PRIVATE ANSWER" });
  emit("session.execution.started", { sessionID: "B" });
  show("B");
  prompt("B", "steer B");
  emit("session.execution.succeeded", { sessionID: "B" });
  await settle();
  assert.equal(sent.at(-1).type, "session.idle");
  assert.equal(sent.at(-1).tend.response, undefined);
});

test("an execution that starts after an idle is reported as working", async () => {
  const { sent, emit, show, prompt, settle } = await harness();
  show("s");
  prompt("s");
  emit("session.execution.started", { sessionID: "s" });
  prompt("s", "new intent");
  emit("session.execution.interrupted", { sessionID: "s", reason: "user" });
  emit("session.execution.started", { sessionID: "s" });
  await settle();
  assert.equal(sent.at(-1).type, "tend.execution.started");
});

test("viewing another project keeps the bound session's directory", async () => {
  const { sent, emit, show, prompt, settle, changeDirectory } = await harness();
  show("A");
  prompt("A");
  show("B");
  changeDirectory("/other-project");
  emit("session.execution.succeeded", { sessionID: "A" });
  await settle();
  assert.equal(sent.at(-1).tend.directory, "/w");
});

const offer = (payload) => (payload.type === "session.idle" ? JSON.stringify({ prompt: "Tend is offering req_1" }) : "");

test("a turn-boundary offer is queued into the bound session through opencode's API", async () => {
  const { prompted, emit, show, prompt, settle } = await harness(offer);
  show("s");
  prompt("s");
  emit("session.execution.succeeded", { sessionID: "s" });
  await settle();
  assert.deepEqual(prompted, [{ sessionID: "s", text: "Tend is offering req_1", delivery: "queue" }]);
});

test("an offer never lands after anyone prompted the session again", async () => {
  const { prompted, emit, show, prompt, settle } = await harness(offer);
  show("s");
  prompt("s");
  emit("session.execution.succeeded", { sessionID: "s" });
  // Another TUI on the shared server prompts the same session.
  show("other");
  prompt("s");
  await settle();
  assert.deepEqual(prompted, []);
});

test("only a clean turn end delivers, and only when tend offers", async () => {
  const { prompted, emit, show, prompt, settle } = await harness(offer);
  show("s");
  prompt("s");
  // Esc: the user stopped the turn; an offer must not start another.
  emit("session.execution.interrupted", { sessionID: "s", reason: "user" });
  prompt("s");
  emit("session.execution.failed", { sessionID: "s", error: { type: "api", message: "x" } });
  await settle();
  assert.deepEqual(prompted, []);
  const quiet = await harness(() => "");
  quiet.show("s");
  quiet.prompt("s");
  quiet.emit("session.execution.succeeded", { sessionID: "s" });
  await quiet.settle();
  assert.deepEqual(quiet.prompted, []);
});

test("only a clean execution end is flagged as a boundary for the hook", async () => {
  const { sent, emit, show, prompt, settle } = await harness();
  show("s");
  prompt("s");
  emit("session.execution.started", { sessionID: "s" });
  emit("session.execution.interrupted", { sessionID: "s", reason: "user" });
  prompt("s");
  emit("session.execution.started", { sessionID: "s" });
  emit("session.execution.failed", { sessionID: "s", error: { type: "api", message: "x" } });
  prompt("s");
  emit("session.execution.started", { sessionID: "s" });
  emit("session.execution.succeeded", { sessionID: "s" });
  await settle();
  const flags = sent.filter((p) => p.type === "session.idle").map((p) => p.tend.boundary === true);
  assert.deepEqual(flags, [false, false, true]);
});
