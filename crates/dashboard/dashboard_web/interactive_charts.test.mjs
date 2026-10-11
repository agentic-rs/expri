import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { JSDOM } from "jsdom";
import { act } from "react";

execFileSync("./node_modules/.bin/esbuild", [
  "interactive_charts.ts", "--bundle", "--format=esm", "--target=es2022", "--jsx=automatic", "--packages=external", "--outfile=.test/interactive_charts.js",
], { cwd: new URL(".", import.meta.url), stdio: "pipe" });
const { createChartController, parseChartPointLabel, parseChartRange, formatChartX, formatChartTick, chartXFraction, dragChartRange, zoomChartRange, restoreChartRange, nearestChartPoint, chartYFraction } = await import("./.test/interactive_charts.js");

const MAX_STEP = 18446744073709551615n;
function range(first, last, x_axis = "step") { return { x_axis, start_x: String(first), end_x: String(last) }; }
function point(step, x, y, value_text = "1") {
  return { run_index: 1, step: String(step), value: Number(value_text), value_text, x, y };
}
function localLabels(time_zone, values, domain) {
  const script = `
    import { formatChartX, formatChartTick } from ${JSON.stringify(new URL("./.test/interactive_charts.js", import.meta.url).href)};
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

function typedChart({ x_axis = "step", domain = ["0", "100"], runs = ["run-a", "run-b"], points } = {}) {
  const samples = points ?? [
    { step: "0", value: 1, x_value: domain[0], timestamp: null },
    { step: "50", value: .5, x_value: String((BigInt(domain[0]) + BigInt(domain[1])) / 2n), timestamp: null },
    { step: "100", value: 0, x_value: domain[1], timestamp: null },
  ];
  return {
    schema_version: 1, x_axis, runs,
    plots: [{ metric_name: "loss", domain: { x_min: domain[0], x_max: domain[1], value_min: 0, value_max: 1 }, series: runs.map((_, index) => ({
      run_index: index + 1,
      summary: { count: samples.length, missing_timestamp_count: samples.filter(point => point.timestamp === null).length, last: samples.at(-1), min: samples.at(-1), max: samples[0] },
      points: samples.map(point => ({ ...point })),
    })) }],
    warnings: [], warnings_truncated: false,
    parameter_rows: [{ name: "batch_size", values: runs.map(() => "8"), differs: false }], parameters_truncated: false,
  };
}

function chartHarness(t) {
  const dom = new JSDOM('<!doctype html><body><div id="chart-preview"></div></body>', { url: "http://localhost/", pretendToBeVisual: true });
  const globals = ["window", "document", "navigator", "HTMLElement", "Node", "Event", "KeyboardEvent", "IS_REACT_ACT_ENVIRONMENT"];
  const previous = new Map(globals.map(name => [name, Object.getOwnPropertyDescriptor(globalThis, name)]));
  for (const name of globals) Object.defineProperty(globalThis, name, { configurable: true, writable: true, value: name === "IS_REACT_ACT_ENVIRONMENT" ? true : dom.window[name] });
  const container = dom.window.document.querySelector("#chart-preview");
  const controller = createChartController(container);
  t.after(() => {
    act(() => controller.dispose());
    dom.window.close();
    for (const [name, descriptor] of previous) descriptor ? Object.defineProperty(globalThis, name, descriptor) : Reflect.deleteProperty(globalThis, name);
  });
  return {
    dom, container, controller,
    render(data) { let result; act(() => { result = controller.replacePreview(data); }); return result; },
    click(selector) { act(() => container.querySelector(selector).click()); },
    key(key) { const region = container.querySelector(".plot-scroll"); act(() => region.dispatchEvent(new dom.window.KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }))); },
    axis() { const label = container.querySelector("[data-chart-range]"); return [label.dataset.startX, label.dataset.endX]; },
  };
}

test("typed preview has constant-time readiness, explicit clearing, and an inert disposed lifecycle", t => {
  const { container, controller, render } = chartHarness(t);
  assert.equal(controller.previewStatus(), "loading");
  assert.equal(controller.isInteracting(), false);
  assert.doesNotThrow(() => controller.setTimeZone("utc"));
  assert.equal(render({ ...typedChart(), plots: [] }), true);
  assert.equal(controller.previewStatus(), "ready", "a no-metrics response is complete");
  assert.match(container.textContent, /No scalar metrics/);
  const original = container.querySelectorAll;
  container.querySelectorAll = () => { throw new Error("Readiness must not walk the DOM"); };
  assert.equal(controller.previewStatus(), "ready");
  container.querySelectorAll = original;
  act(() => controller.clear());
  assert.equal(controller.previewStatus(), "loading");
  assert.equal(container.childNodes.length, 0);
  assert.equal(render(typedChart()), true);
  act(() => controller.dispose());
  assert.equal(controller.previewStatus(), "invalid");
  assert.equal(render(typedChart()), false);
  assert.equal(container.childNodes.length, 0);
  assert.doesNotThrow(() => controller.clear());
  assert.doesNotThrow(() => controller.setTimeZone("local"));
});

test("typed rendering treats names, warnings and bounded parameter prefixes as text", t => {
  const { container, render } = chartHarness(t);
  const data = typedChart({ runs: ['<img src="x">', "run-b"] });
  data.plots[0].metric_name = "<script>metric</script>";
  data.warnings = [{ run_index: 1, message: '<iframe src="bad"> note' }];
  data.parameter_rows = [
    { name: "same bounded prefix…", values: ['<a href="bad">', null], differs: true },
    { name: "same bounded prefix…", values: ["second row", "different"], differs: true },
  ];
  render(data);
  assert.equal(container.querySelector("img,script,iframe,a"), null);
  assert.equal(container.querySelector("[style]"), null, "the parent dashboard needs no inline-style CSP exception");
  assert.equal(container.querySelector(".metric-chart h2").textContent, data.plots[0].metric_name);
  assert.match(container.textContent, /<iframe src="bad"> note/);
  assert.equal(container.querySelectorAll(".params tbody tr").length, 2);
  data.parameter_rows[0].values[0] = "changed first row";
  render(data);
  assert.deepEqual([...container.querySelectorAll(".params tbody tr")].map(row => row.querySelector("td").textContent), ["changed first row", "second row"]);
  render(typedChart({ runs: ["run-a"] }));
  assert.equal(container.querySelector(".parameter-comparison"), null, "single-run parameters stay in the Parameters tab");
});

test("refresh preserves stable plot nodes, absolute zoom, hidden runs, focus and parameter expansion", t => {
  const h = chartHarness(t);
  h.render(typedChart());
  const svg = h.container.querySelector("svg"), details = h.container.querySelector("details");
  details.open = true;
  h.container.scrollTop = 50;
  h.click('[data-chart-zoom="in"]');
  assert.deepEqual(h.axis(), ["25", "75"]);
  h.click('[data-chart-run="2"]');
  const button = h.container.querySelector('[data-chart-run="2"]'); button.focus();
  h.render(typedChart({ domain: ["0", "200"] }));
  assert.equal(h.container.querySelector("svg"), svg);
  assert.equal(h.container.querySelector("details"), details);
  assert.equal(details.open, true);
  assert.equal(h.container.scrollTop, 50);
  assert.deepEqual(h.axis(), ["25", "75"]);
  assert.equal(h.container.querySelector('[data-chart-run="2"]').getAttribute("aria-pressed"), "false");
  assert.equal(h.dom.window.document.activeElement, h.container.querySelector('[data-chart-run="2"]'));
  assert.equal(h.container.querySelectorAll("[data-chart-hit]").length, 1, "refresh never accumulates interaction layers");
  h.click('[data-chart-zoom="reset"]');
  h.render(typedChart({ domain: ["0", "300"] }));
  assert.deepEqual(h.axis(), ["0", "300"], "reset resumes following new data");
});

test("refresh preserves logging order and keyboard identity through repeated and reset steps", t => {
  const h = chartHarness(t);
  const samples = [
    { step: "10", value: .5, x_value: "10", timestamp: null },
    { step: "0", value: -0, x_value: "0", timestamp: null },
    { step: "10", value: .5, x_value: "10", timestamp: null },
    { step: "10", value: .5, x_value: "10", timestamp: null },
  ];
  h.render(typedChart({ domain: ["0", "10"], points: samples }));
  const region = h.container.querySelector(".plot-scroll"); region.focus();
  const row = () => h.container.querySelector('[data-chart-readout-run="1"]');
  h.key("ArrowRight"); assert.match(row().textContent, /sample 1 of 4 displayed/);
  h.key("ArrowRight"); assert.equal(row().dataset.chartValue, "-0");
  h.key("ArrowRight"); h.key("ArrowRight"); assert.match(row().textContent, /sample 4 of 4 displayed/);
  h.render(typedChart({ domain: ["0", "20"], points: [...samples, { step: "20", value: 1, x_value: "20", timestamp: null }] }));
  assert.equal(h.dom.window.document.activeElement, region);
  assert.match(row().textContent, /sample 4 of 5 displayed/, "duplicate occurrence stays anchored after refresh");
  h.key("ArrowRight"); assert.equal(row().dataset.chartStep, "20");
});

test("empty time plots retain visibility and zoom when timestamped data returns; axes reset exact range", t => {
  const h = chartHarness(t);
  const epoch = 1_780_000_000_000_000_000n;
  const data = typedChart({ x_axis: "wall_clock", domain: [String(epoch), String(epoch + 100n)] });
  for (const series of data.plots[0].series) for (const point of series.points) point.timestamp = "2026-05-28T00:00:00Z";
  h.render(data); h.click('[data-chart-zoom="in"]'); h.click('[data-chart-run="2"]');
  const zoom = h.axis();
  h.render({ ...data, plots: [{ ...data.plots[0], domain: null, series: data.plots[0].series.map(series => ({ ...series, points: [] })) }] });
  assert.equal(h.container.querySelector("svg"), null);
  assert.match(h.container.textContent, /No timestamped samples/);
  h.render(data);
  assert.deepEqual(h.axis(), zoom);
  assert.equal(h.container.querySelector('[data-chart-run="2"]').getAttribute("aria-pressed"), "false");
  const svg = h.container.querySelector("svg");
  act(() => h.controller.setTimeZone("utc"));
  assert.equal(h.container.querySelector("svg"), svg);
  assert.equal(h.container.querySelector("[data-chart-range]").dataset.timeZone, "utc");
  const elapsed = typedChart({ x_axis: "elapsed" });
  for (const series of elapsed.plots[0].series) for (const point of series.points) point.timestamp = "2026-05-28T00:00:00Z";
  h.render(elapsed);
  assert.match(h.container.querySelectorAll("circle")[2].getAttribute("aria-label"), / · elapsed 100 ns$/);
  h.render(typedChart());
  assert.deepEqual(h.axis(), ["0", "100"]);
  assert.equal(h.container.querySelector('[data-chart-run="2"]').getAttribute("aria-pressed"), "false");
});

test("full finite domains position extreme values without overflow or sampled-extrema rescaling", t => {
  const h = chartHarness(t);
  const data = typedChart({ runs: ["run-a"] });
  data.plots[0].domain.value_min = -Number.MAX_VALUE;
  data.plots[0].domain.value_max = Number.MAX_VALUE;
  data.plots[0].series[0].points = [
    { step: "0", value: -Number.MAX_VALUE, x_value: "0", timestamp: null },
    { step: "50", value: 0, x_value: "50", timestamp: null },
    { step: "100", value: Number.MAX_VALUE, x_value: "100", timestamp: null },
  ];
  h.render(data);
  assert.deepEqual([...h.container.querySelectorAll("circle")].map(circle => Number(circle.getAttribute("cy"))), [274, 151, 28]);
  assert.ok([...h.container.querySelectorAll(".tick")].every(tick => !/Infinity|NaN/.test(tick.textContent)));
  assert.equal(chartYFraction(0, -Number.MAX_VALUE, Number.MAX_VALUE), .5);
  data.plots[0].series[0].points = [data.plots[0].series[0].points[1]];
  h.render(data);
  assert.equal(Number(h.container.querySelector("circle").getAttribute("cy")), 151, "the full retained domain remains authoritative");
  data.plots[0].domain.value_min = data.plots[0].domain.value_max = Number.MIN_VALUE;
  data.plots[0].series[0].points[0].value = Number.MIN_VALUE;
  h.render(data);
  assert.equal(h.container.querySelector("text.tick title").textContent, "5e-324", "a constant subnormal domain does not underflow during interpolation");
  assert.equal(Number(h.container.querySelector("circle").getAttribute("cy")), 151);
});

test("a formerly empty plot does not steal focus after the user moves to another control", t => {
  const h = chartHarness(t), data = typedChart();
  h.render(data);
  h.container.querySelector(".plot-scroll").focus();
  h.click('[data-chart-run="2"]');
  h.container.querySelector(".plot-scroll").focus();
  h.render({ ...data, plots: [{ ...data.plots[0], domain: null, series: data.plots[0].series.map(series => ({ ...series, points: [] })) }] });
  const outside = h.dom.window.document.createElement("button"); outside.textContent = "Other control";
  h.dom.window.document.body.append(outside); outside.focus();
  h.render(data);
  assert.equal(h.dom.window.document.activeElement, outside);
  assert.equal(h.container.querySelector('[data-chart-run="2"]').getAttribute("aria-pressed"), "false", "only stale focus is cleared; hidden runs are retained");
});

test("a refresh that clamps zoom to one coordinate does not draw hidden out-of-range values", t => {
  const h = chartHarness(t);
  h.render(typedChart({ runs: ["run-a"] })); h.click('[data-chart-zoom="in"]');
  const points = [
    { step: "100", value: 1, x_value: "100", timestamp: null },
    { step: "200", value: 0, x_value: "200", timestamp: null },
  ];
  h.render(typedChart({ runs: ["run-a"], domain: ["100", "200"], points }));
  assert.deepEqual(h.axis(), ["100", "100"]);
  assert.equal(h.container.querySelector("polyline").getAttribute("points"), "518,28", "the out-of-range step must not become a visible vertical segment");
  assert.equal(h.container.querySelectorAll('circle[display="none"]').length, 1);
  h.click('[data-chart-zoom="reset"]');
  assert.equal(h.container.querySelector("polyline").getAttribute("points"), "86,28 950,274");
});

test("controller uses typed point coordinates without DOM label parsing and defers refresh during drag", t => {
  const h = chartHarness(t);
  const first = MAX_STEP - 2n;
  const samples = [0n, 1n, 2n].map((offset, index) => ({ step: String(first + offset), value: index / 2, x_value: String(first + offset), timestamp: null }));
  const data = typedChart({ domain: [String(first), String(MAX_STEP)], points: samples });
  h.render(data);
  assert.deepEqual([...h.container.querySelectorAll('[data-chart-series="1"].point')].map(circle => Number(circle.getAttribute("cx"))), [86, 518, 950]);
  for (const circle of h.container.querySelectorAll("circle")) {
    const original = circle.getAttribute.bind(circle);
    circle.getAttribute = name => { if (name === "aria-label") throw new Error("Typed charts must not reparse accessibility labels"); return original(name); };
  }
  const svg = h.container.querySelector("svg");
  svg.getScreenCTM = () => ({ inverse: () => ({}) });
  svg.createSVGPoint = () => ({ x: 0, y: 0, matrixTransform() { return { x: this.x, y: this.y }; } });
  function pointer(name, x) {
    const event = new h.dom.window.Event(name, { bubbles: true });
    for (const [key, value] of Object.entries({ clientX: x, clientY: 151, pointerId: 1, button: 0, isPrimary: true })) Object.defineProperty(event, key, { value });
    act(() => h.container.querySelector("[data-chart-hit]").dispatchEvent(event));
  }
  pointer("pointerdown", 86);
  assert.equal(h.controller.isInteracting(), true);
  assert.equal(h.render({ ...data, plots: [] }), false);
  assert.equal(h.container.querySelector("svg"), svg);
  pointer("pointerup", 950);
  assert.equal(h.controller.isInteracting(), false);
  assert.equal(h.render(data), true);
  h.key("ArrowRight");
  assert.equal(h.container.querySelector('[data-chart-readout-run="1"]').dataset.chartStep, String(first));
});
