import test from "node:test";
import assert from "node:assert/strict";
import { parseRunSystem, runSystemEnd, RunSystemMonitor, RequestLane } from "./.test/app.js";

const START = 1_000_000;
function inventory(revision = 1) {
  return { revision, inventory: { observed_at_ms: START - 1000, hostname: "captured-host", os_name: "Linux", os_version: null, kernel_version: null, architecture: "x86_64", cpu: { brand: `CPU ${revision}`, vendor: "Vendor", logical_cpu_count: 8, physical_core_count: 4 }, memory_total_bytes: 16 * 1024 ** 3, measurement_scope: "host", devices: [], warnings: [] } };
}
function sample(sequence, ordinal = 1, revision = 1) {
  return { sequence, session_id: "session-a", observed_at_ms: START + ordinal * 1000, interval_ms: 1000, collection_duration_ms: 1, inventory_revision: revision, metrics: { "cpu/usage_percent": { kind: "gauge", value: ordinal * 10 }, "network/rx_bytes": { kind: "counter_delta", value: 10_000_000 } }, counters: {}, warnings: [] };
}
function capture(run_id = "worker:run-a", samples = [sample(900)]) {
  return { run_id, capture_id: "capture-a", machine_id: "machine-a", start_ms: START, end_ms: START + 1_000_000, status: "complete", inventories: [inventory()], samples, metric_summaries: { "cpu/usage_percent": { kind: "gauge", count: 10, min: 10, max: 100, last: 100, mean: 55, unit: "percent" } }, sample_count: samples.length, next_cursor: null, warnings: [] };
}
function reply(value, status = 200) { return { ok: status === 200, status, text: async () => JSON.stringify(value), json: async () => value }; }
function deferred() { let resolve; const promise = new Promise(accept => { resolve = accept; }); return { promise, resolve }; }
async function settled(predicate) { for (let index = 0; index < 100 && !predicate(); index++) await new Promise(resolve => setTimeout(resolve, 0)); assert.ok(predicate(), "retained monitor did not settle"); }
function monitor(t, fetcher, run_ids = ["worker:run-a"]) {
  const original = globalThis.fetch; globalThis.fetch = fetcher;
  const result = new RunSystemMonitor(new RequestLane(), new RequestLane(), () => {});
  t.after(() => { result.cancel(); globalThis.fetch = original; });
  result.setContext("hosted-project:project", run_ids, true);
  return result;
}

test("retained parser preserves exact run keys, validates half-open periods and unavailable evidence", () => {
  assert.deepEqual(parseRunSystem(capture(), "worker:run-a"), capture());
  assert.throws(() => parseRunSystem(capture(), "run-a"), /selected run/);
  const boundary = capture(); boundary.samples[0].observed_at_ms = boundary.end_ms;
  assert.throws(() => parseRunSystem(boundary, boundary.run_id), /selected run/);
  const unavailable = { ...capture("legacy"), capture_id: null, machine_id: null, start_ms: null, end_ms: null, status: "unavailable", inventories: [], samples: [], metric_summaries: {}, sample_count: 0 };
  assert.deepEqual(parseRunSystem(unavailable, "legacy"), unavailable);
  assert.throws(() => parseRunSystem({ ...unavailable, machine_id: "guessed-origin" }, "legacy"));
  assert.throws(() => parseRunSystem({ ...capture(), sample_count: 0 }, "worker:run-a"));
  assert.equal(runSystemEnd({ ...unavailable, start_ms: START, status: "incomplete" }, START + 60_000), START, "no artificial live window for incomplete capture");
});

test("append paging uses raw record ordinals, preserves inventory revisions and original measured counter intervals", async t => {
  let version = 1; const queries = [];
  const m = monitor(t, async url => {
    const query = new URL(url, "http://localhost").searchParams; queries.push(query);
    const cursor = Number(query.get("cursor"));
    if (version === 1 && cursor === 0) return reply({ ...capture(undefined, [sample(900)]), sample_count: 3, next_cursor: 2 });
    if (version === 1) return reply({ ...capture(undefined, [sample(4000, 2, 2)]), inventories: [inventory(2)], sample_count: 3 });
    const next = sample(9000, 3, 2); next.interval_ms = 10_000;
    return reply({ ...capture(undefined, cursor === 3 ? [next] : []), inventories: cursor === 3 ? [inventory(2)] : [], sample_count: 5 });
  });
  await settled(() => !m.snapshot().loading);
  assert.deepEqual(queries.map(query => Number(query.get("cursor"))), [0, 2]);
  assert.deepEqual(m.snapshot().items["worker:run-a"].data.samples.map(sample => sample.sequence), [900, 4000]);
  assert.deepEqual(m.snapshot().items["worker:run-a"].data.inventories.map(record => record.revision), [1, 2]);
  version = 2; assert.equal(await m.refresh(true), "success");
  assert.equal(Number(queries.at(-1).get("cursor")), 3);
  assert.equal(m.snapshot().items["worker:run-a"].cursor, 5);
  assert.deepEqual(m.snapshot().items["worker:run-a"].series.find(series => series.metric_name === "network/rx_bytes").points.map(point => point.value), [10_000_000, 10_000_000, 1_000_000]);
  await m.refresh(true); assert.equal(Number(queries.at(-1).get("cursor")), 5);
  assert.equal(m.snapshot().items["worker:run-a"].data.inventories.length, 2);
});

test("replacing a capture restarts once and follows the new paging baseline", async t => {
  let replacement = false, requests = 0;
  const m = monitor(t, async url => {
    if (++requests > 8) throw new Error("unbounded reset loop");
    const cursor = Number(new URL(url, "http://localhost").searchParams.get("cursor"));
    if (!replacement) return reply(capture());
    return reply({ ...capture(undefined, [sample(cursor === 0 ? 100 : 101, cursor === 0 ? 3 : 4, 2)]), capture_id: "capture-b", machine_id: "machine-b", inventories: [inventory(2)], sample_count: 2, next_cursor: cursor === 0 ? 1 : null });
  });
  await settled(() => !m.snapshot().loading); replacement = true;
  assert.equal(await m.refresh(true), "success");
  const item = m.snapshot().items["worker:run-a"];
  assert.equal(item.data.capture_id, "capture-b"); assert.equal(item.data.machine_id, "machine-b");
  assert.deepEqual(item.data.samples.map(sample => sample.sequence), [100, 101]);
  assert.deepEqual(item.data.inventories.map(record => record.revision), [2]);
});

test("comparison fetches complete summaries cheaply and only expands the focused timeline", async t => {
  const queries = [];
  const m = monitor(t, async url => {
    const query = new URL(url, "http://localhost").searchParams; queries.push(query);
    const cursor = Number(query.get("cursor")), short = Number(query.get("limit")) === 1;
    return reply({ ...capture(query.get("run_id"), short ? [sample(10)] : cursor === 1 ? [sample(20, 2)] : [sample(10), sample(20, 2)]), sample_count: 2, next_cursor: short ? 1 : null });
  }, ["run-a", "run-b"]);
  await settled(() => !m.snapshot().loading);
  assert.equal(m.snapshot().items["run-b"].data.metric_summaries["cpu/usage_percent"].count, 10);
  assert.equal(m.snapshot().items["run-b"].data.samples.length, 1);
  assert.equal(Number(queries.find(query => query.get("run_id") === "run-b").get("limit")), 1);
  m.focusRun("run-b"); await settled(() => !m.snapshot().loading);
  assert.equal(m.snapshot().items["run-b"].data.samples.length, 2);
  assert.equal(queries.at(-1).get("run_id"), "run-b"); assert.equal(Number(queries.at(-1).get("cursor")), 1);
});

test("byte-limited paging stops at sixteen pages and refreshes full summaries without losing its bounded preview", async t => {
  let pages = 0;
  const m = monitor(t, async url => {
    pages++; const cursor = Number(new URL(url, "http://localhost").searchParams.get("cursor"));
    return reply({ ...capture(undefined, [sample(10_000 + cursor, cursor + 1)]), sample_count: 1000, next_cursor: cursor + 1 });
  });
  await settled(() => !m.snapshot().loading);
  assert.equal(pages, 16); assert.equal(m.snapshot().items["worker:run-a"].preview_truncated, true);
  const original = m.snapshot().items["worker:run-a"].data.samples;
  await m.refresh(true); assert.equal(pages, 17); assert.deepEqual(m.snapshot().items["worker:run-a"].data.samples, original);
});

test("quiet refresh supersedes foreground work and clears loading; old source responses cannot restore a capture", async t => {
  const pending = deferred(); let hold = true;
  const m = monitor(t, async () => hold ? pending.promise : reply(capture()));
  assert.equal(m.snapshot().loading, true); hold = false;
  assert.equal(await m.refresh(true), "success"); assert.equal(m.snapshot().loading, false);
  pending.resolve(reply({ ...capture(), machine_id: "old-machine" }));
  await settled(() => m.snapshot().items["worker:run-a"].data.machine_id === "machine-a");
  const other = deferred(); globalThis.fetch = async () => other.promise;
  const old = m.refresh(); m.setContext("new-source", [], false);
  other.resolve(reply(capture())); assert.equal(await old, "cancelled"); assert.deepEqual(m.snapshot().items, {});
});

test("per-run failures preserve successful comparison evidence and retry only requested runs", async t => {
  let failed = true;
  const m = monitor(t, async url => {
    const id = new URL(url, "http://localhost").searchParams.get("run_id");
    return id === "run-a" && failed ? reply({ error: "Capture temporarily unavailable" }, 503) : reply(capture(id));
  }, ["run-a", "run-b"]);
  await settled(() => !m.snapshot().loading);
  assert.match(m.snapshot().errors["run-a"], /temporarily/); assert.ok(m.snapshot().items["run-b"]);
  failed = false; assert.equal(await m.refresh(true, ["run-a"]), "success"); assert.deepEqual(m.snapshot().errors, {});
});
