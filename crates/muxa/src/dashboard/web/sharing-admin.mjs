// Operator-only controls. Recipient pages never load the operator credential.
export function openShareManager(target, request) {
  const dialog = document.createElement("dialog");
  dialog.className = "share-dialog";
  dialog.innerHTML = `<form class="share-create">
    <h2>Share a pane</h2><p class="share-target"></p>
    <label>Invited email <input name="email" type="email" required maxlength="254" autocomplete="off"></label>
    <label>Access <select name="permission"><option value="view">View output</option><option value="prompt">View and send prompts</option></select></label>
    <label>Expires in <select name="ttl"><option value="3600">1 hour</option><option value="14400">4 hours</option><option value="86400">24 hours</option></select></label>
    <p>The invited account can see this pane’s output. Prompt access can run commands in it. Shares also end when the daemon restarts.</p>
    <button type="submit">Create share</button>
    </form><p class="share-message" role="status"></p><div class="share-list"></div><button type="button" class="share-close">Close</button>`;
  dialog.querySelector(".share-target").textContent = target ? `Pane ${target.pane}` : "";
  dialog.querySelector("form").hidden = !target;
  const message = dialog.querySelector(".share-message");
  const list = dialog.querySelector(".share-list");
  async function refresh() {
    const data = await request("/api/shares", { method: "GET" });
    list.replaceChildren();
    const title = document.createElement("h3"); title.textContent = "Shares"; list.append(title);
    if (!data.shares.length) { const empty = document.createElement("p"); empty.textContent = "No shares yet. Choose Share on a pane to invite someone."; list.append(empty); }
    for (const share of data.shares) {
      const row = document.createElement("div"); row.className = "share-row";
      const expired = share.revoked || share.expires_at * 1000 <= Date.now();
      const label = document.createElement("p");
      label.textContent = `${share.pane} · ${share.email} · ${share.permission} · ${expired ? "ended" : `until ${new Date(share.expires_at * 1000).toLocaleString()}`}`;
      row.append(label);
      if (!expired) {
        const link = document.createElement("input"); link.readOnly = true; link.value = share.url; link.setAttribute("aria-label", "Share link");
        link.addEventListener("click", () => link.select()); row.append(link);
        const revoke = document.createElement("button"); revoke.type = "button"; revoke.textContent = "Revoke";
        revoke.addEventListener("click", async () => {
          revoke.disabled = true;
          try { await request(`/api/shares/${encodeURIComponent(share.id)}/revoke`, { method: "POST" }); await refresh(); message.textContent = "Access revoked."; }
          catch (error) { message.textContent = error.message; revoke.disabled = false; }
        }); row.append(revoke);
      }
      list.append(row);
    }
  }
  dialog.querySelector("form").addEventListener("submit", async event => {
    event.preventDefault(); const form = event.currentTarget; const button = form.querySelector("button"); button.disabled = true;
    const fields = new FormData(form);
    try {
      await request("/api/shares", { method: "POST", body: JSON.stringify({ pane: target.pane, socket: target.socket, email: fields.get("email"), permission: fields.get("permission"), ttl_seconds: Number(fields.get("ttl")) }) });
      await refresh(); message.textContent = "Share created. Copy its link and send it to the invited person.";
    } catch (error) { message.textContent = error.message; }
    finally { button.disabled = false; }
  });
  dialog.querySelector(".share-close").addEventListener("click", () => dialog.close());
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog); dialog.showModal();
  refresh().catch(error => { message.textContent = error.message; });
}
