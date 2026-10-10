import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { attachChartInteractions, createChartController, parseChartPointLabel, parseChartRange, formatChartX, formatChartTick, chartXFraction, dragChartRange, zoomChartRange, restoreChartRange, nearestChartPoint } from "./.test/app.js";

const MAX_STEP = 18446744073709551615n;
function range(first, last, x_axis = "step") { return { x_axis, start_x: String(first), end_x: String(last) }; }
function point(step, x, y, value_text = "1") {
  return { run_index: 1, step: String(step), value: Number(value_text), value_text, x, y };
}
function localLabels(time_zone, values, domain) {
  const script = `
    import { formatChartX, formatChartTick } from ${JSON.stringify(new URL("./.test/app.js", import.meta.url).href)};
    const values = ${JSON.stringify(values)}, domain = ${JSON.stringify(domain)};
    console.log(JSON.stringify(values.map(value => ({ full: formatChartX(value, "wall_clock", "local"), tick: formatChartTick(value, domain, "local") }))));
  `;
  return JSON.parse(execFileSync(process.execPath, ["--input-type=module", "-e", script], { env: { ...process.env, TZ: time_zone }, encoding: "utf8" }));
}

test("exact labels retain u64 steps and precise finite value strings", () => {
  const parsed = parseChartPointLabel("Run 8 · step 18446744073709551615 · value -1.7976931348623157e308");
  assert.deepEqual(parsed, { run_index: 8, step: String(MAX_STEP), value: -Number.MAX_VALUE, value_text: "-1.7976931348623157e308" });
  assert.equal(parseChartPointLabel("Run 1 · step 0 · value -0").value_text, "-0");
  assert.equal(Object.is(parseChartPointLabel("Run 1 · step 0 · value -0").value, -0), true);
  assert.deepEqual(parseChartPointLabel("Run 1 · step 12 · value 0.5 · timestamp 2026-10-06T01:02:03.123456789Z · elapsed 2.5 s"), { run_index: 1, step: "12", value: .5, value_text: "0.5" });
  assert.deepEqual(parseChartPointLabel("Run 1 · step 12 · value 0.5 · timestamp 2026-10-06T09:02:03+08:00"), { run_index: 1, step: "12", value: .5, value_text: "0.5" });
  for (const label of [
    "Run 9 · step 1 · value 1", "Run 0 · step 1 · value 1", "Run 1 · step -1 · value 1",
    "Run 1 · step 18446744073709551616 · value 1", "Run 1 · step 1 · value Infinity",
    "Run 1 · step 1 · value NaN", "Run 1 · step 1 · value 1e999", "Run 1 · step 1 · value 0x10",
    "run-id · step 1 · value 1", "Run 1 · step 1 · value 1 extra",
    "Run 1 · step 1 · value 1 · timestamp bad\nlabel", "Run 1 · step 1 · value 1 · axis anything",
  ]) assert.equal(parseChartPointLabel(label), null, label);
});

test("explicit chart ranges validate exact step and signed nanosecond coordinates", () => {
  assert.deepEqual(parseChartRange("step", "0", String(MAX_STEP)), range(0n, MAX_STEP));
  assert.deepEqual(parseChartRange("elapsed", "-2500000000", "0"), range(-2_500_000_000n, 0n, "elapsed"));
  assert.deepEqual(parseChartRange("wall_clock", "-1", "1780000000000000001"), range(-1n, 1_780_000_000_000_000_001n, "wall_clock"));
  for (const [axis, first, last] of [
    ["seconds", "0", "1"], [null, "0", "1"], ["step", null, "1"], ["elapsed", "0", null],
    ["step", "-1", "1"], ["step", "0", String(MAX_STEP + 1n)], ["wall_clock", "2", "1"],
    ["elapsed", "0.5", "1"], ["elapsed", "0", "1e9"], ["wall_clock", "-0", "1"],
    ["elapsed", "01", "2"], ["elapsed", "0", String(1n << 127n)],
    ["wall_clock", String(-(1n << 127n) - 1n), "0"],
  ]) assert.equal(parseChartRange(axis, first, last), null, `${axis}: ${first}–${last}`);
});

test("time zoom keeps adjacent nanoseconds exact and supports pre-epoch and regressing clocks", () => {
  const epoch = 1_780_000_000_000_000_000n, domain = range(epoch, epoch + 2n, "wall_clock");
  assert.equal(chartXFraction(String(epoch + 1n), domain), .5);
  assert.deepEqual(dragChartRange(range(-100n, 100n, "elapsed"), .25, .75), range(-50n, 50n, "elapsed"));
  assert.deepEqual(zoomChartRange(range(-100n, 100n, "wall_clock"), range(-100n, 100n, "wall_clock"), .5), range(-50n, 50n, "wall_clock"));
  assert.deepEqual(restoreChartRange(range(epoch, epoch + 1n, "wall_clock"), range(epoch, epoch + 10n, "wall_clock")), range(epoch, epoch + 1n, "wall_clock"));
  assert.deepEqual(restoreChartRange(range(1n, 2n), range(-20n, 30n, "elapsed")), range(-20n, 30n, "elapsed"), "changing axes resets the inspected domain");
  assert.throws(() => zoomChartRange(range(1n, 2n), range(1n, 2n, "elapsed"), .5), /different chart axes/);
  assert.throws(() => chartXFraction("-1", range(0n, 1n)), /Invalid chart axis coordinate/);
});

test("time labels show readable UTC and elapsed values without losing nanoseconds", () => {
  assert.equal(formatChartX(String(MAX_STEP), "step"), String(MAX_STEP));
  assert.equal(formatChartX("0", "elapsed"), "0 s");
  assert.equal(formatChartX("2500000000", "elapsed"), "2.5 s");
  assert.equal(formatChartX("-1", "elapsed"), "−1 ns");
  assert.equal(formatChartX("2500", "elapsed"), "2.5 µs");
  assert.equal(formatChartX("1234567", "elapsed"), "1.234567 ms");
  assert.equal(formatChartX("62000000001", "elapsed"), "1:02.000000001 min");
  assert.equal(formatChartX("3723123456789", "elapsed"), "1:02:03.123456789 h");
  assert.equal(formatChartX("0", "wall_clock"), "1970-01-01 00:00:00 UTC");
  assert.equal(formatChartX("1", "wall_clock"), "1970-01-01 00:00:00.000000001 UTC");
  assert.equal(formatChartX("-1", "wall_clock"), "1969-12-31 23:59:59.999999999 UTC");
  assert.equal(formatChartX("1700000000123456789", "wall_clock"), "2023-11-14 22:13:20.123456789 UTC");
  assert.equal(formatChartX("1700000000100000000", "wall_clock"), "2023-11-14 22:13:20.1 UTC");
});

test("wall-clock ticks stay compact while preserving precision for midnight zoom", () => {
  const nanos = iso => BigInt(Date.parse(iso)) * 1_000_000n;
  const morning = nanos("2026-10-06T01:02:03Z"), midnight = nanos("2026-10-07T00:00:00Z");
  assert.equal(formatChartTick(String(morning + 1n), range(morning, morning + 2n, "wall_clock")), "01:02:03.000000001");
  assert.equal(formatChartTick(String(midnight - 1n), range(midnight - 1n, midnight, "wall_clock")), "10-06 23:59:59.999999999");
  assert.equal(formatChartTick(String(midnight), range(midnight - 1n, midnight, "wall_clock")), "10-07 00:00:00");
  const previous = nanos("2026-12-31T23:00:00Z"), next = nanos("2027-01-01T01:00:00Z");
  assert.equal(formatChartTick(String(next), range(previous, next, "wall_clock")), "01-01 01:00", "year changes retain useful time within a short range");
  assert.equal(formatChartTick(String(midnight), range(nanos("2025-10-01T00:00:00Z"), midnight, "wall_clock")), "2026-10-07");
  assert.equal(formatChartTick("2500000000", range(0n, 3_000_000_000n, "elapsed")), "2.5 s");
});

test("local date labels keep nanoseconds and the viewer's calendar date across midnight", () => {
  const midnight = BigInt(Date.parse("2026-10-06T16:00:00Z")) * 1_000_000n;
  assert.deepEqual(localLabels("Asia/Shanghai", [String(midnight - 1n), String(midnight)], range(midnight - 1n, midnight, "wall_clock")), [
    { full: "2026-10-06 23:59:59.999999999 UTC+08:00 (Asia/Shanghai)", tick: "10-06 23:59:59.999999999" },
    { full: "2026-10-07 00:00:00 UTC+08:00 (Asia/Shanghai)", tick: "10-07 00:00:00" },
  ]);
  assert.equal(localLabels("Asia/Shanghai", ["-1"], range(-1n, -1n, "wall_clock"))[0].full, "1970-01-01 07:59:59.999999999 UTC+08:00 (Asia/Shanghai)");
  assert.equal(formatChartX(String(midnight - 1n), "wall_clock", "utc"), "2026-10-06 15:59:59.999999999 UTC", "UTC display leaves exact instants unchanged");
});

test("local labels support fractional-hour offsets and historical offset seconds", () => {
  const now = "1700000000123456789";
  assert.match(localLabels("Asia/Kathmandu", [now], range(now, now, "wall_clock"))[0].full, /^2023-11-15 03:58:20\.123456789 UTC\+05:45 \(Asia\/Kat(?:h)?mandu\)$/, "ICU versions may retain either valid IANA spelling");
  const old = BigInt(Date.parse("1900-01-01T00:00:00Z")) * 1_000_000n;
  assert.match(localLabels("Asia/Kathmandu", [String(old)], range(old, old, "wall_clock"))[0].full, /^1900-01-01 05:41:16 UTC\+05:41:16 \(Asia\/Kat(?:h)?mandu\)$/);
});

test("repeated local hours distinguish the offset at each sample during DST changes", () => {
  const first = BigInt(Date.parse("2026-11-01T05:30:00Z")) * 1_000_000n;
  const last = BigInt(Date.parse("2026-11-01T06:30:00Z")) * 1_000_000n;
  assert.deepEqual(localLabels("America/New_York", [String(first), String(last)], range(first, last, "wall_clock")), [
    { full: "2026-11-01 01:30:00 UTC−04:00 (America/New_York)", tick: "01:30:00 UTC−04:00" },
    { full: "2026-11-01 01:30:00 UTC−05:00 (America/New_York)", tick: "01:30:00 UTC−05:00" },
  ]);
  assert.equal(localLabels("UTC", ["0"], range(0n, 0n, "wall_clock"))[0].full, "1970-01-01 00:00:00 UTC");
});

test("adjacent steps above Number precision remain distinct in the plot range", () => {
  const domain = range(MAX_STEP - 2n, MAX_STEP);
  assert.equal(chartXFraction(String(MAX_STEP - 2n), domain), 0);
  assert.equal(chartXFraction(String(MAX_STEP - 1n), domain), .5);
  assert.equal(chartXFraction(String(MAX_STEP), domain), 1);
  assert.equal(chartXFraction("100", range(100n, 100n)), .5);
  assert.throws(() => chartXFraction("1", range(10n, 1n)), /Invalid chart axis range/);
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

test("refresh follows the full domain and keeps zoom at exact absolute steps", () => {
  assert.deepEqual(restoreChartRange(null, range(0n, 200n)), range(0n, 200n), "full-range views follow incoming samples");
  assert.deepEqual(restoreChartRange(range(20n, 80n), range(0n, 200n)), range(20n, 80n), "a growing domain does not move an inspected range");
  assert.deepEqual(restoreChartRange(range(20n, 80n), range(30n, 60n)), range(30n, 60n), "removed samples clamp the zoom to the remaining domain");
  assert.deepEqual(restoreChartRange(range(20n, 80n), range(90n, 100n)), range(90n, 90n));
  assert.deepEqual(restoreChartRange(range(20n, 80n), range(0n, 10n)), range(10n, 10n));
  assert.deepEqual(restoreChartRange(range(MAX_STEP - 2n, MAX_STEP), range(MAX_STEP - 1n, MAX_STEP)), range(MAX_STEP - 1n, MAX_STEP));
  assert.throws(() => restoreChartRange(range(2n, 1n), range(0n, 3n)), /Invalid chart axis range/);
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

test("chart refresh refuses unloaded frames and remains inert after disposal", () => {
  const handlers = new Map();
  const frame = { contentDocument: undefined, addEventListener: (name, callback) => handlers.set(name, callback), removeEventListener: (name, callback) => { if (handlers.get(name) === callback) handlers.delete(name); } };
  const controller = createChartController(frame);
  assert.equal(controller.isInteracting(), false);
  assert.equal(controller.previewStatus(), "loading");
  assert.equal(controller.replacePreview("<html></html>"), false);
  assert.doesNotThrow(() => controller.setTimeZone("utc"), "display choices can change before the frame loads");
  controller.dispose();
  assert.equal(controller.replacePreview("<html></html>"), false);
  assert.doesNotThrow(() => controller.setTimeZone("local"), "disposed controllers stay inert");
  assert.equal(handlers.size, 0);
  assert.doesNotThrow(() => controller.dispose());
});

test("preview status separates slow navigation, completed errors and stale loads", t => {
  const previous_observer = globalThis.MutationObserver;
  let changed;
  globalThis.MutationObserver = class { constructor(callback) { changed = callback; } observe() {} disconnect() {} };
  t.after(() => { globalThis.MutationObserver = previous_observer; });
  const handlers = new Map();
  const frame = {
    ownerDocument: { baseURI: "https://expri.example.net/" }, src: "https://expri.example.net/api/chart?run_id=A", contentDocument: null,
    addEventListener: (name, callback) => handlers.set(name, callback),
    removeEventListener: (name, callback) => { if (handlers.get(name) === callback) handlers.delete(name); },
  };
  const controller = createChartController(frame); t.after(() => controller.dispose());
  assert.equal(controller.previewStatus(), "loading", "inaccessible content before load can still be pending");
  handlers.get("load")();
  assert.equal(controller.previewStatus(), "invalid", "a completed native error page can be retried");
  frame.src = "https://expri.example.net/api/chart?run_id=B";
  assert.equal(controller.previewStatus(), "loading", "a new source is not the completed old request");
  changed();
  frame.contentDocument = { URL: "https://expri.example.net/api/chart?run_id=A", readyState: "complete", title: "", querySelectorAll: () => [], querySelector: () => null };
  handlers.get("load")();
  assert.equal(controller.previewStatus(), "loading", "a stale load never restarts the pending current source");
  frame.contentDocument.URL = frame.src;
  handlers.get("load")();
  assert.equal(controller.previewStatus(), "invalid", "a completed current HTTP error is distinguishable from pending");
  changed();
  assert.equal(controller.previewStatus(), "loading", "same-source reload waits for its own completion");
  frame.contentDocument.title = "Run comparison · expri";
  frame.contentDocument.querySelector = () => ({});
  handlers.get("load")();
  assert.equal(controller.previewStatus(), "ready", "a valid no-metrics preview is ready too");
  frame.src = "https://other.example.net/chart"; changed(); handlers.get("load")();
  assert.equal(controller.previewStatus(), "loading", "foreign frames are never offered for automatic reload");
});
