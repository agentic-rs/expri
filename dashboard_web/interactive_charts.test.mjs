import test from "node:test";
import assert from "node:assert/strict";
import { attachChartInteractions, parseChartPointLabel, chartStepFraction, dragChartRange, zoomChartRange, nearestChartPoint } from "./app.js";

const MAX_STEP = 18446744073709551615n;
function range(first, last) { return { start_step: String(first), end_step: String(last) }; }
function point(step, x, y, value_text = "1") {
  return { run_index: 1, step: String(step), value: Number(value_text), value_text, x, y };
}

test("exact labels retain u64 steps and precise finite value strings", () => {
  const parsed = parseChartPointLabel("Run 8 · step 18446744073709551615 · value -1.7976931348623157e308");
  assert.deepEqual(parsed, { run_index: 8, step: String(MAX_STEP), value: -Number.MAX_VALUE, value_text: "-1.7976931348623157e308" });
  assert.equal(parseChartPointLabel("Run 1 · step 0 · value -0").value_text, "-0");
  assert.equal(Object.is(parseChartPointLabel("Run 1 · step 0 · value -0").value, -0), true);
  for (const label of [
    "Run 9 · step 1 · value 1", "Run 0 · step 1 · value 1", "Run 1 · step -1 · value 1",
    "Run 1 · step 18446744073709551616 · value 1", "Run 1 · step 1 · value Infinity",
    "Run 1 · step 1 · value NaN", "Run 1 · step 1 · value 1e999", "Run 1 · step 1 · value 0x10",
    "run-id · step 1 · value 1", "Run 1 · step 1 · value 1 extra",
  ]) assert.equal(parseChartPointLabel(label), null, label);
});

test("adjacent steps above Number precision remain distinct in the plot range", () => {
  const domain = range(MAX_STEP - 2n, MAX_STEP);
  assert.equal(chartStepFraction(String(MAX_STEP - 2n), domain), 0);
  assert.equal(chartStepFraction(String(MAX_STEP - 1n), domain), .5);
  assert.equal(chartStepFraction(String(MAX_STEP), domain), 1);
  assert.equal(chartStepFraction("100", range(100n, 100n)), .5);
  assert.throws(() => chartStepFraction("1", range(10n, 1n)), /Invalid chart step range/);
});

test("drag zoom is direction independent, inclusive, and preserves exact large steps", () => {
  assert.deepEqual(dragChartRange(range(0n, 100n), .2, .8), range(20n, 80n));
  assert.deepEqual(dragChartRange(range(0n, 100n), .8, .2), range(20n, 80n));
  assert.deepEqual(dragChartRange(range(0n, 100n), -2, 2), range(0n, 100n));
  assert.deepEqual(dragChartRange(range(MAX_STEP - 100n, MAX_STEP), .25, .75), range(MAX_STEP - 75n, MAX_STEP - 25n));
  assert.deepEqual(dragChartRange(range(5n, 5n), .1, .9), range(5n, 5n));
  assert.deepEqual(dragChartRange(range(5n, 6n), .01, .02), range(5n, 6n));
});

test("zoom is bounded by the original domain and never invents fractional steps", () => {
  const full = range(MAX_STEP - 100n, MAX_STEP);
  const zoomed = zoomChartRange(full, full, .5);
  assert.deepEqual(zoomed, range(MAX_STEP - 75n, MAX_STEP - 25n));
  assert.deepEqual(zoomChartRange(zoomed, full, 2), full);
  assert.deepEqual(zoomChartRange(full, full, .5, 0), range(MAX_STEP - 100n, MAX_STEP - 50n));
  assert.deepEqual(zoomChartRange(full, full, .5, 1), range(MAX_STEP - 50n, MAX_STEP));
  assert.deepEqual(zoomChartRange(range(10n, 11n), range(10n, 11n), .00001), range(10n, 11n));
  assert.deepEqual(zoomChartRange(range(10n, 10n), range(10n, 10n), .5), range(10n, 10n));
  for (const factor of [NaN, Infinity, 0, -1]) assert.deepEqual(zoomChartRange(full, full, factor), full);
});

test("nearest sample uses displayed points without interpolating or collapsing reset steps", () => {
  const logged = [point(10, 100, 100, "1"), point(0, 0, 200, "2"), point(10, 100, 140, "1.5"), point(10, 100, 140, "1.5"), point(20, 200, 10, "9")];
  const order = [...logged];
  assert.equal(nearestChartPoint(logged, 100, 145), logged[2]);
  assert.equal(nearestChartPoint(logged, 100, 140, logged[3]), logged[3], "keyboard retains duplicate sample identity");
  assert.equal(nearestChartPoint(logged, 200, 10, logged[4]), logged[4], "keyboard can advance beyond duplicate coordinates");
  assert.equal(nearestChartPoint(logged, 45, 100), logged[1], "comparison picks a recorded sample rather than an interpolated value");
  assert.equal(nearestChartPoint([], 0), null);
  assert.equal(nearestChartPoint(logged, NaN), null);
  assert.deepEqual(logged, order, "logging order stays intact");
});

test("frame lifecycle is safe before a document exists and removes its load handler", () => {
  const handlers = new Map();
  const frame = { contentDocument: undefined, addEventListener: (name, callback) => handlers.set(name, callback), removeEventListener: (name, callback) => { if (handlers.get(name) === callback) handlers.delete(name); } };
  const cleanup = attachChartInteractions(frame);
  assert.equal(typeof handlers.get("load"), "function");
  assert.doesNotThrow(() => handlers.get("load")());
  cleanup();
  assert.equal(handlers.size, 0);
});
