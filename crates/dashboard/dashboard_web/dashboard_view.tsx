import { Fragment, useSyncExternalStore, useState, useEffect, type KeyboardEvent, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { FilesPanel, formatFileSize } from "./dashboard_files";
import { StoragePanel, type StorageCatalog, type StorageKind } from "./dashboard_storage";
import { ProjectStorageManagement } from "./storage_management_view";
import { RunArchivalPanel } from "./run_archival_view";
import { RunAnnotationsPanel, RunTags } from "./run_annotations_view";
import { RunAssets } from "./dashboard_assets";
import { RunDetailDiagnostics, RunRowDiagnostics, runWarnings, warningsOutsideRuns } from "./run_diagnostics";
import { Icon } from "./dashboard_icons";
import { RunControlPopover } from "./run_control_popover";
import { RunDuration, RunDurationProvider, RunStatusTiming } from "./run_duration";
import { queueContext, queueMessage, queuePlaceholder } from "./run_queue";
import { QueueOverview } from "./run_queue_view";
import type { RefreshClock } from "./auto_refresh";
import { ThemeControl } from "./dashboard_theme";
import type { ArchivalAction, ArchivalView } from "./run_archival";
import {
  MAX_RUN_COLUMNS,
  formatRunTableValue,
  runColumnChoices,
  runColumnIdentity,
  runColumnSortKey,
  runColumnValue,
  runSortIsDefault,
  runSortLabel,
} from "./run_table";
import type { ChartAxis, ChartTimeZone } from "./interactive_charts";
import type { SystemMonitorSnapshot } from "./system_monitor";
import { SystemMonitorPage, type SystemMonitorActions } from "./system_monitor_view";
import type { RunSystemSnapshot } from "./run_system";
import { RunSystemPanel, type RunSystemActions } from "./run_system_view";
import {
  apiUrl,
  dateText,
  hasRunningDuration,
  runInProgress,
  RUN_STATUSES,
  runStatusClass,
  formatNumber,
  formatValue,
  jsonObject,
  runIdentity,
  runKeyLabel,
  runLabel,
  runNickname,
  localTimeZoneLabel,
  type ArtifactCatalog,
  type Comparison,
  type ComparisonReduction,
  type Detail,
  type ResultUploadRecord,
  type Json,
  type Review,
  type ReviewTab,
  type Run,
  type RunColumn,
  type RunSort,
  type Source,
  type Warning,
  type WorkspaceView,
} from "./dashboard_model";

export type EmptyRuns = { title: string; message: string; setup: boolean };

function RunSmokeMark({ run }: { run: Run }) {
  return run.smoke === true ? <span className="run-tag run-smoke" title="Smoke run">Smoke</span> : null;
}

export type DashboardUi = {
  project_name: string;
  search: string;
  task: string;
  status: string;
  origin: string;
  archival: ArchivalView;
  run_management_enabled: boolean;
  archival_confirming: boolean;
  archival_busy: boolean;
  archival_error: string | null;
  annotations_busy: boolean;
  annotations_error: string | null;
  project_catalog_note: string | null;
  refreshing: boolean;
  controls_disabled: boolean;
  auto_enabled: boolean;
  freshness: string;
  source_note: string | null;
  global_error: string | null;
  catalog_warnings: Warning[];
  list_warnings: Warning[];
  run_count: string;
  page_label: string;
  list_busy: boolean;
  run_columns_loading: boolean;
  run_columns_error: string | null;
  run_columns_warnings: Warning[];
  run_columns_truncated: boolean;
  run_columns_supported: boolean;
  previous_disabled: boolean;
  next_disabled: boolean;
  list_empty: EmptyRuns | null;
  review_empty: boolean;
  review_loading: boolean;
  review_error: string | null;
  review_warnings: Warning[];
  chart_visible: boolean;
  chart_busy: boolean;
  chart_note: string;
  chart_url: string;
  chart_error: string | null;
  comparison_busy: boolean;
  log_content: string;
  log_note: string;
  artifacts_loading: boolean;
  artifacts_error: string | null;
  storage_search: string;
  storage_loading: boolean;
  storage_error: string | null;
  live_status: string;
};
export type DashboardSnapshot = DashboardUi & {
  sources: Source[];
  source_id: string;
  access_mode: "local" | "hosted";
  project_catalog_enabled: boolean;
  runs: Run[];
  run_columns: RunColumn[];
  available_run_columns: RunColumn[];
  run_sort: RunSort;
  run_reduction: ComparisonReduction;
  selected: string[];
  review: Review | null;
  detail: Detail | null;
  comparison: Comparison | null;
  artifacts: ArtifactCatalog | null;
  selected_files: string[];
  workspace_view: WorkspaceView;
  system_monitor: SystemMonitorSnapshot;
  run_system: RunSystemSnapshot;
  run_system_supported: boolean;
  system_run_aligned: boolean;
  system_now: number;
  storage_kind: StorageKind;
  storage_offset: number;
  storage_catalog: StorageCatalog | null;
  metric_names: string[];
  review_version: number;
  x_axis: ChartAxis;
  time_zone: ChartTimeZone;
  reduction: ComparisonReduction;
};
export type DashboardActions = {
  archival: (value: ArchivalView) => void;
  archival_confirm: () => void;
  archival_cancel: () => void;
  archival_mutate: (action: ArchivalAction) => void;
  annotations_save: (nickname: string | null, tags: string[]) => void;
  filter: (field: "search" | "task" | "status" | "origin", value: string) => void;
  clear_filters: () => void;
  run_column: (column: RunColumn, checked: boolean) => void;
  run_sort: (key: string) => void;
  reset_run_sort: () => void;
  run_reduction: (value: ComparisonReduction) => void;
  refresh_run_columns: () => void;
  source: (value: string) => void;
  refresh: () => void;
  auto_refresh: (enabled: boolean) => void;
  previous: () => void;
  next: () => void;
  select_run: (id: string, checked: boolean) => void;
  inspect_run: (id: string) => void;
  clear_selection: () => void;
  compare: () => void;
  close: () => void;
  tab: (tab: ReviewTab) => void;
  metric: (name: string, checked: boolean) => void;
  add_metric: (name: string) => void;
  reduction: (value: ComparisonReduction) => void;
  axis: (value: ChartAxis) => void;
  time_zone: (value: ChartTimeZone) => void;
  log_stream: (value: "stdout" | "stderr") => void;
  select_file: (path: string, checked: boolean) => void;
  clear_files: () => void;
  refresh_files: () => void;
  workspace_view: (view: WorkspaceView) => void;
  system_monitor: SystemMonitorActions;
  run_system: RunSystemActions;
  system_alignment: (aligned: boolean) => void;
  storage_kind: (kind: StorageKind) => void;
  storage_search: (search: string) => void;
  storage_previous: () => void;
  storage_next: () => void;
  storage_refresh: () => void;
};
export type DashboardStore = {
  subscribe: (listener: () => void) => () => void;
  get_snapshot: () => DashboardSnapshot;
};

type RadioOption<T extends string> = { id: string; value: T; label: string };
function revealOnCompactLayout(id: string): void {
  if (!window.matchMedia?.("(max-width: 1000px)").matches) return;
  const target = document.getElementById(id);
  if (!target) return;
  target.focus({ preventScroll: true });
  target.scrollIntoView?.({
    block: "start",
    behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth",
  });
}
function RadioTags<T extends string>({
  id,
  name,
  legend,
  value,
  options,
  on_change,
  hidden = false,
  description,
  disabled = false,
}: {
  id?: string;
  name: string;
  legend: string;
  value: T;
  options: RadioOption<T>[];
  on_change: (value: T) => void;
  hidden?: boolean;
  description?: string;
  disabled?: boolean;
}) {
  return (
    <fieldset
      id={id}
      className="choice-group"
      hidden={hidden}
      disabled={disabled}
      aria-describedby={description}
    >
      <legend>{legend}</legend>
      <div className={name === "time_zone" ? "time-zone-choices" : undefined}>
        <div className="choice-tags">
          {options.map((option) => (
            <label className="choice-tag" key={option.value}>
              <input
                id={option.id}
                className="sr-only"
                type="radio"
                name={name}
                value={option.value}
                checked={value === option.value}
                onChange={() => on_change(option.value)}
              />
              <span>{option.label}</span>
            </label>
          ))}
        </div>
        {description && (
          <span id={description} className="time-zone-label muted">
            {value === "utc" ? "UTC" : localTimeZoneLabel()}
          </span>
        )}
      </div>
    </fieldset>
  );
}
function Warnings({ id, items }: { id: string; items: Warning[] }) {
  return (
    <div id={id} hidden={!items.length}>
      {items.length > 0 && (
        <div className="notice">
          <ul>
            {items.map((item, index) => (
              <li key={index}>
                {item.run_id ? `${item.run_id}: ` : ""}
                {item.message}
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
function ErrorNotice({ id, message }: { id: string; message: string | null }) {
  return (
    <div id={id} className="notice error" role="alert" hidden={message === null}>
      {message}
    </div>
  );
}
function MetricPicker({
  id,
  snapshot,
  actions,
}: {
  id: string;
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
}) {
  const [name, setName] = useState("");
  const chosen = snapshot.review?.metric_names ?? [];
  const known = [...new Set([...snapshot.metric_names, ...chosen])].sort();
  return (
    <div id={id}>
      <div className="metric-options">
        {known.map((metric) => (
          <label className="metric-choice" key={metric}>
            <input
              type="checkbox"
              checked={chosen.includes(metric)}
              disabled={chosen.length >= 6 && !chosen.includes(metric)}
              onChange={(event) => actions.metric(metric, event.currentTarget.checked)}
            />
            {metric}
          </label>
        ))}
      </div>
      <form
        className="metric-add"
        onSubmit={(event) => {
          event.preventDefault();
          if (name.trim() && chosen.length < 6) {
            actions.add_metric(name.trim());
            setName("");
          }
        }}
      >
        <label className="field">
          Exact metric name
          <input
            type="text"
            placeholder="e.g. validation/loss"
            maxLength={1024}
            value={name}
            onChange={(event) => setName(event.currentTarget.value)}
          />
        </label>
        <button type="submit" className="button secondary" disabled={chosen.length >= 6}>
          Add metric
        </button>
      </form>
      <p className="muted metric-hint">
        {chosen.length
          ? "Select up to six metrics. Enter a name to include a metric outside this preview."
          : "No selection uses the first four recorded metric names."}
      </p>
    </div>
  );
}
function RunColumnPicker({
  snapshot: s,
  actions: a,
  open,
  on_change,
}: {
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
  open: boolean;
  on_change: (open: boolean) => void;
}) {
  const [search, setSearch] = useState("");
  const choices = runColumnChoices(s.available_run_columns, s.run_columns);
  const chosen = new Set(s.run_columns.map(runColumnIdentity));
  const query = search.trim().toLocaleLowerCase();
  const visible = choices.filter(
    (column) =>
      !query ||
      column.label.toLocaleLowerCase().includes(query) ||
      column.key.toLocaleLowerCase().includes(query),
  );
  const disabled =
    s.controls_disabled || s.list_busy || s.run_columns_loading || !s.run_columns_supported;
  return (
    <RunControlPopover id="run-columns" label="Columns" icon="columns" count={s.run_columns.length}
      open={open} disabled={s.controls_disabled} on_change={(value) => { on_change(value); if (value) a.refresh_run_columns(); }}>
      <p id="run-columns-hint" className="muted">
        Runs include task names. Status and start time stay visible. Add up to eight parameter or metric columns.
      </p>
      {s.run_columns_loading && <p role="status">Finding recorded columns…</p>}
      {choices.length > 0 && (
        <label className="field">
          Find a column
          <input
            id="run-column-search"
            type="search"
            maxLength={200}
            value={search}
            onChange={(event) => setSearch(event.currentTarget.value)}
            placeholder="Parameter or metric name"
          />
        </label>
      )}
      {(["param", "metric"] as const).map((kind) => {
        const columns = visible.filter((column) => column.kind === kind);
        if (columns.length === 0) return null;
        return (
          <fieldset key={kind} className="run-column-group" aria-describedby="run-columns-hint">
            <legend>{kind === "param" ? "Parameters" : "Metrics"}</legend>
            <div className="run-column-options">
              {columns.map((column) => {
                const identity = runColumnIdentity(column);
                const selected = chosen.has(identity);
                return (
                  <label className="run-column-choice choice-tag" key={identity}>
                    <input
                      type="checkbox"
                      className="sr-only"
                      checked={selected}
                      disabled={disabled || (chosen.size >= MAX_RUN_COLUMNS && !selected)}
                      aria-label={`${kind === "param" ? "Parameter" : "Metric"} ${column.label}`}
                      onChange={(event) => a.run_column(column, event.currentTarget.checked)}
                    />
                    <span title={column.key}>{column.label}</span>
                  </label>
                );
              })}
            </div>
          </fieldset>
        );
      })}
      {s.run_columns_supported &&
        !s.run_columns_loading &&
        !s.run_columns_error &&
        choices.length === 0 && (
          <p className="muted" role="status">
            No parameters or metrics are recorded in this source yet.
          </p>
        )}
      {query && visible.length === 0 && choices.length > 0 && (
        <p className="muted" role="status">No columns match this name.</p>
      )}
      {s.run_columns_truncated && (
        <p className="muted">
          Column discovery is limited. Selected columns remain available.
        </p>
      )}
      {chosen.size >= MAX_RUN_COLUMNS && <p className="muted">Remove a column to choose another.</p>}
      {(!s.run_columns_supported || s.run_columns_error) && (
        <div
          id="run-columns-feedback"
          className={`notice run-columns-feedback${s.run_columns_supported ? " error" : ""}`}
          role={s.run_columns_supported ? "alert" : "status"}
        >
          <p>{s.run_columns_error ?? "Table columns require an updated expri backend."}</p>
          <button
            type="button"
            className="text-button"
            disabled={s.run_columns_loading || s.controls_disabled}
            onClick={a.refresh_run_columns}
          >
            Retry column discovery
          </button>
        </div>
      )}
      <div className="run-column-settings">
        <RadioTags
          id="run-reduction-options"
          name="run_reduction"
          legend="Metric summary"
          value={s.run_reduction}
          hidden={!s.run_columns.some((column) => column.kind === "metric")}
          disabled={
            s.controls_disabled || s.list_busy || s.run_columns_loading || !s.run_columns_supported
          }
          options={[
            { id: "run-reduction-last", value: "last", label: "Last" },
            { id: "run-reduction-min", value: "min", label: "Min" },
            { id: "run-reduction-max", value: "max", label: "Max" },
          ]}
          on_change={a.run_reduction}
        />
        <button
          id="reset-run-sort"
          className="text-button"
          type="button"
          aria-pressed={runSortIsDefault(s.run_sort)}
          disabled={s.controls_disabled || s.list_busy || runSortIsDefault(s.run_sort)}
          onClick={a.reset_run_sort}
        >
          Newest first
        </button>
        <p id="run-sort-description" className="muted">
          {runSortLabel(s.run_sort, s.run_columns)} · Missing values last
        </p>
      </div>
    </RunControlPopover>
  );
}
function RunSortHeader({
  label,
  sort_key,
  snapshot: s,
  actions: a,
  class_name,
  description,
}: {
  label: string;
  sort_key: string;
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
  class_name: string;
  description?: string;
}) {
  const active = s.run_sort.key === sort_key;
  const direction = active && s.run_sort.direction === "asc" ? "descending" : "ascending";
  return (
    <th
      scope="col"
      className={class_name}
      aria-sort={active ? (s.run_sort.direction === "asc" ? "ascending" : "descending") : "none"}
    >
      <button
        type="button"
        className="run-sort-button"
        disabled={
          s.controls_disabled || s.list_busy || s.run_columns_loading || !s.run_columns_supported
        }
        aria-label={`Sort by ${label} ${direction}`}
        title={label}
        onClick={() => a.run_sort(sort_key)}
      >
        <span>{label}</span>
        <span aria-hidden="true">{active ? (s.run_sort.direction === "asc" ? "↑" : "↓") : "↕"}</span>
      </button>
      {description && <span className="run-column-kind">{description}</span>}
    </th>
  );
}
function MachineFilter({ source, snapshot: s, actions: a }: {
  source: Source;
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
}) {
  const [search, setSearch] = useState("");
  const machines = [...new Set(source.machines ?? [])].sort();
  const visible = machines.filter((machine) =>
    machine === s.origin || machine.toLocaleLowerCase().includes(search.toLocaleLowerCase()),
  );
  return (
    <div className="machine-filter">
      {machines.length > 12 && (
        <label className="field">
          Find a machine
          <input type="search" id="machine-search" maxLength={96} value={search}
            onChange={(event) => setSearch(event.currentTarget.value)} />
        </label>
      )}
      <RadioTags
        id="machine-options"
        name="machine"
        legend="Machine"
        value={s.origin}
        disabled={s.controls_disabled}
        options={[
          { id: "machine-all", value: "", label: "All machines" },
          ...visible.map((machine) => ({ id: `machine-${machine}`, value: machine, label: machine })),
        ]}
        on_change={(value) => a.filter("origin", value)}
      />
    </div>
  );
}
function RunFilterPanel({ snapshot: s, actions: a }: { snapshot: DashboardSnapshot; actions: DashboardActions }) {
  const project = s.sources.find((source) => source.source_id === s.source_id && source.kind === "hosted_project");
  const tasks = [...new Set(s.runs.map((run) => run.task).filter((task): task is string => task !== null))].sort();
  return (
    <div className="run-filter-panel">
      <label className="field">
        Task
        <input
          id="task-input"
          type="search"
          list="task-options"
          placeholder="Any task"
          autoComplete="off"
          disabled={s.controls_disabled}
          value={s.task}
          onChange={(event) => a.filter("task", event.currentTarget.value)}
        />
        <datalist id="task-options">
          {tasks.map((task) => (
            <option key={task} value={task} />
          ))}
        </datalist>
      </label>
      {s.access_mode === "hosted" && <RadioTags
        id="archival-options"
        name="archival"
        legend="Run history"
        value={s.archival}
        disabled={s.controls_disabled}
        options={[
          { id: "archival-active", value: "active", label: "Active" },
          { id: "archival-archived", value: "archived", label: "Archived" },
        ]}
        on_change={a.archival}
      />}
      <RadioTags
        id="status-options"
        name="status"
        legend="Status"
        value={s.status}
        disabled={s.controls_disabled}
        options={[
          { id: "status-any", value: "", label: "Any" },
          ...RUN_STATUSES.map(
            (status) => ({
              id: `status-${status}`,
              value: status,
              label: `${status[0]?.toUpperCase()}${status.slice(1)}`,
            }),
          ),
        ]}
        on_change={(value) => a.filter("status", value)}
      />
      {project && <MachineFilter key={project.source_id} source={project} snapshot={s} actions={a} />}
      <button
        id="clear-filters"
        className="text-button"
        type="button"
        disabled={s.controls_disabled}
        onClick={a.clear_filters}
      >
        Clear filters
      </button>
      <p className="filter-hint sr-only">Changing filters clears the selection.</p>
    </div>
  );
}
function RunBrowser({
  snapshot: s,
  actions: a,
}: {
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
}) {
  const [open_control, setOpenControl] = useState<"filters" | "columns" | "issues" | null>(null);
  useEffect(() => setOpenControl(null), [s.source_id]);
  function changeControl(control: "filters" | "columns" | "issues", open: boolean): void {
    setOpenControl((current) => open ? control : current === control ? null : current);
  }
  const active_filters = [
    ...(s.task ? [{ key: "task", name: "task", label: `Task: ${s.task}`, clear: () => a.filter("task", "") }] : []),
    ...(s.status ? [{ key: "status", name: "status", label: `Status: ${s.status}`, clear: () => a.filter("status", "") }] : []),
    ...(s.origin ? [{ key: "origin", name: "machine", label: `Machine: ${s.origin}`, clear: () => a.filter("origin", "") }] : []),
    ...(s.archival === "archived" ? [{ key: "archival", name: "archive", label: "Archived", clear: () => a.archival("active") }] : []),
  ];
  const filter_count = active_filters.length;
  const seen_issues = new Set<string>();
  const issue_groups = [
    { id: "catalog-warnings", label: "Workspace", items: s.catalog_warnings },
    { id: "list-warnings", label: "Other run records", items: warningsOutsideRuns(s.list_warnings, s.runs) },
    { id: "run-columns-warnings", label: "Column discovery", items: [
      ...warningsOutsideRuns(s.run_columns_warnings, s.runs),
      ...(s.run_columns_error ? [{ message: s.run_columns_error }] : []),
    ] },
  ].map((group) => ({ ...group, items: group.items.filter((item) => {
    const key = JSON.stringify([item.run_id ?? null, item.message]);
    if (seen_issues.has(key)) return false;
    seen_issues.add(key);
    return true;
  }) }));
  const issue_count = issue_groups.reduce((count, group) => count + group.items.length, 0) + Number(s.project_catalog_note !== null);
  useEffect(() => {
    if (!issue_count) setOpenControl((current) => current === "issues" ? null : current);
  }, [issue_count]);
  const project = s.sources.find((source) => source.source_id === s.source_id && source.kind === "hosted_project");
  return (
    <aside className="runs-card run-history" aria-labelledby="runs-heading">
      <div className="section-heading run-history-header">
        <div><h2 id="runs-heading" tabIndex={-1}>Run history</h2><p id="run-count" className="muted">{s.run_count}</p></div>
        <div className="run-history-actions">
          <RunControlPopover id="run-filters" label="Filters" icon="filter" count={filter_count}
            open={open_control === "filters"} disabled={s.controls_disabled} on_change={(open) => changeControl("filters", open)}>
            <RunFilterPanel snapshot={s} actions={a} />
          </RunControlPopover>
          <RunColumnPicker key={s.source_id} snapshot={s} actions={a} open={open_control === "columns"} on_change={(open) => changeControl("columns", open)} />
          <RunControlPopover id="run-issues" label="Issues" icon="issue" count={issue_count}
            open={open_control === "issues"} hidden={!issue_count} on_change={(open) => changeControl("issues", open)}>
            {issue_groups.map((group) => <section className="run-issue-group" key={group.id}>
              {group.items.length > 0 && <h4>{group.label}</h4>}
              <Warnings id={group.id} items={group.items} />
            </section>)}
            <p id="project-catalog-note" className="muted" hidden={s.project_catalog_note === null}>{s.project_catalog_note}</p>
          </RunControlPopover>
        </div>
      </div>
      <div className="run-search-row">
        <div className="field search-field">
          <label className="sr-only" htmlFor="search-input">Search runs</label>
          <span className="search-control">
            <Icon name="search" />
            <input
              id="search-input"
              type="search"
              placeholder="Search runs, tags, or tasks"
              autoComplete="off"
              disabled={s.controls_disabled}
              value={s.search}
              onChange={(event) => a.filter("search", event.currentTarget.value)}
            />
            {s.search && <button type="button" className="search-clear" aria-label="Clear search" disabled={s.controls_disabled} onClick={() => { a.filter("search", ""); document.getElementById("search-input")?.focus(); }}><Icon name="close" /></button>}
          </span>
        </div>
      </div>
      <div className="active-run-filters" aria-label="Active run filters" hidden={!filter_count}>
        {active_filters.map((filter) => <button key={filter.key} id={`clear-filter-${filter.key}`} type="button" className="run-filter-chip"
          title={filter.label} aria-label={`Clear ${filter.name} filter`} disabled={s.controls_disabled} onClick={filter.clear}>
          <span>{filter.label}</span><Icon name="close" />
        </button>)}
        <button id="clear-all-run-filters" type="button" className="text-button" disabled={s.controls_disabled} onClick={a.clear_filters}>Clear all</button>
      </div>
      <div className="table-scroll runs-scroll" id="runs-region" aria-busy={s.list_busy}>
        <table
          className={`runs-table${(s.run_columns.length || s.sources.some((source) => source.source_id === s.source_id && source.kind === "hosted_project")) ? " has-custom-columns" : ""}`}
          data-column-count={s.run_columns.length}
        >
          <thead>
            <tr>
              <th className="selection-column">
                <span className="sr-only">Select for comparison</span>
              </th>
              <RunSortHeader
                label="Run"
                sort_key="run_id"
                class_name="run-column"
                snapshot={s}
                actions={a}
              />
              <RunSortHeader
                label="Status"
                sort_key="status"
                class_name="status-column"
                snapshot={s}
                actions={a}
              />
              {project && <RunSortHeader label="Machine" sort_key="origin" class_name="machine-column" snapshot={s} actions={a} />}
              <RunSortHeader label="Started" sort_key="started_at" class_name="started-column" snapshot={s} actions={a} />
              {s.run_columns.map((column) => (
                <RunSortHeader
                  key={runColumnIdentity(column)}
                  label={column.label}
                  sort_key={runColumnSortKey(column)}
                  class_name="run-custom-column"
                  snapshot={s}
                  actions={a}
                  description={column.kind === "param" ? "Parameter" : `Metric · ${s.run_reduction}`}
                />
              ))}
            </tr>
          </thead>
          <tbody id="run-rows">
            {s.runs.map((run) => {
              const identity = runIdentity(run);
              const selected = s.selected.includes(identity);
              return (
                <tr
                  key={identity}
                  data-run-key={identity}
                  className={[
                    selected && "selected",
                    s.review?.kind === "run" && s.review.run_ids[0] === identity && "active",
                  ]
                    .filter(Boolean)
                    .join(" ")}
                >
                  <td className="selection-column">
                    <input
                      type="checkbox"
                      checked={selected}
                      disabled={s.list_busy || queuePlaceholder(run) || (s.selected.length >= 8 && !selected)}
                      aria-label={`Select ${runLabel(run)} for comparison`}
                      onChange={(event) => {
                        const checked = event.currentTarget.checked;
                        if (s.workspace_view !== "workspace") a.workspace_view("workspace");
                        a.select_run(identity, checked);
                      }}
                    />
                  </td>
                  <td className="run-cell">
                    <button
                      type="button"
                      className={`run-link${runNickname(run) ? " has-nickname" : ""}`}
                      disabled={s.list_busy}
                      title={runLabel(run)}
                      onClick={() => {
                        a.inspect_run(identity);
                        revealOnCompactLayout(s.workspace_view === "system" && s.system_run_aligned ? "system-heading" : "review-title");
                      }}
                    >
                      {runNickname(run) ?? run.run_id}
                    </button>
                    <div className="run-task" title={run.task ?? undefined}>{run.task ?? "—"}</div>
                    <div className="run-summary-meta">
                      {runNickname(run) && <span className="run-meta run-identifier" title={run.run_id}>{run.run_id}</span>}
                      <RunSmokeMark run={run} />
                      <RunTags run={run} limit={2} />
                    </div>
                    {run.archival && run.archival.status !== "active" && <div className="run-meta">
                      {run.archival.status === "archived" ? `Archived · Deletes ${dateText(run.archival.delete_after)}` :
                        run.archival.status === "needs_attention" ? "Deletion needs attention" : "Deletion in progress"}
                    </div>}
                    <RunRowDiagnostics run={run} items={runWarnings(run, s.list_warnings, s.run_columns_warnings)} />
                    {run.table_values_truncated && (
                      <div
                        className="run-value-warning"
                        title="Some selected values exceed the table preview limit."
                      >
                        Limited values
                      </div>
                    )}
                  </td>
                  <td className="status-column">
                    <span
                      className={`status ${runStatusClass(run.status)}`}
                      title={queueContext(run)?.pending_reason ?? queueMessage(run) ?? undefined}
                    >
                      {run.status}
                    </span>
                    {!queuePlaceholder(run) && <RunStatusTiming run={run} />}
                  </td>
                  {project && <td className="machine-column"><span className="machine-name">{run.origin ?? "—"}</span></td>}
                  <td className="started-column"><time dateTime={run.started_at ?? undefined}>{dateText(run.started_at)}</time></td>
                  {s.run_columns.map((column) => {
                    const value = formatRunTableValue(runColumnValue(run, column));
                    return (
                      <td
                        key={runColumnIdentity(column)}
                        className={`run-column-value${value.numeric ? " number" : ""}`}
                        data-column-kind={column.kind}
                        data-column-key={column.key}
                        title={value.title}
                      >
                        {value.text}
                      </td>
                    );
                  })}
                </tr>
              );
            })}
          </tbody>
        </table>
        <div id="list-empty" className="empty-state" hidden={s.list_empty === null}>
          <span className="empty-mark" aria-hidden="true">
            ◌
          </span>
          <h3>{s.list_empty?.title}</h3>
          <p>{s.list_empty?.message}</p>
          {s.list_empty?.setup && (
            <p className="setup-guide">
              <a
                href="https://github.com/agentic-rs/expri/blob/main/docs/self-hosted-service.md"
                target="_blank"
                rel="noopener noreferrer"
              >
                Set up result publishing
              </a>
            </p>
          )}
        </div>
      </div>
      <div className="pagination">
        <button
          id="previous-page"
          className="text-button"
          type="button"
          disabled={s.previous_disabled}
          onClick={a.previous}
        >
          ← Previous
        </button>
        <span id="page-label" className="muted">
          {s.page_label}
        </span>
        <button
          id="next-page"
          className="text-button"
          type="button"
          disabled={s.next_disabled}
          onClick={a.next}
        >
          Next →
        </button>
      </div>
      <div className="selection-bar">
        <div>
          <p id="selection-count" className="selection-count">
            {s.selected.length
              ? `${s.selected.length} of 8 runs selected`
              : "Select runs to compare"}
          </p>
          <div id="selected-runs" className="selected-runs">
            {s.selected.map((id) => {
              const run = s.runs.find((item) => runIdentity(item) === id) ??
                s.comparison?.comparison.runs.find((item) => runIdentity(item.run) === id)?.run ??
                (s.detail && runIdentity(s.detail.run) === id ? s.detail.run : null);
              const label = run ? runLabel(run) : runKeyLabel(id);
              return (
              <button
                type="button"
                className="selection-chip"
                key={id}
                aria-label={`Remove ${label} from comparison`}
                onClick={() => a.select_run(id, false)}
              >
                {label} ×
              </button>
              );
            })}
          </div>
        </div>
        <div className="selection-actions">
          <button
            id="clear-selection"
            className="text-button"
            type="button"
            hidden={!s.selected.length}
            onClick={a.clear_selection}
          >
            Clear selection
          </button>
          <button
            id="compare-button"
            className="button primary"
            type="button"
            disabled={s.selected.length < 2}
            onClick={() => {
              if (s.workspace_view !== "workspace") a.workspace_view("workspace");
              a.compare();
              revealOnCompactLayout("review-title");
            }}
          >
            <Icon name="compare" />Compare runs
          </button>
        </div>
      </div>
      <details className="source-context" hidden={s.source_note === null}>
        <summary>Source details</summary>
        <p id="source-note">{s.source_note}</p>
      </details>
    </aside>
  );
}
function Facts({ values }: { values: [string, ReactNode][] }) {
  return (
    <dl className="facts">
      {values.map(([label, value]) => (
        <Fragment key={label}>
          <dt>{label}</dt>
          <dd>{value}</dd>
        </Fragment>
      ))}
    </dl>
  );
}
function RecordDetails({ title, value }: { title: string; value: Json }) {
  return (
    <details className="detail-details">
      <summary>{title}</summary>
      <pre>{JSON.stringify(value, null, 2) ?? "Not recorded"}</pre>
    </details>
  );
}
function Overview({ detail }: { detail: Detail | null }) {
  const command = detail ? jsonObject(detail.state)["command"] : null;
  const values = detail ? Object.entries(jsonObject(detail.params)) : [];
  const names = detail ? Object.keys(detail.metrics).sort() : [];
  return (
    <div id="run-detail" hidden={!detail}>
      {detail && (queuePlaceholder(detail.run) ? <QueueOverview run={detail.run} /> : (
        <>
          <div className="detail-grid">
            <section className="card">
              <h3>Overview</h3>
              <Facts
                values={[
                  ["Task", detail.run.task ?? "—"],
                  ["Status", detail.run.status],
                  ["Started", dateText(detail.run.started_at)],
                  ["Finished", dateText(detail.run.finished_at)],
                  ["Duration", <RunDuration run={detail.run} />],
                  ["Exit code", formatValue(detail.run.exit_code)],
                ]}
              />
              {command !== undefined && command !== null && (
                <>
                  <h4>Command</h4>
                  <pre className="command">{formatValue(command)}</pre>
                </>
              )}
              {(
                [
                  ["Run record", detail.state],
                  ["Source provenance", detail.snapshot],
                  ["Environment", detail.environment],
                  ["Cached results", detail.cache],
                ] as [string, Json][]
              )
                .filter(([, value]) => value !== null)
                .map(([title, value]) => (
                  <RecordDetails key={title} title={title} value={value} />
                ))}
            </section>
            <section className="card">
              <h3>{detail.params_truncated ? "Parameters · preview" : "Parameters"}</h3>
              {values.length ? (
                <table className="params-table">
                  <tbody>
                    {values.map(([name, value]) => (
                      <tr key={name}>
                        <th scope="row">{name}</th>
                        <td>
                          <code>{formatValue(value)}</code>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              ) : (
                <p className="muted">No parameters were recorded or downloaded.</p>
              )}
            </section>
          </div>
          <RunAssets detail={detail} />
          <section className="card">
            <h3>Metric summaries</h3>
            {detail.metrics_error && <p className="notice error">{detail.metrics_error}</p>}
            {names.length ? (
              <div className="table-scroll">
                <table>
                  <thead>
                    <tr>
                      {["Metric", "Points", "Last", "Minimum", "Maximum"].map((name) => (
                        <th key={name}>{name}</th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {names.map((name) => {
                      const metric = detail.metrics[name];
                      return (
                        metric && (
                          <tr key={name}>
                            <td>{name}</td>
                            <td className="number">{metric.count}</td>
                            <td className="number">{formatNumber(metric.last.value)}</td>
                            <td className="number">{formatNumber(metric.min.value)}</td>
                            <td className="number">{formatNumber(metric.max.value)}</td>
                          </tr>
                        )
                      );
                    })}
                  </tbody>
                </table>
              </div>
            ) : (
              <p className="muted">
                No metric summaries are available. Record metrics in outputs/metrics.jsonl or download
                remote metrics.
              </p>
            )}
            {detail.metrics_truncated && (
              <p className="muted">
                Showing {names.length} of {detail.metric_count} summaries. Use Chart metrics to
                select another series by its exact name.
              </p>
            )}
          </section>
        </>
      ))}
    </div>
  );
}
function ResultUploadSummary({ detail }: { detail: Detail | null }) {
  const result_upload: ResultUploadRecord | null | undefined = detail?.result_upload;
  if (!detail || !result_upload) return null;
  const labels = {
    none: "No result upload",
    pending: "Result upload pending",
    uploading: "Uploading result",
    uploaded: "Result uploaded",
    failed: "Result upload failed",
  };
  return (
    <div
      id="result-upload-summary"
      className={`notice${result_upload.status === "failed" ? " error" : ""}`}
      data-upload-status={result_upload.status}
      data-incomplete={result_upload.incomplete ? "true" : "false"}
      role="status"
    >
      <strong id="result-upload-status">{labels[result_upload.status] ?? "Result upload status unknown"}</strong>
      {result_upload.incomplete && <span> · Partial result</span>}
      {result_upload.status === "uploaded" && (
        <>
          {" · "}
          <a
            id="download-result-zip"
            href={apiUrl("/api/result-zip", {
              source: detail.source.source_id,
              run_id: runIdentity(detail.run),
            })}
            target="_blank"
            rel="noopener noreferrer"
            download
          >
            Download result.zip{result_upload.file ? ` (${formatFileSize(result_upload.file.size)})` : ""}
          </a>
        </>
      )}
      {result_upload.last_error && <p>Upload issue: {result_upload.last_error.slice(0, 512)}</p>}
    </div>
  );
}
function ComparisonValues({ result, busy }: { result: Comparison | null; busy: boolean }) {
  const comparison = result?.comparison;
  return (
    <div id="comparison-values" className="table-scroll" aria-busy={busy}>
      {comparison && (
        <>
          <table className="comparison-table">
            <thead>
              <tr>
                <th>Run</th>
                {comparison.metric_names.map((name) => (
                  <th key={name}>{name}</th>
                ))}
              </tr>
            </thead>
            <tbody>
              {comparison.runs.map((run) => (
                <tr key={run.run_id}>
                  <th scope="row">
                    <span className={runNickname(run.run) ? "run-nickname" : undefined}>{runNickname(run.run) ?? runKeyLabel(runIdentity(run.run))}</span>
                    {runNickname(run.run) && <div className="run-meta run-identifier">{runKeyLabel(runIdentity(run.run))}</div>}
                    <RunSmokeMark run={run.run} />
                    <RunTags run={run.run} />
                  </th>
                  {comparison.metric_names.map((name) => {
                    const point = run.values[name];
                    return (
                      <td key={name} className="number">
                        {point ? formatNumber(point.value) : "—"}
                        {point && <span className="cell-step">step {point.step}</span>}
                      </td>
                    );
                  })}
                </tr>
              ))}
            </tbody>
          </table>
          {!comparison.metric_names.length && (
            <p className="muted">No recorded metrics match this selection.</p>
          )}
        </>
      )}
    </div>
  );
}
function moveTab<T extends string>(
  event: KeyboardEvent<HTMLButtonElement>,
  current: T,
  available: T[],
  select: (value: T) => void,
  prefix: string,
): void {
  if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
  event.preventDefault();
  const index = available.indexOf(current);
  const next =
    event.key === "Home"
      ? available[0]
      : event.key === "End"
        ? available.at(-1)
        : available[
            (index + (event.key === "ArrowRight" ? 1 : available.length - 1)) % available.length
          ];
  if (next) {
    select(next);
    document.getElementById(`${prefix}${next}`)?.focus();
  }
}
function Logs({
  snapshot: s,
  actions: a,
}: {
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
}) {
  const selected = s.review?.log_stream ?? "stdout";
  return (
    <div id="run-logs">
      <section className="card" hidden={!s.detail}>
        <h3>Log tail</h3>
        <div className="log-tabs" role="tablist" aria-label="Log stream">
          {(["stdout", "stderr"] as const).map((stream) => (
            <button
              type="button"
              key={stream}
              id={`log-tab-${stream}`}
              className="log-tab"
              role="tab"
              aria-controls="log-output"
              aria-selected={stream === selected}
              tabIndex={stream === selected ? 0 : -1}
              onClick={() => a.log_stream(stream)}
              onKeyDown={(event) =>
                moveTab(event, stream, ["stdout", "stderr"], a.log_stream, "log-tab-")
              }
            >
              {stream}
            </button>
          ))}
        </div>
        <pre
          id="log-output"
          className="log-content"
          role="tabpanel"
          aria-labelledby={`log-tab-${selected}`}
        >
          {s.log_content}
        </pre>
        <p id="log-note" className="log-note muted">
          {s.log_note}
        </p>
      </section>
    </div>
  );
}
function ReviewWorkspace({
  snapshot: s,
  actions: a,
}: {
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
}) {
  const tabs: ReviewTab[] = ["charts", "system", "overview", "logs", "files"];
  const review_run = s.review?.kind === "run" ? (
    s.detail && runIdentity(s.detail.run) === s.review.run_ids[0] ? s.detail.run :
      s.runs.find((run) => runIdentity(run) === s.review?.run_ids[0])
  ) : null;
  const queue_only = review_run != null && queuePlaceholder(review_run);
  const available: ReviewTab[] = queue_only ? ["overview"] : s.review?.kind === "compare"
    ? s.run_system_supported ? ["charts", "system"] : ["charts"]
    : tabs.filter(tab => tab !== "system" || s.run_system_supported);
  return (
    <div className="review-content">
      <section id="review-empty" className="review-empty" hidden={!s.review_empty}>
        <span className="review-empty-icon"><Icon name="charts" /></span>
        <div>
          <h2>Select a run</h2>
          <p>Open a run from the history to inspect its metrics, parameters, logs, and files. Select two to eight runs to compare.</p>
        </div>
      </section>
      <section id="review-section" hidden={!s.review}>
        <header className="inspector-header review-heading">
          <div className="inspector-identity">
            <p id="review-eyebrow" className="eyebrow">
              {s.review?.kind === "compare" ? "Comparison" : queue_only ? "Job" : "Run"}
            </p>
            <h2 id="review-title" tabIndex={-1} className={`inspector-title${review_run && runNickname(review_run) ? " has-nickname" : ""}`}>
              {s.review?.kind === "compare"
                ? `${s.review.run_ids.length} runs`
                : queue_only && review_run ? review_run.run_id : (review_run && runNickname(review_run)) ?? runKeyLabel(s.review?.run_ids[0] ?? "")}
            </h2>
            {review_run && <div className="inspector-meta">
              <span aria-live="polite" className={`status inspector-status ${runStatusClass(review_run.status)}`}>
                {review_run.status}
              </span>
              {runNickname(review_run) && <code className="run-identifier">{review_run.run_id}</code>}
              {review_run.task && <span title="Task"><span className="sr-only">Task: </span>{review_run.task}</span>}
              {review_run.origin && runNickname(review_run) && <span title="Machine"><span className="sr-only">Machine: </span>{review_run.origin}</span>}
              {review_run.started_at && <time dateTime={review_run.started_at} title="Started"><span className="sr-only">Started: </span>{dateText(review_run.started_at)}</time>}
              {!queue_only && <RunDuration run={review_run} show_unknown={runInProgress(review_run)} />}
              <RunSmokeMark run={review_run} />
              <RunTags run={review_run} />
            </div>}
          </div>
          <button
            id="close-review"
            className="text-button inspector-close"
            type="button"
            hidden={s.review?.origin === "selection"}
            onClick={() => {
              a.close();
              revealOnCompactLayout("runs-heading");
            }}
          >
            <Icon name="close" />{s.selected.length ? "Back to selection" : "Close"}
          </button>
        </header>
        <div className="review-tabs" role="tablist" aria-label="Run inspection">
          {(queue_only ? available : tabs).map((tab) => (
            <button
              key={tab}
              id={`review-tab-${tab}`}
              className="review-tab"
              role="tab"
              aria-controls={`review-panel-${tab}`}
              aria-selected={s.review?.tab === tab}
              tabIndex={s.review?.tab === tab ? 0 : -1}
              disabled={!available.includes(tab)}
              type="button"
              onClick={() => a.tab(tab)}
              onKeyDown={(event) => moveTab(event, tab, available, a.tab, "review-tab-")}
            >
              <Icon name={tab} />{tab === "system" ? "System metrics" : `${tab[0]?.toUpperCase()}${tab.slice(1)}`}
            </button>
          ))}
        </div>
        {!queue_only && s.access_mode === "hosted" && s.review?.kind === "run" && (
          <div className="inspector-toolbar review-management">
            <RunAnnotationsPanel snapshot={s} actions={a} />
            <RunArchivalPanel snapshot={s} actions={a} />
            <ResultUploadSummary detail={s.detail} />
          </div>
        )}
        <div id="review-loading" className="loading-state inspector-loading" hidden={!s.review_loading}>
          Loading run details…
        </div>
        <ErrorNotice id="review-error" message={s.review_error} />
        {s.review?.kind === "run" && <RunDetailDiagnostics
          id="review-run-errors"
          items={review_run ? runWarnings(review_run, s.review_warnings, s.list_warnings, s.run_columns_warnings) : s.review_warnings.filter((item) => item.run_id === s.review?.run_ids[0])}
        />}
        <Warnings id="review-warnings" items={s.review?.kind === "run" ? s.review_warnings.filter((item) => !item.run_id) : s.review_warnings} />
        <div
          id="review-panel-charts"
          role="tabpanel"
          aria-labelledby="review-tab-charts"
          hidden={s.review?.tab !== "charts"}
        >
          <details
            id="run-metric-controls"
            className="metric-settings metric-menu inspector-controls"
            hidden={!s.detail || s.review?.kind !== "run"}
          >
            <summary>
              Metrics <span className="muted">{s.review?.metric_names.length ? `${s.review.metric_names.length} / 6 selected` : "Default selection"}</span>
            </summary>
            <fieldset className="metric-picker">
              <legend>Metrics</legend>
              <MetricPicker
                key={`${s.source_id}:${s.review_version}`}
                id="run-metric-options"
                snapshot={s}
                actions={a}
              />
            </fieldset>
          </details>
          <div id="compare-detail" hidden={!s.comparison || s.review?.kind !== "compare"}>
            <section className="inspector-controls comparison-controls">
              <div className="metric-control-group">
                <details className="metric-settings metric-menu">
                  <summary>
                    Metrics <span className="muted">{s.review?.metric_names.length ? `${s.review.metric_names.length} / 6 selected` : "Default selection"}</span>
                  </summary>
                  <fieldset className="metric-picker">
                    <legend>Comparison metrics</legend>
                    <MetricPicker
                      key={`${s.source_id}:${s.review_version}`}
                      id="compare-metric-options"
                      snapshot={s}
                      actions={a}
                    />
                    <p id="compare-metric-hint" className="muted"></p>
                  </fieldset>
                </details>
                <RadioTags
                  name="reduction"
                  legend="Summary"
                  value={s.reduction}
                  options={[
                    { id: "reduction-last", value: "last", label: "Last" },
                    { id: "reduction-min", value: "min", label: "Minimum" },
                    { id: "reduction-max", value: "max", label: "Maximum" },
                  ]}
                  on_change={a.reduction}
                />
              </div>
            </section>
            <section className="evidence-panel comparison-summary">
              <h3>Metric values</h3>
              <ComparisonValues result={s.comparison} busy={s.comparison_busy} />
            </section>
          </div>
          <section
            id="chart-card"
            className="evidence-panel chart-card"
            hidden={!s.chart_visible}
            aria-busy={s.chart_busy}
          >
            <div className="chart-toolbar">
              <div className="chart-actions">
                <div className="chart-axis-controls">
                  <RadioTags
                    id="x-axis-options"
                    name="x_axis"
                    legend="X-axis"
                    value={s.x_axis}
                    options={[
                      { id: "x-axis-step", value: "step", label: "Step" },
                      { id: "x-axis-elapsed", value: "elapsed", label: "Elapsed time" },
                      { id: "x-axis-wall-clock", value: "wall_clock", label: "Date & time" },
                    ]}
                    on_change={a.axis}
                  />
                  <RadioTags
                    id="time-zone-options"
                    name="time_zone"
                    legend="Timezone"
                    value={s.time_zone}
                    hidden={s.x_axis !== "wall_clock"}
                    description="time-zone-label"
                    options={[
                      { id: "time-zone-local", value: "local", label: "Local" },
                      { id: "time-zone-utc", value: "utc", label: "UTC" },
                    ]}
                    on_change={a.time_zone}
                  />
                </div>
                <a
                  id="open-chart"
                  className="text-button"
                  href={s.chart_url || undefined}
                  target="_blank"
                  rel="noopener noreferrer"
                >
                  <span id="open-chart-label">
                    {s.x_axis === "wall_clock" ? "Open UTC chart" : "Open chart"}
                  </span>{" "}
                  <span aria-hidden="true">↗</span>
                </a>
              </div>
            </div>
            <p id="chart-note" className="muted chart-context">{s.chart_note}</p>
            <ErrorNotice id="chart-error" message={s.chart_error} />
            <div id="chart-preview" role="region" aria-label="Interactive experiment metric curves"></div>
            <p className="chart-footnote muted">
              Hover to inspect · Drag to zoom · Click a legend entry to show or hide a run. Time views omit samples without timestamps.
            </p>
          </section>
        </div>
        <div
          id="review-panel-system"
          role="tabpanel"
          aria-labelledby="review-tab-system"
          hidden={s.review?.tab !== "system"}
        >
          <RunSystemPanel scope_id="inspector" state={s.run_system} actions={a.run_system} runs={[...s.runs, ...(s.detail ? [s.detail.run] : []), ...(s.comparison?.comparison.runs.map(row => row.run) ?? [])]} time_zone={s.time_zone} now={s.system_now} />
        </div>
        <div
          id="review-panel-overview"
          role="tabpanel"
          aria-labelledby="review-tab-overview"
          hidden={s.review?.tab !== "overview"}
        >
          <Overview detail={s.detail} />
        </div>
        <div
          id="review-panel-logs"
          role="tabpanel"
          aria-labelledby="review-tab-logs"
          hidden={s.review?.tab !== "logs"}
        >
          <Logs snapshot={s} actions={a} />
        </div>
        <div
          id="review-panel-files"
          role="tabpanel"
          aria-labelledby="review-tab-files"
          hidden={s.review?.tab !== "files"}
        >
          <FilesPanel
            key={`${s.source_id}:${s.review_version}`}
            catalog={s.artifacts}
            source_id={s.source_id}
            run_id={s.review?.run_ids[0] ?? ""}
            selected={s.selected_files}
            loading={s.artifacts_loading}
            error={s.artifacts_error}
            on_select={a.select_file}
            on_clear={a.clear_files}
            on_refresh={a.refresh_files}
          />
        </div>
      </section>
    </div>
  );
}
function SourceControls({ snapshot: s, actions }: {
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
}) {
  const empty_label = s.refreshing
    ? s.project_catalog_enabled ? "Loading projects…" : "Loading sources…"
    : s.project_catalog_enabled ? "No published projects" : s.access_mode === "hosted" ? "No published sources" : "No sources";
  return s.project_catalog_enabled && s.sources.length > 0 && s.sources.length <= 6 ? (
    <RadioTags
      id="project-options"
      name="project"
      legend="Projects"
      value={s.source_id}
      options={s.sources.map((source) => ({
        id: `project-${source.project_id}`,
        value: source.source_id,
        label: source.label,
      }))}
      on_change={actions.source}
    />
  ) : (
    <label className="field">
      {s.project_catalog_enabled ? "Project" : "Source"}
      <select
        id="source-select"
        aria-label={s.project_catalog_enabled ? "Project" : "Run source"}
        disabled={!s.sources.length}
        value={s.source_id}
        onChange={(event) => actions.source(event.currentTarget.value)}
      >
        {s.sources.length ? s.sources.map((source) => (
          <option key={source.source_id} value={source.source_id}>
            {source.kind === "service" ? `${source.label} · Published` : source.label}
          </option>
        )) : (
          <option value="">{empty_label}</option>
        )}
      </select>
    </label>
  );
}
function WorkspaceNavigation({ snapshot: s, actions, storage_enabled }: {
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
  storage_enabled: boolean;
}) {
  const views: WorkspaceView[] = storage_enabled ? ["workspace", "system", "storage"] : ["workspace", "system"];
  return (
    <nav aria-label="Workspace navigation">
      <fieldset id="workspace-view-options" className="workspace-navigation">
        <legend className="sr-only">Project page</legend>
        {views.map((view) => (
          <label className="navigation-item" key={view}>
            <input
              className="sr-only"
              type="radio"
              name="workspace_view"
              id={`workspace-view-${view}`}
              value={view}
              checked={s.workspace_view === view}
              onChange={(event) => {
                actions.workspace_view(view);
                // Pointer navigation reveals the stacked page. Keyboard radio
                // changes retain focus so arrow keys can continue through pages.
                const native = event.nativeEvent;
                if ("detail" in native && typeof native.detail === "number" && native.detail > 0)
                  revealOnCompactLayout(view === "system" ? "system-heading" : view === "storage" ? "storage-heading" : s.review ? "review-title" : "runs-heading");
              }}
            />
            <span><Icon name={view === "workspace" ? "runs" : view === "system" ? "system" : "storage"} />{view === "workspace" ? "Runs" : view === "system" ? "System" : "Storage"}</span>
          </label>
        ))}
      </fieldset>
    </nav>
  );
}
function RefreshControls({ snapshot: s, actions }: {
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
}) {
  return (
    <div className="refresh-status">
      <div className="refresh-context">
        <label className="auto-refresh">
          <input
            id="auto-refresh-toggle"
            type="checkbox"
            checked={s.auto_enabled}
            onChange={(event) => actions.auto_refresh(event.currentTarget.checked)}
          />
          Auto refresh
        </label>
        <span id="updated-at" className="freshness" title={s.freshness}>{s.freshness}</span>
      </div>
      <button
        id="refresh-button"
        className="button secondary"
        type="button"
        disabled={s.refreshing}
        onClick={actions.refresh}
      >
        <Icon name="refresh" />
        {s.refreshing ? "Refreshing…" : "Refresh"}
      </button>
    </div>
  );
}
export function DashboardView({ store, actions, duration_clock }: {
  store: DashboardStore;
  actions: DashboardActions;
  duration_clock: RefreshClock;
}) {
  const s = useSyncExternalStore(store.subscribe, store.get_snapshot);
  const source_controls = document.getElementById("source-controls-root");
  const utility_controls = document.getElementById("utility-controls-root");
  const navigation = document.getElementById("navigation-root");
  const theme_controls = document.getElementById("theme-controls-root");
  const project = document.getElementById("project-name");
  const kind = document.getElementById("dashboard-kind");
  const active_source = s.sources.find((source) => source.source_id === s.source_id);
  const project_id = active_source?.kind === "hosted_project" ? active_source.project_id : null;
  const storage_visible = s.workspace_view === "storage";
  const inspected_run = s.review?.kind === "run" ? s.detail?.run ?? s.runs.find(run => runIdentity(run) === s.review?.run_ids[0]) : null;
  const queue_only = inspected_run != null && queuePlaceholder(inspected_run);
  const duration_active = s.runs.some(hasRunningDuration) ||
    Boolean(s.review?.kind === "run" && s.detail && hasRunningDuration(s.detail.run));
  return (
    <RunDurationProvider clock={duration_clock} active={duration_active}>
      {project && createPortal(s.project_name, project)}
      {kind && createPortal(s.access_mode === "hosted" ? "Hosted workspace" : "Local workspace", kind)}
      {source_controls && createPortal(<SourceControls snapshot={s} actions={actions} />, source_controls)}
      {utility_controls && createPortal(<RefreshControls snapshot={s} actions={actions} />, utility_controls)}
      {navigation && createPortal(<WorkspaceNavigation snapshot={s} actions={actions} storage_enabled={!!project_id} />, navigation)}
      {theme_controls && createPortal(<ThemeControl compact />, theme_controls)}
      {!navigation && <WorkspaceNavigation snapshot={s} actions={actions} storage_enabled={!!project_id} />}
      <ErrorNotice id="global-error" message={s.global_error} />
      <div id="workspace-page">
        <div className={`workspace-grid history-grid${(s.run_columns.length || project_id) ? " has-wide-table" : ""}${s.review ? " has-review" : ""}`}>
          <RunBrowser snapshot={s} actions={actions} />
          <div className="main-workspace review-workspace">
            <ProjectStorageManagement
              project_id={project_id ?? null}
              active={storage_visible && !!project_id}
              auto_enabled={s.auto_enabled}
              on_project_removed={actions.refresh}
            />
            <div id="run-workspace" hidden={s.workspace_view !== "workspace"}>
              <ReviewWorkspace snapshot={s} actions={actions} />
            </div>
            <div id="system-workspace" hidden={s.workspace_view !== "system"}>
              <div className="workspace-heading"><div><h1 id="system-heading" tabIndex={-1}>System monitor</h1><p className="muted">Whole-machine readings, recorded independently of runs.</p></div></div>
              {!queue_only && s.run_system_supported && s.review?.kind === "run" && <RadioTags
                name="system_context" legend="Period scope" value={s.system_run_aligned ? "run" : "machine"}
                options={[{ id: "system-context-run", value: "run", label: "Run period" }, { id: "system-context-machine", value: "machine", label: "All machines" }]}
                on_change={value => actions.system_alignment(value === "run")}
              />}
              <div id="system-run-context" hidden={!s.system_run_aligned}>
                <RunSystemPanel scope_id="workspace" state={s.run_system} actions={actions.run_system} runs={[...s.runs, ...(s.detail ? [s.detail.run] : [])]} time_zone={s.time_zone} now={s.system_now} />
              </div>
              <div id="system-machine-context" hidden={s.system_run_aligned}>
                <SystemMonitorPage state={s.system_monitor} actions={actions.system_monitor} time_zone={s.time_zone} access_mode={s.access_mode} show_heading={false} />
              </div>
            </div>
            <div id="storage-workspace" hidden={!storage_visible}>
              <div className="workspace-heading storage-heading">
                <h1 id="storage-heading" tabIndex={-1}>Storage</h1>
              </div>
              {project_id && (
                <StoragePanel
                  project_id={project_id}
                  kind={s.storage_kind}
                  search={s.storage_search}
                  offset={s.storage_offset}
                  catalog={s.storage_catalog}
                  loading={s.storage_loading}
                  error={s.storage_error}
                  on_kind={actions.storage_kind}
                  on_search={actions.storage_search}
                  on_previous={actions.storage_previous}
                  on_next={actions.storage_next}
                  on_refresh={actions.storage_refresh}
                />
              )}
            </div>
          </div>
        </div>
      </div>
      <div id="live-status" className="sr-only" role="status" aria-live="polite">{s.live_status}</div>
    </RunDurationProvider>
  );
}
