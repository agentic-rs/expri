export type ParsedChartPoint = { run_index: number; step: string; value: number; value_text: string };
export type ChartAxis = "step" | "elapsed" | "wall_clock";
export type ChartTimeZone = "local" | "utc";
export type ChartPoint = ParsedChartPoint & { x: number; y: number };
export type ChartRange = { x_axis: ChartAxis; start_x: string; end_x: string };
type PlotPoint = ChartPoint & { x_value: string; timestamp: string | null; circle: SVGCircleElement };
type Series = { run_index: number; run_id: string; points: PlotPoint[]; polyline: SVGPolylineElement | null; button: HTMLButtonElement; visible: boolean };
type KeyboardAnchor = { run_id: string; step: string; x_value: string; value_text: string; occurrence: number };
type PlotState = {
  metric_name: string;
  x_axis: ChartAxis;
  range: ChartRange | null;
  hidden_run_ids: string[];
  keyboard_anchor: KeyboardAnchor | null;
  focus: string | null;
  scroll_left: number;
  scroll_top: number;
};
type PlotController = { dispose(): void; isInteracting(): boolean; capture(): PlotState; restore(state: PlotState): void; setTimeZone(time_zone: ChartTimeZone): void };
export type ChartPreviewStatus = "ready" | "loading" | "invalid";
export type ChartController = { dispose(): void; isInteracting(): boolean; previewStatus(): ChartPreviewStatus; replacePreview(html: string): boolean; setTimeZone(time_zone: ChartTimeZone): void };
const MAX_STEP = 18446744073709551615n;
const MIN_COORDINATE = -(1n << 127n), MAX_COORDINATE = (1n << 127n) - 1n;
const NANOS_PER_SECOND = 1_000_000_000n;
const FRACTION_SCALE = 1_000_000_000_000n;
const SVG_NS = "http://www.w3.org/2000/svg";
const LEFT = 86, RIGHT = 950, TOP = 28, BOTTOM = 274;
let chart_sequence = 0;

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
function coordinatesInRange(range: ChartRange): [bigint, bigint] {
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
function timeZoneLabel(time_zone: ChartTimeZone): string {
  return time_zone === "utc" ? "UTC" : new Intl.DateTimeFormat().resolvedOptions().timeZone;
}
function dateParts(value: bigint, time_zone: ChartTimeZone): DateParts | null {
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

function html<K extends keyof HTMLElementTagNameMap>(document: Document, tag: K, text = ""): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag); node.textContent = text; return node;
}
function svgNode<K extends keyof SVGElementTagNameMap>(document: Document, tag: K, attrs: Record<string, string>): SVGElementTagNameMap[K] {
  const node = document.createElementNS(SVG_NS, tag);
  for (const [name, value] of Object.entries(attrs)) node.setAttribute(name, value);
  return node;
}

function plotRange(svg: SVGSVGElement): ChartRange | null {
  if (["data-x-axis", "data-x-min", "data-x-max"].some(name => svg.hasAttribute(name))) {
    return parseChartRange(svg.getAttribute("data-x-axis"), svg.getAttribute("data-x-min"), svg.getAttribute("data-x-max"));
  }
  const tick_steps = [...svg.querySelectorAll<SVGTextElement>("text.tick")]
    .filter(node => Number(node.getAttribute("y")) >= BOTTOM + 12)
    .map(node => node.textContent?.trim() ?? "").filter(value => /^\d{1,20}$/.test(value));
  if (!tick_steps.length) return null;
  const values = tick_steps.map(value => BigInt(value)).sort((first, second) => first < second ? -1 : first > second ? 1 : 0);
  const start = values[0], end = values[values.length - 1];
  if (start === undefined || end === undefined || end > MAX_STEP) return null;
  return { x_axis: "step", start_x: String(start), end_x: String(end) };
}

function pointCoordinate(circle: Element, parsed: ParsedChartPoint, range: ChartRange, explicit: boolean): string | null {
  const value = explicit ? circle.getAttribute("data-x-value") : parsed.step;
  const x_value = value === null ? null : coordinate(value, range.x_axis);
  if (x_value === null || x_value < BigInt(range.start_x) || x_value > BigInt(range.end_x)) return null;
  if (range.x_axis === "step" && x_value !== BigInt(parsed.step)) return null;
  return String(x_value);
}
function pointTimestamp(circle: Element): string | null {
  const timestamp = circle.getAttribute("data-timestamp");
  return timestamp !== null && timestamp.length <= 128 && /^[+-]?\d{4,6}-\d{2}-\d{2}[Tt ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:[Zz]|[+-]\d{2}:\d{2})$/.test(timestamp) ? timestamp : null;
}

const PREVIEW_HTML_TAGS = new Set("html head body meta title style main p section h2 div ul li span code pre details summary table thead tbody tr th td".split(" "));
const PREVIEW_SVG_TAGS = new Set("svg title desc line text polyline circle".split(" "));
const PREVIEW_ATTRIBUTES = new Set("lang charset name content class id role aria-label aria-labelledby tabindex viewBox x y x1 x2 y1 y2 cx cy r fill stroke stroke-width stroke-dasharray points text-anchor transform style scope data-x-axis data-x-min data-x-max data-x-value data-timestamp".split(" "));

function isDashboardPreview(document: Document): boolean {
  if (document.title !== "Run comparison · expri" || !document.querySelector("body > main > .chart-summary")) return false;
  if (document.querySelectorAll("circle.point").length > 4800 || document.querySelectorAll(".plot-scroll > svg").length > 6) return false;
  for (const svg of document.querySelectorAll<SVGSVGElement>(".plot-scroll > svg")) {
    const section = svg.closest("section.card"), metric = section?.querySelector("h2")?.textContent;
    const run_ids = [...(section?.querySelectorAll("ul.legend li code") ?? [])].map(node => node.textContent ?? "");
    const circles = svg.querySelectorAll("circle.point");
    const range = plotRange(svg), explicit = svg.hasAttribute("data-x-axis");
    if (!metric || !run_ids.length || run_ids.length > 8 || new Set(run_ids).size !== run_ids.length || run_ids.some(id => !id) || !range || !circles.length) return false;
    for (const circle of circles) {
      const point = parseChartPointLabel(circle.getAttribute("aria-label") ?? "");
      if (!point || point.run_index > run_ids.length || pointCoordinate(circle, point, range, explicit) === null) return false;
      const timestamp = pointTimestamp(circle);
      if ((range.x_axis !== "step" || circle.hasAttribute("data-timestamp")) && timestamp === null) return false;
      for (const attribute of ["cx", "cy"]) {
        const value = circle.getAttribute(attribute);
        if (value === null || !Number.isFinite(Number(value))) return false;
      }
    }
  }
  return true;
}

function parseDashboardPreview(markup: string): Document | null {
  // Refuse resource-bearing tags before parsing: even an inert DOMParser document
  // can request images and frames. The generated preview contains neither.
  if (markup.length > 2 * 1024 * 1024 || new TextEncoder().encode(markup).byteLength > 2 * 1024 * 1024) return null;
  for (const match of markup.matchAll(/<\s*([a-z][a-z0-9:-]*)\b/gi)) {
    const tag = match[1]?.toLowerCase();
    if (!tag || (!PREVIEW_HTML_TAGS.has(tag) && !PREVIEW_SVG_TAGS.has(tag))) return null;
  }
  const document = new DOMParser().parseFromString(markup, "text/html");
  for (const node of document.querySelectorAll("*")) {
    const allowed = node.namespaceURI === SVG_NS ? PREVIEW_SVG_TAGS : PREVIEW_HTML_TAGS;
    if (![SVG_NS, "http://www.w3.org/1999/xhtml"].includes(node.namespaceURI ?? "") || !allowed.has(node.localName)) return null;
    for (const attribute of node.attributes) if (!PREVIEW_ATTRIBUTES.has(attribute.name)) return null;
    if (node.localName === "meta" && !(node.hasAttribute("charset") || node.getAttribute("name") === "viewport")) return null;
    const style = node.localName === "style" ? node.textContent ?? "" : node.getAttribute("style") ?? "";
    if (/(?:url\s*\(|@import|expression\s*\()/i.test(style)) return null;
  }
  return isDashboardPreview(document) ? document : null;
}

export function createChartController(frame: HTMLIFrameElement): ChartController {
  let plots: PlotController[] = [], current_document: Document | null = null, disposed = false;
  let time_zone: ChartTimeZone = "local";
  let completed_source: string | null = null;
  const hidden_runs = new Map<string, string[]>();
  const clear = (forget_visibility = false) => {
    for (const plot of plots) plot.dispose(); plots = []; current_document = null;
    if (forget_visibility) hidden_runs.clear();
  };
  function currentSource(): URL | null {
    if (!frame.src || !frame.ownerDocument?.baseURI) return null;
    const source = new URL(frame.src, frame.ownerDocument.baseURI), owner = new URL(frame.ownerDocument.baseURI);
    return source.origin === owner.origin ? source : null;
  }
  function loadedDocument(): Document | null {
    try {
      const document = frame.contentDocument;
      const source = currentSource();
      if (!document?.querySelectorAll || !source || document.readyState !== "complete") return null;
      return document.URL === source.href && isDashboardPreview(document) ? document : null;
    } catch { return null; }
  }
  function enhanceDocument(document: Document): void {
    current_document = document;
    for (const svg of document.querySelectorAll<SVGSVGElement>(".plot-scroll > svg")) {
      const plot = enhancePlot(document, svg, time_zone);
      if (plot) plots.push(plot);
    }
  }
  const enhance = (completed = false) => {
    clear(true);
    if (disposed) return;
    try {
      const source = currentSource(), child = frame.contentDocument;
      if (source && ((completed && !child) || (child?.readyState === "complete" && (child.URL === source.href || child.URL.startsWith("about:neterror"))))) completed_source = source.href;
      const document = loadedDocument();
      if (document) enhanceDocument(document);
    } catch { /* A navigation outside the chart origin leaves its content untouched. */ }
  };
  const loaded = () => enhance(true);
  frame.addEventListener("load", loaded);
  const observer = typeof MutationObserver === "undefined" ? null : new MutationObserver(() => { completed_source = null; clear(true); });
  observer?.observe(frame, { attributes: true, attributeFilter: ["src"] });
  enhance();
  return {
    setTimeZone(next) {
      if (disposed || time_zone === next) return;
      time_zone = next;
      for (const plot of plots) plot.setTimeZone(next);
    },
    dispose() { disposed = true; frame.removeEventListener("load", loaded); observer?.disconnect(); clear(true); },
    isInteracting() { return plots.some(plot => plot.isInteracting()); },
    previewStatus() {
      if (disposed) return "loading";
      try {
        const source = currentSource(); if (!source) return "loading";
        if (current_document && loadedDocument() === current_document) return "ready";
        return completed_source === source.href ? "invalid" : "loading";
      } catch { return "loading"; }
    },
    replacePreview(markup) {
      if (disposed || plots.some(plot => plot.isInteracting())) return false;
      const document = loadedDocument();
      if (!document || document !== current_document) return false;
      let next: Document | null;
      try { next = parseDashboardPreview(markup); } catch { return false; }
      if (!next) return false;
      const frame_focused = frame.ownerDocument.activeElement === frame;
      const states = new Map(plots.map(plot => {
        const state = plot.capture();
        if (!frame_focused) state.focus = null;
        return [state.metric_name, state];
      }));
      const metric_names = new Set([...next.querySelectorAll("body > main > section.card > h2")].slice(0, 6).map(node => node.textContent ?? ""));
      for (const name of hidden_runs.keys()) if (!metric_names.has(name)) hidden_runs.delete(name);
      for (const [name, state] of states) if (metric_names.has(name)) hidden_runs.set(name, [...state.hidden_run_ids]);
      const window = document.defaultView, parent = frame.ownerDocument.defaultView;
      const scroll = { x: window?.scrollX ?? 0, y: window?.scrollY ?? 0, parent_x: parent?.scrollX ?? 0, parent_y: parent?.scrollY ?? 0 };
      const previous_details = document.querySelector<HTMLDetailsElement>("details.parameter-comparison");
      const details_open = previous_details?.open ?? false, details_focused = frame_focused && document.activeElement === previous_details?.querySelector("summary");
      const parameter_scroll = previous_details?.querySelector<HTMLElement>(".table-scroll")?.scrollLeft ?? 0;
      const head = [...next.head.childNodes].map(node => document.importNode(node, true));
      const body = [...next.body.childNodes].map(node => document.importNode(node, true));
      clear();
      // Keep the existing document, HTTP CSP and iframe sandbox. New server samples
      // form one coherent bounded snapshot; sampled points are never appended.
      document.head.replaceChildren(...head); document.body.replaceChildren(...body);
      enhanceDocument(document);
      for (const plot of plots) {
        const fresh = plot.capture(), state = states.get(fresh.metric_name);
        if (state) plot.restore(state);
        else {
          // A legacy time view can have no plot. Retain its legend choices, but
          // returning plots start with the current axis's full range and focus.
          const hidden_run_ids = hidden_runs.get(fresh.metric_name);
          if (hidden_run_ids) plot.restore({ ...fresh, hidden_run_ids });
        }
      }
      const details = document.querySelector<HTMLDetailsElement>("details.parameter-comparison");
      if (details) {
        details.open = details_open;
        if (details_focused) details.querySelector("summary")?.focus({ preventScroll: true });
        const table = details.querySelector<HTMLElement>(".table-scroll"); if (table) table.scrollLeft = parameter_scroll;
      }
      window?.scrollTo(scroll.x, scroll.y); parent?.scrollTo(scroll.parent_x, scroll.parent_y);
      return true;
    },
  };
}

export function attachChartInteractions(frame: HTMLIFrameElement): () => void {
  const controller = createChartController(frame);
  return () => controller.dispose();
}

function enhancePlot(document: Document, svg: SVGSVGElement, time_zone: ChartTimeZone): PlotController | null {
  const section = svg.closest<HTMLElement>("section.card");
  const region = svg.parentElement;
  const legend = section?.querySelector<HTMLUListElement>("ul.legend");
  if (!section || !region || !legend || section.hasAttribute("data-interactive-chart")) return null;
  const x_ticks = [...svg.querySelectorAll<SVGTextElement>("text.tick")].filter(node => Number(node.getAttribute("y")) >= BOTTOM + 12);
  const parsed_range = plotRange(svg);
  if (!parsed_range) return null;
  const full_range: ChartRange = parsed_range;
  let range = full_range, follow_full = true, keyboard_index = -1, keyboard_anchor: PlotPoint | null = null;
  let drag_start: { x: number; screen_x: number; pointer_id: number } | null = null;
  let inspection: { x: number; y: number | undefined; anchor: PlotPoint | undefined } | null = null;
  const listeners: (() => void)[] = [];
  const original_nodes = [...svg.childNodes];
  const original_attributes = new Map<Element, Record<string, string | null>>();
  const original_description = region.getAttribute("aria-describedby");
  const axis_label = [...svg.querySelectorAll<SVGTextElement>("text.axis-label")].find(node => Number(node.getAttribute("y")) === BOTTOM + 52);
  const original_axis_label = axis_label?.textContent ?? "";
  const context = full_range.x_axis === "wall_clock" && region.nextElementSibling?.matches("p.muted") ? region.nextElementSibling : null;
  const original_context = context?.textContent ?? "";
  function remember(node: Element, names: string[]): void {
    original_attributes.set(node, Object.fromEntries(names.map(name => [name, node.getAttribute(name)])));
  }
  const original_points = new Map<SVGCircleElement, { cx: string; r: string }>();
  const original_curves = new Map<SVGPolylineElement, string>();
  const original_legends = new Map<HTMLLIElement, Node[]>();
  const metric_name = section.querySelector("h2")?.textContent ?? "metric";
  const series = new Map<number, Series>();
  const legend_items = [...legend.querySelectorAll<HTMLLIElement>("li")];
  for (let index = 0; index < legend_items.length; index++) {
    const item = legend_items[index]; if (!item) continue;
    const run_id = item.querySelector("code")?.textContent ?? `Run ${index + 1}`;
    const button = html(document, "button"); button.type = "button"; button.className = "chart-run-toggle";
    button.setAttribute("data-chart-run", String(index + 1)); button.setAttribute("aria-pressed", "true");
    button.setAttribute("aria-label", `Toggle ${run_id} in ${metric_name}`);
    original_legends.set(item, [...item.childNodes]); button.append(...item.childNodes); item.append(button);
    series.set(index + 1, { run_index: index + 1, run_id, points: [], polyline: null, button, visible: true });
  }
  let preceding_curve: SVGPolylineElement | null = null;
  let invalid_points = false;
  for (const node of svg.querySelectorAll<SVGPolylineElement | SVGCircleElement>("polyline, circle.point")) {
    if (node.tagName.toLowerCase() === "polyline") {
      preceding_curve = node as SVGPolylineElement;
      remember(preceding_curve, ["points", "clip-path", "display", "data-chart-series"]);
      original_curves.set(preceding_curve, preceding_curve.getAttribute("points") ?? "");
      continue;
    }
    const circle = node as SVGCircleElement;
    const parsed = parseChartPointLabel(circle.getAttribute("aria-label") ?? circle.querySelector("title")?.textContent ?? "");
    const item = parsed && series.get(parsed.run_index);
    const x_value = parsed && pointCoordinate(circle, parsed, full_range, svg.hasAttribute("data-x-axis"));
    const timestamp = pointTimestamp(circle);
    const x = Number(circle.getAttribute("cx")), y = Number(circle.getAttribute("cy"));
    if (!parsed || !item || x_value === null || (full_range.x_axis !== "step" && timestamp === null) || circle.getAttribute("cx") === null || circle.getAttribute("cy") === null || !Number.isFinite(x) || !Number.isFinite(y)) { invalid_points = true; continue; }
    item.polyline = preceding_curve;
    item.points.push({ ...parsed, x_value, timestamp, x, y, circle });
    original_points.set(circle, { cx: circle.getAttribute("cx") ?? "", r: circle.getAttribute("r") ?? "2" });
    remember(circle, ["cx", "r", "clip-path", "display", "data-chart-series"]);
    circle.setAttribute("data-chart-series", String(parsed.run_index));
    if (preceding_curve) preceding_curve.setAttribute("data-chart-series", String(parsed.run_index));
  }
  if (invalid_points || ![...series.values()].some(item => item.points.length)) {
    for (const [item, children] of original_legends) item.replaceChildren(...children);
    for (const [node, attributes] of original_attributes) {
      for (const [name, value] of Object.entries(attributes)) value === null ? node.removeAttribute(name) : node.setAttribute(name, value);
    }
    return null;
  }
  section.setAttribute("data-interactive-chart", "true");
  const id = `chart-explorer-${++chart_sequence}`;
  const styles = html(document, "style");
  styles.textContent = `[data-interactive-chart] .chart-explorer-controls{display:flex;align-items:center;gap:7px;flex-wrap:wrap;margin:8px 0;font-size:12px}[data-interactive-chart] .chart-explorer-controls button{font:inherit;border:1px solid var(--line);border-radius:5px;background:var(--paper);color:var(--ink);padding:6px 9px;cursor:pointer}[data-interactive-chart] button:disabled{opacity:.5;cursor:default}[data-interactive-chart] .chart-range{overflow-wrap:anywhere;font:11px ui-monospace,SFMono-Regular,Consolas,monospace}[data-interactive-chart] .chart-explorer-hint{font-size:11px;color:var(--muted);margin:6px 0}[data-interactive-chart] .chart-run-toggle{display:flex;align-items:center;gap:5px;flex-wrap:wrap;max-width:100%;font:inherit;border:0;border-radius:4px;background:transparent;color:var(--ink);padding:4px;cursor:pointer;text-align:left}[data-interactive-chart] .chart-run-toggle[aria-pressed=false]{opacity:.45;text-decoration:line-through}[data-interactive-chart] .chart-readout{font-size:12px;border-top:1px solid var(--line);margin:10px 0 0;padding-top:8px;overflow-wrap:anywhere}[data-interactive-chart] .chart-readout-row{display:flex;flex-wrap:wrap;gap:4px 12px;margin-top:4px}[data-interactive-chart] .chart-readout-row code{font-size:11px}[data-interactive-chart] .chart-readout-label{font-size:11px;color:var(--muted)}[data-interactive-chart] rect[data-chart-hit]{touch-action:pan-y;cursor:crosshair}[data-interactive-chart] :focus-visible{outline:2px solid #4056b4;outline-offset:2px}`;
  const controls = html(document, "div"); controls.className = "chart-explorer-controls";
  const zoom_in = html(document, "button", "+ Zoom in"), zoom_out = html(document, "button", "− Zoom out"), reset = html(document, "button", "Reset zoom");
  for (const [button, action] of [[zoom_in, "in"], [zoom_out, "out"], [reset, "reset"]] as const) {
    button.type = "button"; button.setAttribute("data-chart-zoom", action); controls.append(button);
  }
  const range_label = html(document, "span"); range_label.className = "chart-range"; range_label.setAttribute("data-chart-range", ""); controls.append(range_label);
  const hint = html(document, "p", "Drag across the plot to zoom. Focus the plot: ←/→ inspect samples, +/− zoom, Esc resets."); hint.className = "chart-explorer-hint"; hint.id = `${id}-hint`;
  region.setAttribute("aria-describedby", hint.id);
  const readout = html(document, "div"); readout.className = "chart-readout"; readout.setAttribute("data-chart-readout", ""); readout.setAttribute("role", "status"); readout.setAttribute("aria-live", "off");
  const readout_label = html(document, "div", "Nearest displayed samples · exact recorded values, without interpolation"); readout_label.className = "chart-readout-label";
  const readout_values = html(document, "div"); readout.append(readout_label, readout_values);
  section.insertBefore(styles, region); section.insertBefore(controls, region); section.insertBefore(hint, region); section.append(readout);
  const definitions = svgNode(document, "defs", {});
  const clip = svgNode(document, "clipPath", { id: `${id}-clip` });
  clip.append(svgNode(document, "rect", { x: String(LEFT), y: String(TOP), width: String(RIGHT - LEFT), height: String(BOTTOM - TOP) })); definitions.append(clip); svg.append(definitions);
  const axis = svgNode(document, "g", { "data-chart-x-axis": "" }); svg.append(axis);
  const crosshair = svgNode(document, "line", { "data-chart-crosshair": "", x1: String(LEFT), x2: String(LEFT), y1: String(TOP), y2: String(BOTTOM), stroke: "#9aa7bd", "stroke-width": "1", display: "none", "pointer-events": "none" }); svg.append(crosshair);
  const selection = svgNode(document, "rect", { y: String(TOP), height: String(BOTTOM - TOP), fill: "#4056b4", opacity: ".12", display: "none", "pointer-events": "none" }); svg.append(selection);
  const hit = svgNode(document, "rect", { "data-chart-hit": "", x: String(LEFT), y: String(TOP), width: String(RIGHT - LEFT), height: String(BOTTOM - TOP), fill: "transparent", "pointer-events": "all", "aria-hidden": "true" }); svg.append(hit);
  for (const node of [...original_curves.keys(), ...original_points.keys()]) node.setAttribute("clip-path", `url(#${id}-clip)`);
  for (const node of [...svg.querySelectorAll<SVGLineElement>("line.grid")]) {
    if (Number(node.getAttribute("y1")) === TOP && Number(node.getAttribute("y2")) === BOTTOM) node.remove();
  }
  for (const node of x_ticks) node.remove();
  function listen(target: EventTarget, name: string, callback: (event: Event) => void): void {
    target.addEventListener(name, callback); listeners.push(() => target.removeEventListener(name, callback));
  }
  function inRange(point: PlotPoint): boolean { const value = BigInt(point.x_value); return value >= BigInt(range.start_x) && value <= BigInt(range.end_x); }
  function available(item: Series): PlotPoint[] { return item.visible ? item.points.filter(inRange) : []; }
  function clearHighlights(): void { for (const [circle, original] of original_points) circle.setAttribute("r", original.r); }
  function showAt(x: number, y?: number, announce = false, anchor?: PlotPoint): void {
    const position = Math.max(LEFT, Math.min(RIGHT, x));
    inspection = { x: position, y, anchor };
    crosshair.setAttribute("display", ""); crosshair.setAttribute("x1", String(position)); crosshair.setAttribute("x2", String(position));
    readout.setAttribute("aria-live", announce ? "polite" : "off"); readout_values.replaceChildren(); clearHighlights();
    keyboard_anchor = announce && anchor ? anchor : null;
    let first = true;
    for (const item of series.values()) {
      if (!item.visible || !item.points.length) continue;
      const points = available(item), point = nearestChartPoint(points, position, y, anchor?.run_index === item.run_index ? anchor : undefined);
      const row = html(document, "div"); row.className = "chart-readout-row"; row.setAttribute("data-chart-readout-run", String(item.run_index));
      row.append(html(document, "code", item.run_id));
      if (point) {
        point.circle.setAttribute("r", "4.5");
        row.append(html(document, "span", `step ${point.step}`), html(document, "code", point.value_text), html(document, "span", `sample ${item.points.indexOf(point) + 1} of ${item.points.length} displayed`));
        row.setAttribute("data-chart-step", point.step); row.setAttribute("data-chart-value", point.value_text);
        row.setAttribute("data-chart-x", point.x_value);
        if (range.x_axis === "elapsed") row.append(html(document, "span", `Elapsed ${formatChartX(point.x_value, "elapsed")}`));
        if (range.x_axis === "wall_clock") row.append(html(document, "span", formatChartX(point.x_value, "wall_clock", time_zone)));
        if (point.timestamp) row.append(html(document, "span", `timestamp ${point.timestamp}`));
        if (point.timestamp) row.setAttribute("data-chart-timestamp", point.timestamp);
        if (first) { keyboard_index = points.indexOf(point); first = false; }
      } else row.append(html(document, "span", "No displayed samples in this range"));
      readout_values.append(row);
    }
    if (!readout_values.childNodes.length) readout_values.append(html(document, "span", "No visible runs. Use the legend to show a run."));
  }
  function drawLabels(): void {
    axis.replaceChildren();
    const [start, end] = coordinatesInRange(range), span = end - start;
    const count = span === 0n ? 1 : range.x_axis !== "step" || Math.max(range.start_x.length, range.end_x.length) > 12 ? 3 : 5;
    const seen = new Set<string>();
    for (let index = 0; index < count; index++) {
      const value = count === 1 ? start : start + span * BigInt(index) / BigInt(count - 1);
      const coordinate = String(value); if (seen.has(coordinate)) continue; seen.add(coordinate);
      const label = formatChartTick(coordinate, range, time_zone);
      const x = LEFT + chartXFraction(coordinate, range) * (RIGHT - LEFT);
      axis.append(svgNode(document, "line", { class: "grid", x1: String(x), x2: String(x), y1: String(TOP), y2: String(BOTTOM) }));
      const text = svgNode(document, "text", { class: "tick", x: String(x), y: String(BOTTOM + 23), "text-anchor": count === 1 ? "middle" : index === 0 ? "start" : index === count - 1 ? "end" : "middle" }); text.textContent = label; axis.append(text);
    }
    const first_label = formatChartX(range.start_x, range.x_axis, time_zone), last_label = formatChartX(range.end_x, range.x_axis, time_zone);
    const label = range.x_axis === "step" ? start === end ? "Step" : "Steps" : range.x_axis === "elapsed" ? "Elapsed" : "Date & time";
    range_label.textContent = start === end ? `${label} ${first_label}` : `${label} ${first_label}–${last_label}`;
    range_label.setAttribute("data-x-axis", range.x_axis); range_label.setAttribute("data-start-x", range.start_x); range_label.setAttribute("data-end-x", range.end_x);
    range_label.setAttribute("data-time-zone", time_zone);
    if (range.x_axis === "step") { range_label.setAttribute("data-start-step", range.start_x); range_label.setAttribute("data-end-step", range.end_x); }
    if (range.x_axis === "wall_clock") {
      const zone = timeZoneLabel(time_zone), first = dateParts(start, time_zone), last = dateParts(end, time_zone);
      if (axis_label) axis_label.textContent = `Date & time (${zone})`;
      if (context) context.textContent = first && last ? first.date_text === last.date_text ? `${zone} date: ${first.date_text}.` : `${zone} dates: ${first.date_text}–${last.date_text}.` : `Dates and times are shown in ${zone}.`;
    }
  }
  function draw(): void {
    drawLabels();
    const [start, end] = coordinatesInRange(range), span = end - start;
    for (const item of series.values()) {
      item.button.disabled = !item.points.length;
      item.button.setAttribute("aria-pressed", String(item.visible));
      for (const point of item.points) {
        point.x = LEFT + chartXFraction(point.x_value, range) * (RIGHT - LEFT);
        point.circle.setAttribute("cx", String(point.x));
        point.circle.setAttribute("display", item.visible && inRange(point) ? "" : "none");
      }
      item.polyline?.setAttribute("points", item.points.map(point => `${point.x},${point.y}`).join(" "));
      item.polyline?.setAttribute("display", item.visible ? "" : "none");
    }
    zoom_in.disabled = start === end || span <= 1n;
    zoom_out.disabled = range.start_x === full_range.start_x && range.end_x === full_range.end_x;
    reset.disabled = zoom_out.disabled && follow_full;
    crosshair.setAttribute("display", "none"); clearHighlights();
    readout_values.replaceChildren(html(document, "span", "Hover or use arrow keys to inspect displayed samples."));
    keyboard_index = -1;
    keyboard_anchor = null;
    inspection = null;
  }
  function applyRange(next: ChartRange): void {
    range = next; follow_full = range.start_x === full_range.start_x && range.end_x === full_range.end_x;
    selection.setAttribute("display", "none"); draw();
  }
  function coordinates(event: PointerEvent): { x: number; y: number } | null {
    const matrix = svg.getScreenCTM(); if (!matrix) return null;
    const point = svg.createSVGPoint(); point.x = event.clientX; point.y = event.clientY;
    const local = point.matrixTransform(matrix.inverse()); return { x: Math.max(LEFT, Math.min(RIGHT, local.x)), y: local.y };
  }
  for (const item of series.values()) listen(item.button, "click", () => { item.visible = !item.visible; draw(); });
  listen(zoom_in, "click", () => applyRange(zoomChartRange(range, full_range, .5)));
  listen(zoom_out, "click", () => applyRange(zoomChartRange(range, full_range, 2)));
  listen(reset, "click", () => applyRange(full_range));
  listen(hit, "pointermove", event => {
    const pointer = event as PointerEvent, position = coordinates(pointer); if (!position) return;
    if (drag_start && pointer.pointerId !== drag_start.pointer_id) return;
    if (drag_start) {
      selection.setAttribute("display", ""); selection.setAttribute("x", String(Math.min(drag_start.x, position.x))); selection.setAttribute("width", String(Math.abs(position.x - drag_start.x)));
    } else showAt(position.x, position.y);
  });
  listen(hit, "pointerdown", event => {
    const pointer = event as PointerEvent, position = coordinates(pointer); if (!position || pointer.button !== 0 || pointer.isPrimary === false || drag_start) return;
    drag_start = { x: position.x, screen_x: pointer.clientX, pointer_id: pointer.pointerId };
    try { hit.setPointerCapture?.(pointer.pointerId); } catch { /* Keyboard inspection and uncaptured pointer events still work. */ }
    region.focus(); showAt(position.x, position.y);
  });
  listen(hit, "pointerup", event => {
    const pointer = event as PointerEvent, position = coordinates(pointer), initial = drag_start;
    if (initial && pointer.pointerId !== initial.pointer_id) return;
    drag_start = null;
    selection.setAttribute("display", "none");
    if (initial && position && Math.abs(pointer.clientX - initial.screen_x) >= 6) applyRange(dragChartRange(range, (initial.x - LEFT) / (RIGHT - LEFT), (position.x - LEFT) / (RIGHT - LEFT)));
    else if (position) showAt(position.x, position.y);
    if (hit.hasPointerCapture?.(pointer.pointerId)) hit.releasePointerCapture(pointer.pointerId);
  });
  listen(hit, "pointercancel", event => { if (!drag_start || (event as PointerEvent).pointerId === drag_start.pointer_id) { drag_start = null; selection.setAttribute("display", "none"); } });
  listen(hit, "lostpointercapture", event => { if (drag_start?.pointer_id === (event as PointerEvent).pointerId) { drag_start = null; selection.setAttribute("display", "none"); } });
  listen(hit, "pointerleave", () => { if (!drag_start) crosshair.setAttribute("display", "none"); });
  listen(region, "keydown", event => {
    const key = event as KeyboardEvent;
    if (key.target !== region || key.altKey || key.ctrlKey || key.metaKey) return;
    if (["+", "=", "-", "_", "Escape", "Home", "ArrowLeft", "ArrowRight"].includes(key.key)) key.preventDefault();
    if (key.key === "+" || key.key === "=") applyRange(zoomChartRange(range, full_range, .5));
    else if (key.key === "-" || key.key === "_") applyRange(zoomChartRange(range, full_range, 2));
    else if (key.key === "Escape" || key.key === "Home") { drag_start = null; applyRange(full_range); }
    else if (key.key === "ArrowLeft" || key.key === "ArrowRight") {
      const item = [...series.values()].find(candidate => available(candidate).length);
      const points = item && available(item); if (!points?.length) return;
      keyboard_index = Math.min(points.length - 1, Math.max(0, keyboard_index + (key.key === "ArrowRight" ? 1 : -1)));
      const point = points[keyboard_index]; if (point) showAt(point.x, point.y, true, point);
    }
  });
  draw();
  return {
    setTimeZone(next) {
      if (time_zone === next) return;
      time_zone = next;
      range_label.setAttribute("data-time-zone", time_zone);
      if (range.x_axis !== "wall_clock") return;
      drawLabels();
      if (inspection) {
        const anchor = keyboard_anchor, index = keyboard_index, crosshair_display = crosshair.getAttribute("display");
        showAt(inspection.x, inspection.y, false, inspection.anchor);
        keyboard_anchor = anchor; keyboard_index = index;
        if (crosshair_display !== null) crosshair.setAttribute("display", crosshair_display);
      }
    },
    isInteracting() { return drag_start !== null; },
    capture() {
      const active = document.activeElement;
      let focus: string | null = active === region ? "plot" : active === zoom_in ? "zoom-in" : active === zoom_out ? "zoom-out" : active === reset ? "reset" : null;
      for (const item of series.values()) if (active === item.button) focus = `run:${item.run_id}`;
      const anchored = keyboard_anchor, item = anchored && series.get(anchored.run_index);
      const anchor = anchored && item ? {
        run_id: item.run_id, step: anchored.step, x_value: anchored.x_value, value_text: anchored.value_text,
        occurrence: item.points.filter(point => point.step === anchored.step && point.x_value === anchored.x_value && point.value_text === anchored.value_text).indexOf(anchored),
      } : null;
      return {
        metric_name, x_axis: range.x_axis, range: follow_full ? null : { ...range },
        hidden_run_ids: [...series.values()].filter(item => !item.visible).map(item => item.run_id), keyboard_anchor: anchor,
        focus, scroll_left: region.scrollLeft, scroll_top: region.scrollTop,
      };
    },
    restore(state) {
      range = restoreChartRange(state.range, full_range);
      follow_full = state.range === null || state.x_axis !== full_range.x_axis;
      for (const item of series.values()) item.visible = !state.hidden_run_ids.includes(item.run_id);
      draw();
      const anchor = state.x_axis === full_range.x_axis ? state.keyboard_anchor : null, item = anchor && [...series.values()].find(item => item.run_id === anchor.run_id);
      const point = anchor && item && available(item).filter(point => point.step === anchor.step && point.x_value === anchor.x_value && point.value_text === anchor.value_text)[anchor.occurrence];
      if (point) { showAt(point.x, point.y, false, point); keyboard_anchor = point; }
      const focus = state.focus;
      const focused = focus === "plot" ? region : focus === "zoom-in" ? zoom_in : focus === "zoom-out" ? zoom_out : focus === "reset" ? reset : [...series.values()].find(item => focus === `run:${item.run_id}`)?.button;
      if (focused && !(focused.tagName === "BUTTON" && (focused as HTMLButtonElement).disabled)) focused.focus({ preventScroll: true });
      else if (focus) region.focus({ preventScroll: true });
      region.scrollLeft = state.scroll_left; region.scrollTop = state.scroll_top;
    },
    dispose() {
      for (const remove of listeners) remove();
      for (const [item, children] of original_legends) item.replaceChildren(...children);
      for (const [node, attributes] of original_attributes) {
        for (const [name, value] of Object.entries(attributes)) value === null ? node.removeAttribute(name) : node.setAttribute(name, value);
      }
      svg.replaceChildren(...original_nodes); styles.remove(); controls.remove(); hint.remove(); readout.remove();
      if (axis_label) axis_label.textContent = original_axis_label;
      if (context) context.textContent = original_context;
      if (original_description === null) region.removeAttribute("aria-describedby"); else region.setAttribute("aria-describedby", original_description);
      section.removeAttribute("data-interactive-chart");
    },
  };
}
