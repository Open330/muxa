// The "Needs you" page module itself, under a minimal fake DOM: the model
// tests cannot catch a missing import or a render path that throws.
import test from "node:test";
import assert from "node:assert/strict";

class Node {
  constructor(tag) {
    this.tag = tag;
    this.children = [];
    this.listeners = {};
    this.dataset = {};
    this.attributes = {};
    this.hidden = false;
    this.textContent = "";
    this.className = "";
  }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren(...nodes) { this.children = nodes; }
  addEventListener(type, fn) { (this.listeners[type] ||= []).push(fn); }
  setAttribute(name, value) { this.attributes[name] = value; }
  querySelectorAll() { return []; }
  click() { for (const fn of this.listeners.click || []) fn(); }
  get text() { return [this.textContent, ...this.children.map((c) => c.text)].join(" "); }
}

const nodes = new Map();
globalThis.document = {
  hidden: false,
  activeElement: null,
  getElementById(id) {
    if (!nodes.has(id)) nodes.set(id, new Node("div"));
    return nodes.get(id);
  },
  createElement: (tag) => new Node(tag),
  querySelectorAll: () => [],
  addEventListener() {},
};
globalThis.window = { location: { hash: "" } };
globalThis.localStorage = { getItem: () => null };
globalThis.setInterval = () => 0;
const answered = { kind: "human_answer", request_id: "rq_1", body: "Merge?", status: "completed", answer: "yes", answered_at: "2026-11-12T11:00:00Z" };
globalThis.fetch = async (path) => ({
  ok: true,
  status: 200,
  json: async () =>
    path.includes("history")
      ? { covers: "mba", hosts: [{ alias: "mba", local: true, history: { items: [answered] } }, { alias: "mini", local: false, unavailable: ["host is offline"], history: { items: [] } }] }
      : { hosts: [{ alias: "mba", local: true, control: true, decides_reviews: true, queue: { items: [] } }] },
});

const settle = () => new Promise((resolve) => setTimeout(resolve, 20));

test("the page renders the queue and every host's history", async () => {
  await import("../src/dashboard/web/needs.mjs");
  await settle();
  assert.equal(document.getElementById("status").textContent, "");
  document.getElementById("tab-history").click();
  await settle();
  assert.equal(document.getElementById("status").textContent, "", "no error on the Decided tab");
  const history = document.getElementById("history").text;
  assert.match(history, /Merge\?/);
  assert.match(history, /mini/);
  assert.match(history, /unavailable: host is offline/);
  assert.equal(document.getElementById("covers").textContent, "Covers mba, mini.");
});
