export function responseLatency(request) {
  if (!request.reply?.at || !request.created_at) return null;
  const delta = Date.parse(request.reply.at) - Date.parse(request.created_at);
  return Number.isFinite(delta) && delta >= 0 ? delta : null;
}

export function collaborationSummary(requests) {
  const latencies = requests.map(responseLatency).filter(value => value !== null).sort((a, b) => a - b);
  const middle = Math.floor(latencies.length / 2);
  return {
    total: requests.length,
    replies: requests.filter(request => request.reply).length,
    pending: requests.filter(request => ["queued", "claimed"].includes(request.status)).length,
    attention: requests.filter(request => ["blocked", "failed", "expired", "declined"].includes(request.status)).length,
    medianLatency: latencies.length ? (latencies.length % 2 ? latencies[middle] : (latencies[middle - 1] + latencies[middle]) / 2) : null,
  };
}

export function messagePreview(request, detailsIncluded) {
  if (!detailsIncluded) return "Unlock access to preview message bodies";
  return String(request.body || "").replace(/\s+/g, " ").trim().slice(0, 240) || "Empty message";
}

export function graphFitScale(width, height, viewportWidth, viewportHeight) {
  if (![width, height, viewportWidth, viewportHeight].every(value => Number.isFinite(value) && value > 0)) return 1;
  return Math.max(.1, Math.min(2, (viewportWidth - 16) / width, (viewportHeight - 16) / height));
}

// A separator supports both pointer dragging and keyboard resizing.
export function initResizeHandle(handle, { read, write, min, max, step = 10, axis = "x", deltaToValue = value => value }) {
  let drag = null;
  const apply = value => {
    const bounded = Math.max(min, Math.min(max, value));
    write(bounded);
    handle.setAttribute("aria-valuenow", String(Math.round(bounded)));
  };
  handle.setAttribute("aria-valuemin", String(min));
  handle.setAttribute("aria-valuemax", String(max));
  apply(Number.isFinite(read()) ? read() : min);
  handle.addEventListener("pointerdown", event => {
    if (event.button !== 0) return;
    event.preventDefault();
    drag = { coordinate: axis === "x" ? event.clientX : event.clientY, value: read() };
    handle.setPointerCapture(event.pointerId);
  });
  handle.addEventListener("pointermove", event => {
    if (!drag) return;
    apply(drag.value + deltaToValue((axis === "x" ? event.clientX : event.clientY) - drag.coordinate));
  });
  handle.addEventListener("lostpointercapture", () => { drag = null; });
  handle.addEventListener("pointerup", () => { drag = null; });
  handle.addEventListener("keydown", event => {
    const decrease = axis === "x" ? "ArrowLeft" : "ArrowUp";
    const increase = axis === "x" ? "ArrowRight" : "ArrowDown";
    if (![decrease, increase, "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    apply(event.key === "Home" ? min : event.key === "End" ? max : read() + (event.key === increase ? step : -step));
  });
}

const renderedHTML = new WeakMap();
export function setStableHTML(node, html) {
  if (node.childNodes.length && renderedHTML.get(node) === html) return false;
  node.innerHTML = html;
  renderedHTML.set(node, html);
  return true;
}
export function clearStableHTML(node) {
  if (!node) return;
  renderedHTML.delete(node);
  node.replaceChildren();
}
export function participantLocation(participant) {
  return participant?.console ? 'operator console' : participant?.pane || participant?.agent_session_id || 'unknown';
}
export function messageLink(href, request) {
  const url = new URL(href);
  url.search = '';
  url.hash = new URLSearchParams({ message: request.id }).toString();
  return url.toString();
}
