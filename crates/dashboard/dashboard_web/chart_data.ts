export type ChartAxis = "step" | "elapsed" | "wall_clock";
export type SummaryPoint = { step: string; value: number; timestamp: string | null };
export type ChartDataPoint = SummaryPoint & { x_value: string };
export type ChartSummary = {
  count: number;
  missing_timestamp_count: number;
  last: SummaryPoint | null;
  min: SummaryPoint | null;
  max: SummaryPoint | null;
};
export type ChartSeries = { run_index: number; summary: ChartSummary; points: ChartDataPoint[] };
export type ChartDomain = { x_min: string; x_max: string; value_min: number; value_max: number };
export type ChartPlot = { metric_name: string; domain: ChartDomain | null; series: ChartSeries[] };
export type ChartWarning = { run_index: number; message: string };
export type ChartParameterRow = { name: string; values: (string | null)[]; differs: boolean };
export type ChartData = {
  schema_version: 1;
  x_axis: ChartAxis;
  runs: string[];
  plots: ChartPlot[];
  warnings: ChartWarning[];
  warnings_truncated: boolean;
  parameter_rows: ChartParameterRow[];
  parameters_truncated: boolean;
};

const MAX_STEP = (1n << 64n) - 1n;
const MIN_COORDINATE = -(1n << 127n), MAX_COORDINATE = (1n << 127n) - 1n;
const MAX_POINTS_PER_SERIES = 600, MAX_TOTAL_POINTS = 4800;
const MAX_RUNS = 8, MAX_PLOTS = 6, MAX_WARNINGS = 100, MAX_PARAMETER_ROWS = 24;
const encoder = new TextEncoder();

function invalid(path: string, requirement: string): never {
  // Field paths are constructed here, never from payload text. A server error
  // must not leak arbitrary values into the dashboard's error message.
  throw new Error(`Invalid chart data: ${path} ${requirement}.`);
}
function object(value: unknown, path: string): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) invalid(path, "must be an object");
  return value as Record<string, unknown>;
}
function array(value: unknown, path: string, maximum: number): unknown[] {
  if (!Array.isArray(value)) invalid(path, "must be an array");
  if (value.length > maximum) invalid(path, `must contain at most ${maximum} entries`);
  return value;
}
function text(value: unknown, path: string, maximum: number): string {
  if (typeof value !== "string" || value.length > maximum * 2) invalid(path, `must be text of at most ${maximum} characters`);
  let count = 0;
  for (const character of value) {
    const code = character.codePointAt(0)!;
    if (++count > maximum || (code >= 0xd800 && code <= 0xdfff)) invalid(path, `must be text of at most ${maximum} characters`);
  }
  return value;
}
function name(value: unknown, path: string): string {
  const result = text(value, path, 256);
  if (!result.trim() || /[\u0000-\u001f\u007f-\u009f]/.test(result) || encoder.encode(result).byteLength > 256) {
    invalid(path, "must be nonempty text without control characters, within 256 UTF-8 bytes");
  }
  return result;
}
function boolean(value: unknown, path: string): boolean {
  if (typeof value !== "boolean") invalid(path, "must be a boolean");
  return value;
}
function finite(value: unknown, path: string): number {
  if (typeof value !== "number" || !Number.isFinite(value)) invalid(path, "must be a finite number");
  return value;
}
function count(value: unknown, path: string, maximum = Number.MAX_SAFE_INTEGER): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0 || value > maximum) {
    invalid(path, `must be an integer from 0 to ${maximum}`);
  }
  return value;
}
function runIndex(value: unknown, path: string, run_count: number): number {
  const result = count(value, path, run_count);
  if (result === 0) invalid(path, `must identify a run from 1 to ${run_count}`);
  return result;
}
function integer(value: unknown, path: string, x_axis: ChartAxis): string {
  if (typeof value !== "string" || !/^-?(?:0|[1-9]\d{0,38})$/.test(value) || value === "-0") {
    invalid(path, "must be a canonical integer string");
  }
  const parsed = BigInt(value);
  if (x_axis === "step" ? parsed < 0n || parsed > MAX_STEP : parsed < MIN_COORDINATE || parsed > MAX_COORDINATE) {
    invalid(path, x_axis === "step" ? "must fit an unsigned 64-bit step" : "must fit a signed 128-bit coordinate");
  }
  return value;
}
function timestamp(value: unknown, path: string): string | null {
  if (value === null) return null;
  const result = text(value, path, 128);
  const match = /^([+-]?\d{4,6})-(\d{2})-(\d{2})[Tt ](\d{2}):(\d{2}):(\d{2})(?:\.\d+)?(?:[Zz]|([+-])(\d{2}):(\d{2}))$/.exec(result);
  if (!match) invalid(path, "must be an RFC3339 timestamp or null");
  const year = Number(match[1]), month = Number(match[2]), day = Number(match[3]);
  const days = [31, year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0) ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
  if (month < 1 || month > 12 || day < 1 || day > days[month - 1]! || Number(match[4]) > 23 || Number(match[5]) > 59 || Number(match[6]) > 60
    || (match[7] !== undefined && (Number(match[8]) > 23 || Number(match[9]) > 59))) {
    invalid(path, "must be an RFC3339 timestamp or null");
  }
  return result;
}
function summaryPoint(value: unknown, path: string): SummaryPoint {
  const data = object(value, path);
  return {
    step: integer(data.step, `${path}.step`, "step"),
    value: finite(data.value, `${path}.value`),
    timestamp: timestamp(data.timestamp, `${path}.timestamp`),
  };
}
function summary(value: unknown, path: string): ChartSummary {
  const data = object(value, path), point_count = count(data.count, `${path}.count`);
  const missing_timestamp_count = count(data.missing_timestamp_count, `${path}.missing_timestamp_count`, point_count);
  if (point_count === 0) {
    if (data.last !== null || data.min !== null || data.max !== null) invalid(path, "must have null reductions when count is zero");
    return { count: point_count, missing_timestamp_count, last: null, min: null, max: null };
  }
  const last = summaryPoint(data.last, `${path}.last`), min = summaryPoint(data.min, `${path}.min`), max = summaryPoint(data.max, `${path}.max`);
  if (min.value > max.value || last.value < min.value || last.value > max.value) invalid(path, "must have consistent minimum, maximum, and last values");
  return { count: point_count, missing_timestamp_count, last, min, max };
}
function domain(value: unknown, path: string, x_axis: ChartAxis): ChartDomain | null {
  if (value === null) return null;
  const data = object(value, path);
  const x_min = integer(data.x_min, `${path}.x_min`, x_axis), x_max = integer(data.x_max, `${path}.x_max`, x_axis);
  const value_min = finite(data.value_min, `${path}.value_min`), value_max = finite(data.value_max, `${path}.value_max`);
  if (BigInt(x_min) > BigInt(x_max) || value_min > value_max) invalid(path, "must have ordered minimum and maximum bounds");
  return { x_min, x_max, value_min, value_max };
}

/** Validate a bounded chart response once before passing it to the renderer. */
export function parseChartData(value: unknown): ChartData {
  const data = object(value, "response");
  if (data.schema_version !== 1) invalid("schema_version", "must be 1");
  const x_axis = data.x_axis;
  if (x_axis !== "step" && x_axis !== "elapsed" && x_axis !== "wall_clock") invalid("x_axis", "must be step, elapsed, or wall_clock");
  const runs = array(data.runs, "runs", MAX_RUNS).map((run, index) => name(run, `runs[${index}]`));
  if (!runs.length || new Set(runs).size !== runs.length) invalid("runs", "must contain distinct runs");
  let total_points = 0;
  const metric_names = new Set<string>();
  const plots = array(data.plots, "plots", MAX_PLOTS).map((value, plot_index): ChartPlot => {
    const path = `plots[${plot_index}]`, data = object(value, path);
    const metric_name = name(data.metric_name, `${path}.metric_name`);
    if (metric_names.has(metric_name)) invalid(`${path}.metric_name`, "must be distinct");
    metric_names.add(metric_name);
    const plot_domain = domain(data.domain, `${path}.domain`, x_axis);
    const x_min = plot_domain === null ? null : BigInt(plot_domain.x_min), x_max = plot_domain === null ? null : BigInt(plot_domain.x_max);
    const run_indices = new Set<number>();
    const series = array(data.series, `${path}.series`, runs.length).map((value, series_index): ChartSeries => {
      const series_path = `${path}.series[${series_index}]`, data = object(value, series_path);
      const run_index = runIndex(data.run_index, `${series_path}.run_index`, runs.length);
      if (run_indices.has(run_index)) invalid(`${series_path}.run_index`, "must occur only once per plot");
      run_indices.add(run_index);
      const full_summary = summary(data.summary, `${series_path}.summary`);
      const source_points = array(data.points, `${series_path}.points`, MAX_POINTS_PER_SERIES);
      if ((total_points += source_points.length) > MAX_TOTAL_POINTS) invalid("plots", `must contain at most ${MAX_TOTAL_POINTS} sampled points in total`);
      if (source_points.length > full_summary.count) invalid(`${series_path}.points`, "must not exceed the complete sample count");
      if (source_points.length && plot_domain === null) invalid(`${path}.domain`, "must be present when points are plotted");
      const points = source_points.map((value, point_index): ChartDataPoint => {
        const point_path = `${series_path}.points[${point_index}]`, data = object(value, point_path);
        const point = summaryPoint(data, point_path), x_value = integer(data.x_value, `${point_path}.x_value`, x_axis), x = BigInt(x_value);
        if (x < x_min! || x > x_max! || point.value < plot_domain!.value_min || point.value > plot_domain!.value_max) invalid(point_path, "must lie within the plot domain");
        if (x_axis === "step" && x_value !== point.step) invalid(`${point_path}.x_value`, "must match the point step");
        if (x_axis !== "step" && point.timestamp === null) invalid(`${point_path}.timestamp`, "must be present for a time axis");
        if (point.value < full_summary.min!.value || point.value > full_summary.max!.value) invalid(`${point_path}.value`, "must lie within the complete summary bounds");
        return { ...point, x_value };
      });
      return { run_index, summary: full_summary, points };
    });
    return { metric_name, domain: plot_domain, series };
  });
  const warnings = array(data.warnings, "warnings", MAX_WARNINGS).map((value, index): ChartWarning => {
    const path = `warnings[${index}]`, data = object(value, path);
    return { run_index: runIndex(data.run_index, `${path}.run_index`, runs.length), message: text(data.message, `${path}.message`, 512) };
  });
  const parameter_rows = array(data.parameter_rows, "parameter_rows", MAX_PARAMETER_ROWS).map((value, index): ChartParameterRow => {
    const path = `parameter_rows[${index}]`, data = object(value, path);
    const values = array(data.values, `${path}.values`, runs.length);
    if (values.length !== runs.length) invalid(`${path}.values`, "must have one value for every run");
    return {
      name: text(data.name, `${path}.name`, 1000),
      values: values.map((value, index) => value === null ? null : text(value, `${path}.values[${index}]`, 1000)),
      differs: boolean(data.differs, `${path}.differs`),
    };
  });
  return {
    schema_version: 1, x_axis, runs, plots, warnings, parameter_rows,
    warnings_truncated: boolean(data.warnings_truncated, "warnings_truncated"),
    parameters_truncated: boolean(data.parameters_truncated, "parameters_truncated"),
  };
}
