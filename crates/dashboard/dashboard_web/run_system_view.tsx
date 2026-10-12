import { useState } from "react";
import { formatChartX, type ChartTimeZone } from "./chart_math";
import { runIdentity, runKeyLabel, runLabel, type Run } from "./dashboard_model";
import { runSystemEnd, runSystemItem, type RunSystemAxis, type RunSystemReduction, type RunSystemSnapshot, type RunSystemSummary } from "./run_system";
import { SystemChoices, SystemPlot, systemMetricValue } from "./system_monitor_view";
import { formatFileSize } from "./dashboard_files";

export type RunSystemActions = {
  axis: (axis: RunSystemAxis) => void; reduction: (reduction: RunSystemReduction) => void;
  metric: (metric_name: string, selected: boolean) => void; focus_run: (run_id: string) => void;
  refresh: () => void; time_zone: (time_zone: ChartTimeZone) => void;
};
function at(value: number | null, time_zone: ChartTimeZone): string { return value === null ? "End not recorded" : formatChartX(String(BigInt(value) * 1_000_000n), "wall_clock", time_zone); }
function summaryValue(summary: RunSystemSummary | undefined, reduction: RunSystemReduction, name: string): string {
  const value = summary?.[reduction];
  return value == null ? "—" : systemMetricValue(value, name, summary!.kind);
}
function ownSummary(summaries: Record<string, RunSystemSummary> | undefined, name: string): RunSystemSummary | undefined {
  return summaries && Object.hasOwn(summaries, name) ? summaries[name] : undefined;
}
export function RunSystemPanel({ state: s, actions: a, runs, time_zone, now, scope_id }: {
  state: RunSystemSnapshot; actions: RunSystemActions; runs: Run[]; time_zone: ChartTimeZone; now: number;
  scope_id: "inspector" | "workspace";
}) {
  const [search, setSearch] = useState("");
  const item = runSystemItem(s, s.focused_run_id), data = item?.data;
  const label = (id: string): string => { const run = runs.find(run => runIdentity(run) === id); return run ? runLabel(run) : runKeyLabel(id); };
  const names = [...new Set([...s.selected_metrics, ...s.metric_names])].filter(name => s.selected_metrics.includes(name) || name.toLocaleLowerCase().includes(search.toLocaleLowerCase())).slice(0, 64);
  const inventory = data?.inventories.at(-1)?.inventory;
  const error = Object.hasOwn(s.errors, s.focused_run_id) ? s.errors[s.focused_run_id] : null;
  return <section className="run-system-panel" aria-label="System metrics recorded during runs" aria-busy={s.loading}>
    <div className="run-system-heading"><div><h2>System metrics</h2><p className="muted">Whole-machine readings recorded during the run. Exact captures follow run retention.</p></div>{s.loading && <span className="muted" role="status">Loading retained readings…</span>}</div>
    {s.run_ids.length > 1 && <>
      <div className="inspector-controls"><SystemChoices name={`${scope_id}-run-system-reduction`} legend="Compare summary" value={s.reduction} options={[{ value: "last", label: "Last" }, { value: "min", label: "Minimum" }, { value: "max", label: "Maximum" }, { value: "mean", label: "Mean" }]} on_change={a.reduction} /></div>
      <div className="table-scroll run-system-comparison"><table><thead><tr><th>Run</th>{s.selected_metrics.map(name => <th key={name}>system/{name}</th>)}</tr></thead><tbody>{s.run_ids.map(id => <tr key={id}>
        <th scope="row">{label(id)}{Object.hasOwn(s.errors, id) && <small className="error-text">{s.errors[id]}</small>}</th>
        {s.selected_metrics.map(name => <td key={name} className="number">{summaryValue(ownSummary(runSystemItem(s, id)?.data.metric_summaries, name), s.reduction, name)}</td>)}
      </tr>)}</tbody></table></div>
      <SystemChoices name={`${scope_id}-run-system-focus`} legend="Inspect timeline" value={s.focused_run_id} options={s.run_ids.map(id => ({ value: id, label: label(id) }))} on_change={a.focus_run} />
    </>}
    {error && <div className="notice error" role="alert">{error}<button className="text-button" type="button" onClick={a.refresh} disabled={s.loading}>Retry retained readings</button></div>}
    {data && <div className="run-system-context evidence-panel">
      <div className="system-profile-heading"><strong>{label(data.run_id)}</strong><span className={`status ${data.status === "complete" ? "completed" : data.status === "recording" ? "running" : "unknown"}`}>{data.status}</span>{data.machine_id && <code>{data.machine_id}</code>}</div>
      {data.start_ms !== null && <p className="muted run-system-window">{at(data.start_ms, time_zone)} → {data.end_ms === null && data.status === "recording" ? "Recording" : at(data.end_ms, time_zone)}</p>}
      {inventory && <p className="muted">{inventory.cpu.brand} · {inventory.cpu.logical_cpu_count} logical CPUs · {formatFileSize(inventory.memory_total_bytes)} RAM · {inventory.measurement_scope} · {data.inventories.length} captured inventory revision{data.inventories.length === 1 ? "" : "s"}</p>}
      {data.status === "unavailable" && <p>No retained system capture was recorded for this run. Start system collection before a new run to preserve its readings.</p>}
      {data.status === "incomplete" && <p className="muted">This capture is incomplete. Available recorded readings remain visible.</p>}
      {!!data.warnings.length && <details className="system-warnings"><summary>{data.warnings.length} capture notes</summary><ul>{data.warnings.map((warning, index) => <li key={index}>{warning}</li>)}</ul></details>}
    </div>}
    {!!s.metric_names.length && <>
      <details className="system-metric-picker"><summary>System metrics · {s.selected_metrics.length} selected · Up to six charts</summary>
        <label className="field">Find a system metric<input type="search" maxLength={256} value={search} onChange={event => setSearch(event.currentTarget.value)} /></label>
        <div className="choice-tags">{names.map(name => <label className="choice-tag" key={name}><input className="sr-only" type="checkbox" checked={s.selected_metrics.includes(name)} disabled={!s.selected_metrics.includes(name) && s.selected_metrics.length >= 6} onChange={event => a.metric(name, event.currentTarget.checked)} /><span>system/{name}</span></label>)}</div>
        {names.length < s.metric_names.length && <p className="muted">Showing up to 64 matches. Search for another recorded system metric.</p>}
      </details>
      {data && <details className="run-system-summaries"><summary>Complete capture summaries</summary><div className="table-scroll"><table><thead><tr><th>System metric</th><th>Samples</th><th>Last</th><th>Minimum</th><th>Maximum</th><th>Mean</th></tr></thead><tbody>{s.selected_metrics.map(name => {
        const summary = ownSummary(data.metric_summaries, name);
        return <tr key={name}><th scope="row">system/{name}</th><td className="number">{summary?.count ?? "—"}</td>{(["last", "min", "max", "mean"] as const).map(reduction => <td className="number" key={reduction}>{summaryValue(summary, reduction, name)}</td>)}</tr>;
      })}</tbody></table></div></details>}
      <div className="inspector-controls run-system-chart-controls">
        <SystemChoices name={`${scope_id}-run-system-axis`} legend="Time axis" value={s.x_axis} options={[{ value: "run_elapsed", label: "Run elapsed" }, { value: "wall_clock", label: "Date & time" }]} on_change={a.axis} />
        {s.x_axis === "wall_clock" && <SystemChoices name={`${scope_id}-run-system-time-zone`} legend="Timezone" value={time_zone} options={[{ value: "local", label: "Local" }, { value: "utc", label: "UTC" }]} on_change={a.time_zone} />}
      </div>
      <p className="muted system-data-note">{s.x_axis === "run_elapsed" ? "Time since this run started." : "Original observation times within this run’s recorded period."} Counter rates use each original measured interval. Gaps remain missing.</p>
    </>}
    {item?.preview_truncated && <p className="notice warning">The chart preview shows the first 8192 retained readings or its bounded byte window. Summary values cover the complete capture.</p>}
    <div className="system-charts">{item && data?.start_ms !== null && s.selected_metrics.map(name => {
      const series = item.series.find(series => series.metric_name === name);
      return series?.points.length ? <SystemPlot key={`${s.focused_run_id}:${name}:${s.x_axis}`} series={series} start_ms={data!.start_ms!} end_ms={runSystemEnd(data!, now)} resolution="detail" time_zone={time_zone} x_axis={s.x_axis} title_label={`system/${name}`} summary_last={ownSummary(data!.metric_summaries, name)?.last ?? null} /> : <p className="notice" key={name}>system/{name}: no displayed readings in this capture.</p>;
    })}</div>
    {!s.loading && data && data.status !== "unavailable" && !s.metric_names.length && <p className="notice">No system metric readings were recorded during this run.</p>}
  </section>;
}
