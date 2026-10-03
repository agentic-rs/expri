import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { apiUrl, formatDuration, formatNumber, formatValue, RequestLane, startDashboard } from "./app.js";

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((accept, fail) => { resolve = accept; reject = fail; });
  return { promise, resolve, reject };
}
function response(value, status = 200) {
  return { ok: status >= 200 && status < 300, status, json: async () => value };
}

test("URL encoding preserves source labels and exact metric names", () => {
  const url = new URL(apiUrl("/api/chart", {
    source: "cached:gpu rental & #1",
    run_id: ["run-a", "run-b"],
    metric: ["loss&run_id=outside", "validation/loss + λ"],
    ignored: null,
    empty: "",
  }), "http://127.0.0.1:8765");
  assert.equal(url.pathname, "/api/chart");
  assert.equal(url.searchParams.get("source"), "cached:gpu rental & #1");
  assert.deepEqual(url.searchParams.getAll("run_id"), ["run-a", "run-b"]);
  assert.deepEqual(url.searchParams.getAll("metric"), ["loss&run_id=outside", "validation/loss + λ"]);
  assert.equal(url.searchParams.has("ignored"), false);
  assert.equal(url.searchParams.has("empty"), false);
});

test("formatters handle absent and invalid records without invented live durations", () => {
  const run = { started_at: "2026-10-03T01:00:00Z", finished_at: "2026-10-03T02:01:03Z" };
  assert.equal(formatDuration(run), "1h 1m");
  assert.equal(formatDuration({ ...run, finished_at: null }), "—");
  assert.equal(formatDuration({ ...run, started_at: "invalid" }), "—");
  assert.equal(formatDuration({ ...run, finished_at: "2026-10-03T00:00:00Z" }), "0s");
  assert.equal(formatValue(null), "—");
  assert.equal(formatValue({ learning_rate: 0.1 }), '{"learning_rate":0.1}');
  assert.equal(formatValue("<script>alert(1)</script>"), "<script>alert(1)</script>");
  assert.equal(formatNumber(Infinity), "—");
});

test("new requests abort old work and ignore late successes", async t => {
  const original_fetch = globalThis.fetch;
  t.after(() => { globalThis.fetch = original_fetch; });
  const pending = [];
  globalThis.fetch = (_url, options) => {
    const item = deferred(); pending.push({ ...item, signal: options.signal }); return item.promise;
  };
  const lane = new RequestLane();
  const first = lane.run("/old-source");
  const second = lane.run("/new-source");
  assert.equal(pending[0].signal.aborted, true);
  pending[1].resolve(response({ source_id: "new" }));
  assert.deepEqual(await second, { source_id: "new" });
  pending[0].resolve(response({ source_id: "old" }));
  assert.equal(await first, undefined);
});

test("cancelled review ignores late errors", async t => {
  const original_fetch = globalThis.fetch;
  t.after(() => { globalThis.fetch = original_fetch; });
  const pending = deferred();
  globalThis.fetch = () => pending.promise;
  const lane = new RequestLane();
  const request = lane.run("/api/run"); lane.cancel();
  pending.reject(new Error("disconnected"));
  assert.equal(await request, undefined);
});

test("current server errors remain actionable", async t => {
  const original_fetch = globalThis.fetch;
  t.after(() => { globalThis.fetch = original_fetch; });
  globalThis.fetch = async () => response({ error: "Chart exceeds the 2 MiB size limit; select fewer metrics." }, 413);
  await assert.rejects(new RequestLane().run("/api/chart"), /select fewer metrics/);
});

test("expired sessions navigate to login before parsing the response body", async t => {
  const original_fetch = globalThis.fetch;
  const original_location = globalThis.location;
  t.after(() => {
    globalThis.fetch = original_fetch;
    if (original_location === undefined) delete globalThis.location;
    else globalThis.location = original_location;
  });
  const redirects = [];
  globalThis.location = { assign: path => redirects.push(path) };
  globalThis.fetch = async () => ({
    ok: false,
    status: 401,
    json: () => { throw new Error("login response need not be JSON"); },
  });
  assert.equal(await new RequestLane().run("/api/catalog"), undefined);
  assert.deepEqual(redirects, ["/login"]);
  const pending = deferred();
  globalThis.fetch = () => pending.promise;
  const lane = new RequestLane();
  const request = lane.run("/api/run"); lane.cancel();
  pending.resolve(response(null, 401));
  assert.equal(await request, undefined);
  assert.deepEqual(redirects, ["/login"], "cancelled work must not navigate a newer view");
});

class TestElement {
  constructor(tag) {
    this.tagName = tag.toUpperCase();
    this.textContent = "";
    this.children = [];
    this.dataset = {};
    this.attributes = {};
    this.listeners = {};
    this.value = "";
    this.hidden = false;
    this.disabled = false;
    this.classList = { toggle() {}, add() {} };
  }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  setAttribute(name, value) { this.attributes[name] = value; }
  removeAttribute(name) { delete this.attributes[name]; }
  querySelectorAll() { return []; }
  addEventListener(name, listener) { (this.listeners[name] ??= []).push(listener); }
  emit(name) { for (const listener of this.listeners[name] ?? []) listener({ type: name }); }
}

function dashboard(t) {
  const original_document = globalThis.document;
  const original_fetch = globalThis.fetch;
  t.after(() => {
    globalThis.fetch = original_fetch;
    if (original_document === undefined) delete globalThis.document;
    else globalThis.document = original_document;
  });
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const nodes = new Map([...html.matchAll(/id="([^"]+)"/g)].map(match => [match[1], new TestElement("div")]));
  globalThis.document = {
    title: "expri",
    getElementById: id => nodes.get(id) ?? null,
    createElement: tag => new TestElement(tag),
  };
  return nodes;
}

async function settled(predicate) {
  for (let attempt = 0; attempt < 30; attempt++) {
    if (predicate()) return;
    await new Promise(resolve => setImmediate(resolve));
  }
  assert.fail("dashboard did not finish its pending request");
}

function text(node) {
  return [node.textContent, ...node.children.map(child => text(child))].join(" ");
}

test("empty hosted catalog refreshes into synced runs with opaque source IDs and 20-row pagination", async t => {
  const nodes = dashboard(t);
  const source = { source_id: "opaque&?/#λ", label: "Vision / gpu-1", kind: "service", target_name: null };
  let catalog = { project_name: "Synced experiments", sources: [], initial_source: "", warnings: [], access_mode: "hosted" };
  const requests = [];
  globalThis.fetch = async url => {
    requests.push(url);
    if (url === "/api/catalog") return response(catalog);
    const query = new URL(url, "http://localhost").searchParams;
    assert.equal(query.get("source"), source.source_id);
    assert.equal(query.get("limit"), "20");
    const offset = Number(query.get("offset"));
    const runs = Array.from({ length: 20 }, (_, index) => ({ run_id: `run-${offset + index}`, task: "train", status: "completed", started_at: null, finished_at: null, exit_code: 0 }));
    return response({ source, runs, warnings: [], total_count: 60, offset, next_offset: offset < 40 ? offset + 20 : null });
  };
  startDashboard();
  await settled(() => nodes.get("run-count").textContent === "No synced runs yet");
  assert.deepEqual(requests, ["/api/catalog"], "empty catalog must not request an invalid source");
  assert.equal(nodes.get("logout-form").hidden, false);
  assert.equal(nodes.get("source-select").disabled, true);
  assert.match(text(nodes.get("list-empty")), /No synced runs yet/);
  const guide = nodes.get("list-empty").children.at(-1).children[0];
  assert.match(guide.href, /github\.com\/agentic-rs\/expri\/.*self-hosted-service\.md$/);
  await settled(() => nodes.get("refresh-button").disabled === false);
  catalog = { ...catalog, sources: [source], initial_source: source.source_id, warnings: [{ message: "The overview is limited to 500 runs." }] };
  nodes.get("refresh-button").emit("click");
  await settled(() => nodes.get("run-rows").children.length === 20);
  assert.equal(nodes.get("source-select").value, source.source_id);
  assert.equal(nodes.get("source-select").disabled, false);
  assert.equal(nodes.get("search-input").disabled, false);
  assert.equal(nodes.get("source-select").children[0].textContent, "Vision / gpu-1 · Synced");
  assert.match(nodes.get("source-note").textContent, /^Synced results/);
  assert.match(text(nodes.get("catalog-warnings")), /limited to 500 runs/);
  nodes.get("next-page").emit("click");
  await settled(() => nodes.get("page-label").textContent === "Page 2");
  nodes.get("previous-page").emit("click");
  await settled(() => nodes.get("page-label").textContent === "Page 1");
  nodes.get("search-input").value = "accuracy";
  nodes.get("task-input").value = "train";
  nodes.get("status-select").value = "completed";
  nodes.get("status-select").emit("change");
  await settled(() => requests.some(url => url.includes("search=accuracy")));
  const query = new URL(requests.at(-1), "http://localhost").searchParams;
  assert.equal(query.get("task"), "train");
  assert.equal(query.get("status"), "completed");
  assert.equal(query.get("offset"), "0");
  await settled(() => nodes.get("runs-region").attributes["aria-busy"] === "false");
});

test("local catalogs retain cached labels, 100-row pages, and no logout control", async t => {
  const nodes = dashboard(t);
  const source = { source_id: "cached:gpu", label: "gpu · Cached", kind: "cached", target_name: "gpu" };
  globalThis.fetch = async url => {
    if (url === "/api/catalog") return response({ project_name: "Local workspace", sources: [source], initial_source: source.source_id, warnings: [] });
    assert.equal(new URL(url, "http://localhost").searchParams.get("limit"), "100");
    return response({ source, runs: [], warnings: [], total_count: 0, offset: 0, next_offset: null });
  };
  startDashboard();
  await settled(() => nodes.get("run-count").textContent === "No matching runs");
  assert.equal(nodes.get("logout-form").hidden, true);
  assert.equal(nodes.get("source-select").children[0].textContent, source.label);
  assert.match(nodes.get("source-note").textContent, /^Cached remote results/);
  assert.match(text(nodes.get("list-empty")), /expri runs pull/);
  assert.equal(nodes.get("dashboard-kind").textContent, "expri · Local experiment review");
});

test("login and logout use browser POST forms without application credentials", () => {
  const login = readFileSync(new URL("./login.html", import.meta.url), "utf8");
  const index = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  assert.match(login, /<form[^>]+method="post"[^>]+action="\/login"/);
  assert.match(login, /<input[^>]+name="password"[^>]+type="password"[^>]+autocomplete="current-password"/);
  assert.equal(login.split("<!-- LOGIN_ERROR -->").length, 2);
  assert.doesNotMatch(login, /<script\b|localStorage|Bearer\s|type="hidden"/i);
  assert.match(index, /<form[^>]+id="logout-form"[^>]+method="post"[^>]+action="\/logout"[^>]+hidden/);
});
