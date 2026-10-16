// Operator enrollment page (/auth/enroll). Sends the dashboard token once,
// as JSON from this origin with the X-Muxa-Operator header the server
// requires; the page's CSP blocks native form submission, so the token never
// ends up in a URL.
const form = document.querySelector("#enroll");
const input = document.querySelector("#token");
const status = document.querySelector("#status");

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  const button = form.querySelector("button");
  const token = input.value.trim();
  if (!token) return;
  button.disabled = true;
  status.textContent = "Checking the token…";
  try {
    const resp = await fetch("/auth/enroll", {
      method: "POST",
      credentials: "same-origin",
      cache: "no-store",
      headers: { "Content-Type": "application/json", "X-Muxa-Operator": "1" },
      body: JSON.stringify({ token }),
    });
    let payload = null;
    try {
      payload = await resp.json();
    } catch (_) {
      // Status-only rejections have no body.
    }
    const target = payload?.redirect;
    if (resp.ok && typeof target === "string" && target.startsWith("/") && !target.startsWith("//")) {
      status.textContent = "Registered. Opening the dashboard…";
      window.location.replace(target);
      return;
    }
    input.value = "";
    status.textContent = payload?.error || `Registration failed (${resp.status}).`;
    if (payload?.restart) {
      form.hidden = true;
      const again = document.createElement("a");
      again.href = "/";
      again.textContent = "Back to the dashboard";
      status.append(" ", again);
    }
  } catch (_) {
    status.textContent = "Could not reach the dashboard. Try again.";
  } finally {
    button.disabled = false;
  }
});
