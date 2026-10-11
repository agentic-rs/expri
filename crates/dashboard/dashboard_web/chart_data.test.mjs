import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";

execFileSync("./node_modules/.bin/esbuild", [
  "chart_data.ts", "--bundle", "--format=esm", "--target=es2022", "--outfile=.test/chart_data.js",
], { cwd: new URL(".", import.meta.url), stdio: "pipe" });
const { parseChartData } = await import("./.test/chart_data.js");

const MAX_STEP = (1n << 64n) - 1n;
const MIN_COORDINATE = -(1n << 127n), MAX_COORDINATE = (1n << 127n) - 1n;
function point(step = "0", value = 1, timestamp = null, x_value = step) { return { step, value, timestamp, x_value }; }
function reduction(step = "0", value = 1, timestamp = null) { return { step, value, timestamp }; }
function series(run_index = 1, points = [point()]) {
  return {
    run_index,
    summary: { count: points.length, missing_timestamp_count: points.filter(point => point.timestamp === null).length, last: reduction(), min: reduction(), max: reduction() },
    points,
  };
}
function chart() {
  return {
    schema_version: 1, x_axis: "step", runs: ["run-example"],
    plots: [{ metric_name: "loss", domain: { x_min: "0", x_max: "0", value_min: 1, value_max: 1 }, series: [series()] }],
    warnings: [], warnings_truncated: false, parameter_rows: [], parameters_truncated: false,
  };
}
function rejected(change, expected) {
  const data = chart(); change(data);
  assert.throws(() => parseChartData(data), expected);
}

test("typed charts preserve complete reductions separately from exact sampled steps", () => {
  const data = chart(), plot = data.plots[0], metrics = plot.series[0];
  plot.domain = { x_min: String(MAX_STEP - 2n), x_max: String(MAX_STEP), value_min: -Number.MAX_VALUE, value_max: Number.MAX_VALUE };
  metrics.points = [point(String(MAX_STEP - 2n), -Number.MAX_VALUE), point(String(MAX_STEP - 1n), 0), point(String(MAX_STEP), Number.MAX_VALUE)];
  metrics.summary = {
    count: 100_000, missing_timestamp_count: 100_000,
    last: reduction(String(MAX_STEP), Number.MAX_VALUE), min: reduction("7", -Number.MAX_VALUE), max: reduction("8", Number.MAX_VALUE),
  };
  const result = parseChartData(data);
  assert.deepEqual(result, data);
  assert.equal(result.plots[0].series[0].points[1].step, String(MAX_STEP - 1n));
  assert.equal(result.plots[0].series[0].summary.count, 100_000);
  assert.notEqual(result, data);
  assert.notEqual(result.plots[0].series[0].points[0], metrics.points[0]);
  metrics.points[0].value = 5;
  assert.equal(result.plots[0].series[0].points[0].value, -Number.MAX_VALUE, "the parsed model owns its data");
});

test("time charts retain signed 128-bit coordinates and timestamp precision", () => {
  for (const x_axis of ["elapsed", "wall_clock"]) {
    const data = chart(), plot = data.plots[0], metrics = plot.series[0];
    data.x_axis = x_axis;
    plot.domain.x_min = String(MIN_COORDINATE);
    plot.domain.x_max = String(MAX_COORDINATE);
    metrics.points = [point("0", 1, "1969-12-31T23:59:59.999999999Z", String(MIN_COORDINATE)), point("1", 1, "2026-10-06T09:02:03.000000001+08:00", String(MAX_COORDINATE))];
    metrics.summary.count = 2;
    metrics.summary.missing_timestamp_count = 0;
    assert.deepEqual(parseChartData(data).plots[0].series[0].points, metrics.points);
  }
});

test("sample order, repeated steps, regressing clocks, zero values and negative zero survive parsing", () => {
  const data = chart(), plot = data.plots[0], metrics = plot.series[0];
  plot.domain = { x_min: "0", x_max: "10", value_min: 0, value_max: 1 };
  metrics.points = [point("10"), point("0", 0), point("10", -0), point("10", -0)];
  metrics.summary = { count: 4, missing_timestamp_count: 4, last: reduction("10", -0), min: reduction("0", 0), max: reduction("10", 1) };
  const result = parseChartData(data);
  assert.deepEqual(result.plots[0].series[0].points.map(point => point.step), ["10", "0", "10", "10"]);
  assert.equal(Object.is(result.plots[0].series[0].points[2].value, -0), true);
  assert.equal(Object.is(result.plots[0].series[0].summary.last.value, -0), true);
});

test("empty previews and timestamp-only omissions retain their complete summary", () => {
  const data = chart();
  data.plots = [];
  assert.deepEqual(parseChartData(data).plots, []);
  data.plots = [{ metric_name: "loss", domain: null, series: [{ run_index: 1, summary: { count: 0, missing_timestamp_count: 0, last: null, min: null, max: null }, points: [] }] }];
  assert.deepEqual(parseChartData(data), data);
  data.x_axis = "elapsed";
  data.plots[0].series[0] = series();
  data.plots[0].series[0].points = [];
  assert.equal(parseChartData(data).plots[0].series[0].summary.count, 1);
});

test("chart schema rejects malformed envelopes and fields without echoing server text", () => {
  for (const value of [null, false, "<script>private-data</script>", [], 1]) assert.throws(() => parseChartData(value), /^Error: Invalid chart data:/);
  for (const value of [undefined, 0, 2, "1"]) rejected(data => { data.schema_version = value; }, /schema_version must be 1/);
  rejected(data => { data.x_axis = "<script>private-data</script>"; }, error => {
    assert.match(error.message, /x_axis must be step, elapsed, or wall_clock/);
    assert.equal(error.message.includes("private-data"), false);
    assert.ok(error.message.length < 150);
    return true;
  });
  for (const key of ["runs", "plots", "warnings", "parameter_rows"]) rejected(data => { data[key] = {}; }, new RegExp(`${key} must be an array`));
  for (const key of ["warnings_truncated", "parameters_truncated"]) for (const value of [undefined, null, "false", 0]) rejected(data => { data[key] = value; }, new RegExp(`${key} must be a boolean`));
  rejected(data => { data.plots[0] = null; }, /plots\[0\] must be an object/);
  rejected(data => { data.plots[0].series[0].points[0] = []; }, /points\[0\] must be an object/);
});

test("run identities, metric names and series indices are bounded and unambiguous", () => {
  rejected(data => { data.runs = []; }, /runs must contain distinct runs/);
  rejected(data => { data.runs.push(data.runs[0]); }, /runs must contain distinct runs/);
  rejected(data => { data.runs = Array.from({ length: 9 }, (_, index) => `run-${index}`); }, /at most 8/);
  for (const value of ["", "  ", "name\n", "name\u0080", "🧪".repeat(65), "a".repeat(257), "name\ud800"]) {
    rejected(data => { data.runs[0] = value; }, /runs\[0\]/);
    rejected(data => { data.plots[0].metric_name = value; }, /metric_name/);
  }
  const data = chart(); data.runs[0] = "🧪".repeat(64); data.plots[0].metric_name = "🧪".repeat(64);
  assert.equal(parseChartData(data).runs[0], data.runs[0]);
  rejected(data => { data.plots.push(structuredClone(data.plots[0])); }, /metric_name must be distinct/);
  for (const value of [0, -1, 2, 1.5, "1", NaN]) rejected(data => { data.plots[0].series[0].run_index = value; }, /run_index/);
  rejected(data => { data.runs.push("other-run"); data.plots[0].series.push(structuredClone(data.plots[0].series[0])); }, /run_index must occur only once per plot/);
});

test("step and coordinate strings reject precision loss, overflow and noncanonical forms", () => {
  for (const value of ["-1", String(MAX_STEP + 1n), "01", "-0", "1.0", "1e3", " 1", "+1", "", 1, "1".repeat(10000)]) {
    rejected(data => { data.plots[0].series[0].points[0].step = value; }, /points\[0\].step/);
  }
  for (const value of [String(MIN_COORDINATE - 1n), String(MAX_COORDINATE + 1n), "01", "-0", "1.0", "1e3", " 1", "+1", 1]) {
    rejected(data => { data.x_axis = "elapsed"; data.plots[0].domain.x_min = value; }, /domain.x_min/);
  }
  rejected(data => { data.plots[0].series[0].points[0].x_value = "1"; data.plots[0].domain.x_max = "1"; }, /must match the point step/);
});

test("finite values, complete sample counts and reductions must be consistent", () => {
  for (const value of [NaN, Infinity, -Infinity, "1", null]) {
    rejected(data => { data.plots[0].series[0].points[0].value = value; }, /value must be a finite number/);
    rejected(data => { data.plots[0].series[0].summary.last.value = value; }, /last.value must be a finite number/);
    rejected(data => { data.plots[0].domain.value_min = value; }, /value_min must be a finite number/);
  }
  for (const value of [-1, 0.5, Number.MAX_SAFE_INTEGER + 1, NaN, "1"]) rejected(data => { data.plots[0].series[0].summary.count = value; }, /summary.count/);
  rejected(data => { data.plots[0].series[0].summary.missing_timestamp_count = 2; }, /missing_timestamp_count/);
  rejected(data => { data.plots[0].series[0].summary.last = null; }, /summary.last must be an object/);
  rejected(data => { data.plots[0].series[0].summary.count = 0; data.plots[0].series[0].summary.missing_timestamp_count = 0; }, /null reductions when count is zero/);
  rejected(data => { data.plots[0].series[0].summary.min.value = 2; }, /consistent minimum, maximum, and last/);
  rejected(data => { data.plots[0].series[0].points.push(point()); }, /must not exceed the complete sample count/);
  rejected(data => { data.plots[0].series[0].summary = { count: 0, missing_timestamp_count: 0, last: null, min: null, max: null }; }, /must not exceed the complete sample count/);
  rejected(data => { data.plots[0].domain.value_min = 0; data.plots[0].series[0].points[0].value = 0; }, /must lie within the complete summary bounds/);
});

test("plot domains contain every plotted point and timestamped axes reject missing timestamps", () => {
  rejected(data => { data.plots[0].domain = null; }, /must be present when points are plotted/);
  rejected(data => { data.plots[0].domain.x_min = "1"; }, /ordered minimum and maximum bounds/);
  rejected(data => { data.plots[0].domain.value_min = 2; }, /ordered minimum and maximum bounds/);
  rejected(data => { data.plots[0].series[0].points[0].x_value = "1"; }, /must lie within the plot domain/);
  rejected(data => { data.plots[0].series[0].points[0].value = 2; }, /must lie within the plot domain/);
  rejected(data => { data.x_axis = "wall_clock"; }, /timestamp must be present for a time axis/);
  for (const value of [undefined, "bad", "2026-02-30T00:00:00Z", "2026-01-01T24:00:00Z", "2026-01-01T00:61:00Z", "2026-01-01T00:00:00+24:00", "2026-01-01T00:00:00Z\n", "2026-01-01T00:00:00Z<script>", "a".repeat(129)]) {
    rejected(data => { data.plots[0].series[0].points[0].timestamp = value; }, /points\[0\].timestamp/);
  }
  for (const value of ["2024-02-29T00:00:00.123456789Z", "2016-12-31T23:59:60Z", "-0001-01-01T00:00:00+00:00", "+262000-01-01T00:00:00Z"]) {
    const data = chart(); data.plots[0].series[0].points[0].timestamp = value;
    assert.equal(parseChartData(data).plots[0].series[0].points[0].timestamp, value);
  }
});

test("six metrics, eight runs and the full 4800-point budget validate within fixed bounds", () => {
  const data = chart(); data.runs = Array.from({ length: 8 }, (_, index) => `run-${index}`);
  data.plots[0].domain.x_max = "599";
  data.plots[0].series = data.runs.map((_, index) => series(index + 1, Array.from({ length: 600 }, (_, step) => point(String(step)))));
  data.plots.push(...Array.from({ length: 5 }, (_, index) => ({ metric_name: `metric-${index}`, domain: null, series: [] })));
  assert.equal(parseChartData(data).plots[0].series.reduce((total, series) => total + series.points.length, 0), 4800);
  data.plots[1] = { metric_name: "one-too-many", domain: { x_min: "0", x_max: "0", value_min: 1, value_max: 1 }, series: [series()] };
  assert.throws(() => parseChartData(data), /at most 4800 sampled points in total/);
  rejected(data => { data.plots[0].series[0].points = Array.from({ length: 601 }, () => point()); }, /at most 600 entries/);
  rejected(data => { data.plots = Array.from({ length: 7 }, (_, index) => ({ metric_name: `metric-${index}`, domain: null, series: [] })); }, /at most 6 entries/);
});

test("warning and parameter previews enforce Unicode character budgets and positional run values", () => {
  const data = chart(); data.runs.push("second-run");
  data.warnings = Array.from({ length: 100 }, () => ({ run_index: 2, message: "🧪".repeat(512) }));
  data.parameter_rows = Array.from({ length: 24 }, (_, index) => ({ name: `setting-${index}`, values: ["🧪".repeat(1000), null], differs: true }));
  data.parameter_rows[0].name = "🧪".repeat(1000);
  data.warnings_truncated = true; data.parameters_truncated = true;
  assert.deepEqual(parseChartData(data), data);
  for (const value of [0, 2, -1, "1"]) rejected(data => { data.warnings = [{ run_index: value, message: "note" }]; }, /warnings\[0\].run_index/);
  rejected(data => { data.warnings = [{ run_index: 1, message: "🧪".repeat(513) }]; }, /at most 512 characters/);
  rejected(data => { data.warnings = Array.from({ length: 101 }, () => ({ run_index: 1, message: "note" })); }, /at most 100 entries/);
  rejected(data => { data.parameter_rows = Array.from({ length: 25 }, () => ({ name: "setting", values: ["1"], differs: false })); }, /at most 24 entries/);
  for (const values of [[], ["1", null]]) rejected(data => { data.parameter_rows = [{ name: "setting", values, differs: false }]; }, /parameter_rows\[0\].values/);
  for (const value of ["🧪".repeat(1001), 1, {}, undefined]) rejected(data => { data.parameter_rows = [{ name: "setting", values: [value], differs: false }]; }, /values\[0\]/);
  rejected(data => { data.parameter_rows = [{ name: "setting", values: ["1"], differs: "false" }]; }, /differs must be a boolean/);
});

test("payload text stays data, while unrecognized fields are excluded from the parsed model", () => {
  const data = chart(), markup = '<script src="https://example.invalid/script.js">text</script>';
  data.extra = { resource: markup };
  data.plots[0].metric_name = markup;
  data.plots[0].extra = markup;
  data.plots[0].series[0].points[0].extra = markup;
  data.warnings = [{ run_index: 1, message: markup, extra: markup }];
  data.parameter_rows = [{ name: markup, values: ["{\n  \"example\": true\n}"], differs: false, extra: markup }];
  const result = parseChartData(data);
  assert.equal(result.plots[0].metric_name, markup);
  assert.equal(result.warnings[0].message, markup);
  assert.equal(result.parameter_rows[0].name, markup);
  assert.equal("extra" in result, false);
  assert.equal("extra" in result.plots[0], false);
  assert.equal("extra" in result.plots[0].series[0].points[0], false);
  assert.equal("extra" in result.warnings[0], false);
  assert.equal("extra" in result.parameter_rows[0], false);
});
