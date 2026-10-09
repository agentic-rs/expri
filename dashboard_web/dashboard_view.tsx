import { Fragment, useSyncExternalStore, useState, type KeyboardEvent } from "react";
import { createPortal } from "react-dom";
import { FilesPanel, formatFileSize } from "./dashboard_files";
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
import {
  apiUrl,
  dateText,
  formatDuration,
  formatNumber,
  formatValue,
  jsonObject,
  localTimeZoneLabel,
  type ArtifactCatalog,
  type Comparison,
  type ComparisonReduction,
  type Detail,
  type Json,
  type Review,
  type ReviewTab,
  type Run,
  type RunColumn,
  type RunSort,
  type Source,
  type Warning,
} from "./dashboard_model";

export type EmptyRuns = { title: string; message: string; setup: boolean };
export type DashboardUi = {
  project_name: string;
  search: string;
  task: string;
  status: string;
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
  live_status: string;
};
export type DashboardSnapshot = DashboardUi & {
  sources: Source[];
  source_id: string;
  access_mode: "local" | "hosted";
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
  metric_names: string[];
  review_version: number;
  x_axis: ChartAxis;
  time_zone: ChartTimeZone;
  reduction: ComparisonReduction;
};
export type DashboardActions = {
  filter: (field: "search" | "task" | "status", value: string) => void;
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
};
export type DashboardStore = {
  subscribe: (listener: () => void) => () => void;
  get_snapshot: () => DashboardSnapshot;
};

type RadioOption<T extends string> = { id: string; value: T; label: string };
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
            {items.slice(0, 100).map((item, index) => (
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
}: {
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
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
    <details
      id="run-columns"
      className="run-columns-panel"
      onToggle={(event) => {
        if (event.currentTarget.open) a.refresh_run_columns();
      }}
    >
      <summary>
        Columns <span className="muted">{s.run_columns.length} / {MAX_RUN_COLUMNS}</span>
      </summary>
      <p id="run-columns-hint" className="muted">
        Run and Status stay visible. Choose up to eight parameter or metric columns.
      </p>
      {s.run_columns_loading && <p role="status">Finding recorded columns…</p>}
      <Warnings id="run-columns-warnings" items={s.run_columns_warnings} />
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
    </details>
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
function RunBrowser({
  snapshot: s,
  actions: a,
}: {
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
}) {
  const tasks = [
    ...new Set(s.runs.map((run) => run.task).filter((task): task is string => task !== null)),
  ].sort();
  return (
    <section className="card runs-card" aria-labelledby="runs-heading">
      <div className="section-heading">
        <div>
          <h2 id="runs-heading" tabIndex={-1}>
            Runs
          </h2>
          <p id="run-count" className="muted">
            {s.run_count}
          </p>
        </div>
      </div>
      <div className="filters">
        <label className="field search-field">
          Search
          <input
            id="search-input"
            type="search"
            placeholder="Run ID or task"
            autoComplete="off"
            disabled={s.controls_disabled}
            value={s.search}
            onChange={(event) => a.filter("search", event.currentTarget.value)}
          />
        </label>
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
        <RadioTags
          id="status-options"
          name="status"
          legend="Status"
          value={s.status}
          disabled={s.controls_disabled}
          options={[
            { id: "status-any", value: "", label: "Any" },
            ...["preparing", "running", "completed", "failed", "cancelled", "lost", "unknown"].map(
              (status) => ({
                id: `status-${status}`,
                value: status,
                label: `${status[0]?.toUpperCase()}${status.slice(1)}`,
              }),
            ),
          ]}
          on_change={(value) => a.filter("status", value)}
        />
        <button
          id="clear-filters"
          className="text-button"
          type="button"
          disabled={s.controls_disabled}
          onClick={a.clear_filters}
        >
          Clear filters
        </button>
        <p className="filter-hint">Changing filters clears the selection.</p>
      </div>
      <div className="run-table-toolbar">
        <RunColumnPicker key={s.source_id} snapshot={s} actions={a} />
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
      <Warnings id="list-warnings" items={s.list_warnings} />
      <div className="table-scroll runs-scroll" id="runs-region" aria-busy={s.list_busy}>
        <table
          className={`runs-table${s.run_columns.length ? " has-custom-columns" : ""}`}
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
              const metadata = [dateText(run.started_at), formatDuration(run)].filter(
                (value) => value !== "—",
              );
              if (run.exit_code !== null && run.exit_code !== 0)
                metadata.push(`exit ${run.exit_code}`);
              const selected = s.selected.includes(run.run_id);
              return (
                <tr
                  key={run.run_id}
                  className={[
                    selected && "selected",
                    s.review?.kind === "run" && s.review.run_ids[0] === run.run_id && "active",
                  ]
                    .filter(Boolean)
                    .join(" ")}
                >
                  <td className="selection-column">
                    <input
                      type="checkbox"
                      checked={selected}
                      disabled={s.list_busy || (s.selected.length >= 8 && !selected)}
                      aria-label={`Select ${run.run_id} for comparison`}
                      onChange={(event) => a.select_run(run.run_id, event.currentTarget.checked)}
                    />
                  </td>
                  <td className="run-cell">
                    <button
                      type="button"
                      className="run-link"
                      disabled={s.list_busy}
                      onClick={() => a.inspect_run(run.run_id)}
                    >
                      {run.run_id}
                    </button>
                    <div className="run-task">{run.task ?? "No task recorded"}</div>
                    <div className="run-meta">{metadata.join(" · ")}</div>
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
                      className={`status ${["preparing", "running", "completed", "failed", "cancelled", "lost", "unknown"].includes(run.status) ? run.status : ""}`}
                    >
                      {run.status}
                    </span>
                  </td>
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
                Set up result syncing
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
            {s.selected.map((id) => (
              <button
                type="button"
                className="selection-chip"
                key={id}
                aria-label={`Remove ${id} from comparison`}
                onClick={() => a.select_run(id, false)}
              >
                {id} ×
              </button>
            ))}
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
            onClick={a.compare}
          >
            Compare runs
          </button>
        </div>
      </div>
    </section>
  );
}
function Facts({ values }: { values: [string, string][] }) {
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
      {detail && (
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
                  ["Duration", formatDuration(detail.run)],
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
                <p className="muted">No parameters were recorded or pulled.</p>
              )}
            </section>
          </div>
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
                No metric summaries are available. Record metrics in outputs/metrics.jsonl or pull
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
      )}
    </div>
  );
}
function ArchiveSummary({ detail }: { detail: Detail | null }) {
  const archive = detail?.archive;
  if (!detail || !archive) return null;
  const labels = {
    none: "No archive",
    pending: "Archive pending",
    uploading: "Archiving",
    archived: "Archived",
    failed: "Archive failed",
  };
  return (
    <div id="archive-summary" className="notice" role="status">
      <strong id="archive-status">{labels[archive.status] ?? "Archive status unknown"}</strong>
      {archive.incomplete && <span> · Partial archive</span>}
      {archive.status === "archived" && (
        <>
          {" · "}
          <a
            id="download-archive"
            href={apiUrl("/api/archive", {
              source: detail.source.source_id,
              run_id: detail.run.run_id,
            })}
            target="_blank"
            rel="noopener noreferrer"
            download
          >
            Download archive{archive.file ? ` (${formatFileSize(archive.file.size)})` : ""}
          </a>
        </>
      )}
      {archive.last_error && <p>Archive issue: {archive.last_error.slice(0, 512)}</p>}
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
                  <th scope="row">{run.run_id}</th>
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
  const tabs: ReviewTab[] = ["charts", "overview", "logs", "files"];
  const available: ReviewTab[] = s.review?.kind === "compare" ? ["charts"] : tabs;
  return (
    <div className="review-workspace">
      <section id="review-empty" className="review-empty" hidden={!s.review_empty}>
        <p className="eyebrow">Experiment review</p>
        <h2>Select a run</h2>
        <p>
          Open a run to inspect its curves, parameters, and logs. Select runs to compare their
          recorded results.
        </p>
      </section>
      <section id="review-section" aria-live="polite" hidden={!s.review}>
        <div className="review-heading">
          <div>
            <p id="review-eyebrow" className="eyebrow">
              {s.review?.kind === "compare" ? "Experiment comparison" : "Run details"}
            </p>
            <h2 id="review-title">
              {s.review?.kind === "compare"
                ? `${s.review.run_ids.length} runs`
                : s.review?.run_ids[0] ?? ""}
            </h2>
          </div>
          <button
            id="close-review"
            className="text-button"
            type="button"
            hidden={s.review?.origin === "selection"}
            onClick={a.close}
          >
            {s.selected.length ? "Back to selection" : "Close review"}
          </button>
        </div>
        {s.access_mode === "hosted" && s.review?.kind === "run" && (
          <ArchiveSummary detail={s.detail} />
        )}
        <div className="review-tabs" role="tablist" aria-label="Run inspection">
          {tabs.map((tab) => (
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
              {tab[0]?.toUpperCase()}
              {tab.slice(1)}
            </button>
          ))}
        </div>
        <div id="review-loading" className="card loading-state" hidden={!s.review_loading}>
          Loading experiment details…
        </div>
        <ErrorNotice id="review-error" message={s.review_error} />
        <Warnings id="review-warnings" items={s.review_warnings} />
        <div
          id="review-panel-charts"
          role="tabpanel"
          aria-labelledby="review-tab-charts"
          hidden={s.review?.tab !== "charts"}
        >
          <details
            id="run-metric-controls"
            className="card metric-settings"
            hidden={!s.detail || s.review?.kind !== "run"}
          >
            <summary>
              Chart metrics <span className="muted">Choose up to six</span>
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
            <section className="card comparison-controls">
              <div className="section-heading">
                <div>
                  <h3>Compare selected runs</h3>
                  <p className="muted">Recorded scalar values from this source.</p>
                </div>
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
              <details className="metric-settings">
                <summary>
                  Metrics <span className="muted">Choose up to six</span>
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
            </section>
            <section className="card comparison-summary">
              <h3>Metric values</h3>
              <ComparisonValues result={s.comparison} busy={s.comparison_busy} />
            </section>
          </div>
          <section
            id="chart-card"
            className="card chart-card"
            hidden={!s.chart_visible}
            aria-busy={s.chart_busy}
          >
            <div className="section-heading">
              <div>
                <h3>Metric curves</h3>
                <p id="chart-note" className="muted">
                  {s.chart_note}
                </p>
              </div>
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
            <ErrorNotice id="chart-error" message={s.chart_error} />
            <iframe
              id="chart-frame"
              title="Interactive experiment metric curves"
              sandbox="allow-same-origin"
              loading="lazy"
            ></iframe>
            <p className="chart-footnote muted">
              Hover to inspect values, drag across the x-axis to zoom, and use the legend to show or
              hide runs. Time views omit points without recorded timestamps.
            </p>
          </section>
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
export function DashboardView({
  store,
  actions,
}: {
  store: DashboardStore;
  actions: DashboardActions;
}) {
  const s = useSyncExternalStore(store.subscribe, store.get_snapshot);
  const source_controls = document.getElementById("source-controls-root");
  const project = document.getElementById("project-name");
  const kind = document.getElementById("dashboard-kind");
  return (
    <>
      {project && createPortal(s.project_name, project)}
      {kind &&
        createPortal(
          s.access_mode === "hosted"
            ? "expri · Synced experiment review"
            : "expri · Local experiment review",
          kind,
        )}
      {source_controls &&
        createPortal(
          <>
            <label className="field">
              Source
              <select
                id="source-select"
                aria-label="Run source"
                disabled={!s.sources.length}
                value={s.source_id}
                onChange={(event) => actions.source(event.currentTarget.value)}
              >
                {s.sources.length ? (
                  s.sources.map((source) => (
                    <option key={source.source_id} value={source.source_id}>
                      {source.kind === "service" ? `${source.label} · Synced` : source.label}
                    </option>
                  ))
                ) : (
                  <option value="">
                    {s.refreshing ? "Loading sources…" : "No synced sources"}
                  </option>
                )}
              </select>
            </label>
            <button
              id="refresh-button"
              className="button secondary"
              type="button"
              disabled={s.refreshing}
              onClick={actions.refresh}
            >
              <span aria-hidden="true">↻</span> Refresh
            </button>
          </>,
          source_controls,
        )}
      <div className="workspace-heading">
        <div>
          <p className="eyebrow">Experiments</p>
          <h1>Workspace</h1>
        </div>
        <div className="refresh-status">
          <label className="auto-refresh">
            <input
              id="auto-refresh-toggle"
              type="checkbox"
              checked={s.auto_enabled}
              onChange={(event) => actions.auto_refresh(event.currentTarget.checked)}
            />
            Auto refresh
          </label>
          <span id="updated-at" className="freshness">
            {s.freshness}
          </span>
        </div>
      </div>
      <div id="source-note" className="source-note" hidden={s.source_note === null}>
        {s.source_note}
      </div>
      <ErrorNotice id="global-error" message={s.global_error} />
      <Warnings id="catalog-warnings" items={s.catalog_warnings} />
      <div className={`workspace-grid${s.run_columns.length ? " has-custom-columns" : ""}`}>
        <RunBrowser snapshot={s} actions={actions} />
        <ReviewWorkspace snapshot={s} actions={actions} />
      </div>
      <div id="live-status" className="sr-only" role="status" aria-live="polite">
        {s.live_status}
      </div>
    </>
  );
}
