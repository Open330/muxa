// Operator-only: accounts enrolled with the dashboard token (see
// /auth/enroll). Removing one ends its sessions immediately.
export function openOperatorManager(request) {
  const dialog = document.createElement("dialog");
  dialog.className = "share-dialog";
  dialog.innerHTML = `<h2>Operators</h2>
    <p>Accounts registered with this dashboard's access token can sign in without it. Members of the configured operator group sign in without registering and are not listed. Removing an account signs it out everywhere; it can register again only with the token.</p>
    <p class="share-message" role="status"></p><div class="share-list"></div><button type="button" class="share-close">Close</button>`;
  const message = dialog.querySelector(".share-message");
  const list = dialog.querySelector(".share-list");
  const when = (seconds) => new Date(seconds * 1000).toLocaleString();
  async function refresh() {
    const data = await request("/api/operators", { method: "GET" });
    list.replaceChildren();
    if (!data.enrollment) {
      const off = document.createElement("p");
      off.textContent = "Enrollment is turned off (dashboard.login.enrollment = false); only the operator group can sign in.";
      list.append(off);
    }
    if (!data.operators.length) {
      const empty = document.createElement("p");
      empty.textContent = "No registered accounts. Sign in with an account outside the operator group and enter the access token to register it.";
      list.append(empty);
    }
    for (const operator of data.operators) {
      const row = document.createElement("div");
      row.className = "share-row";
      const label = document.createElement("p");
      const who = operator.email || `subject ${operator.subject}`;
      const flags = [
        operator.current ? "this account" : null,
        operator.active ? null : "other issuer, not honored",
      ].filter(Boolean);
      label.textContent = `${who}${flags.length ? ` (${flags.join(", ")})` : ""} · registered ${when(operator.created_at)} · last sign-in ${when(operator.last_seen_at)}`;
      label.title = `issuer ${operator.issuer} · subject ${operator.subject}`;
      const remove = document.createElement("button");
      remove.type = "button";
      remove.textContent = "Remove";
      remove.addEventListener("click", async () => {
        if (operator.current && !window.confirm("Remove your own account? This browser will be signed out.")) return;
        remove.disabled = true;
        try {
          await request(`/api/operators/${encodeURIComponent(operator.id)}/remove`, { method: "POST" });
          if (operator.current) { window.location.reload(); return; }
          await refresh();
          message.textContent = "Account removed and signed out.";
        } catch (error) {
          message.textContent = error.message;
          remove.disabled = false;
        }
      });
      row.append(label, remove);
      list.append(row);
    }
  }
  dialog.querySelector(".share-close").addEventListener("click", () => dialog.close());
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
  refresh().catch((error) => { message.textContent = error.message; });
}
