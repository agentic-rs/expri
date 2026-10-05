type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Warning = { run_id?: string; message: string };
type Source = { source_id: string; label: string; kind: string; target_name: string | null };
type Run = { run_id: string; task: string | null; status: string; started_at: string | null; finished_at: string | null; exit_code: number | null };
type Point = { step: number; value: number; timestamp?: string };
type Metric = { count: number; last: Point; min: Point; max: Point };
type Catalog = { project_name: string; initial_source: string; sources: Source[]; warnings: Warning[]; access_mode?: "local" | "hosted" };
type RunList = { source: Source; runs: Run[]; warnings: Warning[]; total_count: number; offset: number; next_offset: number | null };
type Detail = { source: Source; run: Run; state: Json; snapshot: Json; environment: Json; params: Json; params_truncated: boolean; metadata_truncated: boolean; metrics: Record<string, Metric>; metric_count: number; metrics_truncated: boolean; metrics_error: string | null; warnings: Warning[]; cache: Json };
type Log = { content: string; stream: string; missing: boolean; truncated: boolean };
type Comparison = { source: Source; comparison: { reduction: string; metric_names: string[]; runs: { run_id: string; run: Run; values: Record<string, Point | null> }[]; warnings: Warning[] } };
type ReviewTab = "charts" | "overview" | "logs";
type Review = { kind: "run" | "compare"; origin: "inspection" | "selection"; run_ids: string[]; metric_names: string[]; metric_selection_set: boolean; tab: ReviewTab; log_stream: "stdout" | "stderr" };
type LogView = { run_id: string; stream: string; output: HTMLElement; note: HTMLElement; loaded: boolean; pending: boolean };

export function apiUrl(path: string, fields: Record<string, string | number | string[] | null>): string {
  const query = new URLSearchParams();
  for (const [name, value] of Object.entries(fields)) {
    if (value === null || value === "") continue;
    for (const item of Array.isArray(value) ? value : [value]) query.append(name, String(item));
  }
  return `${path}?${query}`;
}

export function formatValue(value: Json | undefined): string {
  if (value === undefined || value === null) return "—";
  return typeof value === "string" ? value : JSON.stringify(value);
}

export function formatNumber(value: number): string {
  return Number.isFinite(value) ? new Intl.NumberFormat(undefined, { maximumSignificantDigits: 6 }).format(value) : "—";
}

export function formatDuration(run: Run): string {
  if (!run.started_at || !run.finished_at) return "—";
  const seconds = Math.max(0, Math.round((Date.parse(run.finished_at) - Date.parse(run.started_at)) / 1000));
  if (!Number.isFinite(seconds)) return "—";
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  return `${Math.floor(seconds / 3600)}h ${Math.floor(seconds % 3600 / 60)}m`;
}

export class RequestLane {
  private controller: AbortController | null = null;
  private generation = 0;
  cancel(): void { this.controller?.abort(); this.controller = null; this.generation++; }
  async run<T>(url: string): Promise<T | undefined> {
    this.cancel();
    const generation = this.generation;
    const controller = new AbortController();
    this.controller = controller;
    const timeout = setTimeout(() => controller.abort(), 30_000);
    try {
      const response = await fetch(url, { signal: controller.signal, cache: "no-store" });
      if (generation !== this.generation) return undefined;
      if (response.status === 401) {
        if (typeof location !== "undefined") location.assign("/login");
        return undefined;
      }
      const value: unknown = await response.json();
      if (generation !== this.generation) return undefined;
      if (!response.ok) {
        const error = value as { error?: string };
        throw new Error(error.error ?? `Request failed (${response.status})`);
      }
      return value as T;
    } catch (error) {
      if (generation !== this.generation) return undefined;
      if (controller.signal.aborted) throw new Error("The request timed out. Try Refresh when the connection is available.");
      throw error;
    } finally { clearTimeout(timeout); }
  }
}

function element<K extends keyof HTMLElementTagNameMap>(tag: K, text = "", class_name = ""): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  node.textContent = text;
  node.className = class_name;
  return node;
}
function required<T extends HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (!node) throw new Error(`Missing dashboard element: ${id}`);
  return node as T;
}
function dateText(value: string | null): string {
  if (!value) return "—";
  const date = new Date(value);
  return Number.isNaN(date.valueOf()) ? value : date.toLocaleString();
}
function errorText(error: unknown): string { return error instanceof Error ? error.message : String(error); }
function showError(node: HTMLElement, error: unknown): void { node.textContent = errorText(error); node.hidden = false; }
function warnings(node: HTMLElement, items: Warning[]): void {
  node.replaceChildren(); node.hidden = items.length === 0;
  if (!items.length) return;
  const notice = element("div", "", "notice");
  const list = element("ul");
  for (const item of items.slice(0, 100)) list.append(element("li", `${item.run_id ? `${item.run_id}: ` : ""}${item.message}`));
  notice.append(list); node.append(notice);
}
function facts(values: [string, string][]): HTMLElement {
  const list = element("dl", "", "facts");
  for (const [label, value] of values) list.append(element("dt", label), element("dd", value));
  return list;
}
function records(title: string, value: Json): HTMLElement {
  const details = element("details", "", "detail-details");
  details.append(element("summary", title), element("pre", JSON.stringify(value, null, 2) ?? "Not recorded"));
  return details;
}
function card(title: string): HTMLElement {
  const section = element("section", "", "card"); section.append(element("h3", title)); return section;
}
function jsonObject(value: Json): Record<string, Json> { return value !== null && typeof value === "object" && !Array.isArray(value) ? value : {}; }

export function startDashboard(): void {
  const source_select = required<HTMLSelectElement>("source-select");
  const refresh_button = required<HTMLButtonElement>("refresh-button");
  const search_input = required<HTMLInputElement>("search-input");
  const task_input = required<HTMLInputElement>("task-input");
  const status_select = required<HTMLSelectElement>("status-select");
  const previous_page = required<HTMLButtonElement>("previous-page");
  const next_page = required<HTMLButtonElement>("next-page");
  const global_error = required("global-error");
  const rows = required("run-rows");
  const catalog_lane = new RequestLane();
  const list_lane = new RequestLane();
  const review_lane = new RequestLane();
  const log_lane = new RequestLane();
  const compare_lane = new RequestLane();
  let sources: Source[] = [];
  let source_id = "local";
  let access_mode: "local" | "hosted" = "local";
  let page_size = 100;
  let runs: Run[] = [];
  let offset = 0;
  let next_offset: number | null = null;
  let review: Review | null = null;
  let metric_names: string[] = [];
  let search_timeout: ReturnType<typeof setTimeout> | undefined;
  let selection_timeout: ReturnType<typeof setTimeout> | undefined;
  let refresh_generation = 0;
  let log_view: LogView | null = null;
  const selected = new Set<string>();
  const detail_cache = new Map<string, Detail>();
  const row_checks = new Map<string, HTMLInputElement>();
  const row_buttons = new Map<string, HTMLButtonElement>();
  const review_tabs: ReviewTab[] = ["charts", "overview", "logs"];

  function announce(message: string): void { required("live-status").textContent = message; }
  function showEmptyRuns(filtered: boolean, source?: Source): void {
    const synced = access_mode === "hosted" || source?.kind === "service";
    const empty = required("list-empty"); empty.hidden = false;
    const title = synced && !filtered ? "No synced runs yet" : "No runs found";
    const message = filtered ? "Try changing the filters."
      : synced ? "Sync results from a worker to see them here."
      : source?.kind === "cached" ? "Pull results with expri runs pull, then Refresh."
      : "Start an experiment with expri run to record results here.";
    empty.replaceChildren(element("span", "◌", "empty-mark"), element("h3", title), element("p", message));
    if (synced && !filtered) {
      const paragraph = element("p", "", "setup-guide");
      const link = element("a", "Set up result syncing");
      link.href = "https://github.com/agentic-rs/expri/blob/main/docs/self-hosted-service.md";
      link.target = "_blank"; link.rel = "noopener noreferrer";
      paragraph.append(link); empty.append(paragraph);
    }
  }
  function syncSelection(): void {
    required("selection-count").textContent = selected.size ? `${selected.size} of 8 runs selected` : "Select runs to compare";
    const chips = required("selected-runs"); chips.replaceChildren();
    for (const id of selected) {
      const button = element("button", `${id} ×`, "selection-chip"); button.type = "button";
      button.setAttribute("aria-label", `Remove ${id} from comparison`);
      button.addEventListener("click", () => { selected.delete(id); selectionChanged(); }); chips.append(button);
    }
    required<HTMLButtonElement>("compare-button").disabled = selected.size < 2;
    required("clear-selection").hidden = selected.size === 0;
  }
  function renderRows(): void {
    const focused_check = [...row_checks].find(([, node]) => node === document.activeElement)?.[0];
    const focused_button = [...row_buttons].find(([, node]) => node === document.activeElement)?.[0];
    rows.replaceChildren(); row_checks.clear(); row_buttons.clear();
    for (const run of runs) {
      const row = element("tr");
      row.classList.toggle("selected", selected.has(run.run_id));
      row.classList.toggle("active", review?.kind === "run" && review.run_ids[0] === run.run_id);
      const check_cell = element("td", "", "selection-column");
      const checkbox = element("input"); checkbox.type = "checkbox"; checkbox.checked = selected.has(run.run_id);
      checkbox.disabled = selected.size >= 8 && !checkbox.checked;
      checkbox.setAttribute("aria-label", `Select ${run.run_id} for comparison`);
      checkbox.addEventListener("change", () => {
        checkbox.checked ? selected.add(run.run_id) : selected.delete(run.run_id);
        selectionChanged();
      }); row_checks.set(run.run_id, checkbox);
      check_cell.append(checkbox);
      const run_cell = element("td", "", "run-cell"); const button = element("button", run.run_id, "run-link"); button.type = "button";
      row_buttons.set(run.run_id, button);
      button.addEventListener("click", () => { cancelRefresh(); clearTimeout(selection_timeout); void openRun(run.run_id); });
      const metadata = [dateText(run.started_at), formatDuration(run)].filter(value => value !== "—");
      if (run.exit_code !== null && run.exit_code !== 0) metadata.push(`exit ${run.exit_code}`);
      run_cell.append(button, element("div", run.task ?? "No task recorded", "run-task"), element("div", metadata.join(" · "), "run-meta"));
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
  async function loadRuns(): Promise<boolean> {
    if (!source_id) return false;
    global_error.hidden = true; required("runs-region").setAttribute("aria-busy", "true");
    for (const control of rows.querySelectorAll<HTMLInputElement | HTMLButtonElement>("input, button")) control.disabled = true;
    previous_page.disabled = true; next_page.disabled = true;
    required("run-count").textContent = "Loading run history…";
    try {
      const result = await list_lane.run<RunList>(apiUrl("/api/runs", { source: source_id, search: search_input.value.trim(), task: task_input.value.trim(), status: status_select.value, limit: page_size, offset }));
      if (!result) return false;
      runs = result.runs; offset = result.offset; next_offset = result.next_offset;
      renderRows(); warnings(required("list-warnings"), result.warnings);
      required("run-count").textContent = result.total_count ? `${offset + 1}–${offset + runs.length} of ${result.total_count.toLocaleString()} runs` : "No matching runs";
      required("page-label").textContent = `Page ${Math.floor(offset / page_size) + 1}`;
      previous_page.disabled = offset === 0; next_page.disabled = next_offset === null;
      const empty = required("list-empty"); empty.hidden = runs.length > 0;
      if (!runs.length) {
        showEmptyRuns(Boolean(search_input.value || task_input.value || status_select.value), result.source);
      }
      const options = required("task-options"); options.replaceChildren();
      for (const task of [...new Set(runs.map(run => run.task).filter((task): task is string => task !== null))].sort()) {
        const option = element("option"); option.value = task; options.append(option);
      }
      required("runs-region").setAttribute("aria-busy", "false"); announce(`${result.total_count} matching runs`);
      return true;
    } catch (error) { showError(global_error, error); renderRows(); required("runs-region").setAttribute("aria-busy", "false"); required("run-count").textContent = "Could not read run history"; return false; }
  }
  function cancelRefresh(): void { refresh_generation++; catalog_lane.cancel(); refresh_button.disabled = false; }
  function hideReview(): void {
    clearTimeout(selection_timeout);
    review_lane.cancel(); log_lane.cancel(); compare_lane.cancel(); review = null;
    log_view = null;
    required("review-section").hidden = true; required("review-empty").hidden = false;
    required<HTMLIFrameElement>("chart-frame").removeAttribute("src"); renderRows();
  }
  function closeReview(): void { cancelRefresh(); showSelection(false); }
  function selectionChanged(): void {
    cancelRefresh(); syncSelection(); showSelection(true);
  }
  function showSelection(debounce: boolean): void {
    clearTimeout(selection_timeout);
    const ids = [...selected];
    if (!ids.length) { hideReview(); announce("Selection cleared"); return; }
    beginReview(ids.length === 1 ? "run" : "compare", ids, "selection");
    const pending_review = review;
    metric_names = [...new Set(ids.flatMap(id => Object.keys(detail_cache.get(id)?.metrics ?? {})))].sort();
    const open = () => {
      if (review !== pending_review) return;
      if (ids.length === 1 && ids[0]) void loadRun(ids[0]); else void loadComparison();
    };
    if (debounce) selection_timeout = setTimeout(open, 180);
    else open();
  }
  function beginReview(kind: "run" | "compare", ids: string[], origin: Review["origin"] = "inspection"): void {
    clearTimeout(selection_timeout);
    review_lane.cancel(); log_lane.cancel(); compare_lane.cancel();
    review = { kind, origin, run_ids: ids, metric_names: [], metric_selection_set: false, tab: "charts", log_stream: "stdout" }; metric_names = []; log_view = null;
    required("review-section").hidden = false; required("review-empty").hidden = true;
    required("review-title").textContent = kind === "run" ? ids[0] ?? "Run" : `${ids.length} runs`;
    required("review-eyebrow").textContent = kind === "run" ? "Run details" : "Experiment comparison";
    required("review-loading").hidden = false; required("review-error").hidden = true;
    required("run-detail").hidden = true; required("compare-detail").hidden = true;
    required("run-metric-controls").hidden = true; required("run-logs").replaceChildren();
    required("chart-card").hidden = true; required<HTMLIFrameElement>("chart-frame").removeAttribute("src");
    const close = required<HTMLButtonElement>("close-review"); close.hidden = origin === "selection";
    close.textContent = selected.size ? "Back to selection" : "Close review";
    warnings(required("review-warnings"), []); syncReviewTabs(); renderRows();
  }
  function syncReviewTabs(): void {
    for (const tab of review_tabs) {
      const button = required<HTMLButtonElement>(`review-tab-${tab}`);
      const active = review?.tab === tab;
      button.disabled = review?.kind === "compare" && tab !== "charts";
      button.setAttribute("aria-selected", String(active)); button.tabIndex = active ? 0 : -1;
      required(`review-panel-${tab}`).hidden = !active;
    }
  }
  function selectReviewTab(tab: ReviewTab): void {
    if (!review || (review.kind === "compare" && tab !== "charts")) return;
    review.tab = tab; syncReviewTabs();
    if (tab === "logs") ensureLog();
    else { log_lane.cancel(); if (log_view) log_view.pending = false; }
  }
  function updateChart(): void {
    if (!review) return;
    const url = apiUrl("/api/chart", { source: source_id, run_id: review.run_ids, metric: review.metric_names });
    required<HTMLIFrameElement>("chart-frame").src = url; required<HTMLAnchorElement>("open-chart").href = url;
    required("chart-card").hidden = false;
    required("chart-note").textContent = review.metric_names.length ? `${review.metric_names.length} selected metrics · at most 600 chart points per series` : "First four recorded metrics · at most 600 chart points per series";
  }
  function metricPicker(parent: HTMLElement, known: string[], on_change: () => void): void {
    parent.replaceChildren();
    const choices = element("div", "", "metric-options");
    const chosen = review?.metric_names ?? [];
    for (const name of [...new Set([...known, ...chosen])].sort()) {
      const label = element("label", "", "metric-choice");
      const input = element("input"); input.type = "checkbox"; input.checked = chosen.includes(name);
      input.disabled = chosen.length >= 6 && !input.checked;
      input.addEventListener("change", () => {
        if (!review) return;
        cancelRefresh(); review.metric_selection_set = true;
        review.metric_names = input.checked ? [...review.metric_names, name] : review.metric_names.filter(item => item !== name);
        metricPicker(parent, known, on_change); on_change();
      }); label.append(input, document.createTextNode(name)); choices.append(label);
    }
    const form = element("form", "", "metric-add");
    const label = element("label", "Exact metric name", "field"); const input = element("input");
    input.type = "text"; input.placeholder = "e.g. validation/loss"; input.maxLength = 1024; label.append(input);
    const button = element("button", "Add metric", "button secondary"); button.type = "submit"; button.disabled = chosen.length >= 6;
    form.addEventListener("submit", event => {
      event.preventDefault(); const name = input.value.trim();
      if (!review || !name || review.metric_names.length >= 6) return;
      cancelRefresh(); review.metric_selection_set = true;
      if (!review.metric_names.includes(name)) review.metric_names.push(name);
      metric_names = [...new Set([...metric_names, name])]; metricPicker(parent, metric_names, on_change); on_change();
    }); form.append(label, button); parent.append(choices, form);
    parent.append(element("p", chosen.length ? "Select up to six metrics. Enter a name to include a metric outside this preview." : "No selection uses the first four recorded metric names.", "muted metric-hint"));
  }
  async function loadLog(view: LogView): Promise<void> {
    const { run_id: id, stream, output, note } = view;
    output.textContent = "Loading log tail…"; note.textContent = "";
    view.pending = true;
    try {
      const log = await log_lane.run<Log>(apiUrl("/api/log", { source: source_id, run_id: id, stream, tail: 100 }));
      if (!log || log_view !== view) return;
      output.textContent = log.missing ? "This log has not been recorded or pulled." : log.content || "The log is empty.";
      note.textContent = log.truncated ? "Showing the last 100 lines, capped at 64 KiB. Use expri runs logs for more output." : "Last 100 lines. Refresh to read updated output.";
      view.loaded = true;
    } catch (error) { if (log_view === view) { output.textContent = errorText(error); note.textContent = "Could not read this log."; } }
    finally { if (log_view === view) view.pending = false; }
  }
  function ensureLog(): void {
    if (review?.kind === "run" && review.tab === "logs" && log_view && !log_view.loaded && !log_view.pending) void loadLog(log_view);
  }
  function renderDetail(detail: Detail): void {
    const parent = required("run-detail"); parent.replaceChildren(); parent.hidden = false;
    const grid = element("div", "", "detail-grid"); const overview = card("Overview");
    overview.append(facts([["Task", detail.run.task ?? "—"], ["Status", detail.run.status], ["Started", dateText(detail.run.started_at)], ["Finished", dateText(detail.run.finished_at)], ["Duration", formatDuration(detail.run)], ["Exit code", formatValue(detail.run.exit_code)]]));
    const command = jsonObject(detail.state)["command"];
    if (command !== undefined && command !== null) { overview.append(element("h4", "Command"), element("pre", formatValue(command), "command")); }
    for (const [title, value] of [["Run record", detail.state], ["Source provenance", detail.snapshot], ["Environment", detail.environment], ["Cached results", detail.cache]] as [string, Json][]) {
      if (value !== null) overview.append(records(title, value));
    }
    const params = card(detail.params_truncated ? "Parameters · preview" : "Parameters");
    const values = Object.entries(jsonObject(detail.params));
    if (values.length) {
      const table = element("table", "", "params-table"); const body = element("tbody");
      for (const [name, value] of values) { const row = element("tr"); const heading = element("th", name); heading.scope = "row"; const cell = element("td"); cell.append(element("code", formatValue(value))); row.append(heading, cell); body.append(row); }
      table.append(body); params.append(table);
    } else params.append(element("p", "No parameters were recorded or pulled.", "muted"));
    grid.append(overview, params); parent.append(grid);
    const metrics = card("Metric summaries");
    if (detail.metrics_error) { const error = element("p", detail.metrics_error, "notice error"); metrics.append(error); }
    const names = Object.keys(detail.metrics).sort();
    if (names.length) {
      const scroll = element("div", "", "table-scroll"); const table = element("table"); const head = element("thead"); const headings = element("tr");
      for (const name of ["Metric", "Points", "Last", "Minimum", "Maximum"]) headings.append(element("th", name)); head.append(headings); table.append(head);
      const body = element("tbody");
      for (const name of names) { const metric = detail.metrics[name]; if (!metric) continue; const row = element("tr"); row.append(element("td", name), element("td", String(metric.count), "number"), element("td", formatNumber(metric.last.value), "number"), element("td", formatNumber(metric.min.value), "number"), element("td", formatNumber(metric.max.value), "number")); body.append(row); }
      table.append(body); scroll.append(table); metrics.append(scroll);
    } else metrics.append(element("p", "No metric summaries are available. Record metrics in outputs/metrics.jsonl or pull remote metrics.", "muted"));
    if (detail.metrics_truncated) metrics.append(element("p", `Showing ${names.length} of ${detail.metric_count} summaries. Use Chart metrics to select another series by its exact name.`, "muted"));
    parent.append(metrics); metric_names = names;
    if (review && !review.metric_selection_set) { review.metric_names = names.slice(0, 4); review.metric_selection_set = true; }
    metricPicker(required("run-metric-options"), names, updateChart); required("run-metric-controls").hidden = false;
    const logs = card("Log tail"); const tabs = element("div", "", "log-tabs"); tabs.setAttribute("role", "tablist"); tabs.setAttribute("aria-label", "Log stream");
    const output = element("pre", "", "log-content"); output.id = "log-output"; output.setAttribute("role", "tabpanel");
    const note = element("p", "", "log-note muted");
    const buttons: HTMLButtonElement[] = [];
    const selected_stream = review?.log_stream ?? "stdout";
    for (const stream of ["stdout", "stderr"] as const) {
      const button = element("button", stream, "log-tab"); button.type = "button"; button.id = `log-tab-${stream}`; button.setAttribute("role", "tab"); button.setAttribute("aria-controls", output.id); button.setAttribute("aria-selected", String(stream === selected_stream)); button.tabIndex = stream === selected_stream ? 0 : -1;
      button.addEventListener("click", () => {
        log_lane.cancel();
        if (review) review.log_stream = stream;
        for (const item of buttons) { item.setAttribute("aria-selected", String(item === button)); item.tabIndex = item === button ? 0 : -1; }
        output.setAttribute("aria-labelledby", button.id);
        log_view = { run_id: detail.run.run_id, stream, output, note, loaded: false, pending: false }; ensureLog();
      });
      button.addEventListener("keydown", event => {
        if (["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) {
          event.preventDefault();
          const next = event.key === "Home" ? buttons[0] : event.key === "End" ? buttons.at(-1) : buttons.find(item => item !== button);
          next?.focus(); next?.click();
        }
      });
      buttons.push(button); tabs.append(button);
    }
    output.setAttribute("aria-labelledby", `log-tab-${selected_stream}`); logs.append(tabs, output, note); required("run-logs").replaceChildren(logs);
    log_view = { run_id: detail.run.run_id, stream: selected_stream, output, note, loaded: false, pending: false };
    ensureLog(); updateChart();
  }
  async function openRun(id: string, origin: Review["origin"] = "inspection", previous?: Review): Promise<void> {
    beginReview("run", [id], origin);
    if (previous && review) { review.metric_names = [...previous.metric_names]; review.metric_selection_set = previous.metric_selection_set; review.tab = previous.tab; review.log_stream = previous.log_stream; syncReviewTabs(); }
    await loadRun(id);
  }
  async function loadRun(id: string): Promise<void> {
    try {
      const detail = await review_lane.run<Detail>(apiUrl("/api/run", { source: source_id, run_id: id }));
      if (!detail) return;
      detail_cache.delete(id); detail_cache.set(id, detail);
      while (detail_cache.size > 9) { const oldest = detail_cache.keys().next().value; if (oldest === undefined) break; detail_cache.delete(oldest); }
      required("review-loading").hidden = true;
      warnings(required("review-warnings"), detail.warnings); renderDetail(detail); announce(`Opened ${id}`);
    } catch (error) { required("review-loading").hidden = true; showError(required("review-error"), error); }
  }
  function renderComparison(result: Comparison): void {
    const comparison = result.comparison;
    const parent = required("comparison-values"); parent.replaceChildren();
    const table = element("table", "", "comparison-table"); const head = element("thead"); const headings = element("tr"); headings.append(element("th", "Run"));
    for (const name of comparison.metric_names) headings.append(element("th", name)); head.append(headings); table.append(head);
    const body = element("tbody");
    for (const run of comparison.runs) {
      const row = element("tr"); const label = element("th", run.run_id); label.scope = "row"; row.append(label);
      for (const name of comparison.metric_names) { const point = run.values[name]; const cell = element("td", point ? formatNumber(point.value) : "—", "number"); if (point) cell.append(element("span", `step ${point.step}`, "cell-step")); row.append(cell); }
      body.append(row);
    }
    table.append(body); parent.append(table);
    if (!comparison.metric_names.length) parent.append(element("p", "No recorded metrics match this selection.", "muted"));
    warnings(required("review-warnings"), comparison.warnings);
  }
  async function loadComparison(): Promise<void> {
    if (!review || review.kind !== "compare") return;
    required("review-error").hidden = true; required("comparison-values").setAttribute("aria-busy", "true");
    required("chart-card").hidden = true;
    try {
      const result = await compare_lane.run<Comparison>(apiUrl("/api/compare", { source: source_id, run_id: review.run_ids, metric: review.metric_names, reduction: required<HTMLSelectElement>("reduction-select").value }));
      if (!result || !review || review.kind !== "compare") return;
      metric_names = [...new Set([...metric_names, ...result.comparison.metric_names])].sort();
      if (!review.metric_selection_set) { review.metric_names = result.comparison.metric_names.slice(0, 4); review.metric_selection_set = true; }
      required("review-loading").hidden = true; required("compare-detail").hidden = false;
      renderComparison(result); required("comparison-values").setAttribute("aria-busy", "false");
      metricPicker(required("compare-metric-options"), metric_names, () => { void loadComparison(); }); updateChart();
    } catch (error) { required("review-loading").hidden = true; required("comparison-values").setAttribute("aria-busy", "false"); showError(required("review-error"), error); }
  }
  function openComparison(): void {
    const ids = [...selected]; if (ids.length < 2 || ids.length > 8) return;
    beginReview("compare", ids, "selection");
    metric_names = [...new Set(ids.flatMap(id => Object.keys(detail_cache.get(id)?.metrics ?? {})))].sort();
    void loadComparison();
  }
  function sourceNote(): void {
    const source = sources.find(item => item.source_id === source_id);
    const note = required("source-note"); note.hidden = !source;
    note.textContent = source?.kind === "service" ? "Synced results · Updates arrive from workers. Refresh to read the latest synced files."
      : source?.kind === "cached" ? "Cached remote results · Recorded status may be older than the remote run. Pull updated results with expri runs pull, then Refresh here."
      : "Local results · Status comes from recorded run files. Refresh to read the latest changes.";
  }
  async function refresh(): Promise<void> {
    const generation = ++refresh_generation;
    refresh_button.disabled = true; global_error.hidden = true;
    try {
      const catalog = await catalog_lane.run<Catalog>("/api/catalog"); if (!catalog) return;
      if (generation !== refresh_generation) return;
      sources = catalog.sources;
      access_mode = catalog.access_mode ?? "local";
      page_size = access_mode === "hosted" ? 20 : 100;
      required("logout-form").hidden = access_mode !== "hosted";
      required("dashboard-kind").textContent = access_mode === "hosted" ? "expri · Synced experiment review" : "expri · Local experiment review";
      const previous_source = source_id;
      if (!sources.some(source => source.source_id === source_id) || !source_select.dataset.initialized) {
        source_id = sources.some(source => source.source_id === catalog.initial_source) ? catalog.initial_source : sources[0]?.source_id ?? "";
      }
      source_select.dataset.initialized = "true";
      if (source_id !== previous_source) { runs = []; selected.clear(); hideReview(); syncSelection(); }
      source_select.replaceChildren();
      for (const source of sources) { const option = element("option", source.kind === "service" ? `${source.label} · Synced` : source.label); option.value = source.source_id; source_select.append(option); }
      if (!sources.length) { const option = element("option", "No synced sources"); option.value = ""; source_select.append(option); }
      source_select.value = source_id; source_select.disabled = !sources.length;
      for (const control of [search_input, task_input, status_select, required<HTMLButtonElement>("clear-filters")]) control.disabled = !sources.length;
      required("project-name").textContent = catalog.project_name; document.title = `expri · ${catalog.project_name}`;
      warnings(required("catalog-warnings"), catalog.warnings); sourceNote(); detail_cache.clear(); offset = 0;
      if (!source_id) {
        list_lane.cancel(); runs = []; next_offset = null; renderRows();
        previous_page.disabled = true; next_page.disabled = true;
        required("run-count").textContent = "No synced runs yet"; required("page-label").textContent = "Page 1";
        required("task-options").replaceChildren(); warnings(required("list-warnings"), []);
        required("runs-region").setAttribute("aria-busy", "false"); required("review-empty").hidden = true;
        showEmptyRuns(false); announce("No synced runs yet");
        required("updated-at").textContent = "Refresh after syncing your first run.";
        return;
      }
      const current_review = review;
      const current_source = source_id;
      if (!await loadRuns() || generation !== refresh_generation) return;
      if (current_review === review && current_source === source_id) {
        if (current_review?.kind === "run") { const id = current_review.run_ids[0]; if (id) await openRun(id, current_review.origin, current_review); }
        else if (current_review?.kind === "compare") await loadComparison();
      }
      if (generation === refresh_generation) required("updated-at").textContent = `Last checked at ${new Date().toLocaleTimeString()}. Refresh for updates.`;
    } catch (error) { if (generation === refresh_generation) { showError(global_error, error); required("review-loading").hidden = true; } }
    finally { if (generation === refresh_generation) refresh_button.disabled = false; }
  }
  function resetFilters(): void {
    cancelRefresh(); list_lane.cancel(); clearTimeout(search_timeout); offset = 0;
    selected.clear(); syncSelection(); hideReview(); runs = []; renderRows();
    required("list-empty").hidden = true; required("run-count").textContent = "Applying filters…";
    required("runs-region").setAttribute("aria-busy", "true"); previous_page.disabled = true; next_page.disabled = true;
  }
  function filtersChanged(): void { resetFilters(); void loadRuns(); }
  for (const input of [search_input, task_input]) input.addEventListener("input", () => { resetFilters(); search_timeout = setTimeout(() => { void loadRuns(); }, 250); });
  status_select.addEventListener("change", filtersChanged);
  required("clear-filters").addEventListener("click", () => { search_input.value = ""; task_input.value = ""; status_select.value = ""; clearTimeout(search_timeout); filtersChanged(); });
  previous_page.addEventListener("click", () => { cancelRefresh(); offset = Math.max(0, offset - page_size); void loadRuns(); });
  next_page.addEventListener("click", () => { if (next_offset !== null) { cancelRefresh(); offset = next_offset; void loadRuns(); } });
  source_select.addEventListener("change", () => { cancelRefresh(); clearTimeout(search_timeout); source_id = source_select.value; runs = []; offset = 0; selected.clear(); detail_cache.clear(); hideReview(); syncSelection(); sourceNote(); void loadRuns(); });
  refresh_button.addEventListener("click", () => { clearTimeout(search_timeout); void refresh(); });
  required("clear-selection").addEventListener("click", () => { selected.clear(); selectionChanged(); });
  required("compare-button").addEventListener("click", () => { cancelRefresh(); clearTimeout(selection_timeout); openComparison(); });
  required("close-review").addEventListener("click", closeReview);
  required("reduction-select").addEventListener("change", () => { cancelRefresh(); void loadComparison(); });
  for (const tab of review_tabs) {
    const button = required<HTMLButtonElement>(`review-tab-${tab}`);
    button.addEventListener("click", () => { selectReviewTab(tab); });
    button.addEventListener("keydown", event => {
      if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key) || !review) return;
      event.preventDefault();
      const available = review.kind === "compare" ? ["charts"] as ReviewTab[] : review_tabs;
      const index = available.indexOf(tab);
      const next = event.key === "Home" ? available[0] : event.key === "End" ? available.at(-1)
        : available[(index + (event.key === "ArrowRight" ? 1 : available.length - 1)) % available.length];
      if (next) { selectReviewTab(next); required<HTMLButtonElement>(`review-tab-${next}`).focus(); }
    });
  }
  void refresh();
}

if (typeof document !== "undefined") startDashboard();
