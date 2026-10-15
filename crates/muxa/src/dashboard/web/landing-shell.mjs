// Local demo shell and readline state. Never evaluates JavaScript or OS commands.
export function createLineEditor() {
  return { text: "", cursor: 0, history: [], historyIndex: -1, draft: "" };
}

export function editLine(editor, data, completions = []) {
  const next = { ...editor };
  const chars = Array.from(next.text);
  if (data === "\r" || data === "\n") {
    const command = next.text;
    return { editor: { ...createLineEditor(), history: command.trim() ? [...editor.history, command].slice(-100) : editor.history }, command };
  }
  if (data === "\x03") return { editor: { ...createLineEditor(), history: editor.history }, interrupted: true };
  if (data === "\x01" || data === "\x1b[H" || data === "\x1b[1~") next.cursor = 0;
  else if (data === "\x05" || data === "\x1b[F" || data === "\x1b[4~") next.cursor = chars.length;
  else if (data === "\x1b[D") next.cursor = Math.max(0, next.cursor - 1);
  else if (data === "\x1b[C") next.cursor = Math.min(chars.length, next.cursor + 1);
  else if (data === "\x7f" || data === "\b") { if (next.cursor > 0) chars.splice(--next.cursor, 1); next.text = chars.join(""); }
  else if (data === "\x1b[3~") { chars.splice(next.cursor, 1); next.text = chars.join(""); }
  else if (data === "\x15") { next.text = chars.slice(next.cursor).join(""); next.cursor = 0; }
  else if (data === "\x0b") next.text = chars.slice(0, next.cursor).join("");
  else if (data === "\x17") {
    const before = chars.slice(0, next.cursor).join("");
    const shortened = before.replace(/\S+\s*$/, "");
    next.text = shortened + chars.slice(next.cursor).join(""); next.cursor = Array.from(shortened).length;
  } else if (data === "\x1b[A" || data === "\x1b[B") {
    if (!next.history.length) return { editor: next };
    if (next.historyIndex === -1) next.draft = next.text;
    const up = data === "\x1b[A";
    next.historyIndex = up ? next.historyIndex === -1 ? next.history.length - 1 : Math.max(0, next.historyIndex - 1) : next.historyIndex === -1 ? -1 : next.historyIndex + 1;
    if (next.historyIndex >= next.history.length) next.historyIndex = -1;
    next.text = next.historyIndex === -1 ? next.draft : next.history[next.historyIndex];
    next.cursor = Array.from(next.text).length;
  } else if (data === "\t") {
    const prefix = chars.slice(0, next.cursor).join("");
    const matches = completions.filter(value => value.startsWith(prefix));
    if (matches.length === 1) { next.text = matches[0]; next.cursor = Array.from(next.text).length; }
    else return { editor: next, suggestions: matches };
  } else if (!data.startsWith("\x1b")) {
    const inserted = Array.from(data.replace(/[\x00-\x1f\x7f]/g, ""));
    chars.splice(next.cursor, 0, ...inserted); next.text = chars.join(""); next.cursor += inserted.length;
  }
  return { editor: next };
}

const FILES = {
  "/home/demo/acme/README.md": "# Acme API\nRetry failed requests with exponential backoff.\nRun tests with npm test.\n",
  "/home/demo/acme/package.json": '{"name":"acme-api","scripts":{"test":"vitest run"}}\n',
  "/home/demo/acme/src/retry.ts": "export async function retry(fn, attempts = 3) {\n  for (let n = 0; n < attempts; n++) {\n    try { return await fn(); } catch (error) {\n      if (n === attempts - 1) throw error;\n      await new Promise(r => setTimeout(r, 1000 * 2 ** n));\n    }\n  }\n}\n",
  "/home/demo/acme/tests/retry.test.ts": 'describe("retry", () => {\n  it("retries a failed request", async () => { /* … */ });\n});\n',
};
export const SHELL_COMPLETIONS = ["tmux attach -t acme", "pwd", "ls", "ls -la", "cd src", "cd tests", "cd ..", "cat README.md", "cat src/retry.ts", "echo ", "git status", "git diff", "npm test", "clear", "help", "history", "muxa watch", "muxa attend", "muxa status"];
export function createDemoShell() { return { cwd: "/home/demo/acme", history: [] }; }
function resolvePath(cwd, path) {
  const parts = (path.startsWith("/") ? path : `${cwd}/${path}`).split("/");
  const resolved = [];
  for (const part of parts) { if (part === "..") resolved.pop(); else if (part && part !== ".") resolved.push(part); }
  return "/" + resolved.join("/");
}
function words(line) {
  const result = []; let word = "", quote = null, escaped = false, active = false;
  for (const char of line) {
    if (escaped) { word += char; escaped = false; active = true; }
    else if (char === "\\" && quote !== "'") { escaped = true; active = true; }
    else if (quote) { if (char === quote) quote = null; else word += char; }
    else if (char === '"' || char === "'") { quote = char; active = true; }
    else if (/\s/.test(char)) { if (active) result.push(word); word = ""; active = false; }
    else { word += char; active = true; }
  }
  if (quote || escaped) throw new Error("unclosed quote or escape");
  if (active) result.push(word);
  return result;
}
export function runDemoCommand(shell, command, { status = "claude  %12  waiting_input\ncodex   %13  idle\ngemini  %14  working" } = {}) {
  const next = { ...shell, history: command.trim() ? [...shell.history, command].slice(-100) : shell.history };
  let args; try { args = words(command); } catch (error) { return { shell: next, output: `zsh: ${error.message}` }; }
  if (!args.length) return { shell: next, output: "" };
  const [program, ...rest] = args;
  const result = output => ({ shell: next, output });
  if (program === "tmux") {
    if (["attach", "attach-session", "a"].includes(rest[0])) {
      const target = rest.indexOf("-t");
      if (target >= 0 && rest[target + 1] !== "acme") return result(`can't find session: ${rest[target + 1] || ""}`);
      if (rest.some((arg,index) => index > 0 && arg !== "-t" && index !== target + 1)) return result("Demo tmux: try tmux attach -t acme");
      return { ...result(""), action: "tmux-attach" };
    }
    if (["detach", "detach-client"].includes(rest[0])) return { ...result(""), action: "tmux-detach" };
    return result("A practice session is ready: tmux attach -t acme");
  }
  if (program === "pwd") return result(next.cwd);
  if (program === "clear") return { ...result(""), clear: true };
  if (program === "echo") return result(rest.join(" "));
  if (program === "whoami") return result("demo");
  if (program === "history") return result(next.history.map((line, i) => `${i + 1}  ${line}`).join("\n"));
  if (program === "help") return result("Demo shell: pwd, ls, cd, cat, echo, whoami, history, clear\ntmux attach -t acme: enter the prepared practice session\ngit status / git diff / npm test\nmuxa watch / muxa attend / muxa status\nCtrl+b, then s: open watch. q: return to shell.");
  if (program === "cd") {
    const target = resolvePath(next.cwd, rest[0] || "/home/demo/acme");
    if (Object.keys(FILES).some(file => file.startsWith(target + "/"))) { next.cwd = target; return result(""); }
    return result(`cd: no such directory: ${rest[0]}`);
  }
  if (program === "ls") {
    const path = rest.find(arg => !arg.startsWith("-"));
    const target = path ? resolvePath(next.cwd, path) : next.cwd;
    if (FILES[target]) return result(target.split("/").pop());
    const children = new Set();
    for (const file of Object.keys(FILES)) if (file.startsWith(target + "/")) {
      const tail = file.slice(target.length + 1); children.add(tail.includes("/") ? tail.split("/")[0] + "/" : tail);
    }
    return result(children.size ? [...children].sort().join("  ") : `ls: no such file or directory: ${path || "."}`);
  }
  if (program === "cat") return result(rest.map(path => FILES[resolvePath(next.cwd, path)] ?? `cat: ${path}: no such file`).join(""));
  if (program === "git" && rest[0] === "status") return result("On branch feat/retry\nChanges not staged for commit:\n  modified: src/retry.ts\n  modified: tests/retry.test.ts");
  if (program === "git" && rest[0] === "diff") return result("diff --git a/src/retry.ts b/src/retry.ts\n+ await new Promise(r => setTimeout(r, 1000 * 2 ** n));");
  if (program === "npm" && rest[0] === "test") return result("PASS tests/retry.test.ts\nTest Files  1 passed\nTests       12 passed");
  if (program === "muxa") {
    if (rest[0] === "status") return result("AGENT   PANE STATE\n" + status);
    if (["watch", "attend"].includes(rest[0])) return { ...result(""), action: rest[0] };
    return result("Usage: muxa watch | attend | status");
  }
  return result(`zsh: command not found: ${program}`);
}
