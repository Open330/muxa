import assert from "node:assert/strict";
import test from "node:test";
import { createLineEditor, editLine, createDemoShell, runDemoCommand, SHELL_COMPLETIONS } from "../src/dashboard/web/landing-shell.mjs";

test("readline edits inside a command and deletes Unicode by character", () => {
  let editor = editLine(createLineEditor(), "echo 안녕").editor;
  editor = editLine(editor, "\x7f").editor;
  assert.equal(editor.text, "echo 안");
  editor = editLine(editor, "\x1b[D").editor;
  editor = editLine(editor, "좋은 ").editor;
  assert.equal(editor.text, "echo 좋은 안");
  editor = editLine(editor, "\x01").editor;
  editor = editLine(editor, "\x1b[3~").editor;
  assert.equal(editor.text, "cho 좋은 안");
});

test("history traversal preserves the unfinished draft", () => {
  let editor = editLine(createLineEditor(), "pwd").editor;
  editor = editLine(editor, "\r").editor;
  editor = editLine(editor, "echo draft").editor;
  editor = editLine(editor, "\x1b[A").editor;
  assert.equal(editor.text, "pwd");
  editor = editLine(editor, "\x1b[B").editor;
  assert.equal(editor.text, "echo draft");
});

test("tab completes a unique command and lists ambiguous commands", () => {
  let editor = editLine(createLineEditor(), "muxa att").editor;
  assert.equal(editLine(editor, "\t", SHELL_COMPLETIONS).editor.text, "muxa attend");
  editor = editLine(createLineEditor(), "muxa ").editor;
  assert.deepEqual(editLine(editor, "\t", SHELL_COMPLETIONS).suggestions, ["muxa watch", "muxa attend", "muxa status"]);
});

test("control editing and interrupts retain history", () => {
  let editor = editLine(createLineEditor(), "echo words").editor;
  editor = editLine(editor, "\x17").editor;
  assert.equal(editor.text, "echo ");
  editor = editLine(editor, "\x15").editor;
  assert.equal(editor.text, "");
  assert.equal(editLine(editor, "\x03").interrupted, true);
});

test("demo filesystem resolves relative directories and missing files", () => {
  let shell = createDemoShell();
  shell = runDemoCommand(shell, "cd src").shell;
  assert.equal(runDemoCommand(shell, "pwd").output, "/home/demo/acme/src");
  assert.match(runDemoCommand(shell, "cat retry.ts").output, /export async function retry/);
  assert.equal(runDemoCommand(shell, "ls").output, "retry.ts");
  const missing = runDemoCommand(shell, "cd nowhere");
  assert.equal(missing.shell.cwd, shell.cwd);
  assert.match(missing.output, /no such directory/);
  assert.match(runDemoCommand(shell, "cat missing").output, /no such file/);
});

test("quoted text stays data and unknown commands never execute", () => {
  const shell = createDemoShell();
  assert.equal(runDemoCommand(shell, 'echo "hello world"').output, "hello world");
  assert.equal(runDemoCommand(shell, 'echo "$(rm -rf /)"').output, "$(rm -rf /)");
  assert.match(runDemoCommand(shell, "rm -rf /").output, /command not found/);
  assert.match(runDemoCommand(shell, 'echo "').output, /unclosed quote/);
  assert.equal(runDemoCommand(shell, "muxa attend").action, "attend");
  assert.equal(runDemoCommand(shell, "muxa watch").action, "watch");
});


test("the local shell can attach to the prepared tmux session without executing OS commands",()=>{
  assert.equal(runDemoCommand(createDemoShell(),"tmux attach -t acme").action,"tmux-attach");
  assert.equal(runDemoCommand(createDemoShell(),"tmux a").action,"tmux-attach");
  assert.match(runDemoCommand(createDemoShell(),"tmux attach -t missing").output,/can't find session/);
  assert.equal(runDemoCommand(createDemoShell(),"tmux detach").action,"tmux-detach");
});
