export type ParsedChartPoint = { run_index: number; step: string; value: number; value_text: string };
export type ChartPoint = ParsedChartPoint & { x: number; y: number };
export type StepRange = { start_step: string; end_step: string };
type PlotPoint = ChartPoint & { circle: SVGCircleElement };
type Series = { run_index: number; run_id: string; points: PlotPoint[]; polyline: SVGPolylineElement | null; button: HTMLButtonElement; visible: boolean };
const MAX_STEP = 18446744073709551615n;
const FRACTION_SCALE = 1_000_000_000_000n;
const SVG_NS = "http://www.w3.org/2000/svg";
const LEFT = 86, RIGHT = 950, TOP = 28, BOTTOM = 274;
let chart_sequence = 0;

export function parseChartPointLabel(label: string): ParsedChartPoint | null {
  const match = /^Run ([1-8]) · step (\d+) · value (-?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?)$/.exec(label);
  if (!match) return null;
  const step = match[2], value_text = match[3];
  if (step === undefined || value_text === undefined || step.length > 20 || BigInt(step) > MAX_STEP) return null;
  const value = Number(value_text);
  return Number.isFinite(value) ? { run_index: Number(match[1]), step, value, value_text } : null;
}

function steps(range: StepRange): [bigint, bigint] {
  if (!/^\d{1,20}$/.test(range.start_step) || !/^\d{1,20}$/.test(range.end_step)) throw new Error("Invalid chart step range");
  const start = BigInt(range.start_step), end = BigInt(range.end_step);
  if (start > end || end > MAX_STEP) throw new Error("Invalid chart step range");
  return [start, end];
}
function fractionInteger(fraction: number): bigint {
  return BigInt(Math.round(Math.min(1, Math.max(0, Number.isFinite(fraction) ? fraction : 0)) * Number(FRACTION_SCALE)));
}
function stepAt(range: StepRange, fraction: number): bigint {
  const [start, end] = steps(range);
  return start + (end - start) * fractionInteger(fraction) / FRACTION_SCALE;
}
export function chartStepFraction(step: string, range: StepRange): number {
  const [start, end] = steps(range);
  if (start === end) return .5;
  return Number((BigInt(step) - start) * FRACTION_SCALE / (end - start)) / Number(FRACTION_SCALE);
}
export function dragChartRange(range: StepRange, start_fraction: number, end_fraction: number): StepRange {
  const first = stepAt(range, Math.min(start_fraction, end_fraction));
  const last = stepAt(range, Math.max(start_fraction, end_fraction));
  return first < last ? { start_step: String(first), end_step: String(last) } : range;
}
export function zoomChartRange(range: StepRange, full_range: StepRange, factor: number, center_fraction = .5): StepRange {
  const [full_start, full_end] = steps(full_range), [start, end] = steps(range);
  const full_span = full_end - full_start;
  if (full_span === 0n || !Number.isFinite(factor) || factor <= 0) return range;
  const ratio = BigInt(Math.round(Math.min(1_000_000, factor) * 1_000_000));
  let span = (end - start) * ratio / 1_000_000n;
  if (span < 1n) span = 1n;
  if (span > full_span) span = full_span;
  const anchor = stepAt(range, center_fraction);
  let next_start = anchor - span * fractionInteger(center_fraction) / FRACTION_SCALE;
  if (next_start < full_start) next_start = full_start;
  if (next_start + span > full_end) next_start = full_end - span;
  return { start_step: String(next_start), end_step: String(next_start + span) };
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

export function attachChartInteractions(frame: HTMLIFrameElement): () => void {
  let dispose: (() => void) | null = null;
  const enhance = () => {
    dispose?.(); dispose = null;
    try {
      const document = frame.contentDocument;
      if (!document?.querySelectorAll || document.querySelectorAll("circle.point").length > 4800) return;
      if (document.URL && frame.src && new URL(document.URL).href !== new URL(frame.src, frame.ownerDocument?.baseURI).href) return;
      const cleanups: (() => void)[] = [];
      for (const svg of document.querySelectorAll<SVGSVGElement>(".plot-scroll > svg")) {
        const cleanup = enhancePlot(document, svg);
        if (cleanup) cleanups.push(cleanup);
      }
      dispose = () => { for (const cleanup of cleanups) cleanup(); };
    } catch { /* A navigation outside the chart origin leaves its content untouched. */ }
  };
  frame.addEventListener("load", enhance);
  const observer = typeof MutationObserver === "undefined" ? null : new MutationObserver(() => { dispose?.(); dispose = null; });
  observer?.observe(frame, { attributes: true, attributeFilter: ["src"] });
  enhance();
  return () => { frame.removeEventListener("load", enhance); observer?.disconnect(); dispose?.(); dispose = null; };
}

function enhancePlot(document: Document, svg: SVGSVGElement): (() => void) | null {
  const section = svg.closest<HTMLElement>("section.card");
  const region = svg.parentElement;
  const legend = section?.querySelector<HTMLUListElement>("ul.legend");
  if (!section || !region || !legend || section.hasAttribute("data-interactive-chart")) return null;
  const x_ticks = [...svg.querySelectorAll<SVGTextElement>("text.tick")].filter(node => Number(node.getAttribute("y")) >= BOTTOM + 12);
  const tick_steps = x_ticks.map(node => node.textContent?.trim() ?? "").filter(value => /^\d{1,20}$/.test(value));
  if (!tick_steps.length) return null;
  const sorted_steps = tick_steps.map(value => BigInt(value)).sort((first, second) => first < second ? -1 : first > second ? 1 : 0);
  if ((sorted_steps[sorted_steps.length - 1] ?? MAX_STEP + 1n) > MAX_STEP) return null;
  const full_range: StepRange = { start_step: String(sorted_steps[0]), end_step: String(sorted_steps[sorted_steps.length - 1]) };
  steps(full_range);
  let range = full_range, keyboard_index = -1;
  let drag_start: { x: number; screen_x: number; pointer_id: number } | null = null;
  const listeners: (() => void)[] = [];
  const original_nodes = [...svg.childNodes];
  const original_attributes = new Map<Element, Record<string, string | null>>();
  const original_description = region.getAttribute("aria-describedby");
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
    const x = Number(circle.getAttribute("cx")), y = Number(circle.getAttribute("cy"));
    if (!parsed || !item || circle.getAttribute("cx") === null || circle.getAttribute("cy") === null || !Number.isFinite(x) || !Number.isFinite(y)) { invalid_points = true; continue; }
    item.polyline = preceding_curve;
    item.points.push({ ...parsed, x, y, circle });
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
  function inRange(point: PlotPoint): boolean { const step = BigInt(point.step); return step >= BigInt(range.start_step) && step <= BigInt(range.end_step); }
  function available(item: Series): PlotPoint[] { return item.visible ? item.points.filter(inRange) : []; }
  function clearHighlights(): void { for (const [circle, original] of original_points) circle.setAttribute("r", original.r); }
  function showAt(x: number, y?: number, announce = false, anchor?: PlotPoint): void {
    const position = Math.max(LEFT, Math.min(RIGHT, x));
    crosshair.setAttribute("display", ""); crosshair.setAttribute("x1", String(position)); crosshair.setAttribute("x2", String(position));
    readout.setAttribute("aria-live", announce ? "polite" : "off"); readout_values.replaceChildren(); clearHighlights();
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
        if (first) { keyboard_index = points.indexOf(point); first = false; }
      } else row.append(html(document, "span", "No displayed samples in this range"));
      readout_values.append(row);
    }
    if (!readout_values.childNodes.length) readout_values.append(html(document, "span", "No visible runs. Use the legend to show a run."));
  }
  function draw(): void {
    axis.replaceChildren();
    const [start, end] = steps(range), span = end - start;
    const count = span === 0n ? 1 : Math.max(range.start_step.length, range.end_step.length) > 12 ? 3 : 5;
    const seen = new Set<string>();
    for (let index = 0; index < count; index++) {
      const step = count === 1 ? start : start + span * BigInt(index) / BigInt(count - 1);
      const label = String(step); if (seen.has(label)) continue; seen.add(label);
      const x = LEFT + chartStepFraction(label, range) * (RIGHT - LEFT);
      axis.append(svgNode(document, "line", { class: "grid", x1: String(x), x2: String(x), y1: String(TOP), y2: String(BOTTOM) }));
      const text = svgNode(document, "text", { class: "tick", x: String(x), y: String(BOTTOM + 23), "text-anchor": count === 1 ? "middle" : index === 0 ? "start" : index === count - 1 ? "end" : "middle" }); text.textContent = label; axis.append(text);
    }
    for (const item of series.values()) {
      item.button.disabled = !item.points.length;
      item.button.setAttribute("aria-pressed", String(item.visible));
      for (const point of item.points) {
        point.x = LEFT + chartStepFraction(point.step, range) * (RIGHT - LEFT);
        point.circle.setAttribute("cx", String(point.x));
        point.circle.setAttribute("display", item.visible && inRange(point) ? "" : "none");
      }
      item.polyline?.setAttribute("points", item.points.map(point => `${point.x},${point.y}`).join(" "));
      item.polyline?.setAttribute("display", item.visible ? "" : "none");
    }
    range_label.textContent = start === end ? `Step ${range.start_step}` : `Steps ${range.start_step}–${range.end_step}`;
    range_label.setAttribute("data-start-step", range.start_step); range_label.setAttribute("data-end-step", range.end_step);
    zoom_in.disabled = start === end || span <= 1n;
    zoom_out.disabled = range.start_step === full_range.start_step && range.end_step === full_range.end_step;
    reset.disabled = zoom_out.disabled;
    crosshair.setAttribute("display", "none"); clearHighlights();
    readout_values.replaceChildren(html(document, "span", "Hover or use arrow keys to inspect displayed samples."));
    keyboard_index = -1;
  }
  function applyRange(next: StepRange): void { range = next; selection.setAttribute("display", "none"); draw(); }
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
  return () => {
    for (const remove of listeners) remove();
    for (const [item, children] of original_legends) item.replaceChildren(...children);
    for (const [node, attributes] of original_attributes) {
      for (const [name, value] of Object.entries(attributes)) value === null ? node.removeAttribute(name) : node.setAttribute(name, value);
    }
    svg.replaceChildren(...original_nodes); styles.remove(); controls.remove(); hint.remove(); readout.remove();
    if (original_description === null) region.removeAttribute("aria-describedby"); else region.setAttribute("aria-describedby", original_description);
    section.removeAttribute("data-interactive-chart");
  };
}
