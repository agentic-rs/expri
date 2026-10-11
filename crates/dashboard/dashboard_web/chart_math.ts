import type { ChartAxis } from "./chart_data";
export type { ChartAxis } from "./chart_data";
export type ParsedChartPoint = { run_index: number; step: string; value: number; value_text: string };
export type ChartTimeZone = "local" | "utc";
export type ChartPoint = ParsedChartPoint & { x: number; y: number };
export type ChartRange = { x_axis: ChartAxis; start_x: string; end_x: string };
const MAX_STEP = 18446744073709551615n;
const MIN_COORDINATE = -(1n << 127n), MAX_COORDINATE = (1n << 127n) - 1n;
const NANOS_PER_SECOND = 1_000_000_000n;
const FRACTION_SCALE = 1_000_000_000_000n;
export const LEFT = 86, RIGHT = 950, TOP = 28, BOTTOM = 274;

export function parseChartPointLabel(label: string): ParsedChartPoint | null {
  const match = /^Run ([1-8]) · step (\d+) · value (-?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?)(?: · timestamp [^·\r\n]{1,128})?(?: · elapsed [^·\r\n]{1,80})?$/.exec(label);
  if (!match) return null;
  const step = match[2], value_text = match[3];
  if (step === undefined || value_text === undefined || step.length > 20 || BigInt(step) > MAX_STEP) return null;
  const value = Number(value_text);
  return Number.isFinite(value) ? { run_index: Number(match[1]), step, value, value_text } : null;
}

function coordinate(value: string, x_axis: ChartAxis): bigint | null {
  if (!/^-?(?:0|[1-9]\d{0,38})$/.test(value) || value === "-0") return null;
  const parsed = BigInt(value);
  return x_axis === "step" ? parsed >= 0n && parsed <= MAX_STEP ? parsed : null : parsed >= MIN_COORDINATE && parsed <= MAX_COORDINATE ? parsed : null;
}
export function parseChartRange(x_axis: string | null, start_x: string | null, end_x: string | null): ChartRange | null {
  if (!["step", "elapsed", "wall_clock"].includes(x_axis ?? "") || start_x === null || end_x === null) return null;
  const axis = x_axis as ChartAxis, start = coordinate(start_x, axis), end = coordinate(end_x, axis);
  return start !== null && end !== null && start <= end ? { x_axis: axis, start_x, end_x } : null;
}
export function coordinatesInRange(range: ChartRange): [bigint, bigint] {
  if (!parseChartRange(range.x_axis, range.start_x, range.end_x)) throw new Error("Invalid chart axis range");
  const start = BigInt(range.start_x), end = BigInt(range.end_x);
  return [start, end];
}
function fractionInteger(fraction: number): bigint {
  return BigInt(Math.round(Math.min(1, Math.max(0, Number.isFinite(fraction) ? fraction : 0)) * Number(FRACTION_SCALE)));
}
function xAt(range: ChartRange, fraction: number): bigint {
  const [start, end] = coordinatesInRange(range);
  return start + (end - start) * fractionInteger(fraction) / FRACTION_SCALE;
}
export function chartXFraction(x_value: string, range: ChartRange): number {
  const [start, end] = coordinatesInRange(range), value = coordinate(x_value, range.x_axis);
  if (value === null) throw new Error("Invalid chart axis coordinate");
  if (start === end) return .5;
  return Number((value - start) * FRACTION_SCALE / (end - start)) / Number(FRACTION_SCALE);
}
export function dragChartRange(range: ChartRange, start_fraction: number, end_fraction: number): ChartRange {
  const first = xAt(range, Math.min(start_fraction, end_fraction));
  const last = xAt(range, Math.max(start_fraction, end_fraction));
  return first < last ? { x_axis: range.x_axis, start_x: String(first), end_x: String(last) } : range;
}
export function zoomChartRange(range: ChartRange, full_range: ChartRange, factor: number, center_fraction = .5): ChartRange {
  if (range.x_axis !== full_range.x_axis) throw new Error("Cannot zoom different chart axes");
  const [full_start, full_end] = coordinatesInRange(full_range), [start, end] = coordinatesInRange(range);
  const full_span = full_end - full_start;
  if (full_span === 0n || !Number.isFinite(factor) || factor <= 0) return range;
  const ratio = BigInt(Math.round(Math.min(1_000_000, factor) * 1_000_000));
  let span = (end - start) * ratio / 1_000_000n;
  if (span < 1n) span = 1n;
  if (span > full_span) span = full_span;
  const anchor = xAt(range, center_fraction);
  let next_start = anchor - span * fractionInteger(center_fraction) / FRACTION_SCALE;
  if (next_start < full_start) next_start = full_start;
  if (next_start + span > full_end) next_start = full_end - span;
  return { x_axis: range.x_axis, start_x: String(next_start), end_x: String(next_start + span) };
}
export function restoreChartRange(range: ChartRange | null, full_range: ChartRange): ChartRange {
  const [full_start, full_end] = coordinatesInRange(full_range);
  if (range === null || range.x_axis !== full_range.x_axis) return full_range;
  const [start, end] = coordinatesInRange(range);
  const clamp = (value: bigint) => value < full_start ? full_start : value > full_end ? full_end : value;
  return { x_axis: range.x_axis, start_x: String(clamp(start)), end_x: String(clamp(end)) };
}
type DateParts = { date_text: string; time_text: string; offset_text: string };
export function timeZoneLabel(time_zone: ChartTimeZone): string {
  return time_zone === "utc" ? "UTC" : new Intl.DateTimeFormat().resolvedOptions().timeZone;
}
export function dateParts(value: bigint, time_zone: ChartTimeZone): DateParts | null {
  // Keep fractions in BigInt: Date only receives whole seconds, including before
  // the epoch. No nanoseconds pass through Number or millisecond rounding.
  let seconds = value / NANOS_PER_SECOND, remainder = value % NANOS_PER_SECOND;
  if (remainder < 0n) { seconds -= 1n; remainder += NANOS_PER_SECOND; }
  const date = new Date(Number(seconds) * 1000);
  if (!Number.isFinite(date.getTime())) return null;
  const fraction = remainder === 0n ? "" : `.${String(remainder).padStart(9, "0").replace(/0+$/, "")}`;
  if (time_zone === "utc") {
    const [date_text, time_text] = date.toISOString().replace(/\.\d{3}Z$/, "").split("T");
    return { date_text: date_text!, time_text: `${time_text}${fraction}`, offset_text: "UTC" };
  }
  const year = date.getFullYear(), pad = (value: number) => String(value).padStart(2, "0");
  const year_text = year >= 0 && year <= 9999 ? String(year).padStart(4, "0") : `${year < 0 ? "-" : "+"}${String(Math.abs(year)).padStart(6, "0")}`;
  // Derive the offset at this instant, not today's offset. This also keeps
  // historical offsets with seconds and avoids Date.UTC's special years 0–99.
  const local_as_utc = new Date(0);
  local_as_utc.setUTCFullYear(year, date.getMonth(), date.getDate());
  local_as_utc.setUTCHours(date.getHours(), date.getMinutes(), date.getSeconds(), 0);
  const offset = Number.isFinite(local_as_utc.getTime()) ? (local_as_utc.getTime() - date.getTime()) / 1000 : -date.getTimezoneOffset() * 60;
  const absolute = Math.abs(offset);
  const offset_text = offset === 0 ? "UTC" : `UTC${offset < 0 ? "−" : "+"}${pad(Math.floor(absolute / 3600))}:${pad(Math.floor(absolute / 60) % 60)}${absolute % 60 === 0 ? "" : `:${pad(absolute % 60)}`}`;
  return {
    date_text: `${year_text}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`,
    time_text: `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}${fraction}`, offset_text,
  };
}
export function formatChartX(x_value: string, x_axis: ChartAxis, time_zone: ChartTimeZone = "utc"): string {
  const value = coordinate(x_value, x_axis);
  if (value === null) throw new Error("Invalid chart axis coordinate");
  if (x_axis === "step") return x_value;
  if (x_axis === "elapsed") {
    const negative = value < 0n, absolute = negative ? -value : value;
    const sign = negative ? "−" : "";
    if (absolute !== 0n && absolute < NANOS_PER_SECOND) {
      const [divisor, digits, unit] = absolute < 1000n ? [1n, 0, "ns"] as const : absolute < 1_000_000n ? [1000n, 3, "µs"] as const : [1_000_000n, 6, "ms"] as const;
      const remainder = absolute % divisor;
      const fraction = remainder === 0n ? "" : `.${String(remainder).padStart(digits, "0").replace(/0+$/, "")}`;
      return `${sign}${absolute / divisor}${fraction} ${unit}`;
    }
    const seconds = absolute / NANOS_PER_SECOND, remainder = absolute % NANOS_PER_SECOND;
    const fraction = remainder === 0n ? "" : `.${String(remainder).padStart(9, "0").replace(/0+$/, "")}`;
    if (seconds >= 3600n) return `${sign}${seconds / 3600n}:${String(seconds / 60n % 60n).padStart(2, "0")}:${String(seconds % 60n).padStart(2, "0")}${fraction} h`;
    if (seconds >= 60n) return `${sign}${seconds / 60n}:${String(seconds % 60n).padStart(2, "0")}${fraction} min`;
    return `${sign}${seconds}${fraction} s`;
  }
  const parts = dateParts(value, time_zone);
  if (!parts) return `${x_value} ns since Unix epoch`;
  const zone = timeZoneLabel(time_zone);
  return `${parts.date_text} ${parts.time_text} ${parts.offset_text}${time_zone === "local" && zone !== "UTC" ? ` (${zone})` : ""}`;
}
export function formatChartTick(x_value: string, range: ChartRange, time_zone: ChartTimeZone = "utc"): string {
  const label = formatChartX(x_value, range.x_axis, time_zone);
  if (range.x_axis !== "wall_clock") return label;
  const [start, end] = coordinatesInRange(range);
  const parts = dateParts(BigInt(x_value), time_zone), first = dateParts(start, time_zone), last = dateParts(end, time_zone);
  if (!parts || !first || !last) return label;
  // Offset changes can repeat a local hour. Distinguish those ticks without
  // cluttering ordinary ranges that remain in one offset.
  const offset = first.offset_text !== last.offset_text ? ` ${parts.offset_text}` : "";
  if (first.date_text === last.date_text) return `${parts.time_text}${offset}`;
  if (end - start >= 365n * 86400n * NANOS_PER_SECOND) return parts.date_text;
  const time = end - start < 120n * NANOS_PER_SECOND ? parts.time_text : parts.time_text.slice(0, 5);
  return `${parts.date_text.slice(-5)} ${time}${offset}`;
}
export function nearestChartPoint<T extends ChartPoint>(points: readonly T[], x: number, y?: number, preferred?: T): T | null {
  if (preferred && points.includes(preferred) && preferred.x === x && (y === undefined || preferred.y === y)) return preferred;
  let nearest: T | null = null, distance = Infinity, tie_distance = Infinity;
  for (const point of points) {
    const candidate = Math.abs(point.x - x), tie = y === undefined ? 0 : Math.abs(point.y - y);
    if (candidate < distance - 1e-7 || (Math.abs(candidate - distance) < 1e-7 && tie < tie_distance)) {
      nearest = point; distance = candidate; tie_distance = tie;
    }
  }
  return nearest;
}


export function chartValueText(value: number): string {
  return Object.is(value, -0) ? "-0" : String(value);
}
// Normalize before subtracting, so opposite finite f64 extrema cannot overflow.
export function chartYFraction(value: number, value_min: number, value_max: number): number {
  if (value_min === value_max) return .5;
  const scale = Math.max(Math.abs(value_min), Math.abs(value_max));
  return ((value / scale) - (value_min / scale)) / ((value_max / scale) - (value_min / scale));
}

export function chartValueAtFraction(value_min: number, value_max: number, fraction: number): number {
  if (value_min === value_max) return value_min;
  const scale = Math.max(Math.abs(value_min), Math.abs(value_max));
  return ((value_min / scale) * (1 - fraction) + (value_max / scale) * fraction) * scale;
}
