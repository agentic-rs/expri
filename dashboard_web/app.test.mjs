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
    this.classes = new Set();
    this.classList = {
      toggle: (name, enabled) => enabled ? this.classes.add(name) : this.classes.delete(name),
      add: (...names) => names.forEach(name => this.classes.add(name)),
    };
  }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  setAttribute(name, value) { this.attributes[name] = value; }
  removeAttribute(name) { delete this.attributes[name]; if (name === "src") this.src = ""; }
  querySelectorAll(selector) {
    const tags = selector.split(",").map(tag => tag.trim().toUpperCase());
    return this.children.flatMap(child => [child, ...child.querySelectorAll(selector)]).filter(child => tags.includes(child.tagName));
  }
  addEventListener(name, listener) { (this.listeners[name] ??= []).push(listener); }
  emit(name, fields = {}) { for (const listener of this.listeners[name] ?? []) listener({ type: name, preventDefault() {}, ...fields }); }
  focus() { globalThis.document.activeElement = this; }
  click() { if (!this.disabled) { this.focus(); this.emit("click"); } }
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
    createTextNode: value => { const node = new TestElement("text"); node.textContent = value; return node; },
  };
  return nodes;
}

async function settled(predicate) {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (predicate()) return;
    await new Promise(resolve => setTimeout(resolve, 10));
  }
  assert.fail("dashboard did not finish its pending request");
}

function text(node) {
  return [node.textContent, ...node.children.map(child => text(child))].join(" ");
}
function findElement(node, id) {
  if (node.id === id) return node;
  for (const child of node.children) { const found = findElement(child, id); if (found) return found; }
}

function detailRecord(source, run) {
  const metric = { count: 2, last: { step: 1, value: 0.5 }, min: { step: 1, value: 0.5 }, max: { step: 0, value: 1 } };
  return { source, run, state: { command: "python train.py" }, snapshot: null, environment: null, cache: null, params: { learning_rate: 0.1 }, params_truncated: false, metadata_truncated: false, metrics: { accuracy: metric, loss: metric }, metric_count: 2, metrics_truncated: false, metrics_error: null, warnings: [] };
}

function reviewFixture(t, { count = 3, hosted = false, sources } = {}) {
  const nodes = dashboard(t);
  sources ??= [{ source_id: "local", label: "Local", kind: hosted ? "service" : "local", target_name: null }];
  const runs = Array.from({ length: count }, (_, index) => ({
    run_id: `run-${index}`, task: "train", status: "completed",
    started_at: "2026-10-03T01:00:00Z", finished_at: "2026-10-03T01:00:03Z", exit_code: 0,
  }));
  const model = { nodes, sources, runs, requests: [], override: null, failed_list: false };
  globalThis.fetch = async (url, options) => {
    model.requests.push(url);
    const parsed = new URL(url, "http://localhost");
    const query = parsed.searchParams;
    const overridden = model.override?.(parsed, options);
    if (overridden !== undefined && overridden !== null) return overridden;
    if (parsed.pathname === "/api/catalog") return response({ project_name: "Experiments", sources, initial_source: sources[0].source_id, warnings: [], access_mode: hosted ? "hosted" : "local" });
    const source = sources.find(item => item.source_id === query.get("source"));
    assert.ok(source, "all requests must remain in a known source");
    if (parsed.pathname === "/api/runs") {
      if (model.failed_list) return response({ error: "Results are unavailable. Try Refresh." }, 500);
      const visible = runs.filter(run => !query.get("search") || run.run_id.includes(query.get("search")));
      const offset = Number(query.get("offset"));
      const limit = Number(query.get("limit"));
      return response({ source, runs: visible.slice(offset, offset + limit), warnings: [], total_count: visible.length, offset, next_offset: offset + limit < visible.length ? offset + limit : null });
    }
    const metric = { count: 2, last: { step: 1, value: 0.5 }, min: { step: 1, value: 0.5 }, max: { step: 0, value: 1 } };
    if (parsed.pathname === "/api/run") {
      const run = runs.find(item => item.run_id === query.get("run_id"));
      assert.ok(run);
      return response(detailRecord(source, run));
    }
    if (parsed.pathname === "/api/log") return response({ content: `${query.get("stream")} training complete`, stream: query.get("stream"), missing: false, truncated: false });
    if (parsed.pathname === "/api/compare") {
      const metric_names = query.getAll("metric").length ? query.getAll("metric") : ["accuracy", "loss"];
      return response({ source, comparison: { reduction: query.get("reduction"), metric_names, runs: query.getAll("run_id").map(run_id => ({ run_id, run: runs.find(item => item.run_id === run_id), values: Object.fromEntries(metric_names.map(name => [name, metric.last])) })), warnings: [] } });
    }
    assert.fail(`Unexpected dashboard request: ${url}`);
  };
  startDashboard();
  return model;
}

function selectRow(nodes, index, checked = true) {
  const input = nodes.get("run-rows").children[index].children[0].children[0];
  assert.equal(input.disabled, false, "the row must be selectable");
  input.focus(); input.checked = checked; input.emit("change");
}
function chartQuery(nodes) {
  return new URL(nodes.get("chart-frame").src || "/", "http://localhost").searchParams;
}
function comparisonRows(nodes) {
  return nodes.get("comparison-values").children[0]?.children[1]?.children ?? [];
}

test("run inspection opens Charts and keyboard tabs load logs only on demand", async t => {
  const { nodes, requests } = reviewFixture(t);
  await settled(() => nodes.get("run-rows").children.length === 3);
  assert.equal(nodes.get("run-rows").children[0].children.length, 3);
  assert.match(text(nodes.get("run-rows").children[0].children[1]), /train.*3s/);
  nodes.get("run-rows").children[0].children[1].children[0].click();
  await settled(() => chartQuery(nodes).get("run_id") === "run-0");
  assert.equal(nodes.get("review-tab-charts").attributes["aria-selected"], "true");
  assert.equal(nodes.get("review-panel-charts").hidden, false);
  assert.equal(nodes.get("review-panel-overview").hidden, true);
  assert.equal(nodes.get("review-panel-logs").hidden, true);
  assert.equal(requests.some(url => url.startsWith("/api/log")), false);
  nodes.get("review-tab-charts").emit("keydown", { key: "ArrowRight" });
  assert.equal(nodes.get("review-panel-overview").hidden, false);
  assert.equal(globalThis.document.activeElement, nodes.get("review-tab-overview"));
  assert.match(text(nodes.get("run-detail")), /learning_rate.*0\.1/);
  assert.equal(requests.some(url => url.startsWith("/api/log")), false);
  nodes.get("review-tab-overview").emit("keydown", { key: "End" });
  await settled(() => text(nodes.get("run-logs")).includes("stdout training complete"));
  assert.equal(nodes.get("review-panel-logs").hidden, false);
  assert.equal(nodes.get("review-tab-logs").tabIndex, 0);
  assert.equal(nodes.get("review-tab-charts").tabIndex, -1);
  nodes.get("review-tab-logs").emit("keydown", { key: "Home" });
  nodes.get("review-tab-charts").emit("keydown", { key: "ArrowLeft" });
  assert.equal(nodes.get("review-panel-logs").hidden, false);
  assert.equal(requests.filter(url => url.startsWith("/api/log")).length, 1, "returning to a loaded log must reuse that bounded tail");
});

test("selection updates charts, caps eight runs, and inspection returns to the selection", async t => {
  const { nodes, requests } = reviewFixture(t, { count: 9 });
  await settled(() => nodes.get("run-rows").children.length === 9);
  selectRow(nodes, 0);
  assert.equal(nodes.get("review-section").hidden, false);
  await settled(() => chartQuery(nodes).getAll("run_id").length === 1);
  assert.equal(globalThis.document.activeElement, nodes.get("run-rows").children[0].children[0].children[0], "keyboard checkbox focus must survive the debounced review request");
  assert.deepEqual(chartQuery(nodes).getAll("run_id"), ["run-0"]);
  assert.equal(nodes.get("close-review").hidden, true);
  selectRow(nodes, 1);
  await settled(() => comparisonRows(nodes).length === 2);
  assert.equal(globalThis.document.activeElement, nodes.get("run-rows").children[1].children[0].children[0]);
  assert.deepEqual(chartQuery(nodes).getAll("run_id"), ["run-0", "run-1"]);
  assert.equal(nodes.get("review-tab-overview").disabled, true);
  assert.equal(nodes.get("review-tab-logs").disabled, true);
  const before = requests.filter(url => url.startsWith("/api/compare")).length;
  for (let index = 2; index < 8; index++) selectRow(nodes, index);
  await settled(() => comparisonRows(nodes).length === 8);
  assert.equal(requests.filter(url => url.startsWith("/api/compare")).length, before + 1, "rapid selection changes should make one final comparison request");
  assert.equal(nodes.get("run-rows").children[8].children[0].children[0].disabled, true);
  nodes.get("selected-runs").children[2].click();
  await settled(() => comparisonRows(nodes).length === 7);
  assert.equal(chartQuery(nodes).getAll("run_id").includes("run-2"), false);
  nodes.get("run-rows").children[8].children[1].children[0].click();
  await settled(() => chartQuery(nodes).get("run_id") === "run-8");
  assert.equal(nodes.get("selected-runs").children.length, 7);
  assert.equal(nodes.get("close-review").textContent, "Back to selection");
  nodes.get("close-review").click();
  await settled(() => chartQuery(nodes).getAll("run_id").length === 7);
  nodes.get("clear-selection").click();
  assert.equal(nodes.get("review-section").hidden, true);
  assert.equal(nodes.get("review-empty").hidden, false);
  assert.equal(nodes.get("selected-runs").children.length, 0);
  assert.equal(chartQuery(nodes).getAll("run_id").length, 0);
});

test("pagination preserves explicit selected IDs while edited filters immediately clear the review", async t => {
  const { nodes, requests } = reviewFixture(t, { count: 40, hosted: true });
  await settled(() => nodes.get("run-rows").children.length === 20);
  selectRow(nodes, 0);
  await settled(() => chartQuery(nodes).get("run_id") === "run-0");
  nodes.get("next-page").click();
  await settled(() => nodes.get("page-label").textContent === "Page 2");
  assert.match(text(nodes.get("selected-runs")), /run-0/);
  assert.deepEqual(chartQuery(nodes).getAll("run_id"), ["run-0"]);
  selectRow(nodes, 0);
  await settled(() => comparisonRows(nodes).length === 2);
  assert.deepEqual(chartQuery(nodes).getAll("run_id"), ["run-0", "run-20"]);
  nodes.get("search-input").value = "missing";
  nodes.get("search-input").emit("input");
  assert.equal(nodes.get("selected-runs").children.length, 0);
  assert.equal(nodes.get("review-section").hidden, true);
  assert.equal(chartQuery(nodes).getAll("run_id").length, 0);
  await settled(() => nodes.get("run-count").textContent === "No matching runs");
  const query = new URL(requests.at(-1), "http://localhost").searchParams;
  assert.equal(query.get("search"), "missing");
  assert.equal(query.get("offset"), "0");
});

test("late comparison and catalog responses cannot restore an old source after a source change", async t => {
  const sources = [
    { source_id: "worker&A", label: "Worker A", kind: "service", target_name: null },
    { source_id: "worker&B", label: "Worker B", kind: "service", target_name: null },
  ];
  const model = reviewFixture(t, { sources, hosted: true });
  const { nodes } = model;
  await settled(() => nodes.get("run-rows").children.length === 3);
  const old_comparison = deferred();
  let comparison_started = false;
  model.override = parsed => {
    if (parsed.pathname === "/api/compare") { comparison_started = true; return old_comparison.promise; }
  };
  selectRow(nodes, 0); selectRow(nodes, 1);
  await settled(() => comparison_started);
  const old_catalog = deferred();
  model.override = parsed => parsed.pathname === "/api/catalog" ? old_catalog.promise : undefined;
  nodes.get("refresh-button").click();
  nodes.get("source-select").value = sources[1].source_id;
  nodes.get("source-select").emit("change");
  await settled(() => nodes.get("runs-region").attributes["aria-busy"] === "false");
  nodes.get("run-rows").children[0].children[1].children[0].click();
  await settled(() => chartQuery(nodes).get("source") === sources[1].source_id);
  old_comparison.resolve(response({ source: sources[0], comparison: { metric_names: [], runs: [], warnings: [] } }));
  old_catalog.resolve(response({ project_name: "Stale source", sources: [sources[0]], initial_source: sources[0].source_id, warnings: [], access_mode: "hosted" }));
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.equal(nodes.get("source-select").value, sources[1].source_id);
  assert.equal(chartQuery(nodes).get("source"), sources[1].source_id);
  assert.equal(nodes.get("project-name").textContent, "Experiments");
  assert.equal(nodes.get("selected-runs").children.length, 0);
  assert.equal(nodes.get("review-title").textContent, "run-0");
  assert.equal(nodes.get("refresh-button").disabled, false);
});

test("manual Refresh preserves the review tab and metrics and only updates Last checked after success", async t => {
  const model = reviewFixture(t);
  const { nodes, requests } = model;
  await settled(() => nodes.get("run-rows").children.length === 3);
  nodes.get("run-rows").children[0].children[1].children[0].click();
  await settled(() => chartQuery(nodes).get("run_id") === "run-0");
  const accuracy = nodes.get("run-metric-options").children[0].children[0].children[0];
  accuracy.checked = false; accuracy.emit("change");
  assert.deepEqual(chartQuery(nodes).getAll("metric"), ["loss"]);
  nodes.get("review-tab-overview").click();
  nodes.get("refresh-button").click();
  await settled(() => nodes.get("refresh-button").disabled === false);
  assert.equal(nodes.get("review-panel-overview").hidden, false);
  assert.deepEqual(chartQuery(nodes).getAll("metric"), ["loss"]);
  assert.equal(requests.some(url => url.startsWith("/api/log")), false);
  assert.match(nodes.get("updated-at").textContent, /^Last checked at /);
  nodes.get("review-tab-logs").click();
  await settled(() => text(nodes.get("run-logs")).includes("stdout training complete"));
  findElement(nodes.get("run-logs"), "log-tab-stderr").click();
  await settled(() => text(nodes.get("run-logs")).includes("stderr training complete"));
  nodes.get("refresh-button").click();
  await settled(() => nodes.get("refresh-button").disabled === false);
  assert.equal(nodes.get("review-panel-logs").hidden, false);
  assert.equal(requests.filter(url => url.startsWith("/api/log")).length, 3);
  assert.match(text(nodes.get("run-logs")), /stderr training complete/);
  assert.equal(findElement(nodes.get("run-logs"), "log-tab-stderr").attributes["aria-selected"], "true");
  nodes.get("updated-at").textContent = "Previous successful check";
  model.failed_list = true;
  nodes.get("refresh-button").click();
  await settled(() => nodes.get("refresh-button").disabled === false);
  assert.equal(nodes.get("updated-at").textContent, "Previous successful check");
  assert.match(nodes.get("global-error").textContent, /Results are unavailable/);
});

test("a tab change during Refresh does not strand an unfinished initial run review", async t => {
  const model = reviewFixture(t);
  const { nodes } = model;
  await settled(() => nodes.get("run-rows").children.length === 3);
  const initial_detail = deferred();
  const refresh_catalog = deferred();
  let detail_started = false;
  model.override = parsed => {
    if (parsed.pathname === "/api/run") { detail_started = true; return initial_detail.promise; }
    if (parsed.pathname === "/api/catalog") return refresh_catalog.promise;
  };
  selectRow(nodes, 0);
  await settled(() => detail_started);
  nodes.get("refresh-button").click();
  nodes.get("review-tab-charts").emit("keydown", { key: "ArrowRight" });
  assert.equal(nodes.get("review-panel-overview").hidden, false);
  initial_detail.resolve(response(detailRecord(model.sources[0], model.runs[0])));
  refresh_catalog.resolve(response({ project_name: "Late refresh", sources: model.sources, initial_source: "local", warnings: [] }));
  await settled(() => nodes.get("refresh-button").disabled === false);
  assert.equal(nodes.get("review-loading").hidden, true);
  assert.match(text(nodes.get("run-detail")), /learning_rate.*0\.1/);
  assert.equal(nodes.get("review-panel-overview").hidden, false);
  assert.equal(nodes.get("project-name").textContent, "Late refresh");
  assert.match(nodes.get("updated-at").textContent, /^Last checked at /);
  assert.equal(nodes.get("refresh-button").disabled, false);
  assert.equal(model.requests.some(url => url.startsWith("/api/log")), false);
});

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
