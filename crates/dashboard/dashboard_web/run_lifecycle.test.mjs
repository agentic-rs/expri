import test from "node:test";
import assert from "node:assert/strict";
import { JSDOM } from "jsdom";
import { act, createElement } from "react";

const dom = new JSDOM("<!doctype html><html><body></body></html>", { url: "http://localhost/", pretendToBeVisual: true });
for (const name of ["window", "document", "navigator", "HTMLElement", "HTMLInputElement", "HTMLTextAreaElement", "Node", "Event", "MutationObserver"]) {
  Object.defineProperty(globalThis, name, { configurable: true, writable: true, value: dom.window[name] });
}
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
const { createRoot } = await import("react-dom/client");
const { RunLifecyclePanel, lifecycleActions, lifecyclePanelKey, lifecyclePending, lifecycleScope, parseLifecycleArguments, parseLifecycleReason, parseRunLifecycle, formatDuration, runStatusClass } = await import("./.test/app.js");
const source = { source_id: "hosted-project:vision", project_id: "vision", kind: "hosted_project" };
const run = { run_id: "run-one", run_key: "gpu-a:run-one", origin: "gpu-a", task: "train", status: "running", started_at: "2026-10-18T00:00:00Z", finished_at: null, exit_code: null };
const control = { supported: true, online: true, status: "running", attempt: 2, generation: 8, binding_id: "worker-binding", capabilities: ["pause", "resume", "stop", "continue", "restart"], command: ["python", "train.py", "--epochs", "20"], checkpoints: ["step-100.pt", "step-200.pt"], last_seen_at_ms: Date.now(), request: null };
const empty_catalog = { project_id: "vision", revision: "1", experiments: [] };
const grouped_catalog = { ...empty_catalog, experiments: [{ experiment_id: "exp-training", name: "Training", members: [
  { project_id: "vision", origin: "gpu-a", run_id: "run-one" },
] }] };
const reply = (value, status = 200) => ({ ok: status >= 200 && status < 300, status, json: async () => value });
function deferred() { let resolve; const promise = new Promise(accept => { resolve = accept; }); return { promise, resolve }; }

test("lifecycle actions follow acknowledged state and explicit server capabilities", () => {
  assert.deepEqual(lifecycleActions(control), ["pause", "stop", "continue", "restart"]);
  assert.deepEqual(lifecycleActions({ ...control, status: "waiting" }), ["pause", "stop", "continue", "restart"]);
  assert.deepEqual(lifecycleActions({ ...control, status: "paused" }), ["resume", "stop", "continue", "restart"]);
  for (const status of ["completed", "failed", "cancelled", "lost"]) assert.deepEqual(lifecycleActions({ ...control, status }), ["continue", "restart"]);
  assert.deepEqual(lifecycleActions({ ...control, capabilities: ["stop"] }), ["stop"]);
  assert.deepEqual(lifecycleActions({ ...control, status: "preparing" }), ["stop"]);
  assert.deepEqual(lifecycleActions({ ...control, status: "pending" }), ["stop"]);
  assert.equal(lifecyclePending({ status: "executing" }), true);
  assert.equal(lifecyclePending({ status: "applied" }), false);
  assert.equal(runStatusClass("paused"), "paused");
  assert.equal(formatDuration({ ...run, status: "paused" }, Date.parse("2026-10-18T00:01:05Z")), "1m 5s");
});

test("control scopes cannot cross project, origin or run identities", () => {
  assert.deepEqual(lifecycleScope(run, source), { project_id: "vision", origin: "gpu-a", run_id: "run-one" });
  assert.equal(lifecycleScope({ ...run, origin: "gpu-b" }, source), null);
  assert.equal(lifecycleScope({ ...run, run_key: "gpu-a:other" }, source), null);
  assert.equal(lifecycleScope(run, { ...source, kind: "local" }), null);
});

test("control payloads preserve argument boundaries and reject malformed acknowledgements", () => {
  assert.deepEqual(parseRunLifecycle(control), control);
  assert.deepEqual(parseLifecycleArguments('["train.py", "with space", "", "$(command)"]'), ["train.py", "with space", "", "$(command)"]);
  for (const value of ["--epochs 30", "[1]", '{"arg":"value"}', '["\\u0000"]']) assert.throws(() => parseLifecycleArguments(value), /Arguments/);
  for (const value of [null, {}, { ...control, binding_id: "" }, { ...control, attempt: 0 }, { ...control, capabilities: ["shell"] },
    { ...control, request: { request_id: "x", action: "restart", status: "applied", error: null, new_run_id: "../outside" } }]) {
    assert.throws(() => parseRunLifecycle(value));
  }
});

test("argument and reason limits match server UTF-8 byte bounds", () => {
  assert.equal(parseLifecycleArguments(JSON.stringify(Array(256).fill(""))).length, 256);
  assert.throws(() => parseLifecycleArguments(JSON.stringify(Array(257).fill(""))), /256/);
  assert.equal(parseLifecycleArguments(JSON.stringify(["😀".repeat(2048)]))[0].length, 4096);
  assert.throws(() => parseLifecycleArguments(JSON.stringify(["😀".repeat(2049)])), /8,192/);
  assert.equal(parseLifecycleArguments(JSON.stringify(Array(4).fill("a".repeat(8192)))).length, 4);
  assert.throws(() => parseLifecycleArguments(JSON.stringify([...Array(4).fill("a".repeat(8192)), "x"])), /32,768/);
  assert.equal(parseLifecycleReason("a".repeat(1024)), "a".repeat(1024));
  assert.equal(parseLifecycleReason("😀".repeat(256)), "😀".repeat(256));
  assert.throws(() => parseLifecycleReason("😀".repeat(257)), /1,024 UTF-8/);
  for (const value of ["bad\nreason", "\ttext", "nul\0", "bad\u0085text"]) assert.throws(() => parseLifecycleReason(value), /control/);
  assert.equal(parseLifecycleReason("  Maintenance  "), "Maintenance");
  assert.equal(parseLifecycleReason("  "), "Requested from website");
});

async function fixture(t, values = {}, handler = () => reply(control)) {
  const container = document.createElement("div"); document.body.append(container);
  const root = createRoot(container);
  const previous_fetch = globalThis.fetch, previous_timeout = globalThis.setTimeout, previous_clear = globalThis.clearTimeout;
  const timers = new Map(); let next_timer = 1;
  globalThis.setTimeout = (callback, delay, ...args) => delay === 2000 ? (() => { const id = next_timer++; timers.set(id, callback); return id; })() : previous_timeout(callback, delay, ...args);
  globalThis.clearTimeout = id => { if (!timers.delete(id)) previous_clear(id); };
  const requests = [];
  globalThis.fetch = async (url, options) => { const request = { url, ...options }; requests.push(request); return handler(request); };
  let applied = 0; const opened = [];
  let props = { run, source, detail: null, enabled: true, visible: true, experiment_catalog: empty_catalog, on_applied: () => { applied++; }, on_open_run: id => opened.push(id), ...values };
  const render = async (next = {}) => { props = { ...props, ...next }; await act(async () => root.render(createElement(RunLifecyclePanel, { ...props, key: lifecyclePanelKey(props.source?.source_id ?? "local", props.run) }))); };
  await render();
  t.after(async () => { await act(async () => root.unmount()); container.remove(); globalThis.fetch = previous_fetch; globalThis.setTimeout = previous_timeout; globalThis.clearTimeout = previous_clear; });
  const poll = async () => { const callback = timers.values().next().value; assert.ok(callback, "poll scheduled"); timers.clear(); await act(async () => callback()); };
  const click = async id => { const node = container.querySelector(`#${id}`); assert.ok(node, `missing ${id}`); await act(async () => node.click()); };
  const setValue = async (id, value) => {
    const input = container.querySelector(`#${id}`); assert.ok(input);
    const prototype = input.tagName === "TEXTAREA" ? dom.window.HTMLTextAreaElement.prototype : input.tagName === "SELECT" ? dom.window.HTMLSelectElement.prototype : dom.window.HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(prototype, "value").set.call(input, value);
    await act(async () => input.dispatchEvent(new dom.window.Event(input.tagName === "SELECT" ? "change" : "input", { bubbles: true })));
  };
  return { container, requests, timers, render, poll, click, setValue, applied: () => applied, opened };
}

test("pause remains pending until worker acknowledgement and rapid clicks send one command", async t => {
  const gate = deferred(); let current = control;
  const f = await fixture(t, {}, request => request.method === "POST" ? gate.promise : reply(current));
  const pause = f.container.querySelector("#run-lifecycle-pause");
  await act(async () => { pause.click(); pause.click(); });
  const posts = f.requests.filter(request => request.method === "POST"); assert.equal(posts.length, 1);
  const body = JSON.parse(posts[0].body);
  assert.equal(body.action, "pause"); assert.equal(body.expected_attempt, 2); assert.equal(body.expected_generation, 8); assert.equal(body.binding_id, "worker-binding"); assert.equal(body.grace_seconds, 30);
  current = { ...control, request: { request_id: body.request_id, action: "pause", status: "requested", error: null, new_run_id: null } };
  await act(async () => gate.resolve(reply(current)));
  assert.match(f.container.textContent, /Pause requested · waiting for worker/);
  assert.equal(f.container.querySelector("#run-lifecycle-pause").disabled, true);
  assert.equal(f.container.querySelector("#run-lifecycle-resume"), null);
  current = { ...current, status: "paused", generation: 9, request: { ...current.request, status: "applied" } };
  await f.poll();
  assert.equal(f.container.querySelector("#run-lifecycle-pause"), null);
  assert.equal(f.container.querySelector("#run-lifecycle-resume").disabled, false);
  assert.equal(f.container.querySelector("#run-lifecycle-task-status").textContent.trim(), "Task paused");
  assert.match(f.container.textContent, /suspended in memory/); assert.equal(f.applied(), 1);
  await f.poll(); assert.equal(f.applied(), 1, "acknowledgement refresh happens once");
});

test("continue edits arguments, selects finalized checkpoint and retains exact run identity", async t => {
  const f = await fixture(t, { run: { ...run, status: "completed" } }, request => reply(request.method === "POST" ? { ...control, status: "completed", request: { request_id: JSON.parse(request.body).request_id, action: "continue", status: "requested", error: null, new_run_id: null } } : { ...control, status: "completed" }));
  await f.click("run-lifecycle-continue");
  assert.match(f.container.textContent, /retaining this run ID and all metric history/);
  assert.equal(f.container.querySelectorAll('input[name="lifecycle-checkpoint"]').length, 2);
  await f.setValue("run-lifecycle-arguments", '["train.py", "--epochs", "40", "with space"]');
  await act(async () => f.container.querySelectorAll('input[name="lifecycle-checkpoint"]')[1].click());
  await f.click("run-lifecycle-confirm");
  const body = JSON.parse(f.requests.find(request => request.method === "POST").body);
  assert.equal(body.run_id, "run-one"); assert.equal(body.origin, "gpu-a"); assert.equal(body.action, "continue"); assert.equal(body.checkpoint, "step-200.pt");
  assert.deepEqual(body.arguments, ["train.py", "--epochs", "40", "with space"]);
});

test("restart confirms fresh history and links the acknowledged new run", async t => {
  const f = await fixture(t, {}, request => reply(request.method === "POST" ? { ...control, request: { request_id: JSON.parse(request.body).request_id, action: "restart", status: "applied", error: null, new_run_id: "run-new" } } : control));
  await f.click("run-lifecycle-restart");
  assert.match(f.container.textContent, /new linked run with fresh metrics/);
  assert.match(f.container.textContent, /current task will stop after the worker validates/);
  await f.click("run-lifecycle-confirm");
  assert.equal(f.applied(), 1);
  await f.click("run-lifecycle-open-result"); assert.deepEqual(f.opened, ["gpu-a:run-new"]);
});

test("ungrouped restarts stay ungrouped and joining a run keeps its exact machine scope", async t => {
  const current = { ...control, experiment_assignment_supported: true };
  const f = await fixture(t, {}, () => reply(current));
  await f.click("run-lifecycle-restart");
  assert.equal(f.container.querySelector("#restart-experiment-ungrouped").checked, true);
  assert.equal(f.container.querySelectorAll('input[name="restart-experiment"]').length, 4);
  await f.click("restart-experiment-join_run");
  assert.equal(f.container.querySelector("#restart-experiment-origin").value, "gpu-a");
  assert.equal(f.container.querySelector("#restart-experiment-run-id").value, "run-one");
  await f.setValue("restart-experiment-origin", "gpu-b");
  await f.setValue("restart-experiment-run-id", "run-one");
  await f.click("run-lifecycle-confirm");
  const body = JSON.parse(f.requests.find(request => request.method === "POST").body);
  assert.deepEqual(body.experiment, { kind: "join_run", origin: "gpu-b", run_id: "run-one" });
  assert.equal(body.origin, "gpu-a", "the task remains bound to its original worker");
});

test("restart inherits the source experiment and preserves that selection across polling and retries", async t => {
  const current = { ...control, experiment_assignment_supported: true };
  const f = await fixture(t, { experiment_catalog: grouped_catalog }, request => request.method === "POST" ? reply({ error: "Temporary failure" }, 503) : reply(current));
  await f.click("run-lifecycle-restart");
  assert.equal(f.container.querySelector("#restart-experiment-existing").checked, true);
  assert.equal(f.container.querySelector("#restart-experiment-id").value, "exp-training");
  await f.click("run-lifecycle-confirm");
  await f.render({ experiment_catalog: { ...grouped_catalog, revision: "2", experiments: grouped_catalog.experiments.map(item => ({ ...item, members: [] })) } });
  await f.poll();
  await f.click("run-lifecycle-confirm");
  const posts = f.requests.filter(request => request.method === "POST").map(request => JSON.parse(request.body));
  assert.equal(posts.length, 2);
  assert.equal(posts[0].request_id, posts[1].request_id);
  assert.deepEqual(posts[0].experiment, { kind: "existing", experiment_id: "exp-training" });
  assert.deepEqual(posts[1].experiment, posts[0].experiment);
});

test("restart can explicitly opt out of the source experiment", async t => {
  const f = await fixture(t, { experiment_catalog: grouped_catalog }, () => reply({ ...control, experiment_assignment_supported: true }));
  await f.click("run-lifecycle-restart");
  await f.click("restart-experiment-ungrouped");
  await f.click("run-lifecycle-confirm");
  assert.equal(Object.hasOwn(JSON.parse(f.requests.find(request => request.method === "POST").body), "experiment"), false);
});

test("restart defaults match full run identity and do not guess while the catalog is unavailable", async t => {
  const f = await fixture(t, { experiment_catalog: null }, () => reply({ ...control, experiment_assignment_supported: true }));
  await f.click("run-lifecycle-restart");
  assert.equal(f.container.querySelector('input[name="restart-experiment"]:checked'), null);
  await f.click("run-lifecycle-confirm");
  assert.equal(f.requests.filter(request => request.method === "POST").length, 0);
  assert.match(f.container.textContent, /Experiment membership is unavailable/);
  const other_origin = { ...grouped_catalog, experiments: grouped_catalog.experiments.map(item => ({ ...item, members: item.members.map(member => ({ ...member, origin: "gpu-b" })) })) };
  await f.render({ experiment_catalog: other_origin });
  await f.click("run-lifecycle-restart");
  assert.equal(f.container.querySelector("#restart-experiment-ungrouped").checked, true);
});

test("unknown membership on older workers requires an explicit ungrouped choice", async t => {
  const f = await fixture(t, { experiment_catalog: null });
  await f.click("run-lifecycle-restart");
  await f.click("run-lifecycle-confirm");
  assert.equal(f.requests.filter(request => request.method === "POST").length, 0);
  await f.click("restart-experiment-ungrouped");
  await f.click("run-lifecycle-confirm");
  assert.equal(Object.hasOwn(JSON.parse(f.requests.find(request => request.method === "POST").body), "experiment"), false);
});

test("a grouped run on an older worker requires an explicit ungrouped restart", async t => {
  const f = await fixture(t, { experiment_catalog: grouped_catalog });
  await f.click("run-lifecycle-restart");
  assert.equal(f.container.querySelector("#restart-experiment-existing").checked, true);
  assert.equal(f.container.querySelector("#restart-experiment-existing").disabled, true);
  await f.click("run-lifecycle-confirm");
  assert.equal(f.requests.filter(request => request.method === "POST").length, 0);
  await f.click("restart-experiment-ungrouped");
  await f.click("run-lifecycle-confirm");
  assert.equal(Object.hasOwn(JSON.parse(f.requests.find(request => request.method === "POST").body), "experiment"), false);
});

test("new experiment restart retries reuse the lifecycle request ID and preserve its draft", async t => {
  const current = { ...control, experiment_assignment_supported: true };
  const f = await fixture(t, {}, request => request.method === "POST" ? reply({ error: "Temporary failure" }, 503) : reply(current));
  await f.click("run-lifecycle-restart");
  await f.click("restart-experiment-new");
  await f.setValue("restart-experiment-name", "  Better optimizer  ");
  await f.poll();
  assert.equal(f.container.querySelector("#restart-experiment-name").value, "  Better optimizer  ");
  await f.click("run-lifecycle-confirm");
  await f.click("run-lifecycle-confirm");
  const posts = f.requests.filter(request => request.method === "POST").map(request => JSON.parse(request.body));
  assert.equal(posts.length, 2);
  assert.equal(posts[0].request_id, posts[1].request_id);
  assert.deepEqual(posts[0].experiment, { kind: "new", name: "Better optimizer" });
});

test("restart chooses existing experiment IDs and rejects a removed or foreign selection", async t => {
  const catalog = { project_id: "vision", revision: "1", experiments: [
    { experiment_id: "experiment-first", name: "Same name", members: [] }, { experiment_id: "experiment-second", name: "Same name", members: [] }] };
  const f = await fixture(t, { experiment_catalog: catalog }, () => reply({ ...control, experiment_assignment_supported: true }));
  await f.click("run-lifecycle-restart");
  await f.click("restart-experiment-existing");
  await f.setValue("restart-experiment-id", "experiment-second");
  await f.click("run-lifecycle-confirm");
  assert.deepEqual(JSON.parse(f.requests.find(request => request.method === "POST").body).experiment,
    { kind: "existing", experiment_id: "experiment-second" });
  await f.render({ experiment_catalog: { ...catalog, project_id: "other" } });
  await f.click("run-lifecycle-confirm");
  assert.equal(f.requests.filter(request => request.method === "POST").length, 1);
  assert.match(f.container.textContent, /Choose an available experiment in this project/);
});

test("restart grouping capability loss and stale task generations cannot silently drop a selection", async t => {
  let current = { ...control, experiment_assignment_supported: true };
  const f = await fixture(t, {}, () => reply(current));
  await f.click("run-lifecycle-restart");
  await f.click("restart-experiment-join_run");
  current = { ...current, experiment_assignment_supported: false };
  await f.poll();
  await f.click("run-lifecycle-confirm");
  assert.equal(f.requests.filter(request => request.method === "POST").length, 0);
  assert.match(f.container.textContent, /Experiment assignment is unavailable/);
  current = { ...current, experiment_assignment_supported: true, generation: 9 };
  await f.poll();
  await f.click("run-lifecycle-confirm");
  assert.match(f.container.textContent, /task changed while this form was open/);
  assert.equal(f.requests.filter(request => request.method === "POST").length, 0);
});

test("older workers and read-only dashboards never offer unsupported experiment writes", async t => {
  const f = await fixture(t);
  await f.click("run-lifecycle-restart");
  assert.equal(f.container.querySelector("#restart-experiment-ungrouped").checked, true);
  for (const kind of ["existing", "new", "join_run"]) assert.equal(f.container.querySelector(`#restart-experiment-${kind}`).disabled, true);
  await f.click("run-lifecycle-confirm");
  assert.equal(Object.hasOwn(JSON.parse(f.requests.find(request => request.method === "POST").body), "experiment"), false);
  await f.render({ enabled: false });
  assert.equal(f.container.querySelector("#run-lifecycle-confirm").disabled, true);
});

test("continuation keeps its run membership without a new grouping selection", async t => {
  const f = await fixture(t, {}, () => reply({ ...control, experiment_assignment_supported: true }));
  await f.click("run-lifecycle-continue");
  assert.equal(f.container.querySelector("#restart-experiment"), null);
  await f.click("run-lifecycle-confirm");
  assert.equal(Object.hasOwn(JSON.parse(f.requests.find(request => request.method === "POST").body), "experiment"), false);
});

test("restarted runs link their recorded parent using the selected project or worker scope", async t => {
  const detail = { run, source, state: { restarted_from: "run-parent" }, snapshot: null };
  const f = await fixture(t, { detail });
  assert.match(f.container.querySelector("#run-lifecycle-open-parent").textContent, /Restarted from run-parent/);
  await f.click("run-lifecycle-open-parent");
  const worker_source = { source_id: "hosted:vision:gpu-a", project_id: "vision", origin: "gpu-a", kind: "service" };
  const worker_run = { ...run, run_key: undefined };
  await f.render({ source: worker_source, run: worker_run, detail: { ...detail, source: worker_source, run: worker_run } });
  await f.click("run-lifecycle-open-parent");
  assert.deepEqual(f.opened, ["gpu-a:run-parent", "run-parent"]);
});

test("restart parent links reject unsafe IDs and stale details from another selected run or source", async t => {
  const detail = { run, source, state: { restarted_from: "../outside" }, snapshot: null };
  const f = await fixture(t, { detail });
  assert.equal(f.container.querySelector("#run-lifecycle-open-parent"), null);
  for (const stale of [
    { ...detail, state: { restarted_from: run.run_id } },
    { ...detail, state: { restarted_from: "run-parent" }, run: { ...run, run_id: "other", run_key: "gpu-a:other" } },
    { ...detail, state: { restarted_from: "run-parent" }, source: { ...source, source_id: "hosted-project:other" } },
  ]) {
    await f.render({ detail: stale });
    assert.equal(f.container.querySelector("#run-lifecycle-open-parent"), null);
  }
  assert.deepEqual(f.opened, []);
});

test("resume waits for acknowledgement and stop sends the reviewed grace period", async t => {
  let current = { ...control, status: "paused" };
  const f = await fixture(t, { run: { ...run, status: "paused" } }, request => {
    if (request.method === "POST") {
      const body = JSON.parse(request.body);
      current = { ...current, status: body.action === "resume" ? "waiting" : "cancelled", generation: current.generation + 1,
        request: { request_id: body.request_id, action: body.action, status: "applied", error: null, new_run_id: null } };
    }
    return reply(current);
  });
  await f.click("run-lifecycle-resume");
  assert.ok(f.container.querySelector("#run-lifecycle-pause"), "resume restores the acknowledged waiting phase");
  await f.click("run-lifecycle-stop");
  assert.match(f.container.textContent, /Outputs and checkpoints are retained/);
  await f.setValue("run-lifecycle-grace", "90");
  await f.setValue("run-lifecycle-reason", "Save checkpoint before maintenance");
  await f.click("run-lifecycle-confirm");
  const bodies = f.requests.filter(request => request.method === "POST").map(request => JSON.parse(request.body));
  assert.deepEqual(bodies.map(body => body.action), ["resume", "stop"]);
  assert.equal(bodies[1].grace_seconds, 90); assert.equal(bodies[1].reason, "Save checkpoint before maintenance");
  assert.equal(bodies[1].expected_generation, 9); assert.equal(bodies[1].arguments, null); assert.equal(bodies[1].checkpoint, null);
  assert.equal(f.applied(), 2);
});

test("read-only dashboards cannot dispatch lifecycle commands", async t => {
  const f = await fixture(t, { enabled: false });
  assert.match(f.container.textContent, /Read only/); assert.equal(f.container.querySelector("#run-lifecycle-pause").disabled, true);
  await f.click("run-lifecycle-pause"); assert.equal(f.requests.some(request => request.method === "POST"), false);
  await f.render({ enabled: true });
  await f.render({ run: { ...run, run_id: "run-offline", run_key: "gpu-a:run-offline" } });
  assert.equal(f.container.querySelector("#run-lifecycle-pause").disabled, false);
});

test("offline worker status and queued runs explain unavailable controls", async t => {
  const f = await fixture(t, {}, () => reply({ ...control, online: false }));
  assert.match(f.container.textContent, /Worker offline/); assert.equal(f.container.querySelector("#run-lifecycle-pause").disabled, true);
  await f.render({ run: { ...run, queue: { job_id: "queued" } } });
  assert.match(f.container.textContent, /Queued runs remain managed by the scheduler/);
  const count = f.requests.length; await f.render({ run: { ...run, queue: null }, source: { source_id: "local", kind: "local" } });
  assert.match(f.container.textContent, /published run and a connected Linux/); assert.equal(f.requests.length, count);
});

test("older backends return a quiet upgrade explanation and stop polling", async t => {
  const f = await fixture(t, {}, () => reply({ error: "Unknown endpoint" }, 404));
  assert.match(f.container.textContent, /unavailable on this server/);
  assert.equal(f.container.querySelector("#run-lifecycle-pause"), null);
  assert.equal(f.timers.size, 0);
});

test("a changed task generation invalidates an open continuation form", async t => {
  let current = control;
  const f = await fixture(t, {}, () => reply(current));
  await f.click("run-lifecycle-continue");
  current = { ...control, generation: control.generation + 1 };
  await f.poll(); await f.click("run-lifecycle-confirm");
  assert.match(f.container.textContent, /task changed while this form was open/);
  assert.equal(f.requests.some(request => request.method === "POST"), false);
});

test("retrying an uncertain command delivery reuses its idempotency key", async t => {
  const f = await fixture(t, {}, request => { if (request.method === "POST") throw new Error("Connection lost"); return reply(control); });
  await f.click("run-lifecycle-pause"); await f.click("run-lifecycle-pause");
  const bodies = f.requests.filter(request => request.method === "POST").map(request => JSON.parse(request.body));
  assert.equal(bodies.length, 2); assert.equal(bodies[0].request_id, bodies[1].request_id);
  assert.match(f.container.textContent, /Connection lost/);
});

test("late responses from a previous run and hidden panels cannot mutate the current controls", async t => {
  const gate = deferred(); let first = true;
  const f = await fixture(t, {}, () => { if (first) { first = false; return gate.promise; } return reply({ ...control, status: "paused" }); });
  const old = f.requests[0];
  await f.render({ run: { ...run, run_id: "run-two", run_key: "gpu-a:run-two" } });
  assert.equal(old.signal.aborted, true);
  await act(async () => gate.resolve(reply(control)));
  assert.ok(f.container.querySelector("#run-lifecycle-resume")); assert.equal(f.container.querySelector("#run-lifecycle-pause"), null);
  await f.render({ visible: false }); assert.equal(f.timers.size, 0);
});

test("worker failure is shown without claiming that the run state changed", async t => {
  const f = await fixture(t, {}, request => reply(request.method === "POST" ? { ...control, request: { request_id: JSON.parse(request.body).request_id, action: "pause", status: "failed", error: "Task changed; refresh before retrying.", new_run_id: null } } : control));
  await f.click("run-lifecycle-pause");
  assert.match(f.container.textContent, /Task changed; refresh before retrying/);
  assert.ok(f.container.querySelector("#run-lifecycle-pause")); assert.equal(f.container.querySelector("#run-lifecycle-resume"), null); assert.equal(f.applied(), 0);
});

const disk_requirements = { disk: [{ path: "outputs", min_free_bytes: 20 * 1024 ** 3, resume_free_bytes: 30 * 1024 ** 3 }], recovery_seconds: 30 };
const resource_status = { phase: "blocked", blocked: true, checked_at: "2026-10-20T01:00:00Z", recovery_started_at: null,
  disk: [{ ...disk_requirements.disk[0], available_bytes: 10 * 1024 ** 3, phase: "blocked", error: null }] };
const resource_control = { ...control, requirements: disk_requirements, resource_status, manual_paused: false,
  capabilities: [...control.capabilities, "set_requirements"] };

test("resource controls retain old-server compatibility and never expose a raw configuration action", async t => {
  assert.deepEqual(parseRunLifecycle(resource_control), resource_control);
  assert.equal(lifecycleActions(resource_control).includes("set_requirements"), false);
  const f = await fixture(t, {}, () => reply({ ...resource_control, status: "paused" }));
  assert.ok(f.container.querySelector("#run-resource-edit"));
  assert.equal(f.container.querySelector("#run-lifecycle-set_requirements"), null);
  assert.match(f.container.textContent, /outputs.*10 GiB free.*Minimum 20 GiB/s);
  await f.render({ run: { ...run, run_id: "old-worker", run_key: "gpu-a:old-worker" } });
  const old = parseRunLifecycle(control);
  assert.equal(old.requirements, undefined);
  for (const patch of [{ manual_paused: "yes" }, { requirements: {} }, { resource_status: {} }]) assert.throws(() => parseRunLifecycle({ ...resource_control, ...patch }));
});

test("requirements are opt-in, validate friendly sizes and save exact bytes with the current binding", async t => {
  const initial = { ...resource_control, requirements: { disk: [], recovery_seconds: 30 }, resource_status: null };
  const f = await fixture(t, {}, request => reply(request.method === "POST" ? { ...initial, request: { request_id: JSON.parse(request.body).request_id,
    action: "set_requirements", status: "requested", error: null, new_run_id: null } } : initial));
  await f.click("run-resource-edit");
  assert.match(f.container.textContent, /No disk requirements/);
  await f.click("run-resource-add");
  assert.equal(f.container.querySelector("#run-resource-path-0").value, "outputs");
  assert.equal(f.container.querySelector("#run-resource-min-0").value, "");
  assert.equal(f.container.querySelector("#run-resource-resume-0").value, "");
  await f.click("run-resource-save");
  assert.match(f.container.querySelector("#run-resource-error").textContent, /Enter a disk size/);
  assert.equal(f.requests.some(request => request.method === "POST"), false);
  await f.setValue("run-resource-min-0", "20 GiB");
  await f.setValue("run-resource-resume-0", "10 GiB");
  await f.click("run-resource-save");
  assert.match(f.container.querySelector("#run-resource-error").textContent, /higher recovery limit/);
  await f.setValue("run-resource-resume-0", "30 GiB");
  await f.setValue("run-resource-recovery", "0");
  await f.click("run-resource-save");
  const body = JSON.parse(f.requests.find(request => request.method === "POST").body);
  assert.deepEqual(body.requirements, { ...disk_requirements, recovery_seconds: 0 });
  assert.equal(body.action, "set_requirements"); assert.equal(body.expected_attempt, 2); assert.equal(body.expected_generation, 8);
  assert.equal(body.binding_id, "worker-binding"); assert.equal(body.reason, "Requested from website");
  assert.match(f.container.textContent, /Requirements requested · waiting for worker/);
  assert.equal(f.container.querySelector("#run-resource-save").disabled, true);
});

test("removing all disk paths is an explicit disable request and worker acknowledgement closes the editor", async t => {
  const f = await fixture(t, {}, request => reply(request.method === "POST" ? { ...resource_control, requirements: { disk: [], recovery_seconds: 30 }, resource_status: null,
    request: { request_id: JSON.parse(request.body).request_id, action: "set_requirements", status: "applied", error: null, new_run_id: null } } : resource_control));
  await f.click("run-resource-edit"); await f.click("run-resource-remove-0");
  assert.equal(f.requests.some(request => request.method === "POST"), false);
  await f.click("run-resource-save");
  assert.deepEqual(JSON.parse(f.requests.find(request => request.method === "POST").body).requirements, { disk: [], recovery_seconds: 30 });
  assert.equal(f.container.querySelector("#run-resource-form"), null);
  assert.equal(f.container.querySelector("#run-resource-status"), null);
  assert.equal(f.applied(), 1);
});

test("requirements stay reviewable on read-only dashboards and cannot be sent", async t => {
  const f = await fixture(t, { enabled: false }, () => reply(resource_control));
  await f.click("run-resource-edit");
  assert.equal(f.container.querySelector("#run-resource-min-0").value, "20 GiB");
  assert.equal(f.container.querySelector("#run-resource-form fieldset").disabled, true);
  assert.equal(f.container.querySelector("#run-resource-save"), null);
  assert.equal(f.requests.some(request => request.method === "POST"), false);
});

test("polling preserves unsaved requirements and a changed generation fences their submission", async t => {
  let current = resource_control;
  const f = await fixture(t, {}, () => reply(current));
  await f.click("run-resource-edit"); await f.setValue("run-resource-min-0", "25 GiB");
  await f.poll(); assert.equal(f.container.querySelector("#run-resource-min-0").value, "25 GiB");
  current = { ...current, generation: current.generation + 1 };
  await f.poll(); await f.click("run-resource-save");
  assert.match(f.container.textContent, /task changed while this form was open/);
  assert.equal(f.requests.some(request => request.method === "POST"), false);
});

test("resource pending is stoppable and resource pause keeps manual intent separate", async t => {
  let current = { ...resource_control, status: "pending" };
  const f = await fixture(t, { run: { ...run, status: "pending", started_at: null } }, request => {
    if (request.method === "POST") {
      const body = JSON.parse(request.body);
      current = { ...current, manual_paused: body.action === "pause", request: { request_id: body.request_id, action: body.action,
        status: "applied", error: null, new_run_id: null } };
    }
    return reply(current);
  });
  assert.ok(f.container.querySelector("#run-lifecycle-stop"));
  assert.match(f.container.textContent, /Waiting to start/);
  assert.doesNotMatch(f.container.textContent, /managed by the scheduler/);
  current = { ...current, status: "paused" }; await f.poll();
  assert.equal(f.container.querySelector("#run-lifecycle-pause").textContent, "Keep paused");
  await f.click("run-lifecycle-pause");
  assert.equal(f.container.querySelector("#run-lifecycle-pause"), null);
  assert.match(f.container.textContent, /Manual pause is active/);
  await f.click("run-lifecycle-resume");
  assert.match(f.container.textContent, /resumes automatically after every recovery limit/);
  const bodies = f.requests.filter(request => request.method === "POST").map(request => JSON.parse(request.body));
  assert.deepEqual(bodies.map(body => body.action), ["pause", "resume"]);
});
