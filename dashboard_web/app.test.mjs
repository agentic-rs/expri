import test from "node:test";
import assert from "node:assert/strict";
import { apiUrl, formatDuration, formatNumber, formatValue, RequestLane } from "./app.js";

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
