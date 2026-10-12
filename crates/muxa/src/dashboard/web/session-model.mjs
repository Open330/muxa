// Match tmux's numeric client-view suffix without hiding ordinary names.
export function baseSessionName(value) {
  let name = String(value || "");
  while (/^.+~view~[0-9]+$/.test(name)) name = name.replace(/~view~[0-9]+$/, "");
  return name;
}

// A grouped session lists the same physical pane once per viewing client.
// Keep the base listing (including its stable session id) whenever present.
export function canonicalDashboardPanes(panes = []) {
  const rows = new Map();
  for (const pane of panes) {
    const key = JSON.stringify([pane.host || "", pane.socket || "", pane.window_id, pane.pane_id]);
    const old = rows.get(key);
    if (!old || (baseSessionName(old.session) !== old.session && baseSessionName(pane.session) === pane.session)) {
      rows.set(key, pane);
    }
  }
  return [...rows.values()].map((pane) => ({ ...pane, session: baseSessionName(pane.session) }));
}

// Position on the selected wall-clock window, including gaps and clipping.
export function intervalPlacement(interval, start, end) {
  const s = Math.max(start, Date.parse(interval.started_at));
  const e = Math.min(end, Date.parse(interval.ended_at));
  if (!Number.isFinite(s) || !Number.isFinite(e) || end <= start || e <= s) return null;
  return { start: s, end: e, left: (s - start) / (end - start) * 100, width: (e - s) / (end - start) * 100 };
}
