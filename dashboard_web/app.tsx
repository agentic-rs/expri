import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import { createChartController, type ChartAxis, type ChartTimeZone } from "./interactive_charts";
import {
  AutoRefresh,
  type RefreshAvailability,
  type RefreshOutcome,
  type RefreshState,
} from "./auto_refresh";
import {
  apiUrl,
  errorText,
  parseRunDeepLink,
  RequestError,
  RequestLane,
  type ArtifactCatalog,
  type Catalog,
  type ChartRefreshOutcome,
  type Comparison,
  type ComparisonReduction,
  type DashboardOptions,
  type Detail,
  type Log,
  type LogView,
  type Review,
  type ReviewTab,
  type Run,
  type RunList,
  type Source,
  type Updates,
} from "./dashboard_model";
import { artifactCanSelect } from "./dashboard_files";
import {
  DashboardView,
  type DashboardActions,
  type DashboardSnapshot,
  type DashboardStore,
  type DashboardUi,
} from "./dashboard_view";
export * from "./dashboard_model";
export * from "./interactive_charts";
export * from "./auto_refresh";
export * from "./dashboard_files";

function required<T extends HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (!node) throw new Error(`Missing dashboard element: ${id}`);
  return node as T;
}
export function startDashboard(options: DashboardOptions = {}): () => void {
  const page_document = document;
  const page_window = typeof window === "undefined" ? null : window;
  const catalog_lane = new RequestLane(() => pending_deep_link);
  const list_lane = new RequestLane(() => pending_deep_link);
  const review_lane = new RequestLane(() => pending_deep_link);
  const log_lane = new RequestLane(() => pending_deep_link);
  const compare_lane = new RequestLane(() => pending_deep_link);
  const chart_lane = new RequestLane(() => pending_deep_link);
  const deep_link_lane = new RequestLane(() => pending_deep_link);
  const artifact_lane = new RequestLane(() => pending_deep_link);
  const quiet_lanes = {
    updates: new RequestLane(() => pending_deep_link),
    catalog: new RequestLane(() => pending_deep_link),
    list: new RequestLane(() => pending_deep_link),
    detail: new RequestLane(() => pending_deep_link),
    comparison: new RequestLane(() => pending_deep_link),
    log: new RequestLane(() => pending_deep_link),
    chart: new RequestLane(() => pending_deep_link),
    artifacts: new RequestLane(() => pending_deep_link),
  };
  const foreground_lanes = [
    catalog_lane,
    list_lane,
    review_lane,
    log_lane,
    compare_lane,
    chart_lane,
    deep_link_lane,
    artifact_lane,
  ];
  const now = options.refresh_clock?.now ?? (() => Date.now());
  let auto_refresh: AutoRefresh | null = null;
  let quiet_generation = 0;
  let updates_supported = true;
  let catalog_revision: string | undefined;
  const source_revisions = new Map<string, string>();
  const detail_revisions = new Map<string, string>();
  const chart_revisions = new Map<string, string>();
  const log_revisions = new Map<string, string | null>();
  const artifact_revisions = new Map<string, string>();
  let comparison_revision: { context: string; revision: string } | null = null;
  let last_catalog: Catalog | null = null;
  let last_run_list: RunList | null = null;
  let last_run_list_context = "";
  let list_ready = false;
  let last_catalog_snapshot = -Infinity;
  let last_list_snapshot = -Infinity;
  let last_full_snapshot = -Infinity;
  let last_checked: number | null = null;
  let cached_chart: { context: string; html: string } | null = null;
  let chart_error_context: string | null = null;
  let sources: Source[] = [];
  let source_id = "local";
  let pending_deep_link = parseRunDeepLink(page_window?.location.search ?? "");
  let deep_link_attempted_source: string | null = null;
  let access_mode: "local" | "hosted" = "local";
  let page_size = 100;
  let runs: Run[] = [];
  let offset = 0;
  let next_offset: number | null = null;
  let review: Review | null = null;
  let review_version = 0;
  let missing_review: Review | null = null;
  let metric_names: string[] = [];
  let search_timeout: ReturnType<typeof setTimeout> | undefined;
  let selection_timeout: ReturnType<typeof setTimeout> | undefined;
  let refresh_generation = 0;
  let log_view: LogView | null = null;
  let rendered_detail: Detail | null = null;
  let rendered_comparison: Comparison | null = null;
  let artifacts: ArtifactCatalog | null = null;
  let artifacts_loaded = false;
  const selected_files = new Set<string>();
  const selected = new Set<string>();
  const detail_cache = new Map<string, Detail>();

  let x_axis: ChartAxis = "step";
  let time_zone: ChartTimeZone = "local";
  let reduction: ComparisonReduction = "last";
  let catalog_initialized = false;
  let disposed = false;
  const ui: DashboardUi = {
    project_name: "Experiment workspace",
    search: "",
    task: "",
    status: "",
    refreshing: true,
    controls_disabled: true,
    auto_enabled: true,
    freshness: "Loading recorded results…",
    source_note: null,
    global_error: null,
    catalog_warnings: [],
    list_warnings: [],
    run_count: "Loading run history…",
    page_label: "Page 1",
    list_busy: true,
    previous_disabled: true,
    next_disabled: true,
    list_empty: {
      title: "Loading runs",
      message: "Your run history will appear here.",
      setup: false,
    },
    review_empty: true,
    review_loading: false,
    review_error: null,
    review_warnings: [],
    chart_visible: false,
    chart_busy: false,
    chart_note: "Recorded values by training step.",
    chart_url: "",
    chart_error: null,
    comparison_busy: false,
    log_content: "",
    log_note: "",
    artifacts_loading: false,
    artifacts_error: null,
    live_status: "",
  };
  function snapshot(): DashboardSnapshot {
    return {
      ...ui,
      sources,
      source_id,
      access_mode,
      runs,
      selected: [...selected],
      review: review
        ? { ...review, run_ids: [...review.run_ids], metric_names: [...review.metric_names] }
        : null,
      review_version,
      detail: rendered_detail,
      comparison: rendered_comparison,
      artifacts,
      selected_files: [...selected_files],
      metric_names: [...metric_names],
      x_axis,
      time_zone,
      reduction,
    };
  }
  let view = snapshot();
  const listeners = new Set<() => void>();
  const store: DashboardStore = {
    subscribe: (listener) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    get_snapshot: () => view,
  };
  function publish(): void {
    if (disposed) return;
    view = snapshot();
    flushSync(() => {
      for (const listener of listeners) listener();
    });
  }
  function updateUi(values: Partial<DashboardUi>): void {
    Object.assign(ui, values);
    publish();
  }
  const actions: DashboardActions = {
    filter: (field, value) => {
      discardDeepLink();
      ui[field] = value;
      resetFilters();
      if (field === "status") void loadRuns();
      else
        search_timeout = setTimeout(() => {
          search_timeout = undefined;
          void loadRuns();
        }, 250);
    },
    clear_filters: () => {
      discardDeepLink();
      ui.search = "";
      ui.task = "";
      ui.status = "";
      clearSearchTimeout();
      filtersChanged();
    },
    source: (value) => {
      discardDeepLink();
      cancelRefresh();
      clearSearchTimeout();
      source_id = value;
      runs = [];
      offset = 0;
      selected.clear();
      detail_cache.clear();
      hideReview();
      syncSelection();
      sourceNote();
      void loadRuns();
    },
    refresh: () => {
      clearSearchTimeout();
      void refresh();
    },
    auto_refresh: (enabled) => {
      ui.auto_enabled = enabled;
      auto_refresh?.setEnabled(enabled);
      publish();
    },
    previous: () => {
      discardDeepLink();
      cancelRefresh();
      offset = Math.max(0, offset - page_size);
      void loadRuns();
    },
    next: () => {
      discardDeepLink();
      if (next_offset !== null) {
        cancelRefresh();
        offset = next_offset;
        void loadRuns();
      }
    },
    select_run: (id, checked) => {
      discardDeepLink();
      if (checked && selected.size >= 8) return;
      checked ? selected.add(id) : selected.delete(id);
      selectionChanged();
    },
    inspect_run: (id) => {
      discardDeepLink();
      cancelRefresh();
      clearSelectionTimeout();
      void openRun(id);
    },
    clear_selection: () => {
      discardDeepLink();
      selected.clear();
      selectionChanged();
    },
    compare: () => {
      discardDeepLink();
      cancelRefresh();
      clearSelectionTimeout();
      openComparison();
    },
    close: closeReview,
    tab: selectReviewTab,
    metric: changeMetric,
    add_metric: addMetric,
    reduction: (value) => {
      if (reduction === value) return;
      reduction = value;
      publish();
      cancelRefresh();
      void loadComparison();
    },
    axis: (value) => {
      void changeChartAxis(value);
    },
    time_zone: changeChartTimeZone,
    select_file: (path, checked) => {
      const file = artifacts?.files.find((file) => file.path === path);
      const id = review?.run_ids[0];
      if (
        !file ||
        !id ||
        !artifactCanSelect(file, artifacts?.pull_scope ?? null, source_id, id) ||
        (checked && selected_files.size >= 64)
      )
        return;
      checked ? selected_files.add(path) : selected_files.delete(path);
      publish();
    },
    clear_files: () => {
      selected_files.clear();
      publish();
    },
    refresh_files: () => {
      cancelQuietRefresh();
      void loadArtifacts();
    },
    log_stream: (value) => {
      if (!review || review.kind !== "run" || !rendered_detail) return;
      cancelQuietRefresh();
      log_lane.cancel();
      review.log_stream = value;
      log_view = {
        run_id: rendered_detail.run.run_id,
        stream: value,
        loaded: false,
        pending: false,
      };
      publish();
      ensureLog();
    },
  };
  const react_root = createRoot(required("dashboard-root"));
  flushSync(() => react_root.render(<DashboardView store={store} actions={actions} />));
  const chart_controller =
    options.chart_controller ?? createChartController(required<HTMLIFrameElement>("chart-frame"));
  chart_controller.setTimeZone(time_zone);

  function abortQuietRequests(): void {
    quiet_generation++;
    for (const lane of Object.values(quiet_lanes)) lane.cancel();
    deep_link_lane.cancel();
  }
  function cancelQuietRefresh(): void {
    if (auto_refresh) auto_refresh.interrupt();
    else abortQuietRequests();
  }
  function availability(): RefreshAvailability {
    if (document.visibilityState === "hidden") return "hidden";
    if (typeof navigator !== "undefined" && navigator.onLine === false) return "offline";
    return foreground_lanes.some((lane) => lane.pending) ||
      search_timeout !== undefined ||
      selection_timeout !== undefined
      ? "busy"
      : "ready";
  }
  function showFreshness(state: RefreshState): void {
    const checked =
      last_checked === null
        ? "Waiting for updates"
        : `Last checked at ${new Date(last_checked).toLocaleTimeString()}`;
    const activity = !state.enabled
      ? "Auto updates off"
      : state.availability === "hidden"
        ? "Auto updates paused while hidden"
        : state.availability === "offline"
          ? "Offline · updates resume when connected"
          : state.failed
            ? `Updates unavailable · retrying in ${state.retry_ms / 1_000}s`
            : `Auto updates every ${state.interval_ms / 1_000}s`;
    updateUi({ freshness: `${checked} · ${activity}`, auto_enabled: state.enabled });
  }
  function listUrl(): string {
    return apiUrl("/api/runs", {
      source: source_id,
      search: ui.search.trim(),
      task: ui.task.trim(),
      status: ui.status,
      limit: page_size,
      offset,
    });
  }
  function chartUrl(): string {
    return apiUrl("/api/chart", {
      source: source_id,
      run_id: review?.run_ids ?? [],
      metric: review?.metric_names ?? [],
      x_axis,
    });
  }
  function comparisonUrl(): string {
    return apiUrl("/api/compare", {
      source: source_id,
      run_id: review?.run_ids ?? [],
      metric: review?.metric_names ?? [],
      reduction,
    });
  }
  function boundedSet<T>(cache: Map<string, T>, key: string, value: T): void {
    cache.delete(key);
    cache.set(key, value);
    while (cache.size > 16) {
      const oldest = cache.keys().next().value;
      if (oldest === undefined) break;
      cache.delete(oldest);
    }
  }

  function clearSearchTimeout(): void {
    clearTimeout(search_timeout);
    search_timeout = undefined;
  }
  function clearSelectionTimeout(): void {
    clearTimeout(selection_timeout);
    selection_timeout = undefined;
  }
  function announce(message: string): void {
    updateUi({ live_status: message });
  }
  function showEmptyRuns(filtered: boolean, source?: Source): void {
    const synced = access_mode === "hosted" || source?.kind === "service";
    updateUi({
      list_empty: {
        title: synced && !filtered ? "No synced runs yet" : "No runs found",
        message: filtered
          ? "Try changing the filters."
          : synced
            ? "Sync results from a worker to see them here."
            : source?.kind === "cached"
              ? "Pull results with expri runs pull, then Refresh."
              : "Start an experiment with expri run to record results here.",
        setup: synced && !filtered,
      },
    });
  }
  function syncSelection(): void {
    publish();
  }
  function applyRunList(result: RunList, context: string, quiet = false): void {
    const unchanged =
      last_run_list_context === context && JSON.stringify(last_run_list) === JSON.stringify(result);
    const was_ready = list_ready;
    list_ready = true;
    last_run_list = result;
    last_run_list_context = context;
    last_list_snapshot = now();
    if (quiet && unchanged && was_ready) return;
    runs = result.runs;
    offset = result.offset;
    next_offset = result.next_offset;
    updateUi({
      list_warnings: result.warnings,
      run_count: result.total_count
        ? `${offset + 1}–${offset + runs.length} of ${result.total_count.toLocaleString()} runs`
        : "No matching runs",
      page_label: `Page ${Math.floor(offset / page_size) + 1}`,
      previous_disabled: offset === 0,
      next_disabled: next_offset === null,
      list_empty: null,
      list_busy: false,
    });
    if (!runs.length) showEmptyRuns(Boolean(ui.search || ui.task || ui.status), result.source);
    announce(`${result.total_count} matching runs`);
  }
  async function loadRuns(): Promise<boolean> {
    if (!source_id) return false;
    list_ready = false;
    updateUi({
      global_error: null,
      list_busy: true,
      previous_disabled: true,
      next_disabled: true,
      run_count: "Loading run history…",
    });
    try {
      const url = listUrl();
      const result = await list_lane.run<RunList>(url);
      if (!result) return false;
      applyRunList(result, url);
      return true;
    } catch (error) {
      updateUi({
        global_error: errorText(error),
        list_busy: false,
        run_count: "Could not read run history",
      });
      return false;
    }
  }
  function cancelRefresh(): void {
    cancelQuietRefresh();
    refresh_generation++;
    catalog_lane.cancel();
    updateUi({ refreshing: false });
  }
  function hideReview(): void {
    clearSelectionTimeout();
    review_lane.cancel();
    log_lane.cancel();
    compare_lane.cancel();
    chart_lane.cancel();
    resetArtifacts();
    review = null;
    missing_review = null;
    log_view = null;
    rendered_detail = null;
    rendered_comparison = null;
    clearChartError();
    required<HTMLIFrameElement>("chart-frame").removeAttribute("src");
    updateUi({ review_empty: true, chart_visible: false, log_content: "", log_note: "" });
  }
  function closeReview(): void {
    discardDeepLink();
    cancelRefresh();
    showSelection(false);
  }
  function selectionChanged(): void {
    cancelRefresh();
    syncSelection();
    showSelection(true);
  }
  function showSelection(debounce: boolean): void {
    clearSelectionTimeout();
    const ids = [...selected];
    if (!ids.length) {
      hideReview();
      announce("Selection cleared");
      return;
    }
    beginReview(ids.length === 1 ? "run" : "compare", ids, "selection");
    const pending_review = review;
    metric_names = [
      ...new Set(ids.flatMap((id) => Object.keys(detail_cache.get(id)?.metrics ?? {}))),
    ].sort();
    const open = () => {
      selection_timeout = undefined;
      if (review !== pending_review) return;
      if (ids.length === 1 && ids[0]) void loadRun(ids[0]);
      else void loadComparison();
    };
    if (debounce) selection_timeout = setTimeout(open, 180);
    else open();
  }
  function beginReview(
    kind: "run" | "compare",
    ids: string[],
    origin: Review["origin"] = "inspection",
  ): void {
    clearSelectionTimeout();
    review_lane.cancel();
    log_lane.cancel();
    compare_lane.cancel();
    chart_lane.cancel();
    resetArtifacts();
    detail_revisions.clear();
    chart_revisions.clear();
    log_revisions.clear();
    comparison_revision = null;
    missing_review = null;
    clearChartError();
    review_version++;
    review = {
      kind,
      origin,
      run_ids: ids,
      metric_names: [],
      metric_selection_set: false,
      tab: "charts",
      log_stream: "stdout",
    };
    metric_names = [];
    log_view = null;
    rendered_detail = null;
    rendered_comparison = null;
    cached_chart = null;
    required<HTMLIFrameElement>("chart-frame").removeAttribute("src");
    updateUi({
      review_empty: false,
      review_loading: true,
      review_error: null,
      review_warnings: [],
      chart_visible: false,
      chart_busy: false,
      log_content: "",
      log_note: "",
    });
  }
  function syncReviewTabs(): void {
    publish();
  }
  function selectReviewTab(tab: ReviewTab): void {
    if (!review || (review.kind === "compare" && tab !== "charts")) return;
    cancelQuietRefresh();
    review.tab = tab;
    syncReviewTabs();
    if (tab === "files") ensureArtifacts();
    else {
      artifact_lane.cancel();
      updateUi({ artifacts_loading: false });
    }
    if (tab === "logs") ensureLog();
    else {
      log_lane.cancel();
      if (log_view) log_view.pending = false;
    }
  }
  function updateChart(): void {
    if (!review) return;
    const url = chartUrl();
    const frame = required<HTMLIFrameElement>("chart-frame");
    if ((cached_chart?.context ?? frame.getAttribute("src")) !== url) {
      chart_lane.cancel();
      frame.src = url;
      chart_revisions.delete(url);
      cached_chart = null;
      clearChartError();
      updateUi({ chart_busy: false });
    }
    chartControls(url);
  }
  function chartControls(url: string): void {
    if (!review) return;
    const metrics = review.metric_names.length
      ? `${review.metric_names.length} selected metrics`
      : "First four recorded metrics";
    const axis =
      x_axis === "elapsed"
        ? "Time since each run’s first timestamped metric event"
        : x_axis === "wall_clock"
          ? `Recorded date & time in ${time_zone === "utc" ? "UTC" : "your local timezone"}`
          : "Global step";
    updateUi({
      chart_url: url,
      chart_visible: true,
      chart_note: `${metrics} · ${axis} · at most 600 chart points per series`,
    });
  }
  function syncChartChoices(): void {
    publish();
  }
  function changeChartTimeZone(value: ChartTimeZone): void {
    if (value === time_zone) return;
    time_zone = value;
    syncChartChoices();
    chart_controller.setTimeZone(value);
    if (review && ui.chart_visible) chartControls(chartUrl());
  }
  function clearChartError(context?: string): void {
    if (context !== undefined && chart_error_context !== context) return;
    chart_error_context = null;
    updateUi({ chart_error: null });
  }
  async function changeChartAxis(value: ChartAxis): Promise<void> {
    if (value === x_axis) return;
    cancelRefresh();
    chart_lane.cancel();
    clearChartError();
    x_axis = value;
    syncChartChoices();
    if (!review || !ui.chart_visible) return;
    const current_review = review,
      source = source_id,
      url = chartUrl();
    chartControls(url);
    if (chart_controller.previewStatus() !== "ready" || chart_controller.isInteracting()) {
      updateChart();
      return;
    }
    updateUi({ chart_busy: true });
    try {
      const html = await chart_lane.runText(url);
      if (
        html === undefined ||
        review !== current_review ||
        source_id !== source ||
        chartUrl() !== url
      )
        return;
      if (chart_controller.replacePreview(html)) {
        cached_chart = { context: url, html };
        chart_revisions.delete(url);
        clearChartError(url);
      } else updateChart();
    } catch (error) {
      if (review === current_review && source_id === source && chartUrl() === url) {
        chart_error_context = url;
        updateUi({
          chart_error: `Could not load the selected axis. Showing the last loaded chart. ${errorText(error)}`,
        });
      }
    } finally {
      if (review === current_review && source_id === source && chartUrl() === url)
        updateUi({ chart_busy: false });
    }
  }
  function changeMetric(name: string, checked: boolean): void {
    if (
      !review ||
      (checked && !review.metric_names.includes(name) && review.metric_names.length >= 6)
    )
      return;
    cancelRefresh();
    review.metric_selection_set = true;
    review.metric_names = checked
      ? [...new Set([...review.metric_names, name])]
      : review.metric_names.filter((item) => item !== name);
    publish();
    if (review.kind === "compare") void loadComparison();
    else updateChart();
  }
  function addMetric(name: string): void {
    if (!review || !name || review.metric_names.length >= 6) return;
    metric_names = [...new Set([...metric_names, name])];
    changeMetric(name, true);
  }
  async function loadLog(view: LogView): Promise<void> {
    const { run_id: id, stream } = view;
    updateUi({ log_content: "Loading log tail…", log_note: "" });
    view.pending = true;
    try {
      const log = await log_lane.run<Log>(
        apiUrl("/api/log", { source: source_id, run_id: id, stream, tail: 100 }),
      );
      if (!log || log_view !== view) return;
      updateUi({
        log_content: log.missing
          ? "This log has not been recorded or pulled."
          : log.content || "The log is empty.",
        log_note: log.truncated
          ? "Showing the last 100 lines, capped at 64 KiB. Use expri runs logs for more output."
          : "Last 100 lines. Refresh to read updated output.",
      });
      view.loaded = true;
    } catch (error) {
      if (log_view === view)
        updateUi({ log_content: errorText(error), log_note: "Could not read this log." });
    } finally {
      if (log_view === view) view.pending = false;
    }
  }
  function ensureLog(): void {
    if (
      review?.kind === "run" &&
      review !== missing_review &&
      review.tab === "logs" &&
      log_view &&
      !log_view.loaded &&
      !log_view.pending
    )
      void loadLog(log_view);
  }
  function containsFocus(parent: HTMLElement): boolean {
    return parent.contains(document.activeElement);
  }
  function renderDetail(detail: Detail, preserve = false): boolean {
    const names = Object.keys(detail.metrics).sort(),
      picker = required("run-metric-options");
    const choices_changed = JSON.stringify(metric_names) !== JSON.stringify(names);
    if (preserve && choices_changed && containsFocus(picker)) return false;
    rendered_detail = detail;
    metric_names = names;
    if (review && !review.metric_selection_set) {
      review.metric_names = names.slice(0, 4);
      review.metric_selection_set = true;
    }
    publish();
    if (preserve && log_view?.run_id === detail.run.run_id) return true;
    log_view = {
      run_id: detail.run.run_id,
      stream: review?.log_stream ?? "stdout",
      loaded: false,
      pending: false,
    };
    ensureLog();
    ensureArtifacts();
    if (!preserve || !required<HTMLIFrameElement>("chart-frame").src) updateChart();
    return true;
  }
  async function openRun(
    id: string,
    origin: Review["origin"] = "inspection",
    previous?: Review,
  ): Promise<void> {
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
  function cacheDetail(detail: Detail): void {
    const id = detail.run.run_id;
    detail_cache.delete(id);
    detail_cache.set(id, detail);
    while (detail_cache.size > 9) {
      const oldest = detail_cache.keys().next().value;
      if (oldest === undefined) break;
      detail_cache.delete(oldest);
    }
  }
  function deepLinkSource(): Source | undefined {
    const link = pending_deep_link;
    return link
      ? sources.find(
          (source) => source.project_id === link.project_id && source.origin === link.origin,
        )
      : undefined;
  }
  function discardDeepLink(): void {
    if (!pending_deep_link) return;
    pending_deep_link = null;
    deep_link_lane.cancel();
    sourceNote();
  }
  async function resolveDeepLink(): Promise<boolean> {
    const link = pending_deep_link,
      source = deepLinkSource();
    if (!link || !source || source.source_id !== source_id) return false;
    deep_link_attempted_source = source.source_id;
    try {
      const detail = await deep_link_lane.run<Detail>(
        apiUrl("/api/run", { source: source.source_id, run_id: link.run_id }),
      );
      if (!detail || disposed || pending_deep_link !== link || source_id !== source.source_id)
        return false;
      pending_deep_link = null;
      sourceNote();
      beginReview("run", [link.run_id]);
      cacheDetail(detail);
      updateUi({ review_loading: false, review_warnings: detail.warnings });
      renderDetail(detail);
      announce(`Opened ${link.run_id}`);
      return true;
    } catch (error) {
      if (error instanceof RequestError && error.status === 404) return false;
      throw error;
    }
  }
  async function loadRun(id: string, preserve = false): Promise<boolean> {
    try {
      const detail = await review_lane.run<Detail>(
        apiUrl("/api/run", { source: source_id, run_id: id }),
      );
      if (!detail) return false;
      cacheDetail(detail);
      updateUi({ review_loading: false });
      updateUi({ review_warnings: detail.warnings });
      if (!renderDetail(detail, preserve)) return false;
      if (!preserve) announce(`Opened ${id}`);
      return true;
    } catch (error) {
      updateUi({ review_loading: false });
      updateUi({ review_error: errorText(error) });
      return false;
    }
  }
  function renderComparison(result: Comparison): void {
    if (JSON.stringify(rendered_comparison) === JSON.stringify(result)) return;
    rendered_comparison = result;
    updateUi({ review_warnings: result.comparison.warnings });
  }
  async function loadComparison(preserve = false): Promise<boolean> {
    if (!review || review.kind !== "compare") return false;
    updateUi({ review_error: null });
    updateUi({ comparison_busy: true });
    if (!rendered_comparison) updateUi({ chart_visible: false });
    try {
      const result = await compare_lane.run<Comparison>(comparisonUrl());
      if (!result || !review || review.kind !== "compare") return false;
      const names = [...new Set([...metric_names, ...result.comparison.metric_names])].sort();
      metric_names = names;
      if (!review.metric_selection_set) {
        review.metric_names = result.comparison.metric_names.slice(0, 4);
        review.metric_selection_set = true;
      }
      updateUi({ review_loading: false });
      publish();
      renderComparison(result);
      updateUi({ comparison_busy: false });
      publish();
      if (!preserve || !required<HTMLIFrameElement>("chart-frame").src) updateChart();
      return true;
    } catch (error) {
      updateUi({ review_loading: false });
      updateUi({ comparison_busy: false });
      updateUi({ review_error: errorText(error) });
      return false;
    }
  }
  function openComparison(): void {
    const ids = [...selected];
    if (ids.length < 2 || ids.length > 8) return;
    beginReview("compare", ids, "selection");
    metric_names = [
      ...new Set(ids.flatMap((id) => Object.keys(detail_cache.get(id)?.metrics ?? {}))),
    ].sort();
    void loadComparison();
  }
  function sourceNote(): void {
    const source = sources.find((item) => item.source_id === source_id);
    updateUi({
      source_note: pending_deep_link
        ? `Waiting for ${pending_deep_link.run_id} from ${pending_deep_link.project_id} / ${pending_deep_link.origin} to be published. Choose another source or run to stop waiting.`
        : !source
          ? null
          : source.kind === "service"
            ? "Synced results · Updates arrive when workers sync their recorded files."
            : source.kind === "cached"
              ? "Cached remote results · Pull updated results with expri runs pull; this dashboard watches the local cache."
              : "Local results · Status and updates come from recorded run files.",
    });
  }
  function applyCatalog(catalog: Catalog, quiet = false): void {
    last_catalog_snapshot = now();
    if (quiet && JSON.stringify(last_catalog) === JSON.stringify(catalog)) return;
    last_catalog = catalog;
    sources = catalog.sources;
    access_mode = catalog.access_mode ?? "local";
    page_size = access_mode === "hosted" ? 20 : 100;
    // Session/revision markup stays outside React so externally injected AB labels survive.
    required("logout-form").hidden = access_mode !== "hosted";
    const previous_source = source_id;
    if (!sources.some((source) => source.source_id === source_id) || !catalog_initialized) {
      source_id = sources.some((source) => source.source_id === catalog.initial_source)
        ? catalog.initial_source
        : sources[0]?.source_id ?? "";
    }
    const linked_source = deepLinkSource();
    if (linked_source) source_id = linked_source.source_id;
    catalog_initialized = true;
    if (source_id !== previous_source) {
      offset = 0;
      runs = [];
      selected.clear();
      detail_cache.clear();
      hideReview();
      syncSelection();
    }
    document.title = `expri · ${catalog.project_name}`;
    updateUi({
      controls_disabled: !sources.length,
      project_name: catalog.project_name,
      catalog_warnings: catalog.warnings,
    });
    sourceNote();
  }
  function emptyCatalog(): void {
    list_lane.cancel();
    runs = [];
    next_offset = null;
    updateUi({
      previous_disabled: true,
      next_disabled: true,
      run_count: "No synced runs yet",
      page_label: "Page 1",
      list_warnings: [],
      list_busy: false,
      review_empty: false,
    });
    showEmptyRuns(false);
  }
  async function refreshCurrentChart(lane: RequestLane): Promise<ChartRefreshOutcome> {
    if (!review || review.tab !== "charts") return "applied";
    if (chart_controller.isInteracting()) return "deferred";
    const current_review = review,
      source = source_id,
      url = chartUrl(),
      status = chart_controller.previewStatus();
    if (status === "loading") return "deferred";
    if (status === "invalid") {
      required<HTMLIFrameElement>("chart-frame").src = url;
      chart_revisions.delete(url);
      cached_chart = null;
      throw new Error(
        "The chart preview could not load. Retrying when the connection is available.",
      );
    }
    const html = await lane.runText(url);
    if (
      html === undefined ||
      review !== current_review ||
      source_id !== source ||
      chartUrl() !== url
    )
      return "cancelled";
    if (cached_chart?.context === url && cached_chart.html === html) {
      clearChartError(url);
      return "applied";
    }
    if (!chart_controller.replacePreview(html)) return "deferred";
    cached_chart = { context: url, html };
    clearChartError(url);
    return "applied";
  }
  function applyLog(view: LogView, log: Log): void {
    const content = log.missing
      ? "This log has not been recorded or pulled."
      : log.content || "The log is empty.";
    const output = required("log-output");
    const scroll_top = output.scrollTop,
      at_bottom = output.scrollHeight - output.clientHeight - scroll_top <= 4;
    const changed = ui.log_content !== content;
    updateUi({
      log_content: content,
      log_note: log.truncated
        ? "Showing the last 100 lines, capped at 64 KiB. Use expri runs logs for more output."
        : "Last 100 lines · updates arrive automatically while auto updates are on.",
    });
    if (changed) output.scrollTop = at_bottom ? output.scrollHeight : scroll_top;
    view.loaded = true;
  }
  async function refreshCurrentLog(lane: RequestLane): Promise<boolean> {
    const view = log_view;
    if (review?.tab !== "logs" || !view) return true;
    const log = await lane.run<Log>(
      apiUrl("/api/log", {
        source: source_id,
        run_id: view.run_id,
        stream: view.stream,
        tail: 100,
      }),
    );
    if (!log || log_view !== view || review?.tab !== "logs") return false;
    applyLog(view, log);
    return true;
  }
  function artifactsUrl(): string {
    return apiUrl("/api/artifacts", { source: source_id, run_id: review?.run_ids[0] ?? "" });
  }
  function resetArtifacts(): void {
    artifact_lane.cancel();
    artifacts = null;
    artifacts_loaded = false;
    selected_files.clear();
    artifact_revisions.clear();
    ui.artifacts_loading = false;
    ui.artifacts_error = null;
  }
  function ensureArtifacts(): void {
    if (
      review?.kind === "run" &&
      review !== missing_review &&
      review.tab === "files" &&
      !artifacts_loaded &&
      !artifact_lane.pending
    )
      void loadArtifacts();
  }
  function applyArtifacts(result: ArtifactCatalog): void {
    const id = review?.run_ids[0] ?? "";
    artifacts = result;
    artifacts_loaded = true;
    for (const path of selected_files) {
      const file = result.files.find((file) => file.path === path);
      if (!file || !artifactCanSelect(file, result.pull_scope, source_id, id))
        selected_files.delete(path);
    }
    updateUi({ artifacts_error: null });
  }
  async function loadArtifacts(): Promise<void> {
    await refreshCurrentArtifacts(artifact_lane, () => true, true);
  }
  async function refreshCurrentArtifacts(
    lane: RequestLane,
    guard: () => boolean = () => true,
    foreground = false,
  ): Promise<boolean> {
    const current_review = review;
    if (current_review?.kind !== "run" || current_review.tab !== "files") return true;
    const source = source_id,
      url = artifactsUrl();
    const current = () =>
      !disposed &&
      guard() &&
      review === current_review &&
      source_id === source &&
      review?.tab === "files";
    if (foreground) updateUi({ artifacts_loading: true, artifacts_error: null });
    try {
      const result = await lane.run<ArtifactCatalog>(url);
      if (!result || !current()) return false;
      if (result.source.source_id !== source || result.run_id !== current_review.run_ids[0])
        throw new Error("The file inventory does not match the selected run.");
      applyArtifacts(result);
      return true;
    } catch (error) {
      if (!current()) return false;
      if (error instanceof RequestError && error.status === 404) {
        artifacts_loaded = true;
        updateUi({
          artifacts_error:
            "This server does not provide file browsing yet, or the run is no longer available. Upgrade expri or use the CLI to pull selected run files.",
        });
        return true;
      }
      updateUi({
        artifacts_error: `Could not read file availability. Showing the last successful inventory, if available. ${errorText(error)}`,
      });
      if (!foreground) throw error;
      return false;
    } finally {
      if (current()) updateUi({ artifacts_loading: false });
    }
  }
  async function refresh(): Promise<void> {
    cancelQuietRefresh();
    const generation = ++refresh_generation;
    updateUi({ refreshing: true, global_error: null });
    let successful = false;
    try {
      const catalog = await catalog_lane.run<Catalog>("/api/catalog");
      if (!catalog || generation !== refresh_generation) return;
      applyCatalog(catalog);
      if (!source_id) {
        emptyCatalog();
        successful = true;
      } else {
        const current_review = review,
          current_source = source_id;
        if (!(await loadRuns()) || generation !== refresh_generation) return;
        await resolveDeepLink();
        if (generation !== refresh_generation) return;
        if (current_review === review && current_source === source_id && current_review) {
          if (current_review.kind === "run") {
            const id = current_review.run_ids[0];
            if (id && !(await loadRun(id, true))) return;
          } else if (!(await loadComparison(true))) return;
          if (generation !== refresh_generation || current_review !== review) return;
          if (
            !(await refreshCurrentLog(log_lane)) ||
            !(await refreshCurrentArtifacts(artifact_lane)) ||
            (await refreshCurrentChart(chart_lane)) !== "applied"
          )
            return;
        }
        successful = generation === refresh_generation;
      }
      if (successful) {
        last_checked = now();
        last_full_snapshot = now();
      }
    } catch (error) {
      if (generation === refresh_generation) {
        updateUi({ global_error: errorText(error) });
        updateUi({ review_loading: false });
      }
    } finally {
      if (generation === refresh_generation) {
        updateUi({ refreshing: false });
        auto_refresh?.refreshCompleted(successful);
      }
    }
  }
  async function quietRefresh(): Promise<RefreshOutcome> {
    if (availability() !== "ready") return "cancelled";
    const generation = quiet_generation;
    const current = () => generation === quiet_generation && availability() === "ready";
    const acknowledgements: (() => void)[] = [];
    const deferChart = (): RefreshOutcome => {
      if (current()) for (const acknowledge of acknowledgements) acknowledge();
      return "cancelled";
    };
    const full_snapshot = now() - last_full_snapshot >= 5 * 60_000;
    const complete = (): RefreshOutcome => {
      if (!current()) return "cancelled";
      for (const acknowledge of acknowledgements) acknowledge();
      if (full_snapshot || !updates_supported) last_full_snapshot = now();
      last_checked = now();
      updateUi({ global_error: null });
      return "success";
    };
    let updates: Updates | undefined;
    const probed_source = source_id;
    try {
      if (updates_supported) {
        try {
          updates = await quiet_lanes.updates.run<Updates>(
            apiUrl("/api/updates", {
              source: last_catalog ? source_id : "",
              run_id:
                review?.run_ids ??
                (deepLinkSource()?.source_id === source_id && pending_deep_link
                  ? [pending_deep_link.run_id]
                  : []),
            }),
          );
        } catch (error) {
          if (!(error instanceof RequestError) || error.status !== 404) throw error;
          updates_supported = false;
          auto_refresh?.setInterval(30_000);
        }
        if (!current() || (updates_supported && !updates)) return "cancelled";
      }
      const catalog_changed =
        updates?.catalog_revision !== null &&
        updates?.catalog_revision !== undefined &&
        catalog_revision !== updates.catalog_revision;
      if (
        !updates_supported ||
        full_snapshot ||
        catalog_changed ||
        (updates?.catalog_revision === null && now() - last_catalog_snapshot >= 30_000)
      ) {
        const catalog = await quiet_lanes.catalog.run<Catalog>("/api/catalog");
        if (!catalog || !current()) return "cancelled";
        applyCatalog(catalog, true);
        if (updates?.catalog_revision !== null && updates?.catalog_revision !== undefined) {
          const revision = updates.catalog_revision;
          acknowledgements.push(() => {
            catalog_revision = revision;
          });
        }
      }
      const source_changed = source_id !== probed_source;
      const revision = source_changed ? undefined : updates?.source_revision;
      const list_changed =
        revision !== null && revision !== undefined && source_revisions.get(source_id) !== revision;
      if (!source_id) emptyCatalog();
      else if (
        !updates_supported ||
        full_snapshot ||
        source_changed ||
        !list_ready ||
        last_run_list_context !== listUrl() ||
        list_changed ||
        (revision === null && now() - last_list_snapshot >= 30_000)
      ) {
        const url = listUrl(),
          result = await quiet_lanes.list.run<RunList>(url);
        if (!result || !current() || listUrl() !== url) return "cancelled";
        applyRunList(result, url, true);
        if (revision !== null && revision !== undefined) {
          const source = source_id;
          acknowledgements.push(() => boundedSet(source_revisions, source, revision));
        }
      }
      const linked_source = deepLinkSource();
      if (pending_deep_link && linked_source?.source_id === source_id) {
        const revision = source_changed
          ? undefined
          : updates?.runs.find((item) => item.run_id === pending_deep_link?.run_id);
        if (
          deep_link_attempted_source !== source_id ||
          !updates_supported ||
          full_snapshot ||
          revision?.missing === false
        ) {
          if (await resolveDeepLink()) return complete();
          if (!current()) return "cancelled";
        }
      }
      const current_review = review;
      if (current_review && !source_changed) {
        const revisions = current_review.run_ids.map((id) =>
          updates?.runs.find((item) => item.run_id === id),
        );
        if (revisions.some((item) => item?.missing)) {
          const message =
            "Some selected runs are no longer available in this source. Showing the last successful preview.";
          if (ui.review_error !== message) updateUi({ review_error: message });
          updateUi({ review_loading: false });
          missing_review = current_review;
          return complete();
        }
        if (
          missing_review === current_review &&
          revisions.every((item) => item !== undefined && !item.missing)
        ) {
          missing_review = null;
          updateUi({ review_error: null });
          detail_revisions.clear();
          artifact_revisions.clear();
          chart_revisions.clear();
          log_revisions.clear();
          comparison_revision = null;
        }
        if (
          current_review.tab === "charts" &&
          required<HTMLIFrameElement>("chart-frame").src &&
          chart_controller.previewStatus() !== "ready"
        ) {
          const outcome = await refreshCurrentChart(quiet_lanes.chart);
          return outcome === "deferred" ? deferChart() : "cancelled";
        }
        const view_revision = JSON.stringify(
          revisions.map((item) => [
            item?.run_id,
            item?.metadata_revision,
            item?.metrics_revision,
            item?.missing,
          ]),
        );
        const force =
          !updates_supported || full_snapshot || revisions.some((item) => item === undefined);
        if (current_review.kind === "run" && (current_review.tab !== "logs" || !log_view)) {
          const id = current_review.run_ids[0];
          if (id) {
            const context = apiUrl("/api/run", { source: source_id, run_id: id });
            if (force || detail_revisions.get(context) !== view_revision) {
              const detail = await quiet_lanes.detail.run<Detail>(context);
              if (!detail || !current() || review !== current_review) return "cancelled";
              const warnings_changed =
                JSON.stringify(rendered_detail?.warnings) !== JSON.stringify(detail.warnings);
              if (!renderDetail(detail, true)) return "cancelled";
              boundedSet(detail_cache, id, detail);
              if (warnings_changed) updateUi({ review_warnings: detail.warnings });
              updateUi({ review_loading: false });
              updateUi({ review_error: null });
              if (!current()) return "cancelled";
              acknowledgements.push(() => boundedSet(detail_revisions, context, view_revision));
            }
          }
        } else if (current_review.kind === "compare") {
          const context = comparisonUrl();
          if (
            force ||
            comparison_revision?.context !== context ||
            comparison_revision.revision !== view_revision
          ) {
            const result = await quiet_lanes.comparison.run<Comparison>(context);
            if (!result || !current() || review !== current_review || comparisonUrl() !== context)
              return "cancelled";
            const names = [...new Set([...metric_names, ...result.comparison.metric_names])].sort();
            const names_changed = JSON.stringify(names) !== JSON.stringify(metric_names),
              picker = required("compare-metric-options");
            if (names_changed && containsFocus(picker)) return "cancelled";
            metric_names = names;
            if (!current_review.metric_selection_set) {
              current_review.metric_names = result.comparison.metric_names.slice(0, 4);
              current_review.metric_selection_set = true;
            }
            renderComparison(result);
            updateUi({ review_loading: false });
            publish();
            updateUi({ review_error: null });
            publish();
            if (!required<HTMLIFrameElement>("chart-frame").src) updateChart();
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
        } else if (current_review.tab === "files") {
          const context = artifactsUrl(),
            artifact_revision = JSON.stringify([
              revisions[0]?.metadata_revision,
              updates?.source_revision,
            ]);
          if (force || !artifacts_loaded || artifact_revisions.get(context) !== artifact_revision) {
            if (
              !(await refreshCurrentArtifacts(quiet_lanes.artifacts, current)) ||
              !current() ||
              review !== current_review
            )
              return "cancelled";
            acknowledgements.push(() => boundedSet(artifact_revisions, context, artifact_revision));
          }
        } else if (current_review.tab === "logs" && log_view) {
          const view = log_view,
            context = apiUrl("/api/log", {
              source: source_id,
              run_id: view.run_id,
              stream: view.stream,
              tail: 100,
            });
          const item = revisions[0],
            log_revision = view.stream === "stderr" ? item?.stderr_revision : item?.stdout_revision;
          if (
            force ||
            log_revision === undefined ||
            !log_revisions.has(context) ||
            log_revisions.get(context) !== log_revision
          ) {
            if (!(await refreshCurrentLog(quiet_lanes.log)) || !current() || log_view !== view)
              return "cancelled";
            if (log_revision !== undefined)
              acknowledgements.push(() => boundedSet(log_revisions, context, log_revision));
          }
        }
      }
      return complete();
    } catch {
      if (!current()) return "cancelled";
      // Keep the last successful preview visible; the scheduler reports recovery.
      return "failure";
    }
  }
  function resetFilters(): void {
    cancelRefresh();
    list_lane.cancel();
    clearSearchTimeout();
    offset = 0;
    selected.clear();
    hideReview();
    runs = [];
    updateUi({
      list_empty: null,
      run_count: "Applying filters…",
      list_busy: true,
      previous_disabled: true,
      next_disabled: true,
    });
  }
  function filtersChanged(): void {
    resetFilters();
    void loadRuns();
  }
  auto_refresh = new AutoRefresh({
    run: quietRefresh,
    availability,
    cancel: abortQuietRequests,
    on_state: showFreshness,
    ...(options.refresh_clock ? { clock: options.refresh_clock } : {}),
  });
  const resume = () => auto_refresh?.availabilityChanged();
  document.addEventListener?.("visibilitychange", resume);
  if (typeof window !== "undefined") {
    window.addEventListener("online", resume);
    window.addEventListener("offline", resume);
  }
  auto_refresh.start();
  void refresh();
  return () => {
    if (disposed) return;
    disposed = true;
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
    flushSync(() => react_root.unmount());
    listeners.clear();
  };
}

if (typeof document !== "undefined" && document.getElementById("dashboard-root")) startDashboard();
