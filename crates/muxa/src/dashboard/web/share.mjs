const id = location.pathname.split("/").pop();
const status = document.querySelector("#status");
const output = document.querySelector("#output");
const login = document.querySelector("#login");
const logout = document.querySelector("#logout");
const form = document.querySelector("#prompt-form");
const send = document.querySelector("#send");
let stopped = false;
let inFlight = false;
let sending = false;
let timer;
login.href = `/share/auth/login?share=${encodeURIComponent(id)}`;
async function request(path, options = {}) {
  const response = await fetch(path, { ...options, credentials: "same-origin", cache: "no-store", headers: { "X-Muxa-Share": "1", "Content-Type": "application/json" } });
  const data = response.status === 204 ? {} : await response.json();
  if (!response.ok) { const error = new Error(data.error || "Request failed"); error.status = response.status; throw error; }
  return data;
}
function deny(error) {
  status.textContent = error.message;
  if ([401, 403, 404, 410].includes(error.status)) {
    stopped = true; clearTimeout(timer); output.textContent = ""; form.hidden = true; send.disabled = true;
    login.hidden = error.status !== 401; logout.hidden = error.status === 401;
    document.querySelector("#identity").textContent = "";
  }
}
async function refresh() {
  if (stopped || inFlight || document.hidden) return;
  inFlight = true;
  try {
    const share = await request(`/share/api/${encodeURIComponent(id)}`);
    login.hidden = true; logout.hidden = false;
    document.querySelector("#identity").textContent = `${share.pane} · ${share.email} · ${share.permission} · expires ${new Date(share.expires_at * 1000).toLocaleString()}`;
    const atBottom = output.scrollHeight - output.scrollTop - output.clientHeight < 40;
    if (output.textContent !== share.output) { output.textContent = share.output; if (atBottom) output.scrollTop = output.scrollHeight; }
    form.hidden = share.permission !== "prompt";
    if (!sending) status.textContent = "Connected";
  } catch (error) { deny(error); }
  finally { inFlight = false; if (!stopped) timer = setTimeout(refresh, 2000); }
}
form.addEventListener("submit", async event => {
  event.preventDefault(); if (sending || stopped) return; sending = true; send.disabled = true;
  const text = document.querySelector("#prompt").value;
  try { await request(`/share/api/${encodeURIComponent(id)}/prompt`, { method: "POST", body: JSON.stringify({ text }) }); document.querySelector("#prompt").value = ""; status.textContent = "Prompt sent."; }
  catch (error) { deny(error); }
  finally { sending = false; if (!stopped) send.disabled = false; }
});
logout.addEventListener("click", async () => {
  try { await request("/share/auth/logout", { method: "POST" }); location.reload(); } catch (error) { deny(error); }
});
document.addEventListener("visibilitychange", () => { if (!document.hidden) { clearTimeout(timer); refresh(); } });
if (/^[a-f0-9]{32}$/.test(id)) refresh(); else { stopped = true; status.textContent = "Invalid share link."; }
