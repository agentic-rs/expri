// interactive_charts.ts
var MAX_STEP = 18446744073709551615n;
var FRACTION_SCALE = 1000000000000n;
var SVG_NS = "http://www.w3.org/2000/svg";
var LEFT = 86;
var RIGHT = 950;
var TOP = 28;
var BOTTOM = 274;
var chart_sequence = 0;
function parseChartPointLabel(label) {
  const match = /^Run ([1-8]) · step (\d+) · value (-?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?)$/.exec(label);
  if (!match) return null;
  const step = match[2], value_text = match[3];
  if (step === void 0 || value_text === void 0 || step.length > 20 || BigInt(step) > MAX_STEP) return null;
  const value = Number(value_text);
  return Number.isFinite(value) ? { run_index: Number(match[1]), step, value, value_text } : null;
}
function steps(range) {
  if (!/^\d{1,20}$/.test(range.start_step) || !/^\d{1,20}$/.test(range.end_step)) throw new Error("Invalid chart step range");
  const start = BigInt(range.start_step), end = BigInt(range.end_step);
  if (start > end || end > MAX_STEP) throw new Error("Invalid chart step range");
  return [start, end];
}
function fractionInteger(fraction) {
  return BigInt(Math.round(Math.min(1, Math.max(0, Number.isFinite(fraction) ? fraction : 0)) * Number(FRACTION_SCALE)));
}
function stepAt(range, fraction) {
  const [start, end] = steps(range);
  return start + (end - start) * fractionInteger(fraction) / FRACTION_SCALE;
}
function chartStepFraction(step, range) {
  const [start, end] = steps(range);
  if (start === end) return 0.5;
  return Number((BigInt(step) - start) * FRACTION_SCALE / (end - start)) / Number(FRACTION_SCALE);
}
function dragChartRange(range, start_fraction, end_fraction) {
  const first = stepAt(range, Math.min(start_fraction, end_fraction));
  const last = stepAt(range, Math.max(start_fraction, end_fraction));
  return first < last ? { start_step: String(first), end_step: String(last) } : range;
}
function zoomChartRange(range, full_range, factor, center_fraction = 0.5) {
  const [full_start, full_end] = steps(full_range), [start, end] = steps(range);
  const full_span = full_end - full_start;
  if (full_span === 0n || !Number.isFinite(factor) || factor <= 0) return range;
  const ratio = BigInt(Math.round(Math.min(1e6, factor) * 1e6));
  let span = (end - start) * ratio / 1000000n;
  if (span < 1n) span = 1n;
  if (span > full_span) span = full_span;
  const anchor = stepAt(range, center_fraction);
  let next_start = anchor - span * fractionInteger(center_fraction) / FRACTION_SCALE;
  if (next_start < full_start) next_start = full_start;
  if (next_start + span > full_end) next_start = full_end - span;
  return { start_step: String(next_start), end_step: String(next_start + span) };
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
function attachChartInteractions(frame) {
  let dispose = null;
  const enhance = () => {
    dispose?.();
    dispose = null;
    try {
      const document2 = frame.contentDocument;
      if (!document2?.querySelectorAll || document2.querySelectorAll("circle.point").length > 4800) return;
      if (document2.URL && frame.src && new URL(document2.URL).href !== new URL(frame.src, frame.ownerDocument?.baseURI).href) return;
      const cleanups = [];
      for (const svg of document2.querySelectorAll(".plot-scroll > svg")) {
        const cleanup = enhancePlot(document2, svg);
        if (cleanup) cleanups.push(cleanup);
      }
      dispose = () => {
        for (const cleanup of cleanups) cleanup();
      };
    } catch {
    }
  };
  frame.addEventListener("load", enhance);
  const observer = typeof MutationObserver === "undefined" ? null : new MutationObserver(() => {
    dispose?.();
    dispose = null;
  });
  observer?.observe(frame, { attributes: true, attributeFilter: ["src"] });
  enhance();
  return () => {
    frame.removeEventListener("load", enhance);
    observer?.disconnect();
    dispose?.();
    dispose = null;
  };
}
function enhancePlot(document2, svg) {
  const section = svg.closest("section.card");
  const region = svg.parentElement;
  const legend = section?.querySelector("ul.legend");
  if (!section || !region || !legend || section.hasAttribute("data-interactive-chart")) return null;
  const x_ticks = [...svg.querySelectorAll("text.tick")].filter((node) => Number(node.getAttribute("y")) >= BOTTOM + 12);
  const tick_steps = x_ticks.map((node) => node.textContent?.trim() ?? "").filter((value) => /^\d{1,20}$/.test(value));
  if (!tick_steps.length) return null;
  const sorted_steps = tick_steps.map((value) => BigInt(value)).sort((first, second) => first < second ? -1 : first > second ? 1 : 0);
  if ((sorted_steps[sorted_steps.length - 1] ?? MAX_STEP + 1n) > MAX_STEP) return null;
  const full_range = { start_step: String(sorted_steps[0]), end_step: String(sorted_steps[sorted_steps.length - 1]) };
  steps(full_range);
  let range = full_range, keyboard_index = -1;
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
    const x = Number(circle.getAttribute("cx")), y = Number(circle.getAttribute("cy"));
    if (!parsed || !item || circle.getAttribute("cx") === null || circle.getAttribute("cy") === null || !Number.isFinite(x) || !Number.isFinite(y)) {
      invalid_points = true;
      continue;
    }
    item.polyline = preceding_curve;
    item.points.push({ ...parsed, x, y, circle });
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
    const step = BigInt(point.step);
    return step >= BigInt(range.start_step) && step <= BigInt(range.end_step);
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
    const [start, end] = steps(range), span = end - start;
    const count = span === 0n ? 1 : Math.max(range.start_step.length, range.end_step.length) > 12 ? 3 : 5;
    const seen = /* @__PURE__ */ new Set();
    for (let index = 0; index < count; index++) {
      const step = count === 1 ? start : start + span * BigInt(index) / BigInt(count - 1);
      const label = String(step);
      if (seen.has(label)) continue;
      seen.add(label);
      const x = LEFT + chartStepFraction(label, range) * (RIGHT - LEFT);
      axis.append(svgNode(document2, "line", { class: "grid", x1: String(x), x2: String(x), y1: String(TOP), y2: String(BOTTOM) }));
      const text = svgNode(document2, "text", { class: "tick", x: String(x), y: String(BOTTOM + 23), "text-anchor": count === 1 ? "middle" : index === 0 ? "start" : index === count - 1 ? "end" : "middle" });
      text.textContent = label;
      axis.append(text);
    }
    for (const item of series.values()) {
      item.button.disabled = !item.points.length;
      item.button.setAttribute("aria-pressed", String(item.visible));
      for (const point of item.points) {
        point.x = LEFT + chartStepFraction(point.step, range) * (RIGHT - LEFT);
        point.circle.setAttribute("cx", String(point.x));
        point.circle.setAttribute("display", item.visible && inRange(point) ? "" : "none");
      }
      item.polyline?.setAttribute("points", item.points.map((point) => `${point.x},${point.y}`).join(" "));
      item.polyline?.setAttribute("display", item.visible ? "" : "none");
    }
    range_label.textContent = start === end ? `Step ${range.start_step}` : `Steps ${range.start_step}\u2013${range.end_step}`;
    range_label.setAttribute("data-start-step", range.start_step);
    range_label.setAttribute("data-end-step", range.end_step);
    zoom_in.disabled = start === end || span <= 1n;
    zoom_out.disabled = range.start_step === full_range.start_step && range.end_step === full_range.end_step;
    reset.disabled = zoom_out.disabled;
    crosshair.setAttribute("display", "none");
    clearHighlights();
    readout_values.replaceChildren(html(document2, "span", "Hover or use arrow keys to inspect displayed samples."));
    keyboard_index = -1;
  }
  function applyRange(next) {
    range = next;
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
  return () => {
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
  };
}

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
var RequestLane = class {
  controller = null;
  generation = 0;
  cancel() {
    this.controller?.abort();
    this.controller = null;
    this.generation++;
  }
  async run(url) {
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
      const value = await response.json();
      if (generation !== this.generation) return void 0;
      if (!response.ok) {
        const error = value;
        throw new Error(error.error ?? `Request failed (${response.status})`);
      }
      return value;
    } catch (error) {
      if (generation !== this.generation) return void 0;
      if (controller.signal.aborted) throw new Error("The request timed out. Try Refresh when the connection is available.");
      throw error;
    } finally {
      clearTimeout(timeout);
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
function startDashboard() {
  const source_select = required("source-select");
  attachChartInteractions(required("chart-frame"));
  const refresh_button = required("refresh-button");
  const search_input = required("search-input");
  const task_input = required("task-input");
  const status_select = required("status-select");
  const previous_page = required("previous-page");
  const next_page = required("next-page");
  const global_error = required("global-error");
  const rows = required("run-rows");
  const catalog_lane = new RequestLane();
  const list_lane = new RequestLane();
  const review_lane = new RequestLane();
  const log_lane = new RequestLane();
  const compare_lane = new RequestLane();
  let sources = [];
  let source_id = "local";
  let access_mode = "local";
  let page_size = 100;
  let runs = [];
  let offset = 0;
  let next_offset = null;
  let review = null;
  let metric_names = [];
  let search_timeout;
  let selection_timeout;
  let refresh_generation = 0;
  let log_view = null;
  const selected = /* @__PURE__ */ new Set();
  const detail_cache = /* @__PURE__ */ new Map();
  const row_checks = /* @__PURE__ */ new Map();
  const row_buttons = /* @__PURE__ */ new Map();
  const review_tabs = ["charts", "overview", "logs"];
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
        clearTimeout(selection_timeout);
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
    if (focused_check) row_checks.get(focused_check)?.focus();
    else if (focused_button) row_buttons.get(focused_button)?.focus();
  }
  async function loadRuns() {
    if (!source_id) return false;
    global_error.hidden = true;
    required("runs-region").setAttribute("aria-busy", "true");
    for (const control of rows.querySelectorAll("input, button")) control.disabled = true;
    previous_page.disabled = true;
    next_page.disabled = true;
    required("run-count").textContent = "Loading run history\u2026";
    try {
      const result = await list_lane.run(apiUrl("/api/runs", { source: source_id, search: search_input.value.trim(), task: task_input.value.trim(), status: status_select.value, limit: page_size, offset }));
      if (!result) return false;
      runs = result.runs;
      offset = result.offset;
      next_offset = result.next_offset;
      renderRows();
      warnings(required("list-warnings"), result.warnings);
      required("run-count").textContent = result.total_count ? `${offset + 1}\u2013${offset + runs.length} of ${result.total_count.toLocaleString()} runs` : "No matching runs";
      required("page-label").textContent = `Page ${Math.floor(offset / page_size) + 1}`;
      previous_page.disabled = offset === 0;
      next_page.disabled = next_offset === null;
      const empty = required("list-empty");
      empty.hidden = runs.length > 0;
      if (!runs.length) {
        showEmptyRuns(Boolean(search_input.value || task_input.value || status_select.value), result.source);
      }
      const options = required("task-options");
      options.replaceChildren();
      for (const task of [...new Set(runs.map((run) => run.task).filter((task2) => task2 !== null))].sort()) {
        const option = element("option");
        option.value = task;
        options.append(option);
      }
      required("runs-region").setAttribute("aria-busy", "false");
      announce(`${result.total_count} matching runs`);
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
    refresh_generation++;
    catalog_lane.cancel();
    refresh_button.disabled = false;
  }
  function hideReview() {
    clearTimeout(selection_timeout);
    review_lane.cancel();
    log_lane.cancel();
    compare_lane.cancel();
    review = null;
    log_view = null;
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
    clearTimeout(selection_timeout);
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
      if (review !== pending_review) return;
      if (ids.length === 1 && ids[0]) void loadRun(ids[0]);
      else void loadComparison();
    };
    if (debounce) selection_timeout = setTimeout(open, 180);
    else open();
  }
  function beginReview(kind, ids, origin = "inspection") {
    clearTimeout(selection_timeout);
    review_lane.cancel();
    log_lane.cancel();
    compare_lane.cancel();
    review = { kind, origin, run_ids: ids, metric_names: [], metric_selection_set: false, tab: "charts", log_stream: "stdout" };
    metric_names = [];
    log_view = null;
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
    const url = apiUrl("/api/chart", { source: source_id, run_id: review.run_ids, metric: review.metric_names });
    required("chart-frame").src = url;
    required("open-chart").href = url;
    required("chart-card").hidden = false;
    required("chart-note").textContent = review.metric_names.length ? `${review.metric_names.length} selected metrics \xB7 at most 600 chart points per series` : "First four recorded metrics \xB7 at most 600 chart points per series";
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
    if (review?.kind === "run" && review.tab === "logs" && log_view && !log_view.loaded && !log_view.pending) void loadLog(log_view);
  }
  function renderDetail(detail) {
    const parent = required("run-detail");
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
    metric_names = names;
    if (review && !review.metric_selection_set) {
      review.metric_names = names.slice(0, 4);
      review.metric_selection_set = true;
    }
    metricPicker(required("run-metric-options"), names, updateChart);
    required("run-metric-controls").hidden = false;
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
    updateChart();
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
  async function loadRun(id) {
    try {
      const detail = await review_lane.run(apiUrl("/api/run", { source: source_id, run_id: id }));
      if (!detail) return;
      detail_cache.delete(id);
      detail_cache.set(id, detail);
      while (detail_cache.size > 9) {
        const oldest = detail_cache.keys().next().value;
        if (oldest === void 0) break;
        detail_cache.delete(oldest);
      }
      required("review-loading").hidden = true;
      warnings(required("review-warnings"), detail.warnings);
      renderDetail(detail);
      announce(`Opened ${id}`);
    } catch (error) {
      required("review-loading").hidden = true;
      showError(required("review-error"), error);
    }
  }
  function renderComparison(result) {
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
  async function loadComparison() {
    if (!review || review.kind !== "compare") return;
    required("review-error").hidden = true;
    required("comparison-values").setAttribute("aria-busy", "true");
    required("chart-card").hidden = true;
    try {
      const result = await compare_lane.run(apiUrl("/api/compare", { source: source_id, run_id: review.run_ids, metric: review.metric_names, reduction: required("reduction-select").value }));
      if (!result || !review || review.kind !== "compare") return;
      metric_names = [.../* @__PURE__ */ new Set([...metric_names, ...result.comparison.metric_names])].sort();
      if (!review.metric_selection_set) {
        review.metric_names = result.comparison.metric_names.slice(0, 4);
        review.metric_selection_set = true;
      }
      required("review-loading").hidden = true;
      required("compare-detail").hidden = false;
      renderComparison(result);
      required("comparison-values").setAttribute("aria-busy", "false");
      metricPicker(required("compare-metric-options"), metric_names, () => {
        void loadComparison();
      });
      updateChart();
    } catch (error) {
      required("review-loading").hidden = true;
      required("comparison-values").setAttribute("aria-busy", "false");
      showError(required("review-error"), error);
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
    note.textContent = source?.kind === "service" ? "Synced results \xB7 Updates arrive from workers. Refresh to read the latest synced files." : source?.kind === "cached" ? "Cached remote results \xB7 Recorded status may be older than the remote run. Pull updated results with expri runs pull, then Refresh here." : "Local results \xB7 Status comes from recorded run files. Refresh to read the latest changes.";
  }
  async function refresh() {
    const generation = ++refresh_generation;
    refresh_button.disabled = true;
    global_error.hidden = true;
    try {
      const catalog = await catalog_lane.run("/api/catalog");
      if (!catalog) return;
      if (generation !== refresh_generation) return;
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
        runs = [];
        selected.clear();
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
      detail_cache.clear();
      offset = 0;
      if (!source_id) {
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
        announce("No synced runs yet");
        required("updated-at").textContent = "Refresh after syncing your first run.";
        return;
      }
      const current_review = review;
      const current_source = source_id;
      if (!await loadRuns() || generation !== refresh_generation) return;
      if (current_review === review && current_source === source_id) {
        if (current_review?.kind === "run") {
          const id = current_review.run_ids[0];
          if (id) await openRun(id, current_review.origin, current_review);
        } else if (current_review?.kind === "compare") await loadComparison();
      }
      if (generation === refresh_generation) required("updated-at").textContent = `Last checked at ${(/* @__PURE__ */ new Date()).toLocaleTimeString()}. Refresh for updates.`;
    } catch (error) {
      if (generation === refresh_generation) {
        showError(global_error, error);
        required("review-loading").hidden = true;
      }
    } finally {
      if (generation === refresh_generation) refresh_button.disabled = false;
    }
  }
  function resetFilters() {
    cancelRefresh();
    list_lane.cancel();
    clearTimeout(search_timeout);
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
      void loadRuns();
    }, 250);
  });
  status_select.addEventListener("change", filtersChanged);
  required("clear-filters").addEventListener("click", () => {
    search_input.value = "";
    task_input.value = "";
    status_select.value = "";
    clearTimeout(search_timeout);
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
    clearTimeout(search_timeout);
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
    clearTimeout(search_timeout);
    void refresh();
  });
  required("clear-selection").addEventListener("click", () => {
    selected.clear();
    selectionChanged();
  });
  required("compare-button").addEventListener("click", () => {
    cancelRefresh();
    clearTimeout(selection_timeout);
    openComparison();
  });
  required("close-review").addEventListener("click", closeReview);
  required("reduction-select").addEventListener("change", () => {
    cancelRefresh();
    void loadComparison();
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
  void refresh();
}
if (typeof document !== "undefined") startDashboard();
export {
  RequestLane,
  apiUrl,
  attachChartInteractions,
  chartStepFraction,
  dragChartRange,
  formatDuration,
  formatNumber,
  formatValue,
  nearestChartPoint,
  parseChartPointLabel,
  startDashboard,
  zoomChartRange
};
