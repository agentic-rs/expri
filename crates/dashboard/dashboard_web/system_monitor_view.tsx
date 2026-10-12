import { useState } from "react";
import { formatFileSize } from "./dashboard_files";
import { chartXFraction, chartYFraction, chartValueAtFraction, dateParts, formatChartTick, formatChartX, timeZoneLabel, type ChartRange, type ChartTimeZone } from "./chart_math";
import type { SystemMonitorSnapshot, SystemPeriod, SystemPoint, SystemResolution, SystemSeries } from "./system_monitor";

export type SystemMonitorActions = {
  machine: (machine_id: string) => void;
  period: (period: SystemPeriod) => void;
  resolution: (resolution: SystemResolution) => void;
  metric: (metric_name: string, selected: boolean) => void;
  more_machines: () => void;
  refresh: () => void;
  time_zone: (time_zone: ChartTimeZone) => void;
};
const PERIODS: { value: SystemPeriod; label: string }[] = [
  { value: "1h", label: "1 hour" }, { value: "6h", label: "6 hours" }, { value: "24h", label: "24 hours" },
  { value: "72h", label: "72 hours" }, { value: "7d", label: "7 days" }, { value: "30d", label: "30 days" },
];
function Choices<T extends string>({ name, legend, value, options, on_change }: {
  name: string; legend: string; value: T; options: { value: T; label: string }[]; on_change: (value: T) => void;
}) {
  return <fieldset className="choice-group" id={`${name}-options`}>
    <legend>{legend}</legend>
    <div className="choice-tags">{options.map(option => <label className="choice-tag" key={option.value}>
      <input className="sr-only" type="radio" name={name} id={`${name}-${option.value}`} value={option.value} checked={value === option.value} onChange={() => on_change(option.value)} />
      <span>{option.label}</span>
    </label>)}</div>
  </fieldset>;
}
function timestamp(value: number | null, time_zone: ChartTimeZone): string {
  return value === null ? "No observation yet" : formatChartX(String(BigInt(value) * 1_000_000n), "wall_clock", time_zone);
}
function InventoryTimestamp({ value, time_zone }: { value: number | null; time_zone: ChartTimeZone }) {
  const parts = value === null ? null : dateParts(BigInt(value) * 1_000_000n, time_zone);
  return <span title={timestamp(value, time_zone)}>{parts ? `${parts.date_text.slice(5)} ${parts.time_text.slice(0, 8)} ${parts.offset_text}` : timestamp(value, time_zone)}</span>;
}
function tickValue(value: number, series: SystemSeries): string {
  if (series.metric_name.endsWith("_bytes") && value >= 1024) {
    const units = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    let scaled = value / 1024, index = 0;
    while (scaled >= 1024 && index < units.length - 1) { scaled /= 1024; index++; }
    return `${new Intl.NumberFormat(undefined, { maximumSignificantDigits: 3, useGrouping: false }).format(scaled)}${units[index]}${series.kind === "counter_delta" ? "/s" : ""}`;
  }
  return systemMetricValue(value, series.metric_name, series.kind).replace(/\s/g, "");
}
export function systemMetricValue(value: number, metric_name: string, kind: SystemSeries["kind"]): string {
  if (metric_name.endsWith("_bytes")) return `${formatFileSize(value)}${kind === "counter_delta" ? "/s" : ""}`;
  const number = new Intl.NumberFormat(undefined, { maximumSignificantDigits: 5 }).format(value);
  const suffix = metric_name.endsWith("_percent") ? "%" : metric_name.endsWith("_mhz") ? " MHz" : metric_name.endsWith("_watts") ? " W" : metric_name.endsWith("_celsius") ? " °C" : kind === "counter_delta" ? "/s" : "";
  return `${number}${suffix}`;
}
function sampledPoints(points: SystemPoint[], maximum = 600): SystemPoint[] {
  if (points.length <= maximum) return points;
  const retained = new Set<number>([0, points.length - 1]);
  const bucket_count = Math.floor((maximum - 2) / 4), bucket_size = points.length / bucket_count;
  for (let bucket = 0; bucket < bucket_count; bucket++) {
    const start = Math.floor(bucket * bucket_size), end = Math.min(points.length, Math.floor((bucket + 1) * bucket_size));
    let low = start, high = start;
    for (let index = start; index < end; index++) {
      if (points[index]!.value < points[low]!.value) low = index;
      if (points[index]!.value > points[high]!.value) high = index;
    }
    retained.add(start); retained.add(end - 1); retained.add(low); retained.add(high);
  }
  const indices = [...retained].sort((a, b) => a - b);
  return indices.map((index, position) => ({ ...points[index]!, break_before: points.slice(indices[position - 1] ?? index, index + 1).some(point => point.break_before) }));
}
function SystemPlot({ series, start_ms, end_ms, resolution, time_zone }: {
  series: SystemSeries; start_ms: number; end_ms: number; resolution: SystemResolution; time_zone: ChartTimeZone;
}) {
  const [active_index, setActiveIndex] = useState<number | null>(null);
  const range: ChartRange = { x_axis: "wall_clock", start_x: String(BigInt(start_ms) * 1_000_000n), end_x: String(BigInt(end_ms) * 1_000_000n) };
  const points = sampledPoints(series.points), values = series.points.map(point => point.value);
  const minimum = Math.min(...values), maximum = Math.max(...values);
  const x = (point: SystemPoint): number => 65 + chartXFraction(String(BigInt(point.observed_at_ms) * 1_000_000n), range) * 620;
  const y = (point: SystemPoint): number => 20 + (1 - chartYFraction(point.value, minimum, maximum)) * 180;
  const segments: SystemPoint[][] = [];
  for (const point of points) { if (!segments.length || point.break_before) segments.push([]); segments.at(-1)!.push(point); }
  const active = active_index === null ? null : points[Math.min(active_index, points.length - 1)] ?? null;
  const last = series.points.at(-1);
  const coverage_ms = series.points.reduce((sum, point) => sum + point.coverage_ms, 0);
  return <section className="system-chart evidence-panel" data-system-metric={series.metric_name}>
    <header className="metric-chart-header"><h2>{series.metric_name}</h2>{last && <div className="metric-chart-summary"><span>{resolution === "hourly" ? "Latest mean" : "Last"}</span><strong>{systemMetricValue(last.value, series.metric_name, series.kind)}</strong></div>}</header>
    <svg viewBox="-120 0 840 250" role="img" tabIndex={0} aria-label={`${series.metric_name}, ${resolution === "hourly" ? "hourly means" : "recorded samples"}. Use left and right arrows to inspect readings.`}
      onPointerMove={event => {
        const bounds = event.currentTarget.getBoundingClientRect(), target = (event.clientX - bounds.left) / bounds.width * 840 - 120;
        let nearest = 0;
        for (let index = 1; index < points.length; index++) if (Math.abs(x(points[index]!) - target) < Math.abs(x(points[nearest]!) - target)) nearest = index;
        setActiveIndex(nearest);
      }}
      onPointerLeave={() => setActiveIndex(null)}
      onFocus={() => setActiveIndex(points.length - 1)}
      onBlur={() => setActiveIndex(null)}
      onKeyDown={event => {
        if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
        event.preventDefault();
        setActiveIndex(event.key === "Home" ? 0 : event.key === "End" ? points.length - 1 : Math.max(0, Math.min(points.length - 1, (active_index ?? points.length - 1) + (event.key === "ArrowLeft" ? -1 : 1))));
      }}>
      <title>{series.metric_name}</title><desc>Gaps remain disconnected. Hourly values use measured coverage; missing time is never zero.</desc>
      {Array.from({ length: minimum === maximum ? 1 : 3 }, (_, index) => {
        const fraction = minimum === maximum ? .5 : index / 2, value = chartValueAtFraction(minimum, maximum, fraction), position = 20 + (1 - fraction) * 180;
        return <g key={index}><line className="grid" x1="65" x2="685" y1={position} y2={position} /><text className="tick" x="58" y={position + 4} textAnchor="end"><title>{systemMetricValue(value, series.metric_name, series.kind)}</title>{tickValue(value, series)}</text></g>;
      })}
      {[0, .5, 1].map(fraction => {
        const value = BigInt(range.start_x) + BigInt(Math.round((end_ms - start_ms) * fraction)) * 1_000_000n;
        const tick_value = value / 1_000_000_000n * 1_000_000_000n;
        return <text className="tick" key={fraction} x={65 + fraction * 620} y="225" textAnchor={fraction === 0 ? "start" : fraction === 1 ? "end" : "middle"}><title>{formatChartX(String(value), "wall_clock", time_zone)}</title>{formatChartTick(String(tick_value), range, time_zone)}</text>;
      })}
      {segments.map((segment, index) => <polyline key={index} fill="none" stroke="currentColor" strokeWidth="2" points={segment.map(point => `${x(point)},${y(point)}`).join(" ")} />)}
      {points.map((point, index) => <circle key={index} cx={x(point)} cy={y(point)} r="2" fill="currentColor" />)}
      {active && <g className="system-chart-inspection"><line x1={x(active)} x2={x(active)} y1="20" y2="200" /><circle cx={x(active)} cy={y(active)} r="4" /></g>}
    </svg>
    <div className="system-point-inspection" aria-live="polite">{active ? <><strong>{systemMetricValue(active.value, series.metric_name, series.kind)}</strong> · {timestamp(active.observed_at_ms, time_zone)} · {active.sample_count} sample{active.sample_count === 1 ? "" : "s"} · {(active.coverage_ms / 1000).toLocaleString()}s measured</> : <span className="muted">{series.points.length.toLocaleString()} {resolution === "hourly" ? "hourly means" : "readings"} · {(coverage_ms / 1000).toLocaleString()}s measured{points.length < series.points.length ? " · Curve sampled to 600 points" : ""}</span>}</div>
  </section>;
}
export function SystemMonitorPage({ state: s, actions: a, time_zone, access_mode }: { state: SystemMonitorSnapshot; actions: SystemMonitorActions; time_zone: ChartTimeZone; access_mode: "local" | "hosted" }) {
  const [machine_search, setMachineSearch] = useState(""), [metric_search, setMetricSearch] = useState("");
  const machines = s.machines.filter(machine => machine.machine_id === s.machine_id || `${machine.machine_id} ${machine.inventory.hostname}`.toLocaleLowerCase().includes(machine_search.toLocaleLowerCase()));
  const profile = s.profile, inventory = profile?.inventory;
  const all_metrics = [...new Set([...s.selected_metrics, ...s.metric_names])];
  const visible_metrics = all_metrics.filter(name => s.selected_metrics.includes(name) || name.toLocaleLowerCase().includes(metric_search.toLocaleLowerCase())).slice(0, 64);
  return <section className="system-monitor" aria-labelledby="system-heading" aria-busy={s.loading}>
    <div className="workspace-heading"><div><h1 id="system-heading" tabIndex={-1}>System monitor</h1><p className="muted">Whole-machine readings, recorded independently of runs.</p></div>{s.loading && <span className="muted" role="status">Loading readings…</span>}</div>
    {s.error && <div className="notice error" role="alert">{s.error}{s.loaded_at_ms !== null && <p>Showing the last successful readings.</p>}<button type="button" className="text-button" onClick={a.refresh} disabled={s.loading}>Retry system monitoring</button></div>}
    {!s.machines.length && !s.loading && !s.error && <div className="empty-state"><h2>No recorded machines</h2><p>Start <code>expri system daemon</code> on a machine to collect readings. {access_mode === "hosted" ? "Publish them to this server to view its history here." : "Local readings appear here once collected."}</p></div>}
    {!!s.machines.length && <>
      <div className="system-controls inspector-controls">
        {s.machines.length > 12 && <label className="field">Find a machine<input type="search" maxLength={96} value={machine_search} onChange={event => setMachineSearch(event.currentTarget.value)} /></label>}
        <Choices name="system-machine" legend="Machine" value={s.machine_id} options={machines.map(machine => ({ value: machine.machine_id, label: `${machine.inventory.hostname} · ${machine.machine_id}` }))} on_change={a.machine} />
        {s.machine_cursor && <button className="text-button" type="button" onClick={a.more_machines} disabled={s.loading || s.machines.length >= 400}>Load more machines</button>}
        {s.machine_cursor && s.machines.length >= 400 && <p className="muted">Showing up to 400 recorded machines. Use the service CLI to browse the remaining machines.</p>}
        <Choices name="system-period" legend="Period" value={s.period} options={PERIODS} on_change={a.period} />
        <Choices name="system-resolution" legend="Resolution" value={s.resolution} options={[{ value: "detail", label: "Detail" }, { value: "hourly", label: "Hourly" }]} on_change={a.resolution} />
        <Choices name="system-time-zone" legend="Timezone" value={time_zone} options={[{ value: "local", label: "Local" }, { value: "utc", label: "UTC" }]} on_change={a.time_zone} />
      </div>
      {inventory && profile && <div className="system-inventory evidence-panel">
        <div className="system-profile-heading"><h2>{inventory.hostname}</h2><code>{profile.machine_id}</code><span className="muted">{inventory.measurement_scope}</span></div>
        <dl className="system-inventory-summary">
          <div><dt>CPU</dt><dd>{inventory.cpu.brand} · {inventory.cpu.logical_cpu_count} logical CPUs</dd></div>
          <div><dt>RAM</dt><dd>{formatFileSize(inventory.memory_total_bytes)}</dd></div>
          <div><dt>Platform</dt><dd>{[inventory.os_name, inventory.os_version, inventory.architecture].filter(Boolean).join(" · ")}</dd></div>
          <div><dt>Last observed</dt><dd><InventoryTimestamp value={profile.last_sample_at_ms} time_zone={time_zone} /></dd></div>
          <div><dt>Last received</dt><dd><InventoryTimestamp value={profile.last_received_at_ms} time_zone={time_zone} /></dd></div>
        </dl>
        {!!inventory.devices.length && <details><summary>{inventory.devices.length} devices · Inventory revision {profile.inventory_revision}</summary><div className="table-scroll"><table><thead><tr><th>Kind</th><th>Device</th><th>Capacity</th><th>Details</th></tr></thead><tbody>{inventory.devices.map(device => <tr key={device.device_id}><td>{device.kind}</td><td>{device.name}<br /><code>{device.device_id}</code></td><td>{device.total_bytes === null ? "—" : formatFileSize(device.total_bytes)}</td><td>{Object.entries(device.attributes).map(([key, value]) => <div key={key}>{key}: {value}</div>)}</td></tr>)}</tbody></table></div></details>}
      </div>}
      <p className="system-data-note muted">{s.resolution === "detail" ? "Detail is retained for 72 hours. Counter deltas are shown per second over each measured interval. Gaps and unavailable readings remain missing." : "Hourly charts show means over measured coverage, or average counter rates. Buckets use UTC hours and can extend beyond the selected period; missing time is excluded."} Timezone: {timeZoneLabel(time_zone)}.</p>
      {s.start_ms < s.end_ms && <p className="muted system-range">{timestamp(s.start_ms, time_zone)} → {timestamp(s.end_ms, time_zone)}</p>}
      {s.truncated && <div className="notice warning" role="status">This detail view reached the bounded history limit. The first returned portion is shown. Choose a shorter period or Hourly to cover more time.</div>}
      {!!s.warnings.length && <details className="system-warnings notice"><summary>{s.warnings.length} collection notes</summary><ul>{s.warnings.map(note => <li key={note}>{note}</li>)}</ul></details>}
      {!!s.metric_names.length && <details className="system-metric-picker">
        <summary>Metrics · {s.selected_metrics.length} selected · Up to six charts</summary>
        <label className="field">Find a metric
          <input type="search" value={metric_search} maxLength={256} onChange={event => setMetricSearch(event.currentTarget.value)} />
        </label>
        <div className="choice-tags">{visible_metrics.map(name => <label className="choice-tag" key={name}>
          <input
            className="sr-only" type="checkbox" checked={s.selected_metrics.includes(name)}
            disabled={!s.selected_metrics.includes(name) && s.selected_metrics.length >= 6}
            onChange={event => a.metric(name, event.currentTarget.checked)}
          />
          <span>{name}</span>
        </label>)}</div>
        {visible_metrics.length < s.metric_names.length && <p className="muted">Showing up to 64 matches. Search for other recorded metrics.</p>}
      </details>}
      {!s.loading && !s.error && !s.series.length && <div className="empty-state"><h2>No readings in this period</h2><p>Choose another period or Hourly for older history. Warmup and unavailable readings are not plotted as zero.</p></div>}
      {!!s.series.length && !s.selected_metrics.length && <p className="notice">Select a metric to plot recorded readings.</p>}
      <div className="system-charts">{s.selected_metrics.map(name => {
        const series = s.series.find(series => series.metric_name === name);
        return series && series.points.length ? <SystemPlot key={`${s.machine_id}:${name}:${s.resolution}`} series={series} start_ms={s.start_ms} end_ms={s.end_ms} resolution={s.resolution} time_zone={time_zone} /> : <p className="notice" key={name}>{name}: no readings in this period.</p>;
      })}</div>
    </>}
  </section>;
}
