const id = location.pathname.split("/").pop();
const status = document.querySelector("#status");
const output = document.querySelector("#output");
const login = document.querySelector("#login");
const logout = document.querySelector("#logout");
const logoutAll = document.querySelector("#logout-all");
const form = document.querySelector("#prompt-form");
const selector = document.querySelector("#pane");
let selectedPane;
let isWindow = false;
const input = document.querySelector("#prompt");
const send = document.querySelector("#send");
const commandStatus = document.querySelector("#command-status");
const newCommand = document.querySelector("#new-command");
let stopped = false;
let inFlight = false;
let sending = false;
let connected = false;
let writable = false;
let failures = 0;
let generation = 0;
let pending;
let timer;
const loginFailed = location.hash === "#login-error";
if (loginFailed) history.replaceState(null, "", location.pathname);
login.href = `/share/auth/login?share=${encodeURIComponent(id)}`;

async function request(path, options = {}) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 15000);
  try {
    const response = await fetch(path, { ...options, signal: controller.signal, credentials: "same-origin", cache: "no-store", headers: { "X-Muxa-Share": "1", "Content-Type": "application/json" } });
    let data;
    try { data = response.status === 204 ? {} : await response.json(); } catch { data = {}; }
    if (!response.ok) { const error = new Error(data.error || `Request failed (${response.status})`); error.status = response.status; throw error; }
    return data;
  } finally { clearTimeout(timeout); }
}
function updateSend() { selector.disabled = sending || Boolean(pending); send.disabled = stopped || !connected || !writable || sending; send.textContent = sending ? "Sending…" : pending ? "Check / retry delivery" : "Send command"; }
function deny(error) {
  connected = false;
  if ([401, 403, 404, 410].includes(error.status)) {
    stopped = true; generation++; clearTimeout(timer); output.textContent = ""; form.hidden = true; document.querySelector("#pane-selector").hidden = true;
    login.hidden = error.status !== 401; logout.hidden = error.status === 401; logoutAll.hidden = logout.hidden;
    document.querySelector("#identity").textContent = "";
    status.textContent = loginFailed && error.status === 401 ? "Sign-in failed or was cancelled. Try again with the invited account. If it continues, ask the owner to check the login settings." : error.message;
  } else {
    status.textContent = `Connection interrupted. Displayed output may be stale. Reconnecting… ${error.status ? error.message : ""}`;
  }
  updateSend();
}
async function refresh() {
  if (stopped || inFlight || document.hidden) return;
  inFlight = true;
  const current = generation;
  try {
    const share = await request(`/share/api/${encodeURIComponent(id)}${selectedPane ? `?pane=${encodeURIComponent(selectedPane)}` : ""}`);
    if (stopped || current !== generation) return;
    connected = true; failures = 0; writable = share.permission === "prompt" && share.pane_available !== false;
    isWindow = Boolean(share.window); selectedPane = share.pane;
    document.querySelector("#share-title").textContent = isWindow ? "Shared window" : "Shared pane";
    document.querySelector("#pane-selector").hidden = !isWindow;
    if (JSON.stringify([...selector.options].map(option => option.value)) !== JSON.stringify(share.panes)) {
      selector.replaceChildren(...share.panes.map(pane => { const option = document.createElement("option"); option.value = pane; option.textContent = pane; return option; }));
    }
    selector.value = selectedPane;
    login.hidden = true; logout.hidden = false; logoutAll.hidden = false;
    document.querySelector("#identity").textContent = `${share.pane} · ${share.email} · ${writable ? "View and send commands" : "View only"} · expires ${new Date(share.expires_at * 1000).toLocaleString()}`;
    const atBottom = output.scrollHeight - output.scrollTop - output.clientHeight < 40;
    if (output.textContent !== share.output) { output.textContent = share.output; if (atBottom) output.scrollTop = output.scrollHeight; }
    if (!share.pane_available && pending) { pending = undefined; newCommand.hidden = true; }
    form.hidden = !writable;
    status.textContent = share.pane_available === false ? `${share.notice}. Select another pane.` : `Connected · updated ${new Date().toLocaleTimeString()}`;
    updateSend();
  } catch (error) { if (current === generation && !stopped) { failures++; deny(error); } }
  finally { inFlight = false; if (!stopped) timer = setTimeout(refresh, Math.min(30000, 2000 * 2 ** Math.min(failures, 4))); }
}
form.addEventListener("submit", async event => {
  event.preventDefault(); if (sending || stopped || !connected || !writable) return;
  const text = input.value;
  if (new TextEncoder().encode(text).length > 16384) { commandStatus.textContent = "Command is too long (maximum 16 KiB)."; return; }
  if (pending && pending.text !== text) { commandStatus.textContent = "Check the previous command's output, then choose Start a new command."; return; }
  pending ||= { text, request_id: crypto.randomUUID(), pane: selectedPane };
  sending = true; input.disabled = true; newCommand.disabled = true; updateSend();
  commandStatus.textContent = "Sending…";
  try {
    await request(`/share/api/${encodeURIComponent(id)}/prompt`, { method: "POST", body: JSON.stringify(pending) });
    if (stopped) return;
    input.value = ""; pending = undefined; newCommand.hidden = true; commandStatus.textContent = "Command submitted.";
  } catch (error) {
    commandStatus.textContent = error.status ? error.message : "Delivery is uncertain. Check the output. Retrying this command uses the same request ID to prevent duplicate execution.";
    if ([400, 422].includes(error.status)) pending = undefined;
    if (error.status === 410 && isWindow) { pending = undefined; connected = false; output.textContent = ""; clearTimeout(timer); refresh(); }
    else if ([401, 403, 404, 410].includes(error.status)) deny(error);
    newCommand.hidden = !pending;
  } finally { sending = false; input.disabled = false; newCommand.disabled = false; updateSend(); }
});
selector.addEventListener("change", () => {
  if (pending || sending) return;
  selectedPane = selector.value; generation++; connected = false; output.textContent = "";
  status.textContent = "Opening pane…"; updateSend(); clearTimeout(timer); refresh();
});
newCommand.addEventListener("click", () => {
  if (sending) return;
  pending = undefined; input.value = ""; newCommand.hidden = true;
  commandStatus.textContent = "Ready for a new command. Previous commands are not undone."; updateSend(); input.focus();
});
async function signOut(path) {
  try {
    await request(path, { method: "POST" });
    stopped = true; generation++; output.textContent = ""; location.reload();
  } catch (error) { deny(error); }
}
logout.addEventListener("click", () => signOut("/share/auth/logout"));
logoutAll.addEventListener("click", () => signOut("/share/auth/logout-all"));
document.addEventListener("visibilitychange", () => { if (!document.hidden) { clearTimeout(timer); refresh(); } });
window.addEventListener("online", () => { clearTimeout(timer); refresh(); });
window.addEventListener("offline", () => { connected = false; updateSend(); status.textContent = "Offline. Displayed output may be stale."; });
if (/^[a-f0-9]{32}$/.test(id)) refresh(); else { stopped = true; status.textContent = "Invalid share link."; }
