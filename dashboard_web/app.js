// interactive_charts.ts
var MAX_STEP = 18446744073709551615n;
var MIN_COORDINATE = -(1n << 127n);
var MAX_COORDINATE = (1n << 127n) - 1n;
var NANOS_PER_SECOND = 1000000000n;
var FRACTION_SCALE = 1000000000000n;
var SVG_NS = "http://www.w3.org/2000/svg";
var LEFT = 86;
var RIGHT = 950;
var TOP = 28;
var BOTTOM = 274;
var chart_sequence = 0;
function parseChartPointLabel(label) {
  const match = /^Run ([1-8]) · step (\d+) · value (-?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?)(?: · timestamp [^·\r\n]{1,128})?(?: · elapsed [^·\r\n]{1,80})?$/.exec(label);
  if (!match) return null;
  const step = match[2], value_text = match[3];
  if (step === void 0 || value_text === void 0 || step.length > 20 || BigInt(step) > MAX_STEP) return null;
  const value = Number(value_text);
  return Number.isFinite(value) ? { run_index: Number(match[1]), step, value, value_text } : null;
}
function coordinate(value, x_axis) {
  if (!/^-?(?:0|[1-9]\d{0,38})$/.test(value) || value === "-0") return null;
  const parsed = BigInt(value);
  return x_axis === "step" ? parsed >= 0n && parsed <= MAX_STEP ? parsed : null : parsed >= MIN_COORDINATE && parsed <= MAX_COORDINATE ? parsed : null;
}
function parseChartRange(x_axis, start_x, end_x) {
  if (!["step", "elapsed", "wall_clock"].includes(x_axis ?? "") || start_x === null || end_x === null) return null;
  const axis = x_axis, start = coordinate(start_x, axis), end = coordinate(end_x, axis);
  return start !== null && end !== null && start <= end ? { x_axis: axis, start_x, end_x } : null;
}
function coordinatesInRange(range) {
  if (!parseChartRange(range.x_axis, range.start_x, range.end_x)) throw new Error("Invalid chart axis range");
  const start = BigInt(range.start_x), end = BigInt(range.end_x);
  return [start, end];
}
function fractionInteger(fraction) {
  return BigInt(Math.round(Math.min(1, Math.max(0, Number.isFinite(fraction) ? fraction : 0)) * Number(FRACTION_SCALE)));
}
function xAt(range, fraction) {
  const [start, end] = coordinatesInRange(range);
  return start + (end - start) * fractionInteger(fraction) / FRACTION_SCALE;
}
function chartXFraction(x_value, range) {
  const [start, end] = coordinatesInRange(range), value = coordinate(x_value, range.x_axis);
  if (value === null) throw new Error("Invalid chart axis coordinate");
  if (start === end) return 0.5;
  return Number((value - start) * FRACTION_SCALE / (end - start)) / Number(FRACTION_SCALE);
}
function dragChartRange(range, start_fraction, end_fraction) {
  const first = xAt(range, Math.min(start_fraction, end_fraction));
  const last = xAt(range, Math.max(start_fraction, end_fraction));
  return first < last ? { x_axis: range.x_axis, start_x: String(first), end_x: String(last) } : range;
}
function zoomChartRange(range, full_range, factor, center_fraction = 0.5) {
  if (range.x_axis !== full_range.x_axis) throw new Error("Cannot zoom different chart axes");
  const [full_start, full_end] = coordinatesInRange(full_range), [start, end] = coordinatesInRange(range);
  const full_span = full_end - full_start;
  if (full_span === 0n || !Number.isFinite(factor) || factor <= 0) return range;
  const ratio = BigInt(Math.round(Math.min(1e6, factor) * 1e6));
  let span = (end - start) * ratio / 1000000n;
  if (span < 1n) span = 1n;
  if (span > full_span) span = full_span;
  const anchor = xAt(range, center_fraction);
  let next_start = anchor - span * fractionInteger(center_fraction) / FRACTION_SCALE;
  if (next_start < full_start) next_start = full_start;
  if (next_start + span > full_end) next_start = full_end - span;
  return { x_axis: range.x_axis, start_x: String(next_start), end_x: String(next_start + span) };
}
function restoreChartRange(range, full_range) {
  const [full_start, full_end] = coordinatesInRange(full_range);
  if (range === null || range.x_axis !== full_range.x_axis) return full_range;
  const [start, end] = coordinatesInRange(range);
  const clamp = (value) => value < full_start ? full_start : value > full_end ? full_end : value;
  return { x_axis: range.x_axis, start_x: String(clamp(start)), end_x: String(clamp(end)) };
}
function formatChartX(x_value, x_axis) {
  const value = coordinate(x_value, x_axis);
  if (value === null) throw new Error("Invalid chart axis coordinate");
  if (x_axis === "step") return x_value;
  if (x_axis === "elapsed") {
    const negative = value < 0n, absolute = negative ? -value : value;
    const sign = negative ? "\u2212" : "";
    if (absolute !== 0n && absolute < NANOS_PER_SECOND) {
      const [divisor, digits, unit] = absolute < 1000n ? [1n, 0, "ns"] : absolute < 1000000n ? [1000n, 3, "\xB5s"] : [1000000n, 6, "ms"];
      const remainder3 = absolute % divisor;
      const fraction3 = remainder3 === 0n ? "" : `.${String(remainder3).padStart(digits, "0").replace(/0+$/, "")}`;
      return `${sign}${absolute / divisor}${fraction3} ${unit}`;
    }
    const seconds2 = absolute / NANOS_PER_SECOND, remainder2 = absolute % NANOS_PER_SECOND;
    const fraction2 = remainder2 === 0n ? "" : `.${String(remainder2).padStart(9, "0").replace(/0+$/, "")}`;
    if (seconds2 >= 3600n) return `${sign}${seconds2 / 3600n}:${String(seconds2 / 60n % 60n).padStart(2, "0")}:${String(seconds2 % 60n).padStart(2, "0")}${fraction2} h`;
    if (seconds2 >= 60n) return `${sign}${seconds2 / 60n}:${String(seconds2 % 60n).padStart(2, "0")}${fraction2} min`;
    return `${sign}${seconds2}${fraction2} s`;
  }
  let seconds = value / NANOS_PER_SECOND, remainder = value % NANOS_PER_SECOND;
  if (remainder < 0n) {
    seconds -= 1n;
    remainder += NANOS_PER_SECOND;
  }
  const date = new Date(Number(seconds) * 1e3);
  if (!Number.isFinite(date.getTime())) return `${x_value} ns since Unix epoch`;
  const fraction = remainder === 0n ? "" : `.${String(remainder).padStart(9, "0").replace(/0+$/, "")}`;
  return `${date.toISOString().replace(/\.\d{3}Z$/, "").replace("T", " ")}${fraction} UTC`;
}
function formatChartTick(x_value, range) {
  const label = formatChartX(x_value, range.x_axis);
  if (range.x_axis !== "wall_clock") return label;
  const [start, end] = coordinatesInRange(range);
  const parts = /^(\S+) (\S+) UTC$/.exec(label), first = /^(\S+) /.exec(formatChartX(range.start_x, "wall_clock")), last = /^(\S+) /.exec(formatChartX(range.end_x, "wall_clock"));
  if (!parts?.[1] || !parts[2] || !first?.[1] || !last?.[1]) return label;
  if (first[1] === last[1]) return parts[2];
  if (end - start >= 365n * 86400n * NANOS_PER_SECOND) return parts[1];
  const time = end - start < 120n * NANOS_PER_SECOND ? parts[2] : parts[2].slice(0, 5);
  return `${parts[1].slice(-5)} ${time}`;
}
function nearestChartPoint(points, x, y, preferred) {
  if (preferred && points.includes(preferred) && preferred.x === x && (y === void 0 || preferred.y === y)) return preferred;
  let nearest = null, distance = Infinity, tie_distance = Infinity;
  for (const point of points) {
    const candidate = Math.abs(point.x - x), tie = y === void 0 ? 0 : Math.abs(point.y - y);
    if (candidate < distance - 1e-7 || Math.abs(candidate - distance) < 1e-7 && tie < tie_distance) {
      nearest = point;
      distance = candidate;
      tie_distance = tie;
    }
  }
  return nearest;
}
function html(document2, tag, text = "") {
  const node = document2.createElement(tag);
  node.textContent = text;
  return node;
}
function svgNode(document2, tag, attrs) {
  const node = document2.createElementNS(SVG_NS, tag);
  for (const [name, value] of Object.entries(attrs)) node.setAttribute(name, value);
  return node;
}
function plotRange(svg) {
  if (["data-x-axis", "data-x-min", "data-x-max"].some((name) => svg.hasAttribute(name))) {
    return parseChartRange(svg.getAttribute("data-x-axis"), svg.getAttribute("data-x-min"), svg.getAttribute("data-x-max"));
  }
  const tick_steps = [...svg.querySelectorAll("text.tick")].filter((node) => Number(node.getAttribute("y")) >= BOTTOM + 12).map((node) => node.textContent?.trim() ?? "").filter((value) => /^\d{1,20}$/.test(value));
  if (!tick_steps.length) return null;
  const values = tick_steps.map((value) => BigInt(value)).sort((first, second) => first < second ? -1 : first > second ? 1 : 0);
  const start = values[0], end = values[values.length - 1];
  if (start === void 0 || end === void 0 || end > MAX_STEP) return null;
  return { x_axis: "step", start_x: String(start), end_x: String(end) };
}
function pointCoordinate(circle, parsed, range, explicit) {
  const value = explicit ? circle.getAttribute("data-x-value") : parsed.step;
  const x_value = value === null ? null : coordinate(value, range.x_axis);
  if (x_value === null || x_value < BigInt(range.start_x) || x_value > BigInt(range.end_x)) return null;
  if (range.x_axis === "step" && x_value !== BigInt(parsed.step)) return null;
  return String(x_value);
}
function pointTimestamp(circle) {
  const timestamp = circle.getAttribute("data-timestamp");
  return timestamp !== null && timestamp.length <= 128 && /^[+-]?\d{4,6}-\d{2}-\d{2}[Tt ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:[Zz]|[+-]\d{2}:\d{2})$/.test(timestamp) ? timestamp : null;
}
var PREVIEW_HTML_TAGS = new Set("html head body meta title style main p section h2 div ul li span code pre details summary table thead tbody tr th td".split(" "));
var PREVIEW_SVG_TAGS = new Set("svg title desc line text polyline circle".split(" "));
var PREVIEW_ATTRIBUTES = new Set("lang charset name content class id role aria-label aria-labelledby tabindex viewBox x y x1 x2 y1 y2 cx cy r fill stroke stroke-width stroke-dasharray points text-anchor transform style scope data-x-axis data-x-min data-x-max data-x-value data-timestamp".split(" "));
function isDashboardPreview(document2) {
  if (document2.title !== "Run comparison \xB7 expri" || !document2.querySelector("body > main > .chart-summary")) return false;
  if (document2.querySelectorAll("circle.point").length > 4800 || document2.querySelectorAll(".plot-scroll > svg").length > 6) return false;
  for (const svg of document2.querySelectorAll(".plot-scroll > svg")) {
    const section = svg.closest("section.card"), metric = section?.querySelector("h2")?.textContent;
    const run_ids = [...section?.querySelectorAll("ul.legend li code") ?? []].map((node) => node.textContent ?? "");
    const circles = svg.querySelectorAll("circle.point");
    const range = plotRange(svg), explicit = svg.hasAttribute("data-x-axis");
    if (!metric || !run_ids.length || run_ids.length > 8 || new Set(run_ids).size !== run_ids.length || run_ids.some((id) => !id) || !range || !circles.length) return false;
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
function parseDashboardPreview(markup) {
  if (markup.length > 2 * 1024 * 1024 || new TextEncoder().encode(markup).byteLength > 2 * 1024 * 1024) return null;
  for (const match of markup.matchAll(/<\s*([a-z][a-z0-9:-]*)\b/gi)) {
    const tag = match[1]?.toLowerCase();
    if (!tag || !PREVIEW_HTML_TAGS.has(tag) && !PREVIEW_SVG_TAGS.has(tag)) return null;
  }
  const document2 = new DOMParser().parseFromString(markup, "text/html");
  for (const node of document2.querySelectorAll("*")) {
    const allowed = node.namespaceURI === SVG_NS ? PREVIEW_SVG_TAGS : PREVIEW_HTML_TAGS;
    if (![SVG_NS, "http://www.w3.org/1999/xhtml"].includes(node.namespaceURI ?? "") || !allowed.has(node.localName)) return null;
    for (const attribute of node.attributes) if (!PREVIEW_ATTRIBUTES.has(attribute.name)) return null;
    if (node.localName === "meta" && !(node.hasAttribute("charset") || node.getAttribute("name") === "viewport")) return null;
    const style = node.localName === "style" ? node.textContent ?? "" : node.getAttribute("style") ?? "";
    if (/(?:url\s*\(|@import|expression\s*\()/i.test(style)) return null;
  }
  return isDashboardPreview(document2) ? document2 : null;
}
function createChartController(frame) {
  let plots = [], current_document = null, disposed = false;
  let completed_source = null;
  const hidden_runs = /* @__PURE__ */ new Map();
  const clear = (forget_visibility = false) => {
    for (const plot of plots) plot.dispose();
    plots = [];
    current_document = null;
    if (forget_visibility) hidden_runs.clear();
  };
  function currentSource() {
    if (!frame.src || !frame.ownerDocument?.baseURI) return null;
    const source = new URL(frame.src, frame.ownerDocument.baseURI), owner = new URL(frame.ownerDocument.baseURI);
    return source.origin === owner.origin ? source : null;
  }
  function loadedDocument() {
    try {
      const document2 = frame.contentDocument;
      const source = currentSource();
      if (!document2?.querySelectorAll || !source || document2.readyState !== "complete") return null;
      return document2.URL === source.href && isDashboardPreview(document2) ? document2 : null;
    } catch {
      return null;
    }
  }
  function enhanceDocument(document2) {
    current_document = document2;
    for (const svg of document2.querySelectorAll(".plot-scroll > svg")) {
      const plot = enhancePlot(document2, svg);
      if (plot) plots.push(plot);
    }
  }
  const enhance = (completed = false) => {
    clear(true);
    if (disposed) return;
    try {
      const source = currentSource(), child = frame.contentDocument;
      if (source && (completed && !child || child?.readyState === "complete" && (child.URL === source.href || child.URL.startsWith("about:neterror")))) completed_source = source.href;
      const document2 = loadedDocument();
      if (document2) enhanceDocument(document2);
    } catch {
    }
  };
  const loaded = () => enhance(true);
  frame.addEventListener("load", loaded);
  const observer = typeof MutationObserver === "undefined" ? null : new MutationObserver(() => {
    completed_source = null;
    clear(true);
  });
  observer?.observe(frame, { attributes: true, attributeFilter: ["src"] });
  enhance();
  return {
    dispose() {
      disposed = true;
      frame.removeEventListener("load", loaded);
      observer?.disconnect();
      clear(true);
    },
    isInteracting() {
      return plots.some((plot) => plot.isInteracting());
    },
    previewStatus() {
      if (disposed) return "loading";
      try {
        const source = currentSource();
        if (!source) return "loading";
        if (current_document && loadedDocument() === current_document) return "ready";
        return completed_source === source.href ? "invalid" : "loading";
      } catch {
        return "loading";
      }
    },
    replacePreview(markup) {
      if (disposed || plots.some((plot) => plot.isInteracting())) return false;
      const document2 = loadedDocument();
      if (!document2 || document2 !== current_document) return false;
      let next;
      try {
        next = parseDashboardPreview(markup);
      } catch {
        return false;
      }
      if (!next) return false;
      const frame_focused = frame.ownerDocument.activeElement === frame;
      const states = new Map(plots.map((plot) => {
        const state = plot.capture();
        if (!frame_focused) state.focus = null;
        return [state.metric_name, state];
      }));
      const metric_names = new Set([...next.querySelectorAll("body > main > section.card > h2")].slice(0, 6).map((node) => node.textContent ?? ""));
      for (const name of hidden_runs.keys()) if (!metric_names.has(name)) hidden_runs.delete(name);
      for (const [name, state] of states) if (metric_names.has(name)) hidden_runs.set(name, [...state.hidden_run_ids]);
      const window2 = document2.defaultView, parent = frame.ownerDocument.defaultView;
      const scroll = { x: window2?.scrollX ?? 0, y: window2?.scrollY ?? 0, parent_x: parent?.scrollX ?? 0, parent_y: parent?.scrollY ?? 0 };
      const previous_details = document2.querySelector("details.parameter-comparison");
      const details_open = previous_details?.open ?? false, details_focused = frame_focused && document2.activeElement === previous_details?.querySelector("summary");
      const parameter_scroll = previous_details?.querySelector(".table-scroll")?.scrollLeft ?? 0;
      const head = [...next.head.childNodes].map((node) => document2.importNode(node, true));
      const body = [...next.body.childNodes].map((node) => document2.importNode(node, true));
      clear();
      document2.head.replaceChildren(...head);
      document2.body.replaceChildren(...body);
      enhanceDocument(document2);
      for (const plot of plots) {
        const fresh = plot.capture(), state = states.get(fresh.metric_name);
        if (state) plot.restore(state);
        else {
          const hidden_run_ids = hidden_runs.get(fresh.metric_name);
          if (hidden_run_ids) plot.restore({ ...fresh, hidden_run_ids });
        }
      }
      const details = document2.querySelector("details.parameter-comparison");
      if (details) {
        details.open = details_open;
        if (details_focused) details.querySelector("summary")?.focus({ preventScroll: true });
        const table = details.querySelector(".table-scroll");
        if (table) table.scrollLeft = parameter_scroll;
      }
      window2?.scrollTo(scroll.x, scroll.y);
      parent?.scrollTo(scroll.parent_x, scroll.parent_y);
      return true;
    }
  };
}
function attachChartInteractions(frame) {
  const controller = createChartController(frame);
  return () => controller.dispose();
}
function enhancePlot(document2, svg) {
  const section = svg.closest("section.card");
  const region = svg.parentElement;
  const legend = section?.querySelector("ul.legend");
  if (!section || !region || !legend || section.hasAttribute("data-interactive-chart")) return null;
  const x_ticks = [...svg.querySelectorAll("text.tick")].filter((node) => Number(node.getAttribute("y")) >= BOTTOM + 12);
  const parsed_range = plotRange(svg);
  if (!parsed_range) return null;
  const full_range = parsed_range;
  let range = full_range, follow_full = true, keyboard_index = -1, keyboard_anchor = null;
  let drag_start = null;
  const listeners = [];
  const original_nodes = [...svg.childNodes];
  const original_attributes = /* @__PURE__ */ new Map();
  const original_description = region.getAttribute("aria-describedby");
  function remember(node, names) {
    original_attributes.set(node, Object.fromEntries(names.map((name) => [name, node.getAttribute(name)])));
  }
  const original_points = /* @__PURE__ */ new Map();
  const original_curves = /* @__PURE__ */ new Map();
  const original_legends = /* @__PURE__ */ new Map();
  const metric_name = section.querySelector("h2")?.textContent ?? "metric";
  const series = /* @__PURE__ */ new Map();
  const legend_items = [...legend.querySelectorAll("li")];
  for (let index = 0; index < legend_items.length; index++) {
    const item = legend_items[index];
    if (!item) continue;
    const run_id = item.querySelector("code")?.textContent ?? `Run ${index + 1}`;
    const button = html(document2, "button");
    button.type = "button";
    button.className = "chart-run-toggle";
    button.setAttribute("data-chart-run", String(index + 1));
    button.setAttribute("aria-pressed", "true");
    button.setAttribute("aria-label", `Toggle ${run_id} in ${metric_name}`);
    original_legends.set(item, [...item.childNodes]);
    button.append(...item.childNodes);
    item.append(button);
    series.set(index + 1, { run_index: index + 1, run_id, points: [], polyline: null, button, visible: true });
  }
  let preceding_curve = null;
  let invalid_points = false;
  for (const node of svg.querySelectorAll("polyline, circle.point")) {
    if (node.tagName.toLowerCase() === "polyline") {
      preceding_curve = node;
      remember(preceding_curve, ["points", "clip-path", "display", "data-chart-series"]);
      original_curves.set(preceding_curve, preceding_curve.getAttribute("points") ?? "");
      continue;
    }
    const circle = node;
    const parsed = parseChartPointLabel(circle.getAttribute("aria-label") ?? circle.querySelector("title")?.textContent ?? "");
    const item = parsed && series.get(parsed.run_index);
    const x_value = parsed && pointCoordinate(circle, parsed, full_range, svg.hasAttribute("data-x-axis"));
    const timestamp = pointTimestamp(circle);
    const x = Number(circle.getAttribute("cx")), y = Number(circle.getAttribute("cy"));
    if (!parsed || !item || x_value === null || full_range.x_axis !== "step" && timestamp === null || circle.getAttribute("cx") === null || circle.getAttribute("cy") === null || !Number.isFinite(x) || !Number.isFinite(y)) {
      invalid_points = true;
      continue;
    }
    item.polyline = preceding_curve;
    item.points.push({ ...parsed, x_value, timestamp, x, y, circle });
    original_points.set(circle, { cx: circle.getAttribute("cx") ?? "", r: circle.getAttribute("r") ?? "2" });
    remember(circle, ["cx", "r", "clip-path", "display", "data-chart-series"]);
    circle.setAttribute("data-chart-series", String(parsed.run_index));
    if (preceding_curve) preceding_curve.setAttribute("data-chart-series", String(parsed.run_index));
  }
  if (invalid_points || ![...series.values()].some((item) => item.points.length)) {
    for (const [item, children] of original_legends) item.replaceChildren(...children);
    for (const [node, attributes] of original_attributes) {
      for (const [name, value] of Object.entries(attributes)) value === null ? node.removeAttribute(name) : node.setAttribute(name, value);
    }
    return null;
  }
  section.setAttribute("data-interactive-chart", "true");
  const id = `chart-explorer-${++chart_sequence}`;
  const styles = html(document2, "style");
  styles.textContent = `[data-interactive-chart] .chart-explorer-controls{display:flex;align-items:center;gap:7px;flex-wrap:wrap;margin:8px 0;font-size:12px}[data-interactive-chart] .chart-explorer-controls button{font:inherit;border:1px solid var(--line);border-radius:5px;background:var(--paper);color:var(--ink);padding:6px 9px;cursor:pointer}[data-interactive-chart] button:disabled{opacity:.5;cursor:default}[data-interactive-chart] .chart-range{overflow-wrap:anywhere;font:11px ui-monospace,SFMono-Regular,Consolas,monospace}[data-interactive-chart] .chart-explorer-hint{font-size:11px;color:var(--muted);margin:6px 0}[data-interactive-chart] .chart-run-toggle{display:flex;align-items:center;gap:5px;flex-wrap:wrap;max-width:100%;font:inherit;border:0;border-radius:4px;background:transparent;color:var(--ink);padding:4px;cursor:pointer;text-align:left}[data-interactive-chart] .chart-run-toggle[aria-pressed=false]{opacity:.45;text-decoration:line-through}[data-interactive-chart] .chart-readout{font-size:12px;border-top:1px solid var(--line);margin:10px 0 0;padding-top:8px;overflow-wrap:anywhere}[data-interactive-chart] .chart-readout-row{display:flex;flex-wrap:wrap;gap:4px 12px;margin-top:4px}[data-interactive-chart] .chart-readout-row code{font-size:11px}[data-interactive-chart] .chart-readout-label{font-size:11px;color:var(--muted)}[data-interactive-chart] rect[data-chart-hit]{touch-action:pan-y;cursor:crosshair}[data-interactive-chart] :focus-visible{outline:2px solid #4056b4;outline-offset:2px}`;
  const controls = html(document2, "div");
  controls.className = "chart-explorer-controls";
  const zoom_in = html(document2, "button", "+ Zoom in"), zoom_out = html(document2, "button", "\u2212 Zoom out"), reset = html(document2, "button", "Reset zoom");
  for (const [button, action] of [[zoom_in, "in"], [zoom_out, "out"], [reset, "reset"]]) {
    button.type = "button";
    button.setAttribute("data-chart-zoom", action);
    controls.append(button);
  }
  const range_label = html(document2, "span");
  range_label.className = "chart-range";
  range_label.setAttribute("data-chart-range", "");
  controls.append(range_label);
  const hint = html(document2, "p", "Drag across the plot to zoom. Focus the plot: \u2190/\u2192 inspect samples, +/\u2212 zoom, Esc resets.");
  hint.className = "chart-explorer-hint";
  hint.id = `${id}-hint`;
  region.setAttribute("aria-describedby", hint.id);
  const readout = html(document2, "div");
  readout.className = "chart-readout";
  readout.setAttribute("data-chart-readout", "");
  readout.setAttribute("role", "status");
  readout.setAttribute("aria-live", "off");
  const readout_label = html(document2, "div", "Nearest displayed samples \xB7 exact recorded values, without interpolation");
  readout_label.className = "chart-readout-label";
  const readout_values = html(document2, "div");
  readout.append(readout_label, readout_values);
  section.insertBefore(styles, region);
  section.insertBefore(controls, region);
  section.insertBefore(hint, region);
  section.append(readout);
  const definitions = svgNode(document2, "defs", {});
  const clip = svgNode(document2, "clipPath", { id: `${id}-clip` });
  clip.append(svgNode(document2, "rect", { x: String(LEFT), y: String(TOP), width: String(RIGHT - LEFT), height: String(BOTTOM - TOP) }));
  definitions.append(clip);
  svg.append(definitions);
  const axis = svgNode(document2, "g", { "data-chart-x-axis": "" });
  svg.append(axis);
  const crosshair = svgNode(document2, "line", { "data-chart-crosshair": "", x1: String(LEFT), x2: String(LEFT), y1: String(TOP), y2: String(BOTTOM), stroke: "#9aa7bd", "stroke-width": "1", display: "none", "pointer-events": "none" });
  svg.append(crosshair);
  const selection = svgNode(document2, "rect", { y: String(TOP), height: String(BOTTOM - TOP), fill: "#4056b4", opacity: ".12", display: "none", "pointer-events": "none" });
  svg.append(selection);
  const hit = svgNode(document2, "rect", { "data-chart-hit": "", x: String(LEFT), y: String(TOP), width: String(RIGHT - LEFT), height: String(BOTTOM - TOP), fill: "transparent", "pointer-events": "all", "aria-hidden": "true" });
  svg.append(hit);
  for (const node of [...original_curves.keys(), ...original_points.keys()]) node.setAttribute("clip-path", `url(#${id}-clip)`);
  for (const node of [...svg.querySelectorAll("line.grid")]) {
    if (Number(node.getAttribute("y1")) === TOP && Number(node.getAttribute("y2")) === BOTTOM) node.remove();
  }
  for (const node of x_ticks) node.remove();
  function listen(target, name, callback) {
    target.addEventListener(name, callback);
    listeners.push(() => target.removeEventListener(name, callback));
  }
  function inRange(point) {
    const value = BigInt(point.x_value);
    return value >= BigInt(range.start_x) && value <= BigInt(range.end_x);
  }
  function available(item) {
    return item.visible ? item.points.filter(inRange) : [];
  }
  function clearHighlights() {
    for (const [circle, original] of original_points) circle.setAttribute("r", original.r);
  }
  function showAt(x, y, announce = false, anchor) {
    const position = Math.max(LEFT, Math.min(RIGHT, x));
    crosshair.setAttribute("display", "");
    crosshair.setAttribute("x1", String(position));
    crosshair.setAttribute("x2", String(position));
    readout.setAttribute("aria-live", announce ? "polite" : "off");
    readout_values.replaceChildren();
    clearHighlights();
    keyboard_anchor = announce && anchor ? anchor : null;
    let first = true;
    for (const item of series.values()) {
      if (!item.visible || !item.points.length) continue;
      const points = available(item), point = nearestChartPoint(points, position, y, anchor?.run_index === item.run_index ? anchor : void 0);
      const row = html(document2, "div");
      row.className = "chart-readout-row";
      row.setAttribute("data-chart-readout-run", String(item.run_index));
      row.append(html(document2, "code", item.run_id));
      if (point) {
        point.circle.setAttribute("r", "4.5");
        row.append(html(document2, "span", `step ${point.step}`), html(document2, "code", point.value_text), html(document2, "span", `sample ${item.points.indexOf(point) + 1} of ${item.points.length} displayed`));
        row.setAttribute("data-chart-step", point.step);
        row.setAttribute("data-chart-value", point.value_text);
        row.setAttribute("data-chart-x", point.x_value);
        if (range.x_axis === "elapsed") row.append(html(document2, "span", `Elapsed ${formatChartX(point.x_value, "elapsed")}`));
        if (range.x_axis === "wall_clock") row.append(html(document2, "span", formatChartX(point.x_value, "wall_clock")));
        if (point.timestamp) row.append(html(document2, "span", `timestamp ${point.timestamp}`));
        if (point.timestamp) row.setAttribute("data-chart-timestamp", point.timestamp);
        if (first) {
          keyboard_index = points.indexOf(point);
          first = false;
        }
      } else row.append(html(document2, "span", "No displayed samples in this range"));
      readout_values.append(row);
    }
    if (!readout_values.childNodes.length) readout_values.append(html(document2, "span", "No visible runs. Use the legend to show a run."));
  }
  function draw() {
    axis.replaceChildren();
    const [start, end] = coordinatesInRange(range), span = end - start;
    const count = span === 0n ? 1 : range.x_axis !== "step" || Math.max(range.start_x.length, range.end_x.length) > 12 ? 3 : 5;
    const seen = /* @__PURE__ */ new Set();
    for (let index = 0; index < count; index++) {
      const value = count === 1 ? start : start + span * BigInt(index) / BigInt(count - 1);
      const coordinate2 = String(value);
      if (seen.has(coordinate2)) continue;
      seen.add(coordinate2);
      const label2 = formatChartTick(coordinate2, range);
      const x = LEFT + chartXFraction(coordinate2, range) * (RIGHT - LEFT);
      axis.append(svgNode(document2, "line", { class: "grid", x1: String(x), x2: String(x), y1: String(TOP), y2: String(BOTTOM) }));
      const text = svgNode(document2, "text", { class: "tick", x: String(x), y: String(BOTTOM + 23), "text-anchor": count === 1 ? "middle" : index === 0 ? "start" : index === count - 1 ? "end" : "middle" });
      text.textContent = label2;
      axis.append(text);
    }
    for (const item of series.values()) {
      item.button.disabled = !item.points.length;
      item.button.setAttribute("aria-pressed", String(item.visible));
      for (const point of item.points) {
        point.x = LEFT + chartXFraction(point.x_value, range) * (RIGHT - LEFT);
        point.circle.setAttribute("cx", String(point.x));
        point.circle.setAttribute("display", item.visible && inRange(point) ? "" : "none");
      }
      item.polyline?.setAttribute("points", item.points.map((point) => `${point.x},${point.y}`).join(" "));
      item.polyline?.setAttribute("display", item.visible ? "" : "none");
    }
    const first_label = formatChartX(range.start_x, range.x_axis), last_label = formatChartX(range.end_x, range.x_axis);
    const label = range.x_axis === "step" ? start === end ? "Step" : "Steps" : range.x_axis === "elapsed" ? "Elapsed" : "UTC time";
    range_label.textContent = start === end ? `${label} ${first_label}` : `${label} ${first_label}\u2013${last_label}`;
    range_label.setAttribute("data-x-axis", range.x_axis);
    range_label.setAttribute("data-start-x", range.start_x);
    range_label.setAttribute("data-end-x", range.end_x);
    if (range.x_axis === "step") {
      range_label.setAttribute("data-start-step", range.start_x);
      range_label.setAttribute("data-end-step", range.end_x);
    }
    zoom_in.disabled = start === end || span <= 1n;
    zoom_out.disabled = range.start_x === full_range.start_x && range.end_x === full_range.end_x;
    reset.disabled = zoom_out.disabled && follow_full;
    crosshair.setAttribute("display", "none");
    clearHighlights();
    readout_values.replaceChildren(html(document2, "span", "Hover or use arrow keys to inspect displayed samples."));
    keyboard_index = -1;
    keyboard_anchor = null;
  }
  function applyRange(next) {
    range = next;
    follow_full = range.start_x === full_range.start_x && range.end_x === full_range.end_x;
    selection.setAttribute("display", "none");
    draw();
  }
  function coordinates(event) {
    const matrix = svg.getScreenCTM();
    if (!matrix) return null;
    const point = svg.createSVGPoint();
    point.x = event.clientX;
    point.y = event.clientY;
    const local = point.matrixTransform(matrix.inverse());
    return { x: Math.max(LEFT, Math.min(RIGHT, local.x)), y: local.y };
  }
  for (const item of series.values()) listen(item.button, "click", () => {
    item.visible = !item.visible;
    draw();
  });
  listen(zoom_in, "click", () => applyRange(zoomChartRange(range, full_range, 0.5)));
  listen(zoom_out, "click", () => applyRange(zoomChartRange(range, full_range, 2)));
  listen(reset, "click", () => applyRange(full_range));
  listen(hit, "pointermove", (event) => {
    const pointer = event, position = coordinates(pointer);
    if (!position) return;
    if (drag_start && pointer.pointerId !== drag_start.pointer_id) return;
    if (drag_start) {
      selection.setAttribute("display", "");
      selection.setAttribute("x", String(Math.min(drag_start.x, position.x)));
      selection.setAttribute("width", String(Math.abs(position.x - drag_start.x)));
    } else showAt(position.x, position.y);
  });
  listen(hit, "pointerdown", (event) => {
    const pointer = event, position = coordinates(pointer);
    if (!position || pointer.button !== 0 || pointer.isPrimary === false || drag_start) return;
    drag_start = { x: position.x, screen_x: pointer.clientX, pointer_id: pointer.pointerId };
    try {
      hit.setPointerCapture?.(pointer.pointerId);
    } catch {
    }
    region.focus();
    showAt(position.x, position.y);
  });
  listen(hit, "pointerup", (event) => {
    const pointer = event, position = coordinates(pointer), initial = drag_start;
    if (initial && pointer.pointerId !== initial.pointer_id) return;
    drag_start = null;
    selection.setAttribute("display", "none");
    if (initial && position && Math.abs(pointer.clientX - initial.screen_x) >= 6) applyRange(dragChartRange(range, (initial.x - LEFT) / (RIGHT - LEFT), (position.x - LEFT) / (RIGHT - LEFT)));
    else if (position) showAt(position.x, position.y);
    if (hit.hasPointerCapture?.(pointer.pointerId)) hit.releasePointerCapture(pointer.pointerId);
  });
  listen(hit, "pointercancel", (event) => {
    if (!drag_start || event.pointerId === drag_start.pointer_id) {
      drag_start = null;
      selection.setAttribute("display", "none");
    }
  });
  listen(hit, "lostpointercapture", (event) => {
    if (drag_start?.pointer_id === event.pointerId) {
      drag_start = null;
      selection.setAttribute("display", "none");
    }
  });
  listen(hit, "pointerleave", () => {
    if (!drag_start) crosshair.setAttribute("display", "none");
  });
  listen(region, "keydown", (event) => {
    const key = event;
    if (key.target !== region || key.altKey || key.ctrlKey || key.metaKey) return;
    if (["+", "=", "-", "_", "Escape", "Home", "ArrowLeft", "ArrowRight"].includes(key.key)) key.preventDefault();
    if (key.key === "+" || key.key === "=") applyRange(zoomChartRange(range, full_range, 0.5));
    else if (key.key === "-" || key.key === "_") applyRange(zoomChartRange(range, full_range, 2));
    else if (key.key === "Escape" || key.key === "Home") {
      drag_start = null;
      applyRange(full_range);
    } else if (key.key === "ArrowLeft" || key.key === "ArrowRight") {
      const item = [...series.values()].find((candidate) => available(candidate).length);
      const points = item && available(item);
      if (!points?.length) return;
      keyboard_index = Math.min(points.length - 1, Math.max(0, keyboard_index + (key.key === "ArrowRight" ? 1 : -1)));
      const point = points[keyboard_index];
      if (point) showAt(point.x, point.y, true, point);
    }
  });
  draw();
  return {
    isInteracting() {
      return drag_start !== null;
    },
    capture() {
      const active = document2.activeElement;
      let focus = active === region ? "plot" : active === zoom_in ? "zoom-in" : active === zoom_out ? "zoom-out" : active === reset ? "reset" : null;
      for (const item2 of series.values()) if (active === item2.button) focus = `run:${item2.run_id}`;
      const anchored = keyboard_anchor, item = anchored && series.get(anchored.run_index);
      const anchor = anchored && item ? {
        run_id: item.run_id,
        step: anchored.step,
        x_value: anchored.x_value,
        value_text: anchored.value_text,
        occurrence: item.points.filter((point) => point.step === anchored.step && point.x_value === anchored.x_value && point.value_text === anchored.value_text).indexOf(anchored)
      } : null;
      return {
        metric_name,
        x_axis: range.x_axis,
        range: follow_full ? null : { ...range },
        hidden_run_ids: [...series.values()].filter((item2) => !item2.visible).map((item2) => item2.run_id),
        keyboard_anchor: anchor,
        focus,
        scroll_left: region.scrollLeft,
        scroll_top: region.scrollTop
      };
    },
    restore(state) {
      range = restoreChartRange(state.range, full_range);
      follow_full = state.range === null || state.x_axis !== full_range.x_axis;
      for (const item2 of series.values()) item2.visible = !state.hidden_run_ids.includes(item2.run_id);
      draw();
      const anchor = state.x_axis === full_range.x_axis ? state.keyboard_anchor : null, item = anchor && [...series.values()].find((item2) => item2.run_id === anchor.run_id);
      const point = anchor && item && available(item).filter((point2) => point2.step === anchor.step && point2.x_value === anchor.x_value && point2.value_text === anchor.value_text)[anchor.occurrence];
      if (point) {
        showAt(point.x, point.y, false, point);
        keyboard_anchor = point;
      }
      const focus = state.focus;
      const focused = focus === "plot" ? region : focus === "zoom-in" ? zoom_in : focus === "zoom-out" ? zoom_out : focus === "reset" ? reset : [...series.values()].find((item2) => focus === `run:${item2.run_id}`)?.button;
      if (focused && !(focused.tagName === "BUTTON" && focused.disabled)) focused.focus({ preventScroll: true });
      else if (focus) region.focus({ preventScroll: true });
      region.scrollLeft = state.scroll_left;
      region.scrollTop = state.scroll_top;
    },
    dispose() {
      for (const remove of listeners) remove();
      for (const [item, children] of original_legends) item.replaceChildren(...children);
      for (const [node, attributes] of original_attributes) {
        for (const [name, value] of Object.entries(attributes)) value === null ? node.removeAttribute(name) : node.setAttribute(name, value);
      }
      svg.replaceChildren(...original_nodes);
      styles.remove();
      controls.remove();
      hint.remove();
      readout.remove();
      if (original_description === null) region.removeAttribute("aria-describedby");
      else region.setAttribute("aria-describedby", original_description);
      section.removeAttribute("data-interactive-chart");
    }
  };
}

// auto_refresh.ts
var AutoRefresh = class {
  constructor(options) {
    this.options = options;
    this.clock = options.clock ?? { now: () => Date.now(), set_timeout: (callback, delay) => setTimeout(callback, delay), clear_timeout: (timer) => clearTimeout(timer) };
    this.interval_ms = options.interval_ms ?? 5e3;
    this.retry_ms = this.interval_ms;
    this.previous_availability = options.availability();
  }
  options;
  clock;
  timer = null;
  enabled = true;
  disposed = false;
  running = false;
  generation = 0;
  interval_ms;
  retry_ms;
  failed = false;
  resume_immediately = false;
  previous_availability;
  start() {
    this.schedule(this.interval_ms);
  }
  setEnabled(enabled) {
    if (this.disposed || this.enabled === enabled) return;
    this.enabled = enabled;
    this.interrupt();
    if (enabled && this.running) this.resume_immediately = true;
    if (enabled && !this.running) this.schedule(0);
  }
  setInterval(delay_ms) {
    this.interval_ms = delay_ms;
    if (!this.failed) this.retry_ms = delay_ms;
  }
  interrupt() {
    this.generation++;
    this.clearTimer();
    this.options.cancel();
    if (!this.running) this.schedule(this.retry_ms);
    else this.publish();
  }
  availabilityChanged() {
    const availability = this.options.availability();
    const was_paused = this.previous_availability === "hidden" || this.previous_availability === "offline";
    this.previous_availability = availability;
    if (availability === "hidden" || availability === "offline") this.interrupt();
    else if (was_paused) {
      this.resume_immediately = true;
      this.clearTimer();
      if (!this.running) {
        this.resume_immediately = false;
        this.schedule(0);
      } else this.publish();
    } else this.publish();
  }
  refreshCompleted(success) {
    if (success) {
      this.failed = false;
      this.retry_ms = this.interval_ms;
    }
    this.clearTimer();
    if (!this.running) this.schedule(this.retry_ms);
    else this.publish();
  }
  dispose() {
    this.disposed = true;
    this.enabled = false;
    this.generation++;
    this.clearTimer();
    this.options.cancel();
  }
  clearTimer() {
    if (this.timer !== null) this.clock.clear_timeout(this.timer);
    this.timer = null;
  }
  publish() {
    this.options.on_state({ enabled: this.enabled, availability: this.options.availability(), running: this.running, interval_ms: this.interval_ms, retry_ms: this.retry_ms, failed: this.failed });
  }
  schedule(delay_ms) {
    this.clearTimer();
    if (this.disposed) return;
    const availability = this.options.availability();
    this.previous_availability = availability;
    if (this.enabled && !this.running && availability !== "hidden" && availability !== "offline") this.timer = this.clock.set_timeout(() => {
      this.timer = null;
      void this.tick();
    }, delay_ms);
    this.publish();
  }
  async tick() {
    if (this.disposed || !this.enabled || this.running) return;
    if (this.options.availability() !== "ready") {
      this.schedule(this.retry_ms);
      return;
    }
    this.running = true;
    const generation = this.generation;
    this.publish();
    let outcome;
    try {
      outcome = await this.options.run();
    } catch {
      outcome = "failure";
    }
    this.running = false;
    if (this.disposed) return;
    if (generation === this.generation) {
      if (outcome === "success") {
        this.failed = false;
        this.retry_ms = this.interval_ms;
      } else if (outcome === "failure") {
        this.failed = true;
        this.retry_ms = Math.min(6e4, Math.max(this.interval_ms, this.retry_ms * 2));
      }
    }
    const delay = this.resume_immediately ? 0 : this.retry_ms;
    this.resume_immediately = false;
    this.schedule(delay);
  }
};

// app.ts
function apiUrl(path, fields) {
  const query = new URLSearchParams();
  for (const [name, value] of Object.entries(fields)) {
    if (value === null || value === "") continue;
    for (const item of Array.isArray(value) ? value : [value]) query.append(name, String(item));
  }
  return `${path}?${query}`;
}
function formatValue(value) {
  if (value === void 0 || value === null) return "\u2014";
  return typeof value === "string" ? value : JSON.stringify(value);
}
function formatNumber(value) {
  return Number.isFinite(value) ? new Intl.NumberFormat(void 0, { maximumSignificantDigits: 6 }).format(value) : "\u2014";
}
function formatDuration(run) {
  if (!run.started_at || !run.finished_at) return "\u2014";
  const seconds = Math.max(0, Math.round((Date.parse(run.finished_at) - Date.parse(run.started_at)) / 1e3));
  if (!Number.isFinite(seconds)) return "\u2014";
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  return `${Math.floor(seconds / 3600)}h ${Math.floor(seconds % 3600 / 60)}m`;
}
var RequestError = class extends Error {
  constructor(message, status) {
    super(message);
    this.status = status;
  }
  status;
};
async function boundedText(response, maximum_bytes) {
  const declared = response.headers?.get("content-length");
  if (declared && Number(declared) > maximum_bytes) throw new Error("The chart exceeds the 2 MiB preview limit. Select fewer metrics.");
  if (!response.body?.getReader) {
    const value2 = await response.text();
    if (new TextEncoder().encode(value2).length > maximum_bytes) throw new Error("The chart exceeds the 2 MiB preview limit. Select fewer metrics.");
    return value2;
  }
  const reader = response.body.getReader(), decoder = new TextDecoder();
  let length = 0, value = "";
  try {
    for (; ; ) {
      const part = await reader.read();
      if (part.done) break;
      length += part.value.byteLength;
      if (length > maximum_bytes) {
        await reader.cancel();
        throw new Error("The chart exceeds the 2 MiB preview limit. Select fewer metrics.");
      }
      value += decoder.decode(part.value, { stream: true });
    }
    return value + decoder.decode();
  } finally {
    reader.releaseLock();
  }
}
var RequestLane = class {
  controller = null;
  generation = 0;
  get pending() {
    return this.controller !== null;
  }
  cancel() {
    this.controller?.abort();
    this.controller = null;
    this.generation++;
  }
  run(url) {
    return this.request(url, (response) => response.json());
  }
  runText(url, maximum_bytes = 2 * 1024 * 1024) {
    return this.request(url, (response) => boundedText(response, maximum_bytes));
  }
  async request(url, parse) {
    this.cancel();
    const generation = this.generation;
    const controller = new AbortController();
    this.controller = controller;
    const timeout = setTimeout(() => controller.abort(), 3e4);
    try {
      const response = await fetch(url, { signal: controller.signal, cache: "no-store" });
      if (generation !== this.generation) return void 0;
      if (response.status === 401) {
        if (typeof location !== "undefined") location.assign("/login");
        return void 0;
      }
      if (!response.ok) {
        let message = `Request failed (${response.status})`;
        try {
          const error = await response.json();
          message = error.error ?? message;
        } catch {
        }
        if (generation !== this.generation) return void 0;
        throw new RequestError(message, response.status);
      }
      const value = await parse(response);
      return generation === this.generation ? value : void 0;
    } catch (error) {
      if (generation !== this.generation) return void 0;
      if (controller.signal.aborted) throw new Error("The request timed out. Try Refresh when the connection is available.");
      throw error;
    } finally {
      clearTimeout(timeout);
      if (this.controller === controller) this.controller = null;
    }
  }
};
function element(tag, text = "", class_name = "") {
  const node = document.createElement(tag);
  node.textContent = text;
  node.className = class_name;
  return node;
}
function required(id) {
  const node = document.getElementById(id);
  if (!node) throw new Error(`Missing dashboard element: ${id}`);
  return node;
}
function dateText(value) {
  if (!value) return "\u2014";
  const date = new Date(value);
  return Number.isNaN(date.valueOf()) ? value : date.toLocaleString();
}
function errorText(error) {
  return error instanceof Error ? error.message : String(error);
}
function showError(node, error) {
  node.textContent = errorText(error);
  node.hidden = false;
}
function warnings(node, items) {
  node.replaceChildren();
  node.hidden = items.length === 0;
  if (!items.length) return;
  const notice = element("div", "", "notice");
  const list = element("ul");
  for (const item of items.slice(0, 100)) list.append(element("li", `${item.run_id ? `${item.run_id}: ` : ""}${item.message}`));
  notice.append(list);
  node.append(notice);
}
function facts(values) {
  const list = element("dl", "", "facts");
  for (const [label, value] of values) list.append(element("dt", label), element("dd", value));
  return list;
}
function records(title, value) {
  const details = element("details", "", "detail-details");
  details.append(element("summary", title), element("pre", JSON.stringify(value, null, 2) ?? "Not recorded"));
  return details;
}
function card(title) {
  const section = element("section", "", "card");
  section.append(element("h3", title));
  return section;
}
function jsonObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? value : {};
}
function startDashboard(options = {}) {
  const page_document = document;
  const page_window = typeof window === "undefined" ? null : window;
  const source_select = required("source-select");
  const chart_controller = options.chart_controller ?? createChartController(required("chart-frame"));
  const refresh_button = required("refresh-button");
  const search_input = required("search-input");
  const task_input = required("task-input");
  const status_select = required("status-select");
  const x_axis_select = required("x-axis-select");
  x_axis_select.value = "step";
  let x_axis = "step";
  const previous_page = required("previous-page");
  const next_page = required("next-page");
  const global_error = required("global-error");
  const rows = required("run-rows");
  const catalog_lane = new RequestLane();
  const list_lane = new RequestLane();
  const review_lane = new RequestLane();
  const log_lane = new RequestLane();
  const compare_lane = new RequestLane();
  const chart_lane = new RequestLane();
  const quiet_lanes = { updates: new RequestLane(), catalog: new RequestLane(), list: new RequestLane(), detail: new RequestLane(), comparison: new RequestLane(), log: new RequestLane(), chart: new RequestLane() };
  const foreground_lanes = [catalog_lane, list_lane, review_lane, log_lane, compare_lane, chart_lane];
  const now = options.refresh_clock?.now ?? (() => Date.now());
  const auto_toggle = required("auto-refresh-toggle");
  auto_toggle.checked = true;
  let auto_refresh = null;
  let quiet_generation = 0;
  let updates_supported = true;
  let catalog_revision;
  const source_revisions = /* @__PURE__ */ new Map();
  const detail_revisions = /* @__PURE__ */ new Map();
  const chart_revisions = /* @__PURE__ */ new Map();
  const log_revisions = /* @__PURE__ */ new Map();
  let comparison_revision = null;
  let last_catalog = null;
  let last_run_list = null;
  let last_run_list_context = "";
  let list_ready = false;
  let last_catalog_snapshot = -Infinity;
  let last_list_snapshot = -Infinity;
  let last_full_snapshot = -Infinity;
  let last_checked = null;
  let cached_chart = null;
  let chart_error_context = null;
  let sources = [];
  let source_id = "local";
  let access_mode = "local";
  let page_size = 100;
  let runs = [];
  let offset = 0;
  let next_offset = null;
  let review = null;
  let missing_review = null;
  let metric_names = [];
  let search_timeout;
  let selection_timeout;
  let refresh_generation = 0;
  let log_view = null;
  let rendered_detail = null;
  let rendered_comparison = null;
  const selected = /* @__PURE__ */ new Set();
  const detail_cache = /* @__PURE__ */ new Map();
  const row_checks = /* @__PURE__ */ new Map();
  const row_buttons = /* @__PURE__ */ new Map();
  const review_tabs = ["charts", "overview", "logs"];
  function abortQuietRequests() {
    quiet_generation++;
    for (const lane of Object.values(quiet_lanes)) lane.cancel();
  }
  function cancelQuietRefresh() {
    if (auto_refresh) auto_refresh.interrupt();
    else abortQuietRequests();
  }
  function availability() {
    if (document.visibilityState === "hidden") return "hidden";
    if (typeof navigator !== "undefined" && navigator.onLine === false) return "offline";
    return foreground_lanes.some((lane) => lane.pending) || search_timeout !== void 0 || selection_timeout !== void 0 ? "busy" : "ready";
  }
  function showFreshness(state) {
    const checked = last_checked === null ? "Waiting for updates" : `Last checked at ${new Date(last_checked).toLocaleTimeString()}`;
    const activity = !state.enabled ? "Auto updates off" : state.availability === "hidden" ? "Auto updates paused while hidden" : state.availability === "offline" ? "Offline \xB7 updates resume when connected" : state.failed ? `Updates unavailable \xB7 retrying in ${state.retry_ms / 1e3}s` : `Auto updates every ${state.interval_ms / 1e3}s`;
    required("updated-at").textContent = `${checked} \xB7 ${activity}`;
  }
  function listUrl() {
    return apiUrl("/api/runs", { source: source_id, search: search_input.value.trim(), task: task_input.value.trim(), status: status_select.value, limit: page_size, offset });
  }
  function chartUrl() {
    return apiUrl("/api/chart", { source: source_id, run_id: review?.run_ids ?? [], metric: review?.metric_names ?? [], x_axis });
  }
  function comparisonUrl() {
    return apiUrl("/api/compare", { source: source_id, run_id: review?.run_ids ?? [], metric: review?.metric_names ?? [], reduction: required("reduction-select").value });
  }
  function boundedSet(cache, key, value) {
    cache.delete(key);
    cache.set(key, value);
    while (cache.size > 16) {
      const oldest = cache.keys().next().value;
      if (oldest === void 0) break;
      cache.delete(oldest);
    }
  }
  function clearSearchTimeout() {
    clearTimeout(search_timeout);
    search_timeout = void 0;
  }
  function clearSelectionTimeout() {
    clearTimeout(selection_timeout);
    selection_timeout = void 0;
  }
  function announce(message) {
    required("live-status").textContent = message;
  }
  function showEmptyRuns(filtered, source) {
    const synced = access_mode === "hosted" || source?.kind === "service";
    const empty = required("list-empty");
    empty.hidden = false;
    const title = synced && !filtered ? "No synced runs yet" : "No runs found";
    const message = filtered ? "Try changing the filters." : synced ? "Sync results from a worker to see them here." : source?.kind === "cached" ? "Pull results with expri runs pull, then Refresh." : "Start an experiment with expri run to record results here.";
    empty.replaceChildren(element("span", "\u25CC", "empty-mark"), element("h3", title), element("p", message));
    if (synced && !filtered) {
      const paragraph = element("p", "", "setup-guide");
      const link = element("a", "Set up result syncing");
      link.href = "https://github.com/agentic-rs/expri/blob/main/docs/self-hosted-service.md";
      link.target = "_blank";
      link.rel = "noopener noreferrer";
      paragraph.append(link);
      empty.append(paragraph);
    }
  }
  function syncSelection() {
    required("selection-count").textContent = selected.size ? `${selected.size} of 8 runs selected` : "Select runs to compare";
    const chips = required("selected-runs");
    chips.replaceChildren();
    for (const id of selected) {
      const button = element("button", `${id} \xD7`, "selection-chip");
      button.type = "button";
      button.setAttribute("aria-label", `Remove ${id} from comparison`);
      button.addEventListener("click", () => {
        selected.delete(id);
        selectionChanged();
      });
      chips.append(button);
    }
    required("compare-button").disabled = selected.size < 2;
    required("clear-selection").hidden = selected.size === 0;
  }
  function renderRows() {
    const focused_check = [...row_checks].find(([, node]) => node === document.activeElement)?.[0];
    const focused_button = [...row_buttons].find(([, node]) => node === document.activeElement)?.[0];
    rows.replaceChildren();
    row_checks.clear();
    row_buttons.clear();
    for (const run of runs) {
      const row = element("tr");
      row.classList.toggle("selected", selected.has(run.run_id));
      row.classList.toggle("active", review?.kind === "run" && review.run_ids[0] === run.run_id);
      const check_cell = element("td", "", "selection-column");
      const checkbox = element("input");
      checkbox.type = "checkbox";
      checkbox.checked = selected.has(run.run_id);
      checkbox.disabled = selected.size >= 8 && !checkbox.checked;
      checkbox.setAttribute("aria-label", `Select ${run.run_id} for comparison`);
      checkbox.addEventListener("change", () => {
        checkbox.checked ? selected.add(run.run_id) : selected.delete(run.run_id);
        selectionChanged();
      });
      row_checks.set(run.run_id, checkbox);
      check_cell.append(checkbox);
      const run_cell = element("td", "", "run-cell");
      const button = element("button", run.run_id, "run-link");
      button.type = "button";
      row_buttons.set(run.run_id, button);
      button.addEventListener("click", () => {
        cancelRefresh();
        clearSelectionTimeout();
        void openRun(run.run_id);
      });
      const metadata = [dateText(run.started_at), formatDuration(run)].filter((value) => value !== "\u2014");
      if (run.exit_code !== null && run.exit_code !== 0) metadata.push(`exit ${run.exit_code}`);
      run_cell.append(button, element("div", run.task ?? "No task recorded", "run-task"), element("div", metadata.join(" \xB7 "), "run-meta"));
      const status_cell = element("td");
      const badge = element("span", run.status, "status");
      if (["preparing", "running", "completed", "failed", "cancelled", "lost", "unknown"].includes(run.status)) badge.classList.add(run.status);
      status_cell.append(badge);
      row.append(check_cell, run_cell, status_cell);
      rows.append(row);
    }
    if (focused_check) row_checks.get(focused_check)?.focus({ preventScroll: true });
    else if (focused_button) row_buttons.get(focused_button)?.focus({ preventScroll: true });
  }
  function applyRunList(result, context, quiet = false) {
    const unchanged = last_run_list_context === context && JSON.stringify(last_run_list) === JSON.stringify(result);
    const was_ready = list_ready;
    list_ready = true;
    last_run_list = result;
    last_run_list_context = context;
    last_list_snapshot = now();
    if (quiet && unchanged && was_ready) return;
    runs = result.runs;
    offset = result.offset;
    next_offset = result.next_offset;
    if (!quiet || !unchanged) {
      renderRows();
      warnings(required("list-warnings"), result.warnings);
    }
    required("run-count").textContent = result.total_count ? `${offset + 1}\u2013${offset + runs.length} of ${result.total_count.toLocaleString()} runs` : "No matching runs";
    required("page-label").textContent = `Page ${Math.floor(offset / page_size) + 1}`;
    previous_page.disabled = offset === 0;
    next_page.disabled = next_offset === null;
    const empty = required("list-empty");
    empty.hidden = runs.length > 0;
    if (!runs.length) {
      showEmptyRuns(Boolean(search_input.value || task_input.value || status_select.value), result.source);
    }
    const options2 = required("task-options");
    options2.replaceChildren();
    for (const task of [...new Set(runs.map((run) => run.task).filter((task2) => task2 !== null))].sort()) {
      const option = element("option");
      option.value = task;
      options2.append(option);
    }
    required("runs-region").setAttribute("aria-busy", "false");
    announce(`${result.total_count} matching runs`);
  }
  async function loadRuns() {
    if (!source_id) return false;
    list_ready = false;
    global_error.hidden = true;
    required("runs-region").setAttribute("aria-busy", "true");
    for (const control of rows.querySelectorAll("input, button")) control.disabled = true;
    previous_page.disabled = true;
    next_page.disabled = true;
    required("run-count").textContent = "Loading run history\u2026";
    try {
      const url = listUrl();
      const result = await list_lane.run(url);
      if (!result) return false;
      applyRunList(result, url);
      return true;
    } catch (error) {
      showError(global_error, error);
      renderRows();
      required("runs-region").setAttribute("aria-busy", "false");
      required("run-count").textContent = "Could not read run history";
      return false;
    }
  }
  function cancelRefresh() {
    cancelQuietRefresh();
    refresh_generation++;
    catalog_lane.cancel();
    refresh_button.disabled = false;
  }
  function hideReview() {
    clearSelectionTimeout();
    review_lane.cancel();
    log_lane.cancel();
    compare_lane.cancel();
    chart_lane.cancel();
    review = null;
    missing_review = null;
    log_view = null;
    clearChartError();
    required("review-section").hidden = true;
    required("review-empty").hidden = false;
    required("chart-frame").removeAttribute("src");
    renderRows();
  }
  function closeReview() {
    cancelRefresh();
    showSelection(false);
  }
  function selectionChanged() {
    cancelRefresh();
    syncSelection();
    showSelection(true);
  }
  function showSelection(debounce) {
    clearSelectionTimeout();
    const ids = [...selected];
    if (!ids.length) {
      hideReview();
      announce("Selection cleared");
      return;
    }
    beginReview(ids.length === 1 ? "run" : "compare", ids, "selection");
    const pending_review = review;
    metric_names = [...new Set(ids.flatMap((id) => Object.keys(detail_cache.get(id)?.metrics ?? {})))].sort();
    const open = () => {
      selection_timeout = void 0;
      if (review !== pending_review) return;
      if (ids.length === 1 && ids[0]) void loadRun(ids[0]);
      else void loadComparison();
    };
    if (debounce) selection_timeout = setTimeout(open, 180);
    else open();
  }
  function beginReview(kind, ids, origin = "inspection") {
    clearSelectionTimeout();
    review_lane.cancel();
    log_lane.cancel();
    compare_lane.cancel();
    chart_lane.cancel();
    detail_revisions.clear();
    chart_revisions.clear();
    log_revisions.clear();
    comparison_revision = null;
    missing_review = null;
    clearChartError();
    review = { kind, origin, run_ids: ids, metric_names: [], metric_selection_set: false, tab: "charts", log_stream: "stdout" };
    metric_names = [];
    log_view = null;
    rendered_detail = null;
    rendered_comparison = null;
    cached_chart = null;
    required("review-section").hidden = false;
    required("review-empty").hidden = true;
    required("review-title").textContent = kind === "run" ? ids[0] ?? "Run" : `${ids.length} runs`;
    required("review-eyebrow").textContent = kind === "run" ? "Run details" : "Experiment comparison";
    required("review-loading").hidden = false;
    required("review-error").hidden = true;
    required("run-detail").hidden = true;
    required("compare-detail").hidden = true;
    required("run-metric-controls").hidden = true;
    required("run-logs").replaceChildren();
    required("chart-card").hidden = true;
    required("chart-frame").removeAttribute("src");
    required("chart-card").setAttribute("aria-busy", "false");
    const close = required("close-review");
    close.hidden = origin === "selection";
    close.textContent = selected.size ? "Back to selection" : "Close review";
    warnings(required("review-warnings"), []);
    syncReviewTabs();
    renderRows();
  }
  function syncReviewTabs() {
    for (const tab of review_tabs) {
      const button = required(`review-tab-${tab}`);
      const active = review?.tab === tab;
      button.disabled = review?.kind === "compare" && tab !== "charts";
      button.setAttribute("aria-selected", String(active));
      button.tabIndex = active ? 0 : -1;
      required(`review-panel-${tab}`).hidden = !active;
    }
  }
  function selectReviewTab(tab) {
    if (!review || review.kind === "compare" && tab !== "charts") return;
    cancelQuietRefresh();
    review.tab = tab;
    syncReviewTabs();
    if (tab === "logs") ensureLog();
    else {
      log_lane.cancel();
      if (log_view) log_view.pending = false;
    }
  }
  function updateChart() {
    if (!review) return;
    const url = chartUrl();
    const frame = required("chart-frame");
    if ((cached_chart?.context ?? frame.getAttribute("src")) !== url) {
      chart_lane.cancel();
      frame.src = url;
      chart_revisions.delete(url);
      cached_chart = null;
      clearChartError();
      required("chart-card").setAttribute("aria-busy", "false");
    }
    chartControls(url);
  }
  function chartControls(url) {
    if (!review) return;
    required("open-chart").href = url;
    required("chart-card").hidden = false;
    const metrics = review.metric_names.length ? `${review.metric_names.length} selected metrics` : "First four recorded metrics";
    const axis = x_axis === "elapsed" ? "Time since each run\u2019s first timestamped metric event" : x_axis === "wall_clock" ? "Recorded time in UTC" : "Global step";
    required("chart-note").textContent = `${metrics} \xB7 ${axis} \xB7 at most 600 chart points per series`;
  }
  function clearChartError(context) {
    if (context !== void 0 && chart_error_context !== context) return;
    chart_error_context = null;
    required("chart-error").hidden = true;
  }
  async function changeChartAxis() {
    const value = x_axis_select.value;
    if (value !== "step" && value !== "elapsed" && value !== "wall_clock") {
      x_axis_select.value = x_axis;
      return;
    }
    if (value === x_axis) return;
    cancelRefresh();
    chart_lane.cancel();
    clearChartError();
    x_axis = value;
    if (!review || required("chart-card").hidden) return;
    const current_review = review, source = source_id, url = chartUrl();
    chartControls(url);
    if (chart_controller.previewStatus() !== "ready" || chart_controller.isInteracting()) {
      updateChart();
      return;
    }
    required("chart-card").setAttribute("aria-busy", "true");
    try {
      const html2 = await chart_lane.runText(url);
      if (html2 === void 0 || review !== current_review || source_id !== source || chartUrl() !== url) return;
      if (chart_controller.replacePreview(html2)) {
        cached_chart = { context: url, html: html2 };
        chart_revisions.delete(url);
        clearChartError(url);
      } else updateChart();
    } catch (error) {
      if (review === current_review && source_id === source && chartUrl() === url) {
        chart_error_context = url;
        showError(required("chart-error"), new Error(`Could not load the selected axis. Showing the last loaded chart. ${errorText(error)}`));
      }
    } finally {
      if (review === current_review && source_id === source && chartUrl() === url) required("chart-card").setAttribute("aria-busy", "false");
    }
  }
  function metricPicker(parent, known, on_change) {
    parent.replaceChildren();
    const choices = element("div", "", "metric-options");
    const chosen = review?.metric_names ?? [];
    for (const name of [.../* @__PURE__ */ new Set([...known, ...chosen])].sort()) {
      const label2 = element("label", "", "metric-choice");
      const input2 = element("input");
      input2.type = "checkbox";
      input2.checked = chosen.includes(name);
      input2.disabled = chosen.length >= 6 && !input2.checked;
      input2.addEventListener("change", () => {
        if (!review) return;
        cancelRefresh();
        review.metric_selection_set = true;
        review.metric_names = input2.checked ? [...review.metric_names, name] : review.metric_names.filter((item) => item !== name);
        metricPicker(parent, known, on_change);
        on_change();
      });
      label2.append(input2, document.createTextNode(name));
      choices.append(label2);
    }
    const form = element("form", "", "metric-add");
    const label = element("label", "Exact metric name", "field");
    const input = element("input");
    input.type = "text";
    input.placeholder = "e.g. validation/loss";
    input.maxLength = 1024;
    label.append(input);
    const button = element("button", "Add metric", "button secondary");
    button.type = "submit";
    button.disabled = chosen.length >= 6;
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const name = input.value.trim();
      if (!review || !name || review.metric_names.length >= 6) return;
      cancelRefresh();
      review.metric_selection_set = true;
      if (!review.metric_names.includes(name)) review.metric_names.push(name);
      metric_names = [.../* @__PURE__ */ new Set([...metric_names, name])];
      metricPicker(parent, metric_names, on_change);
      on_change();
    });
    form.append(label, button);
    parent.append(choices, form);
    parent.append(element("p", chosen.length ? "Select up to six metrics. Enter a name to include a metric outside this preview." : "No selection uses the first four recorded metric names.", "muted metric-hint"));
  }
  async function loadLog(view) {
    const { run_id: id, stream, output, note } = view;
    output.textContent = "Loading log tail\u2026";
    note.textContent = "";
    view.pending = true;
    try {
      const log = await log_lane.run(apiUrl("/api/log", { source: source_id, run_id: id, stream, tail: 100 }));
      if (!log || log_view !== view) return;
      output.textContent = log.missing ? "This log has not been recorded or pulled." : log.content || "The log is empty.";
      note.textContent = log.truncated ? "Showing the last 100 lines, capped at 64 KiB. Use expri runs logs for more output." : "Last 100 lines. Refresh to read updated output.";
      view.loaded = true;
    } catch (error) {
      if (log_view === view) {
        output.textContent = errorText(error);
        note.textContent = "Could not read this log.";
      }
    } finally {
      if (log_view === view) view.pending = false;
    }
  }
  function ensureLog() {
    if (review?.kind === "run" && review !== missing_review && review.tab === "logs" && log_view && !log_view.loaded && !log_view.pending) void loadLog(log_view);
  }
  function containsFocus(parent) {
    return parent.contains(document.activeElement);
  }
  function renderOverview(detail) {
    const parent = required("run-detail");
    const expanded = new Set([...parent.querySelectorAll("details")].filter((node) => node.open).map((node) => node.querySelectorAll("summary")[0]?.textContent));
    const focused = [...parent.querySelectorAll("summary")].find((node) => node === document.activeElement)?.textContent;
    parent.replaceChildren();
    parent.hidden = false;
    const grid = element("div", "", "detail-grid");
    const overview = card("Overview");
    overview.append(facts([["Task", detail.run.task ?? "\u2014"], ["Status", detail.run.status], ["Started", dateText(detail.run.started_at)], ["Finished", dateText(detail.run.finished_at)], ["Duration", formatDuration(detail.run)], ["Exit code", formatValue(detail.run.exit_code)]]));
    const command = jsonObject(detail.state)["command"];
    if (command !== void 0 && command !== null) {
      overview.append(element("h4", "Command"), element("pre", formatValue(command), "command"));
    }
    for (const [title, value] of [["Run record", detail.state], ["Source provenance", detail.snapshot], ["Environment", detail.environment], ["Cached results", detail.cache]]) {
      if (value !== null) overview.append(records(title, value));
    }
    const params = card(detail.params_truncated ? "Parameters \xB7 preview" : "Parameters");
    const values = Object.entries(jsonObject(detail.params));
    if (values.length) {
      const table = element("table", "", "params-table");
      const body = element("tbody");
      for (const [name, value] of values) {
        const row = element("tr");
        const heading = element("th", name);
        heading.scope = "row";
        const cell = element("td");
        cell.append(element("code", formatValue(value)));
        row.append(heading, cell);
        body.append(row);
      }
      table.append(body);
      params.append(table);
    } else params.append(element("p", "No parameters were recorded or pulled.", "muted"));
    grid.append(overview, params);
    parent.append(grid);
    const metrics = card("Metric summaries");
    if (detail.metrics_error) {
      const error = element("p", detail.metrics_error, "notice error");
      metrics.append(error);
    }
    const names = Object.keys(detail.metrics).sort();
    if (names.length) {
      const scroll = element("div", "", "table-scroll");
      const table = element("table");
      const head = element("thead");
      const headings = element("tr");
      for (const name of ["Metric", "Points", "Last", "Minimum", "Maximum"]) headings.append(element("th", name));
      head.append(headings);
      table.append(head);
      const body = element("tbody");
      for (const name of names) {
        const metric = detail.metrics[name];
        if (!metric) continue;
        const row = element("tr");
        row.append(element("td", name), element("td", String(metric.count), "number"), element("td", formatNumber(metric.last.value), "number"), element("td", formatNumber(metric.min.value), "number"), element("td", formatNumber(metric.max.value), "number"));
        body.append(row);
      }
      table.append(body);
      scroll.append(table);
      metrics.append(scroll);
    } else metrics.append(element("p", "No metric summaries are available. Record metrics in outputs/metrics.jsonl or pull remote metrics.", "muted"));
    if (detail.metrics_truncated) metrics.append(element("p", `Showing ${names.length} of ${detail.metric_count} summaries. Use Chart metrics to select another series by its exact name.`, "muted"));
    parent.append(metrics);
    for (const node of parent.querySelectorAll("details")) {
      const summary = node.querySelectorAll("summary")[0];
      node.open = expanded.has(summary?.textContent);
      if (focused && summary?.textContent === focused) summary.focus({ preventScroll: true });
    }
  }
  function renderDetail(detail, preserve = false) {
    const names = Object.keys(detail.metrics).sort(), picker = required("run-metric-options");
    const choices_changed = JSON.stringify(metric_names) !== JSON.stringify(names);
    if (preserve && choices_changed && containsFocus(picker)) return false;
    if (!preserve || JSON.stringify(rendered_detail) !== JSON.stringify(detail)) renderOverview(detail);
    rendered_detail = detail;
    metric_names = names;
    if (review && !review.metric_selection_set) {
      review.metric_names = names.slice(0, 4);
      review.metric_selection_set = true;
    }
    if (!preserve || choices_changed) metricPicker(picker, names, updateChart);
    required("run-metric-controls").hidden = false;
    if (preserve && log_view?.run_id === detail.run.run_id) return true;
    const logs = card("Log tail");
    const tabs = element("div", "", "log-tabs");
    tabs.setAttribute("role", "tablist");
    tabs.setAttribute("aria-label", "Log stream");
    const output = element("pre", "", "log-content");
    output.id = "log-output";
    output.setAttribute("role", "tabpanel");
    const note = element("p", "", "log-note muted");
    const buttons = [];
    const selected_stream = review?.log_stream ?? "stdout";
    for (const stream of ["stdout", "stderr"]) {
      const button = element("button", stream, "log-tab");
      button.type = "button";
      button.id = `log-tab-${stream}`;
      button.setAttribute("role", "tab");
      button.setAttribute("aria-controls", output.id);
      button.setAttribute("aria-selected", String(stream === selected_stream));
      button.tabIndex = stream === selected_stream ? 0 : -1;
      button.addEventListener("click", () => {
        cancelQuietRefresh();
        log_lane.cancel();
        if (review) review.log_stream = stream;
        for (const item of buttons) {
          item.setAttribute("aria-selected", String(item === button));
          item.tabIndex = item === button ? 0 : -1;
        }
        output.setAttribute("aria-labelledby", button.id);
        log_view = { run_id: detail.run.run_id, stream, output, note, loaded: false, pending: false };
        ensureLog();
      });
      button.addEventListener("keydown", (event) => {
        if (["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) {
          event.preventDefault();
          const next = event.key === "Home" ? buttons[0] : event.key === "End" ? buttons.at(-1) : buttons.find((item) => item !== button);
          next?.focus();
          next?.click();
        }
      });
      buttons.push(button);
      tabs.append(button);
    }
    output.setAttribute("aria-labelledby", `log-tab-${selected_stream}`);
    logs.append(tabs, output, note);
    required("run-logs").replaceChildren(logs);
    log_view = { run_id: detail.run.run_id, stream: selected_stream, output, note, loaded: false, pending: false };
    ensureLog();
    if (!preserve || !required("chart-frame").src) updateChart();
    return true;
  }
  async function openRun(id, origin = "inspection", previous) {
    beginReview("run", [id], origin);
    if (previous && review) {
      review.metric_names = [...previous.metric_names];
      review.metric_selection_set = previous.metric_selection_set;
      review.tab = previous.tab;
      review.log_stream = previous.log_stream;
      syncReviewTabs();
    }
    await loadRun(id);
  }
  async function loadRun(id, preserve = false) {
    try {
      const detail = await review_lane.run(apiUrl("/api/run", { source: source_id, run_id: id }));
      if (!detail) return false;
      detail_cache.delete(id);
      detail_cache.set(id, detail);
      while (detail_cache.size > 9) {
        const oldest = detail_cache.keys().next().value;
        if (oldest === void 0) break;
        detail_cache.delete(oldest);
      }
      required("review-loading").hidden = true;
      warnings(required("review-warnings"), detail.warnings);
      if (!renderDetail(detail, preserve)) return false;
      if (!preserve) announce(`Opened ${id}`);
      return true;
    } catch (error) {
      required("review-loading").hidden = true;
      showError(required("review-error"), error);
      return false;
    }
  }
  function renderComparison(result) {
    if (JSON.stringify(rendered_comparison) === JSON.stringify(result)) return;
    rendered_comparison = result;
    const comparison = result.comparison;
    const parent = required("comparison-values");
    parent.replaceChildren();
    const table = element("table", "", "comparison-table");
    const head = element("thead");
    const headings = element("tr");
    headings.append(element("th", "Run"));
    for (const name of comparison.metric_names) headings.append(element("th", name));
    head.append(headings);
    table.append(head);
    const body = element("tbody");
    for (const run of comparison.runs) {
      const row = element("tr");
      const label = element("th", run.run_id);
      label.scope = "row";
      row.append(label);
      for (const name of comparison.metric_names) {
        const point = run.values[name];
        const cell = element("td", point ? formatNumber(point.value) : "\u2014", "number");
        if (point) cell.append(element("span", `step ${point.step}`, "cell-step"));
        row.append(cell);
      }
      body.append(row);
    }
    table.append(body);
    parent.append(table);
    if (!comparison.metric_names.length) parent.append(element("p", "No recorded metrics match this selection.", "muted"));
    warnings(required("review-warnings"), comparison.warnings);
  }
  async function loadComparison(preserve = false) {
    if (!review || review.kind !== "compare") return false;
    required("review-error").hidden = true;
    required("comparison-values").setAttribute("aria-busy", "true");
    if (!rendered_comparison) required("chart-card").hidden = true;
    try {
      const result = await compare_lane.run(comparisonUrl());
      if (!result || !review || review.kind !== "compare") return false;
      const names = [.../* @__PURE__ */ new Set([...metric_names, ...result.comparison.metric_names])].sort();
      const choices_changed = JSON.stringify(names) !== JSON.stringify(metric_names), initial = !rendered_comparison;
      metric_names = names;
      if (!review.metric_selection_set) {
        review.metric_names = result.comparison.metric_names.slice(0, 4);
        review.metric_selection_set = true;
      }
      required("review-loading").hidden = true;
      required("compare-detail").hidden = false;
      renderComparison(result);
      required("comparison-values").setAttribute("aria-busy", "false");
      if (!preserve || initial || choices_changed) metricPicker(required("compare-metric-options"), metric_names, () => {
        void loadComparison();
      });
      if (!preserve || !required("chart-frame").src) updateChart();
      return true;
    } catch (error) {
      required("review-loading").hidden = true;
      required("comparison-values").setAttribute("aria-busy", "false");
      showError(required("review-error"), error);
      return false;
    }
  }
  function openComparison() {
    const ids = [...selected];
    if (ids.length < 2 || ids.length > 8) return;
    beginReview("compare", ids, "selection");
    metric_names = [...new Set(ids.flatMap((id) => Object.keys(detail_cache.get(id)?.metrics ?? {})))].sort();
    void loadComparison();
  }
  function sourceNote() {
    const source = sources.find((item) => item.source_id === source_id);
    const note = required("source-note");
    note.hidden = !source;
    note.textContent = source?.kind === "service" ? "Synced results \xB7 Updates arrive when workers sync their recorded files." : source?.kind === "cached" ? "Cached remote results \xB7 Pull updated results with expri runs pull; this dashboard watches the local cache." : "Local results \xB7 Status and updates come from recorded run files.";
  }
  function applyCatalog(catalog, quiet = false) {
    last_catalog_snapshot = now();
    if (quiet && JSON.stringify(last_catalog) === JSON.stringify(catalog)) return;
    last_catalog = catalog;
    sources = catalog.sources;
    access_mode = catalog.access_mode ?? "local";
    page_size = access_mode === "hosted" ? 20 : 100;
    required("logout-form").hidden = access_mode !== "hosted";
    required("dashboard-kind").textContent = access_mode === "hosted" ? "expri \xB7 Synced experiment review" : "expri \xB7 Local experiment review";
    const previous_source = source_id;
    if (!sources.some((source) => source.source_id === source_id) || !source_select.dataset.initialized) {
      source_id = sources.some((source) => source.source_id === catalog.initial_source) ? catalog.initial_source : sources[0]?.source_id ?? "";
    }
    source_select.dataset.initialized = "true";
    if (source_id !== previous_source) {
      offset = 0;
      runs = [];
      selected.clear();
      detail_cache.clear();
      hideReview();
      syncSelection();
    }
    source_select.replaceChildren();
    for (const source of sources) {
      const option = element("option", source.kind === "service" ? `${source.label} \xB7 Synced` : source.label);
      option.value = source.source_id;
      source_select.append(option);
    }
    if (!sources.length) {
      const option = element("option", "No synced sources");
      option.value = "";
      source_select.append(option);
    }
    source_select.value = source_id;
    source_select.disabled = !sources.length;
    for (const control of [search_input, task_input, status_select, required("clear-filters")]) control.disabled = !sources.length;
    required("project-name").textContent = catalog.project_name;
    document.title = `expri \xB7 ${catalog.project_name}`;
    warnings(required("catalog-warnings"), catalog.warnings);
    sourceNote();
  }
  function emptyCatalog() {
    list_lane.cancel();
    runs = [];
    next_offset = null;
    renderRows();
    previous_page.disabled = true;
    next_page.disabled = true;
    required("run-count").textContent = "No synced runs yet";
    required("page-label").textContent = "Page 1";
    required("task-options").replaceChildren();
    warnings(required("list-warnings"), []);
    required("runs-region").setAttribute("aria-busy", "false");
    required("review-empty").hidden = true;
    showEmptyRuns(false);
  }
  async function refreshCurrentChart(lane) {
    if (!review || review.tab !== "charts") return "applied";
    if (chart_controller.isInteracting()) return "deferred";
    const current_review = review, source = source_id, url = chartUrl(), status = chart_controller.previewStatus();
    if (status === "loading") return "deferred";
    if (status === "invalid") {
      required("chart-frame").src = url;
      chart_revisions.delete(url);
      cached_chart = null;
      throw new Error("The chart preview could not load. Retrying when the connection is available.");
    }
    const html2 = await lane.runText(url);
    if (html2 === void 0 || review !== current_review || source_id !== source || chartUrl() !== url) return "cancelled";
    if (cached_chart?.context === url && cached_chart.html === html2) {
      clearChartError(url);
      return "applied";
    }
    if (!chart_controller.replacePreview(html2)) return "deferred";
    cached_chart = { context: url, html: html2 };
    clearChartError(url);
    return "applied";
  }
  function applyLog(view, log) {
    const content = log.missing ? "This log has not been recorded or pulled." : log.content || "The log is empty.";
    const output = view.output;
    if (output.textContent !== content) {
      const scroll_top = output.scrollTop, at_bottom = output.scrollHeight - output.clientHeight - scroll_top <= 4;
      output.textContent = content;
      output.scrollTop = at_bottom ? output.scrollHeight : scroll_top;
    }
    view.note.textContent = log.truncated ? "Showing the last 100 lines, capped at 64 KiB. Use expri runs logs for more output." : "Last 100 lines \xB7 updates arrive automatically while auto updates are on.";
    view.loaded = true;
  }
  async function refreshCurrentLog(lane) {
    const view = log_view;
    if (review?.tab !== "logs" || !view) return true;
    const log = await lane.run(apiUrl("/api/log", { source: source_id, run_id: view.run_id, stream: view.stream, tail: 100 }));
    if (!log || log_view !== view || review?.tab !== "logs") return false;
    applyLog(view, log);
    return true;
  }
  async function refresh() {
    cancelQuietRefresh();
    const generation = ++refresh_generation;
    refresh_button.disabled = true;
    global_error.hidden = true;
    let successful = false;
    try {
      const catalog = await catalog_lane.run("/api/catalog");
      if (!catalog || generation !== refresh_generation) return;
      applyCatalog(catalog);
      if (!source_id) {
        emptyCatalog();
        successful = true;
      } else {
        const current_review = review, current_source = source_id;
        if (!await loadRuns() || generation !== refresh_generation) return;
        if (current_review === review && current_source === source_id && current_review) {
          if (current_review.kind === "run") {
            const id = current_review.run_ids[0];
            if (id && !await loadRun(id, true)) return;
          } else if (!await loadComparison(true)) return;
          if (generation !== refresh_generation || current_review !== review) return;
          if (!await refreshCurrentLog(log_lane) || await refreshCurrentChart(chart_lane) !== "applied") return;
        }
        successful = generation === refresh_generation;
      }
      if (successful) {
        last_checked = now();
        last_full_snapshot = now();
      }
    } catch (error) {
      if (generation === refresh_generation) {
        showError(global_error, error);
        required("review-loading").hidden = true;
      }
    } finally {
      if (generation === refresh_generation) {
        refresh_button.disabled = false;
        auto_refresh?.refreshCompleted(successful);
      }
    }
  }
  async function quietRefresh() {
    if (availability() !== "ready") return "cancelled";
    const generation = quiet_generation;
    const current = () => generation === quiet_generation && availability() === "ready";
    const acknowledgements = [];
    const deferChart = () => {
      if (current()) for (const acknowledge of acknowledgements) acknowledge();
      return "cancelled";
    };
    const full_snapshot = now() - last_full_snapshot >= 5 * 6e4;
    const complete = () => {
      if (!current()) return "cancelled";
      for (const acknowledge of acknowledgements) acknowledge();
      if (full_snapshot || !updates_supported) last_full_snapshot = now();
      last_checked = now();
      global_error.hidden = true;
      return "success";
    };
    let updates;
    const probed_source = source_id;
    try {
      if (updates_supported) {
        try {
          updates = await quiet_lanes.updates.run(apiUrl("/api/updates", { source: last_catalog ? source_id : "", run_id: review?.run_ids ?? [] }));
        } catch (error) {
          if (!(error instanceof RequestError) || error.status !== 404) throw error;
          updates_supported = false;
          auto_refresh?.setInterval(3e4);
        }
        if (!current() || updates_supported && !updates) return "cancelled";
      }
      const catalog_changed = updates?.catalog_revision !== null && updates?.catalog_revision !== void 0 && catalog_revision !== updates.catalog_revision;
      if (!updates_supported || full_snapshot || catalog_changed || updates?.catalog_revision === null && now() - last_catalog_snapshot >= 3e4) {
        const catalog = await quiet_lanes.catalog.run("/api/catalog");
        if (!catalog || !current()) return "cancelled";
        applyCatalog(catalog, true);
        if (updates?.catalog_revision !== null && updates?.catalog_revision !== void 0) {
          const revision2 = updates.catalog_revision;
          acknowledgements.push(() => {
            catalog_revision = revision2;
          });
        }
      }
      const source_changed = source_id !== probed_source;
      const revision = source_changed ? void 0 : updates?.source_revision;
      const list_changed = revision !== null && revision !== void 0 && source_revisions.get(source_id) !== revision;
      if (!source_id) emptyCatalog();
      else if (!updates_supported || full_snapshot || source_changed || !list_ready || last_run_list_context !== listUrl() || list_changed || revision === null && now() - last_list_snapshot >= 3e4) {
        const url = listUrl(), result = await quiet_lanes.list.run(url);
        if (!result || !current() || listUrl() !== url) return "cancelled";
        applyRunList(result, url, true);
        if (revision !== null && revision !== void 0) {
          const source = source_id;
          acknowledgements.push(() => boundedSet(source_revisions, source, revision));
        }
      }
      const current_review = review;
      if (current_review && !source_changed) {
        const revisions = current_review.run_ids.map((id) => updates?.runs.find((item) => item.run_id === id));
        if (revisions.some((item) => item?.missing)) {
          const message = "Some selected runs are no longer available in this source. Showing the last successful preview.";
          const notice = required("review-error");
          if (notice.hidden || notice.textContent !== message) showError(notice, message);
          required("review-loading").hidden = true;
          missing_review = current_review;
          return complete();
        }
        if (missing_review === current_review && revisions.every((item) => item !== void 0 && !item.missing)) {
          missing_review = null;
          required("review-error").hidden = true;
          detail_revisions.clear();
          chart_revisions.clear();
          log_revisions.clear();
          comparison_revision = null;
        }
        if (current_review.tab === "charts" && required("chart-frame").src && chart_controller.previewStatus() !== "ready") {
          const outcome = await refreshCurrentChart(quiet_lanes.chart);
          return outcome === "deferred" ? deferChart() : "cancelled";
        }
        const view_revision = JSON.stringify(revisions.map((item) => [item?.run_id, item?.metadata_revision, item?.metrics_revision, item?.missing]));
        const force = !updates_supported || full_snapshot || revisions.some((item) => item === void 0);
        if (current_review.kind === "run" && (current_review.tab !== "logs" || !log_view)) {
          const id = current_review.run_ids[0];
          if (id) {
            const context = apiUrl("/api/run", { source: source_id, run_id: id });
            if (force || detail_revisions.get(context) !== view_revision) {
              const detail = await quiet_lanes.detail.run(context);
              if (!detail || !current() || review !== current_review) return "cancelled";
              const warnings_changed = JSON.stringify(rendered_detail?.warnings) !== JSON.stringify(detail.warnings);
              if (!renderDetail(detail, true)) return "cancelled";
              boundedSet(detail_cache, id, detail);
              if (warnings_changed) warnings(required("review-warnings"), detail.warnings);
              required("review-loading").hidden = true;
              required("review-error").hidden = true;
              if (!current()) return "cancelled";
              acknowledgements.push(() => boundedSet(detail_revisions, context, view_revision));
            }
          }
        } else if (current_review.kind === "compare") {
          const context = comparisonUrl();
          if (force || comparison_revision?.context !== context || comparison_revision.revision !== view_revision) {
            const result = await quiet_lanes.comparison.run(context);
            if (!result || !current() || review !== current_review || comparisonUrl() !== context) return "cancelled";
            const names = [.../* @__PURE__ */ new Set([...metric_names, ...result.comparison.metric_names])].sort();
            const names_changed = JSON.stringify(names) !== JSON.stringify(metric_names), picker = required("compare-metric-options");
            if (names_changed && containsFocus(picker)) return "cancelled";
            metric_names = names;
            if (!current_review.metric_selection_set) {
              current_review.metric_names = result.comparison.metric_names.slice(0, 4);
              current_review.metric_selection_set = true;
            }
            renderComparison(result);
            required("review-loading").hidden = true;
            required("compare-detail").hidden = false;
            required("review-error").hidden = true;
            if (names_changed || !picker.children.length) metricPicker(picker, metric_names, () => {
              void loadComparison();
            });
            if (!required("chart-frame").src) updateChart();
            acknowledgements.push(() => {
              comparison_revision = { context, revision: view_revision };
            });
          }
        }
        if (current_review.tab === "charts") {
          const context = chartUrl();
          if (force || chart_revisions.get(context) !== view_revision) {
            const outcome = await refreshCurrentChart(quiet_lanes.chart);
            if (outcome === "deferred") return deferChart();
            if (outcome === "cancelled") return "cancelled";
            if (!current() || review !== current_review) return "cancelled";
            acknowledgements.push(() => boundedSet(chart_revisions, context, view_revision));
          }
        } else if (current_review.tab === "logs" && log_view) {
          const view = log_view, context = apiUrl("/api/log", { source: source_id, run_id: view.run_id, stream: view.stream, tail: 100 });
          const item = revisions[0], log_revision = view.stream === "stderr" ? item?.stderr_revision : item?.stdout_revision;
          if (force || log_revision === void 0 || !log_revisions.has(context) || log_revisions.get(context) !== log_revision) {
            if (!await refreshCurrentLog(quiet_lanes.log) || !current() || log_view !== view) return "cancelled";
            if (log_revision !== void 0) acknowledgements.push(() => boundedSet(log_revisions, context, log_revision));
          }
        }
      }
      return complete();
    } catch {
      if (!current()) return "cancelled";
      return "failure";
    }
  }
  function resetFilters() {
    cancelRefresh();
    list_lane.cancel();
    clearSearchTimeout();
    offset = 0;
    selected.clear();
    syncSelection();
    hideReview();
    runs = [];
    renderRows();
    required("list-empty").hidden = true;
    required("run-count").textContent = "Applying filters\u2026";
    required("runs-region").setAttribute("aria-busy", "true");
    previous_page.disabled = true;
    next_page.disabled = true;
  }
  function filtersChanged() {
    resetFilters();
    void loadRuns();
  }
  for (const input of [search_input, task_input]) input.addEventListener("input", () => {
    resetFilters();
    search_timeout = setTimeout(() => {
      search_timeout = void 0;
      void loadRuns();
    }, 250);
  });
  status_select.addEventListener("change", filtersChanged);
  required("clear-filters").addEventListener("click", () => {
    search_input.value = "";
    task_input.value = "";
    status_select.value = "";
    clearSearchTimeout();
    filtersChanged();
  });
  previous_page.addEventListener("click", () => {
    cancelRefresh();
    offset = Math.max(0, offset - page_size);
    void loadRuns();
  });
  next_page.addEventListener("click", () => {
    if (next_offset !== null) {
      cancelRefresh();
      offset = next_offset;
      void loadRuns();
    }
  });
  source_select.addEventListener("change", () => {
    cancelRefresh();
    clearSearchTimeout();
    source_id = source_select.value;
    runs = [];
    offset = 0;
    selected.clear();
    detail_cache.clear();
    hideReview();
    syncSelection();
    sourceNote();
    void loadRuns();
  });
  refresh_button.addEventListener("click", () => {
    clearSearchTimeout();
    void refresh();
  });
  required("clear-selection").addEventListener("click", () => {
    selected.clear();
    selectionChanged();
  });
  required("compare-button").addEventListener("click", () => {
    cancelRefresh();
    clearSelectionTimeout();
    openComparison();
  });
  required("close-review").addEventListener("click", closeReview);
  required("reduction-select").addEventListener("change", () => {
    cancelRefresh();
    void loadComparison();
  });
  x_axis_select.addEventListener("change", () => {
    void changeChartAxis();
  });
  for (const tab of review_tabs) {
    const button = required(`review-tab-${tab}`);
    button.addEventListener("click", () => {
      selectReviewTab(tab);
    });
    button.addEventListener("keydown", (event) => {
      if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key) || !review) return;
      event.preventDefault();
      const available = review.kind === "compare" ? ["charts"] : review_tabs;
      const index = available.indexOf(tab);
      const next = event.key === "Home" ? available[0] : event.key === "End" ? available.at(-1) : available[(index + (event.key === "ArrowRight" ? 1 : available.length - 1)) % available.length];
      if (next) {
        selectReviewTab(next);
        required(`review-tab-${next}`).focus();
      }
    });
  }
  auto_refresh = new AutoRefresh({ run: quietRefresh, availability, cancel: abortQuietRequests, on_state: showFreshness, ...options.refresh_clock ? { clock: options.refresh_clock } : {} });
  auto_toggle.addEventListener("change", () => auto_refresh?.setEnabled(auto_toggle.checked));
  const resume = () => auto_refresh?.availabilityChanged();
  document.addEventListener?.("visibilitychange", resume);
  if (typeof window !== "undefined") {
    window.addEventListener("online", resume);
    window.addEventListener("offline", resume);
  }
  auto_refresh.start();
  void refresh();
  return () => {
    auto_refresh?.dispose();
    chart_controller.dispose();
    for (const lane of foreground_lanes) lane.cancel();
    clearSearchTimeout();
    clearSelectionTimeout();
    page_document.removeEventListener?.("visibilitychange", resume);
    if (page_window) {
      page_window.removeEventListener("online", resume);
      page_window.removeEventListener("offline", resume);
    }
  };
}
if (typeof document !== "undefined") startDashboard();
export {
  AutoRefresh,
  RequestError,
  RequestLane,
  apiUrl,
  attachChartInteractions,
  chartXFraction,
  createChartController,
  dragChartRange,
  formatChartTick,
  formatChartX,
  formatDuration,
  formatNumber,
  formatValue,
  nearestChartPoint,
  parseChartPointLabel,
  parseChartRange,
  restoreChartRange,
  startDashboard,
  zoomChartRange
};
