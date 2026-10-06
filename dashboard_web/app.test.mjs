import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { JSDOM } from "jsdom";
import { act } from "react";

const browser_globals = ["window", "document", "navigator", "HTMLElement", "HTMLInputElement", "HTMLSelectElement", "HTMLIFrameElement", "Node", "Event", "KeyboardEvent", "MutationObserver", "requestAnimationFrame", "cancelAnimationFrame"];
function installWindow(window) {
  const previous = new Map(browser_globals.map(name => [name, Object.getOwnPropertyDescriptor(globalThis, name)]));
  for (const name of browser_globals) Object.defineProperty(globalThis, name, { configurable: true, writable: true, value: typeof window[name] === "function" && name.endsWith("AnimationFrame") ? window[name].bind(window) : window[name] });
  return () => {
    for (const [name, descriptor] of previous) descriptor ? Object.defineProperty(globalThis, name, descriptor) : Reflect.deleteProperty(globalThis, name);
  };
}
// React DOM detects input support during import. Give it a real document without
// a dashboard root, then mount each test in its own independent browser document.
const baseline = new JSDOM("<!doctype html><html><body></body></html>", { url: "http://localhost/", pretendToBeVisual: true });
installWindow(baseline.window);
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
const { apiUrl, formatDuration, formatNumber, formatValue, localTimeZoneLabel, RequestLane, startDashboard } = await import("./.test/app.js");

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

function dashboard(t) {
  const original_fetch = globalThis.fetch;
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const dom = new JSDOM(html, { url: "http://localhost/", pretendToBeVisual: true });
  const restore = installWindow(dom.window);
  let navigation_count = 0;
  const record = records => { navigation_count += records.filter(item => item.type === "attributes" && item.target.id === "chart-frame").length; };
  const observer = new dom.window.MutationObserver(record);
  observer.observe(dom.window.document.body, { subtree: true, attributes: true, attributeFilter: ["src"] });
  const nodes = {
    document: dom.window.document,
    cleanup: null,
    get(id) { const node = dom.window.document.getElementById(id); assert.ok(node, `Missing rendered element: ${id}`); return node; },
    navigationCount() { record(observer.takeRecords()); return navigation_count; },
  };
  t.after(async () => {
    if (nodes.cleanup) await act(async () => { nodes.cleanup(); await microtasks(); });
    observer.disconnect(); dom.window.close(); restore();
    globalThis.fetch = original_fetch;
  });
  return nodes;
}

async function mountDashboard(nodes, options = {}) {
  await act(async () => { nodes.cleanup = startDashboard(options); await microtasks(); });
}
async function microtasks() { for (let index = 0; index < 32; index++) await Promise.resolve(); }
async function click(node) { await act(async () => { node.focus(); node.click(); await microtasks(); }); }
async function emit(node, name, fields = {}) {
  await act(async () => {
    const window = node.ownerDocument?.defaultView ?? node.defaultView;
    const EventType = name.startsWith("key") ? window.KeyboardEvent : window.Event;
    node.dispatchEvent(new EventType(name, { bubbles: true, cancelable: true, ...fields }));
    await microtasks();
  });
}
function setValue(node, value) {
  const prototype = node.ownerDocument.defaultView[node.tagName === "SELECT" ? "HTMLSelectElement" : "HTMLInputElement"].prototype;
  Object.getOwnPropertyDescriptor(prototype, "value").set.call(node, value);
}
function frameNavigations(nodes) { return nodes.navigationCount(); }

async function settled(predicate) {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (predicate()) return;
    await act(async () => { await new Promise(resolve => setTimeout(resolve, 10)); });
  }
  assert.fail("dashboard did not finish its pending request");
}

function text(node) { return node.textContent; }
function findElement(node, id) { return node.id === id ? node : node.querySelector(`#${id}`); }

function detailRecord(source, run, value = 0.5) {
  const metric = { count: 2, last: { step: 1, value }, min: { step: 1, value }, max: { step: 0, value: 1 } };
  return { source, run, state: { command: "python train.py" }, snapshot: null, environment: null, cache: null, params: { learning_rate: 0.1 }, params_truncated: false, metadata_truncated: false, metrics: { accuracy: metric, loss: metric }, metric_count: 2, metrics_truncated: false, metrics_error: null, warnings: [] };
}

async function reviewFixture(t, { count = 3, hosted = false, sources, refresh_clock, updates = false, chart_controller, catalog_failure = false } = {}) {
  const nodes = dashboard(t);
  sources ??= [{ source_id: "local", label: "Local", kind: hosted ? "service" : "local", target_name: null }];
  const runs = Array.from({ length: count }, (_, index) => ({
    run_id: `run-${index}`, task: "train", status: "completed",
    started_at: "2026-10-03T01:00:00Z", finished_at: "2026-10-03T01:00:03Z", exit_code: 0,
  }));
  const model = { nodes, sources, runs, requests: [], override: null, failed_list: false, failed_catalog: catalog_failure, missing_run_ids: new Set(), catalog_revision: "catalog-1", list_revision: "list-1", metadata_revision: "metadata-1", metrics_revision: "metrics-1", stdout_revision: "stdout-1", stderr_revision: "stderr-1", metric_value: 0.5, chart_html: "<html>chart-1</html>", logs: {} };
  globalThis.fetch = async (url, options) => {
    model.requests.push(url);
    const parsed = new URL(url, "http://localhost");
    const query = parsed.searchParams;
    const overridden = model.override?.(parsed, options);
    if (overridden !== undefined && overridden !== null) return overridden;
    if (parsed.pathname === "/api/updates") return updates ? response({ catalog_revision: model.catalog_revision, source_revision: model.list_revision, runs: query.getAll("run_id").map(run_id => {
      const missing = model.missing_run_ids.has(run_id);
      return { run_id, metadata_revision: missing ? null : model.metadata_revision, metrics_revision: missing ? null : model.metrics_revision, stdout_revision: missing ? null : model.stdout_revision, stderr_revision: missing ? null : model.stderr_revision, missing };
    }) }) : response({ error: "Unknown endpoint" }, 404);
    if (parsed.pathname === "/api/catalog") return model.failed_catalog ? response({ error: "Catalog temporarily unavailable" }, 503) : response({ project_name: "Experiments", sources, initial_source: sources[0].source_id, warnings: [], access_mode: hosted ? "hosted" : "local" });
    const source = sources.find(item => item.source_id === query.get("source"));
    assert.ok(source, "all requests must remain in a known source");
    if (parsed.pathname === "/api/runs") {
      if (model.failed_list) return response({ error: "Results are unavailable. Try Refresh." }, 500);
      const visible = runs.filter(run => !query.get("search") || run.run_id.includes(query.get("search")));
      const offset = Number(query.get("offset"));
      const limit = Number(query.get("limit"));
      return response({ source, runs: visible.slice(offset, offset + limit), warnings: [], total_count: visible.length, offset, next_offset: offset + limit < visible.length ? offset + limit : null });
    }
    const metric = { count: 2, last: { step: 1, value: model.metric_value }, min: { step: 1, value: model.metric_value }, max: { step: 0, value: 1 } };
    if (parsed.pathname === "/api/run") {
      if (model.missing_run_ids.has(query.get("run_id"))) return response({ error: "Run not found" }, 404);
      const run = runs.find(item => item.run_id === query.get("run_id"));
      assert.ok(run);
      return response(detailRecord(source, run, model.metric_value));
    }
    if (parsed.pathname === "/api/log") return response({ content: model.logs[query.get("stream")] ?? `${query.get("stream")} training complete`, stream: query.get("stream"), missing: false, truncated: false });
    if (parsed.pathname === "/api/chart") return { ok: true, status: 200, text: async () => model.chart_html };
    if (parsed.pathname === "/api/compare") {
      const metric_names = query.getAll("metric").length ? query.getAll("metric") : ["accuracy", "loss"];
      return response({ source, comparison: { reduction: query.get("reduction"), metric_names, runs: query.getAll("run_id").map(run_id => ({ run_id, run: runs.find(item => item.run_id === run_id), values: Object.fromEntries(metric_names.map(name => [name, metric.last])) })), warnings: [] } });
    }
    assert.fail(`Unexpected dashboard request: ${url}`);
  };
  await mountDashboard(nodes, { ...(refresh_clock ? { refresh_clock } : {}), ...(chart_controller ? { chart_controller } : {}) });
  return model;
}

async function selectRow(nodes, index, checked = true) {
  const input = runCheckbox(nodes, index);
  assert.equal(input.disabled, false, "the row must be selectable");
  if (input.checked !== checked) await click(input);
}
function runCheckbox(nodes, index) { return nodes.get("run-rows").children[index].querySelector('input[type="checkbox"]'); }
function runButton(nodes, index) { return nodes.get("run-rows").children[index].querySelector("button"); }
function metricCheckbox(nodes, name) {
  const label = [...nodes.get("run-metric-options").querySelectorAll("label.metric-choice")].find(item => item.textContent === name);
  assert.ok(label, `Missing metric control: ${name}`); return label.querySelector("input");
}
function chartQuery(nodes) {
  return new URL(nodes.get("chart-frame").src || "/", "http://localhost").searchParams;
}
function comparisonRows(nodes) {
  return nodes.get("comparison-values").querySelectorAll("tbody tr");
}

async function chooseAxis(nodes, value) {
  await click(nodes.get(`x-axis-${value.replace("_", "-")}`));
}
function selectedAxis(nodes) { return ["step", "elapsed", "wall_clock"].find(value => nodes.get(`x-axis-${value.replace("_", "-")}`).checked); }
async function chooseTimeZone(nodes, value) { await click(nodes.get(`time-zone-${value}`)); }
async function chooseReduction(nodes, value) { await click(nodes.get(`reduction-${value}`)); }

test("React renders small fixed choices as native radio tags with accessible group labels", async t => {
  const { nodes } = await reviewFixture(t);
  const document = nodes.document;
  for (const name of ["x_axis", "time_zone", "reduction"]) {
    const inputs = [...document.querySelectorAll(`input[type="radio"][name="${name}"]`)];
    assert.equal(inputs.length, name === "time_zone" ? 2 : 3);
    assert.equal(inputs.filter(input => input.checked).length, 1);
  }
  assert.equal(nodes.get("x-axis-options").querySelector("legend").textContent, "X-axis");
  assert.equal(nodes.get("time-zone-options").getAttribute("aria-describedby"), "time-zone-label");
  assert.equal(nodes.get("time-zone-options").hidden, true);
  assert.equal(nodes.get("x-axis-wall-clock").closest("label").textContent, "Date & time");
  assert.equal(document.querySelector("#x-axis-select, #reduction-select"), null);
});

test("axis choice applies to run and comparison charts and survives manual refresh", async t => {
  const model = await reviewFixture(t), { nodes } = model;
  await settled(() => nodes.get("run-rows").children.length === 3);
  await selectRow(nodes, 0);
  await settled(() => chartQuery(nodes).get("run_id") === "run-0");
  assert.equal(chartQuery(nodes).get("x_axis"), "step");
  await chooseAxis(nodes, "elapsed");
  assert.equal(chartQuery(nodes).get("x_axis"), "elapsed");
  assert.match(nodes.get("chart-note").textContent, /first timestamped metric event/);
  await selectRow(nodes, 1);
  await settled(() => chartQuery(nodes).getAll("run_id").length === 2);
  assert.equal(chartQuery(nodes).get("x_axis"), "elapsed");
  await chooseAxis(nodes, "wall_clock");
  assert.equal(chartQuery(nodes).get("x_axis"), "wall_clock");
  assert.match(nodes.get("chart-note").textContent, /your local timezone/);
  assert.equal(nodes.get("time-zone-options").hidden, false);
  assert.equal(nodes.get("time-zone-label").textContent, localTimeZoneLabel());
  assert.equal(nodes.get("open-chart-label").textContent, "Open UTC chart");
  assert.equal(new URL(nodes.get("open-chart").href, "http://localhost").searchParams.get("x_axis"), "wall_clock");
  assert.deepEqual(chartQuery(nodes).getAll("run_id"), ["run-0", "run-1"]);
  await click(nodes.get("refresh-button"));
  await settled(() => nodes.get("refresh-button").disabled === false);
  assert.equal(selectedAxis(nodes), "wall_clock");
  assert.equal(chartQuery(nodes).get("x_axis"), "wall_clock");
  nodes.get("x-axis-elapsed").checked = false; await emit(nodes.get("x-axis-elapsed"), "change");
  assert.equal(selectedAxis(nodes), "wall_clock", "deselected radio events do not switch axes");
  await chooseAxis(nodes, "step");
  assert.equal(chartQuery(nodes).get("x_axis"), "step");
  assert.equal(nodes.get("time-zone-options").hidden, true);
  assert.equal(nodes.get("open-chart-label").textContent, "Open chart");
});

test("summary tags select reductions in one action and retain the choice on refresh", async t => {
  const model = await reviewFixture(t), { nodes } = model;
  await settled(() => nodes.get("run-rows").children.length === 3);
  await selectRow(nodes, 0); await selectRow(nodes, 1);
  await settled(() => model.requests.some(url => url.startsWith("/api/compare")));
  assert.equal(nodes.get("reduction-last").checked, true);
  await chooseReduction(nodes, "min");
  await settled(() => new URL(model.requests.filter(url => url.startsWith("/api/compare")).at(-1), "http://localhost").searchParams.get("reduction") === "min");
  assert.equal(nodes.get("reduction-min").checked, true);
  assert.equal(nodes.get("reduction-last").checked, false);
  await chooseReduction(nodes, "max");
  await settled(() => new URL(model.requests.filter(url => url.startsWith("/api/compare")).at(-1), "http://localhost").searchParams.get("reduction") === "max");
  assert.equal(nodes.get("reduction-max").checked, true);
  assert.equal(nodes.get("reduction-min").checked, false);
  await click(nodes.get("refresh-button"));
  await settled(() => nodes.get("refresh-button").disabled === false);
  assert.equal(new URL(model.requests.filter(url => url.startsWith("/api/compare")).at(-1), "http://localhost").searchParams.get("reduction"), "max");
  assert.equal(nodes.get("reduction-max").checked, true);
});

test("run inspection opens Charts and keyboard tabs load logs only on demand", async t => {
  const { nodes, requests } = await reviewFixture(t);
  await settled(() => nodes.get("run-rows").children.length === 3);
  assert.equal(nodes.get("run-rows").children[0].children.length, 3);
  assert.match(text(nodes.get("run-rows").children[0].children[1]), /train.*3s/);
  await click(runButton(nodes, 0));
  await settled(() => chartQuery(nodes).get("run_id") === "run-0");
  assert.equal(nodes.get("review-tab-charts").getAttribute("aria-selected"), "true");
  assert.equal(nodes.get("review-panel-charts").hidden, false);
  assert.equal(nodes.get("review-panel-overview").hidden, true);
  assert.equal(nodes.get("review-panel-logs").hidden, true);
  assert.equal(requests.some(url => url.startsWith("/api/log")), false);
  await emit(nodes.get("review-tab-charts"), "keydown", { key: "ArrowRight" });
  assert.equal(nodes.get("review-panel-overview").hidden, false);
  assert.equal(globalThis.document.activeElement, nodes.get("review-tab-overview"));
  assert.match(text(nodes.get("run-detail")), /learning_rate.*0\.1/);
  assert.equal(requests.some(url => url.startsWith("/api/log")), false);
  await emit(nodes.get("review-tab-overview"), "keydown", { key: "End" });
  await settled(() => text(nodes.get("run-logs")).includes("stdout training complete"));
  assert.equal(nodes.get("review-panel-logs").hidden, false);
  assert.equal(nodes.get("review-tab-logs").tabIndex, 0);
  assert.equal(nodes.get("review-tab-charts").tabIndex, -1);
  await emit(nodes.get("review-tab-logs"), "keydown", { key: "Home" });
  await emit(nodes.get("review-tab-charts"), "keydown", { key: "ArrowLeft" });
  assert.equal(nodes.get("review-panel-logs").hidden, false);
  assert.equal(requests.filter(url => url.startsWith("/api/log")).length, 1, "returning to a loaded log must reuse that bounded tail");
});

test("selection updates charts, caps eight runs, and inspection returns to the selection", async t => {
  const { nodes, requests } = await reviewFixture(t, { count: 9 });
  await settled(() => nodes.get("run-rows").children.length === 9);
  await selectRow(nodes, 0);
  assert.equal(nodes.get("review-section").hidden, false);
  await settled(() => chartQuery(nodes).getAll("run_id").length === 1);
  assert.equal(globalThis.document.activeElement, runCheckbox(nodes, 0), "keyboard checkbox focus must survive the debounced review request");
  assert.deepEqual(chartQuery(nodes).getAll("run_id"), ["run-0"]);
  assert.equal(nodes.get("close-review").hidden, true);
  await selectRow(nodes, 1);
  await settled(() => comparisonRows(nodes).length === 2);
  assert.equal(globalThis.document.activeElement, runCheckbox(nodes, 1));
  assert.deepEqual(chartQuery(nodes).getAll("run_id"), ["run-0", "run-1"]);
  assert.equal(nodes.get("review-tab-overview").disabled, true);
  assert.equal(nodes.get("review-tab-logs").disabled, true);
  const before = requests.filter(url => url.startsWith("/api/compare")).length;
  for (let index = 2; index < 8; index++) await selectRow(nodes, index);
  await settled(() => comparisonRows(nodes).length === 8);
  assert.equal(requests.filter(url => url.startsWith("/api/compare")).length, before + 1, "rapid selection changes should make one final comparison request");
  assert.equal(runCheckbox(nodes, 8).disabled, true);
  await click(nodes.get("selected-runs").children[2]);
  await settled(() => comparisonRows(nodes).length === 7);
  assert.equal(chartQuery(nodes).getAll("run_id").includes("run-2"), false);
  await click(runButton(nodes, 8));
  await settled(() => chartQuery(nodes).get("run_id") === "run-8");
  assert.equal(nodes.get("selected-runs").children.length, 7);
  assert.equal(nodes.get("close-review").textContent, "Back to selection");
  await click(nodes.get("close-review"));
  await settled(() => chartQuery(nodes).getAll("run_id").length === 7);
  await click(nodes.get("clear-selection"));
  assert.equal(nodes.get("review-section").hidden, true);
  assert.equal(nodes.get("review-empty").hidden, false);
  assert.equal(nodes.get("selected-runs").children.length, 0);
  assert.equal(chartQuery(nodes).getAll("run_id").length, 0);
});

test("pagination preserves explicit selected IDs while edited filters immediately clear the review", async t => {
  const { nodes, requests } = await reviewFixture(t, { count: 40, hosted: true });
  await settled(() => nodes.get("run-rows").children.length === 20);
  await selectRow(nodes, 0);
  await settled(() => chartQuery(nodes).get("run_id") === "run-0");
  await click(nodes.get("next-page"));
  await settled(() => nodes.get("page-label").textContent === "Page 2");
  assert.match(text(nodes.get("selected-runs")), /run-0/);
  assert.deepEqual(chartQuery(nodes).getAll("run_id"), ["run-0"]);
  await selectRow(nodes, 0);
  await settled(() => comparisonRows(nodes).length === 2);
  assert.deepEqual(chartQuery(nodes).getAll("run_id"), ["run-0", "run-20"]);
  setValue(nodes.get("search-input"), "missing");
  await emit(nodes.get("search-input"), "input");
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
  const model = await reviewFixture(t, { sources, hosted: true });
  const { nodes } = model;
  await settled(() => nodes.get("run-rows").children.length === 3);
  const old_comparison = deferred();
  let comparison_started = false;
  model.override = parsed => {
    if (parsed.pathname === "/api/compare") { comparison_started = true; return old_comparison.promise; }
  };
  await selectRow(nodes, 0); await selectRow(nodes, 1);
  await settled(() => comparison_started);
  const old_catalog = deferred();
  model.override = parsed => parsed.pathname === "/api/catalog" ? old_catalog.promise : undefined;
  await click(nodes.get("refresh-button"));
  setValue(nodes.get("source-select"), sources[1].source_id);
  await emit(nodes.get("source-select"), "change");
  await settled(() => nodes.get("runs-region").getAttribute("aria-busy") === "false");
  await click(runButton(nodes, 0));
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
  const model = await reviewFixture(t);
  const { nodes, requests } = model;
  await settled(() => nodes.get("run-rows").children.length === 3);
  await click(runButton(nodes, 0));
  await settled(() => chartQuery(nodes).get("run_id") === "run-0");
  const accuracy = metricCheckbox(nodes, "accuracy");
  await click(accuracy);
  assert.deepEqual(chartQuery(nodes).getAll("metric"), ["loss"]);
  await click(nodes.get("review-tab-overview"));
  await click(nodes.get("refresh-button"));
  await settled(() => nodes.get("refresh-button").disabled === false);
  assert.equal(nodes.get("review-panel-overview").hidden, false);
  assert.deepEqual(chartQuery(nodes).getAll("metric"), ["loss"]);
  assert.equal(requests.some(url => url.startsWith("/api/log")), false);
  assert.match(nodes.get("updated-at").textContent, /^Last checked at /);
  await click(nodes.get("review-tab-logs"));
  await settled(() => text(nodes.get("run-logs")).includes("stdout training complete"));
  await click(findElement(nodes.get("run-logs"), "log-tab-stderr"));
  await settled(() => text(nodes.get("run-logs")).includes("stderr training complete"));
  await click(nodes.get("refresh-button"));
  await settled(() => nodes.get("refresh-button").disabled === false);
  assert.equal(nodes.get("review-panel-logs").hidden, false);
  assert.equal(requests.filter(url => url.startsWith("/api/log")).length, 3);
  assert.match(text(nodes.get("run-logs")), /stderr training complete/);
  assert.equal(findElement(nodes.get("run-logs"), "log-tab-stderr").getAttribute("aria-selected"), "true");
  const previous_check = nodes.get("updated-at").textContent.split(" · ")[0];
  model.failed_list = true;
  await click(nodes.get("refresh-button"));
  await settled(() => nodes.get("refresh-button").disabled === false);
  assert.equal(nodes.get("updated-at").textContent.split(" · ")[0], previous_check);
  assert.match(nodes.get("global-error").textContent, /Results are unavailable/);
});

test("a tab change during Refresh does not strand an unfinished initial run review", async t => {
  const model = await reviewFixture(t);
  const { nodes } = model;
  await settled(() => nodes.get("run-rows").children.length === 3);
  const initial_detail = deferred();
  const refresh_catalog = deferred();
  let detail_started = false;
  model.override = parsed => {
    if (parsed.pathname === "/api/run") { detail_started = true; return initial_detail.promise; }
    if (parsed.pathname === "/api/catalog") return refresh_catalog.promise;
  };
  await selectRow(nodes, 0);
  await settled(() => detail_started);
  await click(nodes.get("refresh-button"));
  await emit(nodes.get("review-tab-charts"), "keydown", { key: "ArrowRight" });
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
  await mountDashboard(nodes);
  await settled(() => nodes.get("run-count").textContent === "No synced runs yet");
  assert.deepEqual(requests, ["/api/catalog"], "empty catalog must not request an invalid source");
  assert.equal(nodes.get("logout-form").hidden, false);
  assert.equal(nodes.get("source-select").disabled, true);
  assert.match(text(nodes.get("list-empty")), /No synced runs yet/);
  const guide = Array.from(nodes.get("list-empty").children).at(-1).children[0];
  assert.match(guide.href, /github\.com\/agentic-rs\/expri\/.*self-hosted-service\.md$/);
  await settled(() => nodes.get("refresh-button").disabled === false);
  catalog = { ...catalog, sources: [source], initial_source: source.source_id, warnings: [{ message: "The overview is limited to 500 runs." }] };
  await emit(nodes.get("refresh-button"), "click");
  await settled(() => nodes.get("run-rows").children.length === 20);
  assert.equal(nodes.get("source-select").value, source.source_id);
  assert.equal(nodes.get("source-select").disabled, false);
  assert.equal(nodes.get("search-input").disabled, false);
  assert.equal(nodes.get("source-select").children[0].textContent, "Vision / gpu-1 · Synced");
  assert.match(nodes.get("source-note").textContent, /^Synced results/);
  assert.match(text(nodes.get("catalog-warnings")), /limited to 500 runs/);
  await emit(nodes.get("next-page"), "click");
  await settled(() => nodes.get("page-label").textContent === "Page 2");
  await emit(nodes.get("previous-page"), "click");
  await settled(() => nodes.get("page-label").textContent === "Page 1");
  setValue(nodes.get("search-input"), "accuracy");
  await emit(nodes.get("search-input"), "input");
  setValue(nodes.get("task-input"), "train");
  await emit(nodes.get("task-input"), "input");
  setValue(nodes.get("status-select"), "completed");
  await emit(nodes.get("status-select"), "change");
  await settled(() => requests.some(url => url.includes("search=accuracy")));
  const query = new URL(requests.at(-1), "http://localhost").searchParams;
  assert.equal(query.get("task"), "train");
  assert.equal(query.get("status"), "completed");
  assert.equal(query.get("offset"), "0");
  await settled(() => nodes.get("runs-region").getAttribute("aria-busy") === "false");
});

test("local catalogs retain cached labels, 100-row pages, and no logout control", async t => {
  const nodes = dashboard(t);
  const source = { source_id: "cached:gpu", label: "gpu · Cached", kind: "cached", target_name: "gpu" };
  globalThis.fetch = async url => {
    if (url === "/api/catalog") return response({ project_name: "Local workspace", sources: [source], initial_source: source.source_id, warnings: [] });
    assert.equal(new URL(url, "http://localhost").searchParams.get("limit"), "100");
    return response({ source, runs: [], warnings: [], total_count: 0, offset: 0, next_offset: null });
  };
  await mountDashboard(nodes);
  await settled(() => nodes.get("run-count").textContent === "No matching runs");
  assert.equal(nodes.get("logout-form").hidden, true);
  assert.equal(nodes.get("source-select").children[0].textContent, source.label);
  assert.match(nodes.get("source-note").textContent, /^Cached remote results/);
  assert.match(text(nodes.get("list-empty")), /expri runs pull/);
  assert.equal(nodes.get("dashboard-kind").textContent, "expri · Local experiment review");
});

test("login and logout use browser POST forms without application credentials", async t => {
  const login = readFileSync(new URL("./login.html", import.meta.url), "utf8");
  assert.match(login, /<form[^>]+method="post"[^>]+action="\/login"/);
  assert.match(login, /<input[^>]+name="password"[^>]+type="password"[^>]+autocomplete="current-password"/);
  assert.equal(login.split("<!-- LOGIN_ERROR -->").length, 2);
  assert.doesNotMatch(login, /<script\b|localStorage|Bearer\s|type="hidden"/i);
  const { nodes } = await reviewFixture(t, { hosted: true });
  const logout = nodes.get("logout-form");
  assert.equal(logout.method, "post");
  assert.equal(logout.getAttribute("action"), "/logout");
  assert.equal(logout.hidden, false);
  assert.equal(logout.querySelector('button[type="submit"]').textContent, "Sign out");
  assert.equal(logout.querySelector("input"), null, "logout does not serialize session credentials into the application DOM");
});

async function flush() { await act(async () => { await microtasks(); }); }
class RefreshClock {
  time = 0;
  sequence = 0;
  timers = new Map();
  now = () => this.time;
  set_timeout = (callback, delay_ms) => { const id = ++this.sequence; this.timers.set(id, { callback, due: this.time + delay_ms }); return id; };
  clear_timeout = timer => { this.timers.delete(timer); };
  async advance(delay_ms) {
    const target = this.time + delay_ms;
    for (;;) {
      const next = [...this.timers].filter(([, timer]) => timer.due <= target).sort((first, second) => first[1].due - second[1].due)[0];
      if (!next) break;
      this.time = next[1].due; this.timers.delete(next[0]);
      await act(async () => { next[1].callback(); await microtasks(); });
    }
    this.time = target; await flush();
  }
}
async function autoFixture(t, fields = {}) {
  const clock = new RefreshClock(), chart = { interacting: false, ready: true, status: "ready", previews: [], time_zones: [], disposals: 0 };
  const controller = { dispose() { chart.disposals++; }, setTimeZone: value => chart.time_zones.push(value), isInteracting: () => chart.interacting, previewStatus: () => chart.status, replacePreview: html => { if (!chart.ready) return false; chart.previews.push(html); return true; } };
  const model = await reviewFixture(t, { hosted: true, updates: true, ...fields, refresh_clock: clock, chart_controller: controller });
  return { ...model, model, clock, chart, page_document: globalThis.document };
}
async function inspect(model, index = 0) {
  await settled(() => model.nodes.get("run-rows").children.length > index);
  await click(runButton(model.nodes, index));
  await settled(() => chartQuery(model.nodes).get("run_id") === model.runs[index].run_id);
}
function requestCounts(requests) {
  return requests.reduce((counts, url) => { const path = new URL(url, "http://localhost").pathname; counts[path] = (counts[path] ?? 0) + 1; return counts; }, {});
}

test("React retains the actual chart iframe across reviews, tags, tabs and refresh", async t => {
  const model = await autoFixture(t), { nodes } = model;
  const frame = nodes.get("chart-frame");
  const assert_stable = () => {
    assert.equal(nodes.get("chart-frame"), frame, "state updates must not remount the chart iframe");
    assert.equal(frame.isConnected, true);
    assert.equal(frame.getAttribute("sandbox"), "allow-same-origin");
  };
  await inspect(model); assert_stable(); await model.clock.advance(5_000);
  const document = frame.contentDocument, writes = frameNavigations(nodes);
  await chooseAxis(nodes, "wall_clock"); assert_stable();
  await chooseTimeZone(nodes, "utc"); assert_stable();
  assert.equal(frame.contentDocument, document, "axis and timezone controls retain the existing chart security context");
  assert.equal(frameNavigations(nodes), writes);
  await click(nodes.get("review-tab-overview")); assert_stable();
  await click(nodes.get("refresh-button")); assert_stable();
  await click(nodes.get("review-tab-charts"));
  await selectRow(nodes, 0); await selectRow(nodes, 1);
  await settled(() => comparisonRows(nodes).length === 2); assert_stable();
  await chooseReduction(nodes, "max"); assert_stable();
  await click(nodes.get("clear-selection")); assert_stable();
  assert.equal(nodes.get("review-section").hidden, true);
});

test("unmount aborts foreground requests, removes React content and disposes the chart once", async t => {
  const model = await autoFixture(t), pending = deferred(); let signal;
  t.after(() => pending.resolve(response(detailRecord(model.sources[0], model.runs[0]))));
  model.model.override = (parsed, options) => {
    if (parsed.pathname === "/api/run") { signal = options.signal; return pending.promise; }
  };
  const button = runButton(model.nodes, 0);
  await click(button);
  assert.ok(signal && !signal.aborted);
  const count = model.requests.length, cleanup = model.nodes.cleanup;
  await act(async () => { cleanup(); cleanup(); await microtasks(); });
  model.nodes.cleanup = null;
  assert.equal(signal.aborted, true);
  assert.equal(model.chart.disposals, 1);
  assert.equal(model.clock.timers.size, 0);
  assert.equal(model.nodes.document.getElementById("dashboard-root").childElementCount, 0);
  pending.resolve(response(detailRecord(model.sources[0], model.runs[0])));
  await flush(); await model.clock.advance(60_000); await click(button);
  assert.equal(model.requests.length, count, "late responses and detached controls must not restart work after unmount");
  assert.equal(model.nodes.document.getElementById("chart-frame"), null);
});

test("unmount cancels an outstanding automatic probe and every scheduled refresh", async t => {
  const model = await autoFixture(t); await inspect(model); await model.clock.advance(5_000);
  const pending = deferred(); let signal;
  t.after(() => pending.resolve(response({ catalog_revision: "late", source_revision: "late", runs: [] })));
  model.model.override = (parsed, options) => {
    if (parsed.pathname === "/api/updates") { signal = options.signal; return pending.promise; }
  };
  await model.clock.advance(5_000);
  assert.ok(signal && !signal.aborted);
  const count = model.requests.length;
  await act(async () => { model.nodes.cleanup(); await microtasks(); });
  model.nodes.cleanup = null;
  assert.equal(signal.aborted, true);
  assert.equal(model.chart.disposals, 1);
  assert.equal(model.clock.timers.size, 0);
  pending.resolve(response({ catalog_revision: "late", source_revision: "late", runs: [] }));
  await flush(); await model.clock.advance(90_000);
  assert.equal(model.requests.length, count);
});

test("auto checks settle into probe-only traffic without rebuilding controls or navigating the chart", async t => {
  const model = await autoFixture(t); await inspect(model);
  const { nodes, clock, chart, requests } = model;
  await clock.advance(5_000); // Establish a safe baseline after the initial iframe navigation.
  const counts = requestCounts(requests), frame_writes = frameNavigations(nodes);
  const choices = nodes.get("run-metric-options").children[0], overview = nodes.get("run-detail").children[0];
  const checkbox = choices.children[0].children[0]; checkbox.focus();
  await clock.advance(15_000);
  assert.equal(requestCounts(requests)["/api/updates"], counts["/api/updates"] + 3);
  for (const path of ["/api/catalog", "/api/runs", "/api/run", "/api/chart"]) assert.equal(requestCounts(requests)[path], counts[path], path);
  assert.equal(frameNavigations(nodes), frame_writes);
  assert.equal(nodes.get("run-metric-options").children[0], choices);
  assert.equal(nodes.get("run-detail").children[0], overview);
  assert.equal(globalThis.document.activeElement, checkbox);
  assert.equal(chart.previews.length, 1);
});

test("an exact metric draft survives updates within its review and clears for another run or source", async t => {
  const sources = [
    { source_id: "hosted:a", label: "A", kind: "service", target_name: null },
    { source_id: "hosted:b", label: "B", kind: "service", target_name: null },
  ];
  const model = await autoFixture(t, { sources }); await inspect(model);
  const { nodes } = model;
  const input = () => {
    const node = nodes.get("run-metric-options").querySelector('.metric-add input[type="text"]');
    assert.ok(node, "Exact metric name input must be rendered"); return node;
  };
  await click(nodes.get("run-metric-controls").querySelector("summary"));
  const draft = input(); draft.focus(); setValue(draft, "validation/draft");
  await emit(draft, "input");
  await model.clock.advance(5_000);
  model.model.metrics_revision = "metrics-2"; model.model.metric_value = 0.125;
  await model.clock.advance(5_000);
  assert.equal(input(), draft, "a quiet update must retain the draft's DOM node");
  assert.equal(draft.value, "validation/draft");
  assert.equal(nodes.document.activeElement, draft, "a quiet update must preserve typing focus");
  await click(nodes.get("refresh-button"));
  await settled(() => nodes.get("refresh-button").disabled === false);
  assert.equal(input(), draft); assert.equal(draft.value, "validation/draft");
  await click(nodes.get("review-tab-overview")); await click(nodes.get("review-tab-charts"));
  await chooseAxis(nodes, "elapsed");
  assert.equal(input(), draft); assert.equal(draft.value, "validation/draft");
  await inspect(model, 1);
  assert.notEqual(input(), draft, "a new run must receive a fresh metric draft");
  assert.equal(draft.isConnected, false); assert.equal(input().value, "");
  setValue(input(), "validation/other-run"); await emit(input(), "input");
  setValue(nodes.get("source-select"), "hosted:b"); await emit(nodes.get("source-select"), "change");
  await settled(() => nodes.get("runs-region").getAttribute("aria-busy") === "false");
  await inspect(model, 1);
  assert.equal(chartQuery(nodes).get("source"), "hosted:b");
  assert.equal(input().value, "", "the same run ID on another source must not inherit its draft");
});

test("changed metrics update only the active run and chart while preserving the metric selection", async t => {
  const model = await autoFixture(t); await inspect(model);
  const accuracy = metricCheckbox(model.nodes, "accuracy");
  await click(accuracy);
  await model.clock.advance(5_000);
  const counts = requestCounts(model.requests), choices = model.nodes.get("run-metric-options").children[0];
  const writes = frameNavigations(model.nodes);
  model.model.metrics_revision = "metrics-2"; model.model.metric_value = 0.125; model.model.chart_html = "<html>chart-2</html>";
  await model.clock.advance(5_000);
  assert.equal(requestCounts(model.requests)["/api/run"], counts["/api/run"] + 1);
  assert.equal(requestCounts(model.requests)["/api/chart"], counts["/api/chart"] + 1);
  for (const path of ["/api/catalog", "/api/runs", "/api/log"]) assert.equal(requestCounts(model.requests)[path], counts[path]);
  assert.equal(model.nodes.get("run-metric-options").children[0], choices);
  assert.deepEqual(chartQuery(model.nodes).getAll("metric"), ["loss"]);
  assert.equal(frameNavigations(model.nodes), writes);
  assert.equal(model.chart.previews.at(-1), "<html>chart-2</html>");
  assert.match(text(model.nodes.get("run-detail")), /0\.125/);
});

test("time-axis changes replace the preview in place and remain selected for incremental updates", async t => {
  const model = await autoFixture(t); await inspect(model); await model.clock.advance(5_000);
  const writes = frameNavigations(model.nodes);
  model.model.chart_html = "<html>elapsed-chart</html>";
  await chooseAxis(model.nodes, "elapsed");
  await settled(() => model.chart.previews.at(-1) === "<html>elapsed-chart</html>");
  assert.equal(frameNavigations(model.nodes), writes, "axis switching preserves the iframe and its visibility state");
  assert.equal(model.nodes.get("chart-card").getAttribute("aria-busy"), "false");
  assert.equal(new URL(model.nodes.get("open-chart").href, "http://localhost").searchParams.get("x_axis"), "elapsed");
  model.model.metrics_revision = "metrics-2"; model.model.chart_html = "<html>elapsed-chart-updated</html>";
  await model.clock.advance(5_000);
  assert.equal(model.chart.previews.at(-1), "<html>elapsed-chart-updated</html>");
  assert.equal(new URL(model.requests.filter(url => url.startsWith("/api/chart")).at(-1), "http://localhost").searchParams.get("x_axis"), "elapsed");
  assert.equal(selectedAxis(model.nodes), "elapsed");
  assert.equal(frameNavigations(model.nodes), writes);
});

test("timezone tags redraw locally without requests and persist across axes and refresh", async t => {
  const model = await autoFixture(t); await inspect(model); await model.clock.advance(5_000);
  assert.deepEqual(model.chart.time_zones, ["local"], "date & time defaults to the viewer’s timezone");
  await chooseAxis(model.nodes, "wall_clock");
  await settled(() => model.nodes.get("chart-card").getAttribute("aria-busy") === "false");
  const requests = model.requests.length, writes = frameNavigations(model.nodes), previews = [...model.chart.previews];
  model.nodes.get("time-zone-utc").focus(); await chooseTimeZone(model.nodes, "utc");
  assert.deepEqual(model.chart.time_zones, ["local", "utc"]);
  assert.equal(model.nodes.get("time-zone-utc").checked, true);
  assert.equal(model.nodes.get("time-zone-local").checked, false);
  assert.equal(model.nodes.get("time-zone-label").textContent, "UTC");
  assert.match(model.nodes.get("chart-note").textContent, /Recorded date & time in UTC/);
  assert.equal(model.requests.length, requests, "timezone is a display choice and does not refetch the chart");
  assert.equal(frameNavigations(model.nodes), writes);
  assert.deepEqual(model.chart.previews, previews);
  assert.equal(globalThis.document.activeElement, model.nodes.get("time-zone-utc"));
  await chooseTimeZone(model.nodes, "utc");
  model.nodes.get("time-zone-local").checked = false; await emit(model.nodes.get("time-zone-local"), "change");
  assert.deepEqual(model.chart.time_zones, ["local", "utc"], "unchanged and deselected radio events do not redraw");
  await chooseAxis(model.nodes, "elapsed");
  await settled(() => model.nodes.get("chart-card").getAttribute("aria-busy") === "false");
  assert.equal(model.nodes.get("time-zone-options").hidden, true);
  await chooseAxis(model.nodes, "wall_clock");
  await settled(() => model.nodes.get("chart-card").getAttribute("aria-busy") === "false");
  assert.equal(model.nodes.get("time-zone-utc").checked, true);
  model.model.metrics_revision = "metrics-2"; model.model.chart_html = "<html>utc-updated-chart</html>";
  await model.clock.advance(5_000);
  assert.equal(model.chart.previews.at(-1), "<html>utc-updated-chart</html>");
  assert.equal(model.nodes.get("time-zone-utc").checked, true);
  assert.equal(selectedAxis(model.nodes), "wall_clock");
  await chooseTimeZone(model.nodes, "local");
  assert.equal(model.nodes.get("time-zone-label").textContent, localTimeZoneLabel());
  assert.deepEqual(model.chart.time_zones, ["local", "utc", "local"]);
});

test("late axis responses cannot replace a newer choice or review", async t => {
  const model = await autoFixture(t); await inspect(model);
  const pending = deferred(), signals = [];
  model.model.override = (parsed, options) => {
    if (parsed.pathname === "/api/chart" && parsed.searchParams.get("x_axis") === "elapsed") {
      signals.push(options.signal); return pending.promise;
    }
  };
  await chooseAxis(model.nodes, "elapsed");
  assert.equal(model.nodes.get("chart-card").getAttribute("aria-busy"), "true");
  model.model.chart_html = "<html>wall-clock-chart</html>";
  await chooseAxis(model.nodes, "wall_clock");
  await settled(() => model.chart.previews.at(-1) === "<html>wall-clock-chart</html>");
  assert.equal(signals[0].aborted, true);
  pending.resolve({ ok: true, status: 200, text: async () => "<html>stale-elapsed-chart</html>" });
  await flush();
  assert.equal(model.chart.previews.at(-1), "<html>wall-clock-chart</html>");
  assert.equal(model.nodes.get("chart-card").getAttribute("aria-busy"), "false");
  const next = deferred();
  model.model.override = parsed => parsed.pathname === "/api/chart" && parsed.searchParams.get("x_axis") === "elapsed" ? next.promise : null;
  await chooseAxis(model.nodes, "elapsed");
  await click(runButton(model.nodes, 1));
  await settled(() => chartQuery(model.nodes).get("run_id") === "run-1");
  next.resolve({ ok: true, status: 200, text: async () => "<html>old-review-chart</html>" });
  await flush();
  assert.equal(model.chart.previews.includes("<html>old-review-chart</html>"), false);
  assert.equal(chartQuery(model.nodes).get("x_axis"), "elapsed");
});

test("an axis load failure retains the last chart and automatic recovery clears its scoped error", async t => {
  const model = await autoFixture(t); await inspect(model); await model.clock.advance(5_000);
  const previews = [...model.chart.previews], writes = frameNavigations(model.nodes);
  model.model.override = parsed => parsed.pathname === "/api/chart" ? response({ error: "Connection temporarily unavailable" }, 503) : null;
  await chooseAxis(model.nodes, "elapsed");
  await settled(() => model.nodes.get("chart-error").hidden === false);
  assert.deepEqual(model.chart.previews, previews);
  assert.equal(frameNavigations(model.nodes), writes);
  assert.match(model.nodes.get("chart-error").textContent, /Showing the last loaded chart/);
  model.model.override = null; model.model.chart_html = "<html>recovered-elapsed-chart</html>";
  await model.clock.advance(5_000);
  assert.equal(model.chart.previews.at(-1), "<html>recovered-elapsed-chart</html>");
  assert.equal(model.nodes.get("chart-error").hidden, true);
  assert.equal(selectedAxis(model.nodes), "elapsed");
});

test("changing only the chart axis does not dismiss an unrelated metadata failure", async t => {
  const model = await autoFixture(t); await inspect(model);
  model.model.override = parsed => parsed.pathname === "/api/run" ? response({ error: "Run metadata temporarily unavailable" }, 503) : null;
  await click(model.nodes.get("refresh-button"));
  await settled(() => model.nodes.get("review-error").hidden === false);
  const error = model.nodes.get("review-error");
  model.model.chart_html = "<html>elapsed-chart</html>";
  await chooseAxis(model.nodes, "elapsed");
  await settled(() => model.chart.previews.at(-1) === "<html>elapsed-chart</html>");
  assert.equal(error.hidden, false);
  assert.equal(error.textContent, "Run metadata temporarily unavailable");
});

test("auto and manual refresh preserve pagination and explicit selections outside the page", async t => {
  const model = await autoFixture(t, { count: 45 }); await inspect(model);
  await selectRow(model.nodes, 0);
  await settled(() => chartQuery(model.nodes).get("run_id") === "run-0");
  await click(model.nodes.get("next-page"));
  await settled(() => model.nodes.get("page-label").textContent === "Page 2");
  await model.clock.advance(5_000);
  const focused = runCheckbox(model.nodes, 0); focused.focus();
  model.model.list_revision = "list-2"; model.runs[20].status = "running";
  await model.clock.advance(5_000);
  assert.equal(model.nodes.get("page-label").textContent, "Page 2");
  assert.equal(runButton(model.nodes, 0).textContent, "run-20");
  assert.match(text(model.nodes.get("selected-runs")), /run-0/);
  assert.deepEqual(chartQuery(model.nodes).getAll("run_id"), ["run-0"]);
  assert.equal(globalThis.document.activeElement, runCheckbox(model.nodes, 0));
  const writes = frameNavigations(model.nodes);
  await click(model.nodes.get("refresh-button"));
  await settled(() => model.nodes.get("refresh-button").disabled === false);
  assert.equal(model.nodes.get("page-label").textContent, "Page 2");
  assert.equal(frameNavigations(model.nodes), writes);
});

test("only the visible log stream refreshes and a reader's scroll position is retained", async t => {
  const model = await autoFixture(t); await inspect(model);
  await click(model.nodes.get("review-tab-logs"));
  await settled(() => text(model.nodes.get("run-logs")).includes("stdout training complete"));
  await click(findElement(model.nodes.get("run-logs"), "log-tab-stderr"));
  await settled(() => text(model.nodes.get("run-logs")).includes("stderr training complete"));
  await model.clock.advance(5_000);
  const output = findElement(model.nodes.get("run-logs"), "log-output"), stderr = findElement(model.nodes.get("run-logs"), "log-tab-stderr");
  Object.defineProperty(output, "scrollHeight", { configurable: true, value: 1_000 }); Object.defineProperty(output, "clientHeight", { configurable: true, value: 100 }); output.scrollTop = 200;
  const counts = requestCounts(model.requests);
  model.model.stdout_revision = "stdout-2";
  await model.clock.advance(5_000);
  assert.equal(requestCounts(model.requests)["/api/log"], counts["/api/log"]);
  model.model.stderr_revision = "stderr-2"; model.model.logs.stderr = "new stderr output";
  await model.clock.advance(5_000);
  assert.equal(output.textContent, "new stderr output");
  assert.equal(output.scrollTop, 200);
  assert.equal(findElement(model.nodes.get("run-logs"), "log-tab-stderr"), stderr);
  assert.equal(stderr.getAttribute("aria-selected"), "true");
  assert.equal(requestCounts(model.requests)["/api/chart"], counts["/api/chart"]);
  assert.equal(requestCounts(model.requests)["/api/run"], counts["/api/run"]);
});

test("a failed cycle keeps its successful timestamp and retries unacknowledged revisions", async t => {
  const model = await autoFixture(t); await inspect(model); await model.clock.advance(5_000);
  const checked = model.nodes.get("updated-at").textContent.split(" · ")[0], previews = model.chart.previews.length;
  model.model.metrics_revision = "metrics-2"; model.model.metric_value = 7; model.model.chart_html = "<html>chart-2</html>";
  model.model.override = parsed => parsed.pathname === "/api/run" ? response({ error: "temporarily unavailable" }, 503) : undefined;
  await model.clock.advance(5_000);
  assert.equal(model.nodes.get("updated-at").textContent.split(" · ")[0], checked);
  assert.match(model.nodes.get("updated-at").textContent, /retrying in 10s/);
  assert.equal(model.chart.previews.length, previews);
  model.model.override = null;
  await model.clock.advance(10_000);
  assert.equal(model.chart.previews.at(-1), "<html>chart-2</html>");
  assert.match(text(model.nodes.get("run-detail")), /7/);
  assert.match(model.nodes.get("updated-at").textContent, /Auto updates every 5s/);
});

test("an active chart drag defers adoption and the next check still applies the new revision", async t => {
  const model = await autoFixture(t); await inspect(model); await model.clock.advance(5_000);
  model.chart.interacting = true; model.model.metrics_revision = "metrics-2"; model.model.chart_html = "<html>chart-2</html>";
  await model.clock.advance(5_000); assert.equal(model.chart.previews.length, 1);
  const counts = requestCounts(model.requests);
  await model.clock.advance(10_000);
  for (const path of ["/api/catalog", "/api/runs", "/api/run", "/api/chart"]) assert.equal(requestCounts(model.requests)[path], counts[path], "a held drag does not reread completed views");
  model.chart.interacting = false;
  await model.clock.advance(5_000);
  assert.equal(model.chart.previews.length, 2);
  assert.equal(model.chart.previews.at(-1), "<html>chart-2</html>");
});

test("automatic recovery completes an initially failed comparison and reopening a review gets a fresh baseline", async t => {
  const model = await autoFixture(t);
  await settled(() => model.nodes.get("run-rows").children.length === 3);
  model.model.override = parsed => parsed.pathname === "/api/compare" ? response({ error: "temporarily unavailable" }, 503) : undefined;
  await selectRow(model.nodes, 0); await selectRow(model.nodes, 1);
  await settled(() => model.nodes.get("review-error").hidden === false);
  assert.equal(model.nodes.get("chart-frame").src, "");
  model.model.override = null;
  await model.clock.advance(5_000);
  assert.equal(model.nodes.get("review-loading").hidden, true);
  assert.equal(model.nodes.get("review-error").hidden, true);
  assert.deepEqual(chartQuery(model.nodes).getAll("run_id"), ["run-0", "run-1"]);
  assert.equal(model.nodes.get("compare-metric-options").children.length > 0, true);
  assert.equal(model.chart.previews.length, 1);
  const writes = frameNavigations(model.nodes);
  await chooseReduction(model.nodes, "max");
  await settled(() => model.nodes.get("comparison-values").getAttribute("aria-busy") === "false");
  assert.equal(frameNavigations(model.nodes), writes, "scalar reduction does not navigate identical chart metrics");
  await click(model.nodes.get("clear-selection"));
  await selectRow(model.nodes, 0); await selectRow(model.nodes, 1);
  await settled(() => chartQuery(model.nodes).getAll("run_id").length === 2);
  await model.clock.advance(5_000);
  assert.equal(model.chart.previews.length, 2, "a new review validates its initial preview again");
});

test("a completed invalid initial chart retries navigation once and slow pending navigation is left alone", async t => {
  const model = await autoFixture(t); await inspect(model);
  model.chart.status = "invalid";
  const writes = frameNavigations(model.nodes), counts = requestCounts(model.requests);
  await model.clock.advance(5_000);
  assert.equal(frameNavigations(model.nodes), writes + 1);
  assert.equal(model.chart.previews.length, 0);
  assert.equal(requestCounts(model.requests)["/api/chart"], counts["/api/chart"]);
  model.chart.status = "loading";
  await model.clock.advance(15_000);
  assert.equal(frameNavigations(model.nodes), writes + 1, "polls must not restart an outstanding iframe navigation");
  assert.equal(requestCounts(model.requests)["/api/chart"], counts["/api/chart"]);
  model.chart.status = "ready";
  await model.clock.advance(5_000);
  assert.equal(model.chart.previews.length, 1);
  assert.equal(frameNavigations(model.nodes), writes + 1);
  await model.clock.advance(5_000);
  assert.equal(model.chart.previews.length, 1, "successful recovery establishes its normal revision baseline");
});

test("a failed initial hosted catalog recovers through a catalog-only probe", async t => {
  const source = { source_id: "hosted:worker", label: "Worker", kind: "service", target_name: null };
  const model = await autoFixture(t, { sources: [source], catalog_failure: true });
  await settled(() => model.nodes.get("global-error").hidden === false);
  assert.match(model.nodes.get("global-error").textContent, /Catalog temporarily unavailable/);
  model.model.failed_catalog = false;
  model.model.override = parsed => {
    if (parsed.pathname !== "/api/updates") return;
    if (parsed.searchParams.get("source") === "local") return response({ error: "Unknown hosted source" }, 400);
    if (!parsed.searchParams.has("source")) return response({ catalog_revision: "catalog-1", source_revision: null, runs: [] });
  };
  await model.clock.advance(5_000);
  assert.equal(model.nodes.get("run-rows").children.length, 3);
  assert.equal(model.nodes.get("source-select").value, source.source_id);
  assert.equal(model.nodes.get("global-error").hidden, true);
  const probe = new URL(model.requests.find(url => url.startsWith("/api/updates")), "http://localhost");
  assert.equal(probe.searchParams.has("source"), false);
});

test("an initially failed run can recover directly into Logs without overlapping log requests", async t => {
  const model = await autoFixture(t);
  await settled(() => model.nodes.get("run-rows").children.length === 3);
  model.model.override = parsed => parsed.pathname === "/api/run" ? response({ error: "Run temporarily unavailable" }, 503) : undefined;
  await click(runButton(model.nodes, 0));
  await settled(() => model.nodes.get("review-error").hidden === false);
  await click(model.nodes.get("review-tab-logs"));
  assert.equal(requestCounts(model.requests)["/api/log"], undefined);
  model.model.override = null;
  await model.clock.advance(5_000);
  await settled(() => text(model.nodes.get("run-logs")).includes("stdout training complete"));
  assert.equal(model.nodes.get("review-panel-logs").hidden, false);
  assert.equal(model.nodes.get("review-error").hidden, true);
  assert.equal(model.nodes.get("review-loading").hidden, true);
  assert.equal(requestCounts(model.requests)["/api/log"], 1, "detail recovery starts one initial foreground log load");
  assert.equal(requestCounts(model.requests)["/api/chart"], undefined);
});

test("failed page loads recover even with unchanged revisions and identical retries restore controls", async t => {
  const model = await autoFixture(t, { count: 45 });
  await settled(() => model.nodes.get("run-rows").children.length === 20);
  await model.clock.advance(5_000);
  model.model.failed_list = true;
  await click(model.nodes.get("next-page"));
  await settled(() => model.nodes.get("run-count").textContent === "Could not read run history");
  model.model.failed_list = false;
  await model.clock.advance(5_000);
  assert.equal(model.nodes.get("page-label").textContent, "Page 2");
  assert.equal(runButton(model.nodes, 0).textContent, "run-20");
  model.model.failed_list = true;
  await click(model.nodes.get("refresh-button"));
  await settled(() => model.nodes.get("refresh-button").disabled === false);
  assert.equal(model.nodes.get("run-count").textContent, "Could not read run history");
  const row = model.nodes.get("run-rows").children[0];
  model.model.failed_list = false;
  await model.clock.advance(5_000);
  assert.equal(model.nodes.get("run-rows").children[0], row, "an identical quiet retry does not rebuild the table");
  assert.equal(model.nodes.get("previous-page").disabled, false);
  assert.equal(model.nodes.get("next-page").disabled, false);
  assert.match(model.nodes.get("run-count").textContent, /21–40 of 45 runs/);
  assert.equal(model.nodes.get("global-error").hidden, true);
});

test("missing selected runs retain their preview without artifact retries and reappear with identical revisions", async t => {
  const model = await autoFixture(t, { hosted: false });
  model.model.catalog_revision = null; model.model.list_revision = null;
  await settled(() => model.nodes.get("run-rows").children.length === 3);
  await selectRow(model.nodes, 0);
  await settled(() => chartQuery(model.nodes).get("run_id") === "run-0");
  await model.clock.advance(5_000);
  const counts = requestCounts(model.requests), writes = frameNavigations(model.nodes);
  const removed = model.runs.shift(); model.model.missing_run_ids.add(removed.run_id);
  await model.clock.advance(20_000);
  assert.match(model.nodes.get("review-error").textContent, /selected runs are no longer available.*last successful preview/);
  assert.equal(model.nodes.get("review-error").hidden, false);
  assert.equal(frameNavigations(model.nodes), writes);
  assert.equal(model.chart.previews.length, 1);
  for (const path of ["/api/run", "/api/chart", "/api/log"]) assert.equal(requestCounts(model.requests)[path], counts[path]);
  await click(model.nodes.get("review-tab-logs")); await flush();
  await model.clock.advance(5_000);
  assert.equal(requestCounts(model.requests)["/api/log"], counts["/api/log"], "known missing logs are not requested when changing tabs");
  assert.match(text(model.nodes.get("selected-runs")), /run-0/);
  model.runs.unshift(removed); model.model.missing_run_ids.delete(removed.run_id);
  await model.clock.advance(5_000);
  assert.equal(model.nodes.get("review-error").hidden, true);
  assert.match(text(model.nodes.get("run-logs")), /stdout training complete/);
  await click(model.nodes.get("review-tab-charts")); await model.clock.advance(5_000);
  assert.equal(requestCounts(model.requests)["/api/run"], counts["/api/run"] + 1);
  assert.equal(requestCounts(model.requests)["/api/chart"], counts["/api/chart"] + 1);
  assert.equal(frameNavigations(model.nodes), writes);
  assert.equal(model.nodes.get("review-error").hidden, true);
});

test("completed chart load failures use exponential backoff while a valid recovery restores five-second checks", async t => {
  const model = await autoFixture(t); await inspect(model);
  model.chart.status = "invalid";
  const writes = frameNavigations(model.nodes);
  let retries = 0;
  for (const [delay, retry] of [[5_000, 10], [10_000, 20], [20_000, 40], [40_000, 60]]) {
    await model.clock.advance(delay); retries++;
    assert.equal(frameNavigations(model.nodes), writes + retries);
    assert.match(model.nodes.get("updated-at").textContent, new RegExp(`retrying in ${retry}s`));
  }
  model.chart.status = "ready";
  await model.clock.advance(60_000);
  assert.equal(model.chart.previews.length, 1);
  assert.match(model.nodes.get("updated-at").textContent, /Auto updates every 5s/);
});

test("source changes cancel only obsolete background work and late probes cannot restore it", async t => {
  const sources = [{ source_id: "hosted:a", label: "A", kind: "service", target_name: null }, { source_id: "hosted:b", label: "B", kind: "service", target_name: null }];
  const model = await autoFixture(t, { sources }); await inspect(model); await model.clock.advance(5_000);
  const pending = deferred(); let signal;
  model.model.override = (parsed, options) => {
    if (parsed.pathname === "/api/updates") { signal = options.signal; return pending.promise; }
  };
  await model.clock.advance(5_000);
  setValue(model.nodes.get("source-select"), "hosted:b"); await emit(model.nodes.get("source-select"), "change");
  await settled(() => model.nodes.get("run-rows").children.length === 3);
  assert.equal(signal.aborted, true);
  model.model.override = null;
  pending.resolve(response({ catalog_revision: "late", source_revision: "late", runs: [] })); await flush();
  assert.equal(model.nodes.get("source-select").value, "hosted:b");
  assert.equal(model.nodes.get("review-section").hidden, true);
  assert.equal(model.nodes.get("chart-frame").src, "");
  await model.clock.advance(5_000);
  const probes = model.requests.filter(url => url.startsWith("/api/updates"));
  assert.equal(new URL(probes.at(-1), "http://localhost").searchParams.get("source"), "hosted:b");
});

test("hidden pages stop probes and resume once without disturbing the current review", async t => {
  const model = await autoFixture(t); await inspect(model); await model.clock.advance(5_000);
  const probes = requestCounts(model.requests)["/api/updates"], writes = frameNavigations(model.nodes);
  Object.defineProperty(model.page_document, "visibilityState", { configurable: true, value: "hidden" }); await emit(model.page_document, "visibilitychange");
  await model.clock.advance(90_000);
  assert.equal(requestCounts(model.requests)["/api/updates"], probes);
  Object.defineProperty(model.page_document, "visibilityState", { configurable: true, value: "visible" }); await emit(model.page_document, "visibilitychange");
  await model.clock.advance(0);
  assert.equal(requestCounts(model.requests)["/api/updates"], probes + 1);
  assert.equal(frameNavigations(model.nodes), writes);
});

test("older backends fall back once to bounded 30-second snapshots without replacing unchanged charts", async t => {
  const model = await autoFixture(t, { updates: false }); await inspect(model);
  await model.clock.advance(5_000);
  const counts = requestCounts(model.requests), writes = frameNavigations(model.nodes);
  assert.equal(counts["/api/updates"], 1);
  assert.match(model.nodes.get("updated-at").textContent, /Auto updates every 30s/);
  await model.clock.advance(30_000);
  assert.equal(requestCounts(model.requests)["/api/updates"], 1);
  assert.equal(requestCounts(model.requests)["/api/chart"], counts["/api/chart"] + 1);
  assert.equal(model.chart.previews.length, 1);
  assert.equal(frameNavigations(model.nodes), writes);
});

test("local catalog and list snapshots stay modest while selected update probes remain frequent", async t => {
  const model = await autoFixture(t, { hosted: false }); await inspect(model);
  model.model.catalog_revision = null; model.model.list_revision = null;
  const counts = requestCounts(model.requests);
  await model.clock.advance(25_000);
  assert.equal(requestCounts(model.requests)["/api/updates"], 5);
  assert.equal(requestCounts(model.requests)["/api/catalog"], counts["/api/catalog"]);
  assert.equal(requestCounts(model.requests)["/api/runs"], counts["/api/runs"]);
  await model.clock.advance(5_000);
  assert.equal(requestCounts(model.requests)["/api/catalog"], counts["/api/catalog"] + 1);
  assert.equal(requestCounts(model.requests)["/api/runs"], counts["/api/runs"] + 1);
});

test("chart text requests enforce their byte limit and release pending request state", async t => {
  const original_fetch = globalThis.fetch; t.after(() => { globalThis.fetch = original_fetch; });
  globalThis.fetch = async () => ({ ok: true, status: 200, text: async () => "λ λ" });
  const lane = new RequestLane();
  await assert.rejects(lane.runText("/api/chart", 4), /preview limit/);
  assert.equal(lane.pending, false);
  assert.equal(await lane.runText("/api/chart", 5), "λ λ");
  let cancelled = false;
  globalThis.fetch = async () => ({ ok: true, status: 200, body: new ReadableStream({ start(controller) { controller.enqueue(new Uint8Array(6)); }, cancel() { cancelled = true; } }) });
  await assert.rejects(lane.runText("/api/chart", 5), /preview limit/);
  assert.equal(cancelled, true);
  assert.equal(lane.pending, false);
});
