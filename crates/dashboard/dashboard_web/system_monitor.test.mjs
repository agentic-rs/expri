import test from "node:test";
import assert from "node:assert/strict";
import { parseSystemMachine, parseSystemMachines, parseSystemHistory, systemHistorySeries, SystemMonitor, RequestLane } from "./.test/app.js";

const NOW = Date.parse("2026-10-12T02:30:00Z"), HOUR = 3_600_000;
function machine(machine_id = "machine-a") {
  return { machine_id, inventory_revision: 1, last_sample_at_ms: NOW - 1000, last_received_at_ms: NOW,
    inventory: { observed_at_ms: NOW - HOUR, hostname: "workstation", os_name: "Linux", os_version: "6", kernel_version: null, architecture: "x86_64", cpu: { brand: "CPU", vendor: "Vendor", logical_cpu_count: 8, physical_core_count: 4 }, memory_total_bytes: 16 * 1024 ** 3, measurement_scope: "host", devices: [], warnings: [] } };
}
function sample(sequence, observed_at_ms = NOW - 20_000 + sequence * 1000, metrics = { "cpu/usage_percent": { kind: "gauge", value: sequence * 10 } }) {
  return { sequence, session_id: "session-a", observed_at_ms, interval_ms: 1000, collection_duration_ms: 1, inventory_revision: 1, metrics, counters: {}, warnings: [] };
}
function history(samples, machine_id = "machine-a", next_cursor = null) { return { machine_id, resolution: "detail", samples, hours: [], next_cursor }; }
function reply(value, status = 200) { return { ok: status === 200, status, json: async () => value, text: async () => JSON.stringify(value) }; }
async function settle(guard) { for (let index = 0; index < 100 && !guard(); index++) await new Promise(resolve => setTimeout(resolve, 0)); assert.ok(guard(), "monitor did not settle"); }
function deferred() { let resolve; const promise = new Promise(accept => { resolve = accept; }); return { promise, resolve }; }

test("system parsers validate identities, bounds and preserve exact cumulative counters", () => {
  assert.deepEqual(parseSystemMachine(machine()), machine());
  assert.throws(() => parseSystemMachines({ machines: [machine(), machine()], next_cursor: null }), /invalid/);
  assert.throws(() => parseSystemMachine({ ...machine(), machine_id: "../outside" }), /invalid/);
  const reading = sample(1); reading.counters["network/total_bytes"] = "18446744073709551615";
  assert.equal(parseSystemHistory(history([reading]), "machine-a", "detail").samples[0].counters["network/total_bytes"], "18446744073709551615");
  assert.throws(() => parseSystemHistory(history([reading]), "machine-b", "detail"), /invalid/);
  assert.throws(() => parseSystemHistory(history([{ ...reading, interval_ms: 120001 }]), "machine-a", "detail"), /invalid/);
  assert.throws(() => parseSystemHistory(history([sample(2, NOW - 1000, { transfer: { kind: "counter_delta", value: -1 } })]), "machine-a", "detail"), /invalid/);
});

test("raw monitor series convert measured delta rates and preserve missing readings, pauses and session gaps", () => {
  const rows = [sample(1, NOW - 10_000, { cpu: { kind: "gauge", value: 40 }, bytes: { kind: "counter_delta", value: 200 } }), sample(2, NOW - 9000, { bytes: { kind: "counter_delta", value: 100 } }), sample(3, NOW - 8000, { cpu: { kind: "gauge", value: 50 }, bytes: { kind: "counter_delta", value: 100 } }), sample(4, NOW - 1000, { cpu: { kind: "gauge", value: 10 } })];
  rows[0].interval_ms = 2000; rows[3].session_id = "session-b";
  const series = systemHistorySeries(history(rows));
  assert.deepEqual(series.find(series => series.metric_name === "bytes").points.map(point => point.value), [100, 100, 100]);
  assert.deepEqual(series.find(series => series.metric_name === "cpu").points.map(point => point.break_before), [true, true, true]);
  assert.deepEqual(series.find(series => series.metric_name === "cpu").points.map(point => point.value), [40, 50, 10]);
  const zero = sample(1, NOW - 1000, { bytes: { kind: "counter_delta", value: 10 } }); zero.interval_ms = 0;
  assert.deepEqual(systemHistorySeries(history([zero])), []);
});

test("hourly series use measured coverage means and rates rather than sample count or full hour duration", () => {
  const aggregate = { kind: "gauge", sample_count: 2, coverage_ms: 20_000, sum: 600_000, min: 20, max: 40, last: 40, last_at_ms: NOW - 1 };
  const hour = { hour_start_ms: Math.floor(NOW / HOUR) * HOUR, revision: 1, sample_count: 2, first_sample_at_ms: NOW - 20_000, last_sample_at_ms: NOW - 1, inventory_revisions: [1], metrics: { cpu: aggregate, transfer: { ...aggregate, kind: "counter_delta", sum: 2000 }, warmup: { ...aggregate, coverage_ms: 0, sum: 0 } } };
  const series = systemHistorySeries({ resolution: "hourly", samples: [], hours: [hour] });
  assert.equal(series.find(series => series.metric_name === "cpu").points[0].value, 30);
  assert.equal(series.find(series => series.metric_name === "transfer").points[0].value, 100);
  assert.equal(series.some(series => series.metric_name === "warmup"), false);
});

test("monitor paginates bounded history, keeps metric selection and rejects mismatched late machine responses", async t => {
  const original_fetch = globalThis.fetch; t.after(() => { globalThis.fetch = original_fetch; });
  const requests = [], pending = deferred(); let hold_a = false;
  globalThis.fetch = async url => {
    const request = new URL(url, "http://localhost"); requests.push(request);
    if (request.pathname === "/api/system-machines") return reply({ machines: [machine(), machine("machine-b")], next_cursor: null });
    if (request.pathname === "/api/system-profile") return reply(machine(request.searchParams.get("machine_id")));
    const machine_id = request.searchParams.get("machine_id");
    if (hold_a && machine_id === "machine-a") return pending.promise;
    return reply(history([sample(request.searchParams.has("cursor") ? 2 : 1)], machine_id, request.searchParams.has("cursor") ? null : 1));
  };
  const monitor = new SystemMonitor(new RequestLane(), new RequestLane(), () => NOW, () => {}); t.after(() => monitor.cancel());
  monitor.setActive(true); await settle(() => monitor.snapshot().loaded_at_ms !== null && !monitor.snapshot().loading);
  assert.equal(monitor.snapshot().series[0].points.length, 2);
  monitor.chooseMetric("cpu/usage_percent", false);
  await monitor.refresh(true);
  assert.deepEqual(monitor.snapshot().selected_metrics, []);
  hold_a = true; const old = monitor.refresh(); await settle(() => requests.at(-1).pathname === "/api/system-history" && requests.at(-1).searchParams.get("machine_id") === "machine-a");
  monitor.chooseMachine("machine-b"); await settle(() => monitor.snapshot().profile?.machine_id === "machine-b" && !monitor.snapshot().loading);
  pending.resolve(reply(history([sample(4)], "machine-a")));
  assert.equal(await old, "cancelled"); assert.equal(monitor.snapshot().profile.machine_id, "machine-b");
});

test("monitor bounds byte-limited history pages and reports truncation without fetching forever", async t => {
  const original_fetch = globalThis.fetch; t.after(() => { globalThis.fetch = original_fetch; }); let pages = 0;
  globalThis.fetch = async url => {
    const request = new URL(url, "http://localhost");
    if (request.pathname === "/api/system-machines") return reply({ machines: [machine()], next_cursor: null });
    if (request.pathname === "/api/system-profile") return reply(machine());
    pages++; return reply(history([sample(pages)], "machine-a", pages));
  };
  const monitor = new SystemMonitor(new RequestLane(), new RequestLane(), () => NOW, () => {}); t.after(() => monitor.cancel());
  monitor.setActive(true); await settle(() => !monitor.snapshot().loading);
  assert.equal(pages, 16); assert.equal(monitor.snapshot().truncated, true); assert.equal(monitor.snapshot().series[0].points.length, 16);
});

test("unsupported and empty stores do not invent machine statistics", async t => {
  const original_fetch = globalThis.fetch; t.after(() => { globalThis.fetch = original_fetch; }); let available = false;
  globalThis.fetch = async () => available ? reply({ machines: [], next_cursor: null }) : reply({ error: "Unavailable" }, 404);
  const monitor = new SystemMonitor(new RequestLane(), new RequestLane(), () => NOW, () => {}); t.after(() => monitor.cancel());
  monitor.setActive(true); await settle(() => !monitor.snapshot().loading);
  assert.equal(monitor.snapshot().unsupported, true); assert.deepEqual(monitor.snapshot().series, []);
  available = true; assert.equal(await monitor.refresh(), "success");
  assert.equal(monitor.snapshot().unsupported, false); assert.equal(monitor.snapshot().profile, null); assert.equal(monitor.snapshot().error, null);
});

test("a failed first machine history retains catalog and inventory so another machine remains selectable", async t => {
  const original_fetch = globalThis.fetch; t.after(() => { globalThis.fetch = original_fetch; });
  globalThis.fetch = async url => {
    const request = new URL(url, "http://localhost"), machine_id = request.searchParams.get("machine_id");
    if (request.pathname === "/api/system-machines") return reply({ machines: [machine(), machine("machine-b")], next_cursor: null });
    if (request.pathname === "/api/system-profile") return reply(machine(machine_id));
    return machine_id === "machine-a" ? reply({ error: "History temporarily unavailable" }, 503) : reply(history([sample(1)], machine_id));
  };
  const monitor = new SystemMonitor(new RequestLane(), new RequestLane(), () => NOW, () => {}); t.after(() => monitor.cancel());
  monitor.setActive(true); await settle(() => !monitor.snapshot().loading);
  assert.equal(monitor.snapshot().machines.length, 2); assert.equal(monitor.snapshot().profile.machine_id, "machine-a");
  assert.match(monitor.snapshot().error, /History temporarily unavailable/);
  monitor.chooseMachine("machine-b"); await settle(() => !monitor.snapshot().loading);
  assert.equal(monitor.snapshot().profile.machine_id, "machine-b"); assert.equal(monitor.snapshot().error, null); assert.equal(monitor.snapshot().series[0].points.length, 1);
});

test("oversized monitor responses fail before JSON parsing with the actual system read limit", async t => {
  const original_fetch = globalThis.fetch; t.after(() => { globalThis.fetch = original_fetch; });
  globalThis.fetch = async () => ({ ok: true, status: 200, text: async () => " ".repeat(512 * 1024 + 1) });
  const monitor = new SystemMonitor(new RequestLane(), new RequestLane(), () => NOW, () => {}); t.after(() => monitor.cancel());
  monitor.setActive(true); await settle(() => !monitor.snapshot().loading);
  assert.match(monitor.snapshot().error, /system response exceeds the 512 KiB/); assert.doesNotMatch(monitor.snapshot().error, /chart|2 MiB/);
});
