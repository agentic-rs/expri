import { createElement } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import type { ChartAxis, ChartData, ChartPlot } from "./chart_data";
import { ChartPreview } from "./chart_renderer";
import {
  LEFT, RIGHT, TOP, BOTTOM, chartValueText, chartXFraction, chartYFraction,
  coordinatesInRange, dateParts, formatChartX, formatChartTick, nearestChartPoint,
  restoreChartRange, timeZoneLabel, dragChartRange, zoomChartRange,
  type ChartTimeZone, type ChartPoint, type ChartRange,
} from "./chart_math";
export * from "./chart_math";

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
export type ChartController = {
  dispose(): void;
  isInteracting(): boolean;
  previewStatus(): ChartPreviewStatus;
  replacePreview(data: ChartData): boolean;
  clear(): void;
  setTimeZone(time_zone: ChartTimeZone): void;
};
const SVG_NS = "http://www.w3.org/2000/svg";
let chart_sequence = 0;

function html<K extends keyof HTMLElementTagNameMap>(document: Document, tag: K, text = ""): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag); node.textContent = text; return node;
}
function svgNode<K extends keyof SVGElementTagNameMap>(document: Document, tag: K, attrs: Record<string, string>): SVGElementTagNameMap[K] {
  const node = document.createElementNS(SVG_NS, tag);
  for (const [name, value] of Object.entries(attrs)) node.setAttribute(name, value);
  return node;
}


// The request layer validates JSON once. Readiness is cached rather than scanning
// the SVG on every five-second refresh probe.
export function createChartController(container: HTMLElement): ChartController {
  const root = createRoot(container);
  const document = container.ownerDocument;
  let plots: PlotController[] = [], disposed = false;
  let status: ChartPreviewStatus = "loading", time_zone: ChartTimeZone = "local";
  const saved_states = new Map<string, PlotState>();
  function capture(): void {
    const active = document.activeElement;
    // An empty metric has no controller to recapture focus. If the user moves
    // to another live control, its remembered focus must not return later.
    if (active?.isConnected && active !== document.body && active !== document.documentElement) {
      for (const state of saved_states.values()) state.focus = null;
    }
    for (const plot of plots) {
      const state = plot.capture(); saved_states.set(state.metric_name, state);
    }
  }
  function disposePlots(): void {
    for (const plot of plots) plot.dispose();
    plots = [];
  }
  return {
    previewStatus() { return status; },
    isInteracting() { return !disposed && plots.some(plot => plot.isInteracting()); },
    setTimeZone(next) {
      if (disposed || time_zone === next) return;
      time_zone = next;
      for (const plot of plots) plot.setTimeZone(next);
    },
    clear() {
      if (disposed) return;
      disposePlots(); saved_states.clear();
      flushSync(() => root.render(null));
      status = "loading";
    },
    dispose() {
      if (disposed) return;
      disposed = true; disposePlots(); saved_states.clear();
      flushSync(() => root.unmount());
      status = "invalid";
    },
    replacePreview(data) {
      if (disposed || plots.some(plot => plot.isInteracting())) return false;
      const scroll_left = container.scrollLeft, scroll_top = container.scrollTop;
      capture();
      // Restore React-owned glyphs before reconciliation, and remove only the
      // imperative interaction layer. Keys retain the plot and parameter DOM.
      disposePlots();
      flushSync(() => root.render(createElement(ChartPreview, { data })));
      const sections = [...container.querySelectorAll<HTMLElement>("section[data-chart-metric]")];
      for (let index = 0; index < data.plots.length; index++) {
        const model = data.plots[index], section = sections[index];
        if (!model || !section) continue;
        const svg = section.querySelector<SVGSVGElement>(".plot-scroll > svg");
        if (!svg || !model.domain) continue;
        const plot = enhancePlot(document, svg, time_zone, model, data, saved_states.get(model.metric_name));
        if (!plot) continue;
        plots.push(plot);
      }
      // Retain states for an empty metric so a later timestamped sample restores
      // its hidden runs and absolute zoom, but discard metrics no longer chosen.
      const metrics = new Set(data.plots.map(plot => plot.metric_name));
      for (const name of saved_states.keys()) if (!metrics.has(name)) saved_states.delete(name);
      container.scrollLeft = scroll_left; container.scrollTop = scroll_top;
      status = "ready";
      return true;
    },
  };
}

function enhancePlot(document: Document, svg: SVGSVGElement, time_zone: ChartTimeZone, model: ChartPlot, data: ChartData, initial_state?: PlotState): PlotController | null {
  const section = svg.closest<HTMLElement>("section[data-chart-metric]");
  const region = svg.parentElement;
  const legend = section?.querySelector<HTMLUListElement>("ul.legend");
  if (!section || !region || !legend || section.hasAttribute("data-interactive-chart")) return null;
  const x_ticks = [...svg.querySelectorAll<SVGTextElement>("text.tick")].filter(node => Number(node.getAttribute("y")) >= BOTTOM + 12);
  if (!model.domain) return null;
  const full_range: ChartRange = { x_axis: data.x_axis, start_x: model.domain.x_min, end_x: model.domain.x_max };
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
  const metric_name = model.metric_name;
  const series = new Map<number, Series>();
  const legend_items = [...legend.querySelectorAll<HTMLLIElement>("li")];
  for (let index = 0; index < legend_items.length; index++) {
    const item = legend_items[index]; if (!item) continue;
    const run_id = data.runs[index]!;
    const button = html(document, "button"); button.type = "button"; button.className = "chart-run-toggle";
    button.setAttribute("data-chart-run", String(index + 1)); button.setAttribute("aria-pressed", "true");
    button.setAttribute("aria-label", `Toggle ${run_id} in ${metric_name}`);
    original_legends.set(item, [...item.childNodes]); button.append(...item.childNodes); item.append(button);
    series.set(index + 1, { run_index: index + 1, run_id, points: [], polyline: null, button, visible: true });
  }
  const circles = [...svg.querySelectorAll<SVGCircleElement>("circle.point")];
  let circle_index = 0;
  for (const source of model.series) {
    const item = series.get(source.run_index);
    if (!item) continue;
    const polyline = svg.querySelector<SVGPolylineElement>(`polyline[data-chart-run-index="${source.run_index}"]`);
    item.polyline = polyline;
    if (polyline) {
      remember(polyline, ["points", "clip-path", "display", "data-chart-series"]);
      original_curves.set(polyline, polyline.getAttribute("points") ?? "");
      polyline.setAttribute("data-chart-series", String(source.run_index));
    }
    for (const point of source.points) {
      const circle = circles[circle_index++];
      if (!circle) continue;
      const x = LEFT + chartXFraction(point.x_value, full_range) * (RIGHT - LEFT);
      const y = TOP + (1 - chartYFraction(point.value, model.domain.value_min, model.domain.value_max)) * (BOTTOM - TOP);
      item.points.push({ run_index: source.run_index, step: point.step, value: point.value,
        value_text: chartValueText(point.value), x_value: point.x_value, timestamp: point.timestamp, x, y, circle });
      original_points.set(circle, { cx: circle.getAttribute("cx") ?? "", r: "2" });
      remember(circle, ["cx", "r", "clip-path", "display", "data-chart-series"]);
      circle.setAttribute("data-chart-series", String(source.run_index));
    }
  }
  section.setAttribute("data-interactive-chart", "true");
  const id = `chart-explorer-${++chart_sequence}`;
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
  section.insertBefore(controls, region); section.insertBefore(hint, region); section.append(readout);
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
      // A zero-span range places every coordinate at the center. Exclude hidden
      // coordinates so a clamped zoom cannot invent a vertical curve.
      const curve_points = span === 0n ? item.points.filter(inRange) : item.points;
      item.polyline?.setAttribute("points", curve_points.map(point => `${point.x},${point.y}`).join(" "));
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
  const controller: PlotController = {
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
      svg.replaceChildren(...original_nodes); controls.remove(); hint.remove(); readout.remove();
      if (axis_label) axis_label.textContent = original_axis_label;
      if (context) context.textContent = original_context;
      if (original_description === null) region.removeAttribute("aria-describedby"); else region.setAttribute("aria-describedby", original_description);
      section.removeAttribute("data-interactive-chart");
    },
  };
  if (initial_state) controller.restore(initial_state); else draw();
  return controller;
}
