import { apiUrl, errorText, RequestError, RequestLane } from "./dashboard_model";
import { parseSystemHistory, parseSystemMachine, systemHistorySeries, type SystemInventory, type SystemMetricKind, type SystemSample, type SystemSeries } from "./system_monitor";

export type RunSystemAxis = "run_elapsed" | "wall_clock";
export type RunSystemReduction = "last" | "min" | "max" | "mean";
export type RunSystemSummary = { kind: SystemMetricKind; count: number; min: number | null; max: number | null; last: number | null; mean: number | null; unit: string | null };
export type RunSystemData = {
  run_id: string; capture_id: string | null; machine_id: string | null; start_ms: number | null; end_ms: number | null;
  status: "recording" | "complete" | "incomplete" | "unavailable";
  inventories: { revision: number; inventory: SystemInventory }[]; samples: SystemSample[];
  metric_summaries: Record<string, RunSystemSummary>; sample_count: number; next_cursor: number | null; warnings: string[];
};
export type RunSystemItem = { data: RunSystemData; cursor: number; preview_truncated: boolean; series: SystemSeries[] };
export type RunSystemSnapshot = {
  source_id: string; run_ids: string[]; focused_run_id: string; items: Record<string, RunSystemItem>; errors: Record<string, string>;
  loading: boolean; metric_names: string[]; selected_metrics: string[]; x_axis: RunSystemAxis; reduction: RunSystemReduction;
};
const MAX_PREVIEW_SAMPLES = 8192, MAX_PAGES = 16;
const READ_LIMIT = 512 * 1024;
const READ_LIMIT_MESSAGE = "The retained system response exceeds the 512 KiB read limit. Use the CLI to inspect the complete capture.";
function invalid(): never { throw new Error("The retained system response does not match the selected run or exceeds its limits."); }
function object(value: unknown): Record<string, unknown> { if (!value || typeof value !== "object" || Array.isArray(value)) invalid(); return value as Record<string, unknown>; }
function integer(value: unknown): number { if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) invalid(); return value; }
function nullableInteger(value: unknown): number | null { return value === null ? null : integer(value); }
function finite(value: unknown): number { if (typeof value !== "number" || !Number.isFinite(value)) invalid(); return value; }
function nullableFinite(value: unknown): number | null { return value === null ? null : finite(value); }
function text(value: unknown, maximum: number): string { if (typeof value !== "string" || value.length > maximum || /[\u0000-\u001f\u007f]/.test(value)) invalid(); return value; }
function component(value: unknown): string | null { if (value === null) return null; const result = text(value, 96); if (!/^[A-Za-z0-9._-]+$/.test(result) || result === "." || result === "..") invalid(); return result; }
function array(value: unknown, maximum: number): unknown[] { if (!Array.isArray(value) || value.length > maximum) invalid(); return value; }
export function parseRunSystem(value: unknown, run_id: string): RunSystemData {
  const data = object(value);
  if (data.run_id !== run_id || typeof data.status !== "string" || !["recording", "complete", "incomplete", "unavailable"].includes(data.status)) invalid();
  const capture_id = component(data.capture_id), machine_id = component(data.machine_id), start_ms = nullableInteger(data.start_ms), end_ms = nullableInteger(data.end_ms);
  if (data.status !== "unavailable" && (!capture_id || !machine_id || start_ms === null)) invalid();
  if (data.status === "unavailable" && (capture_id !== null || machine_id !== null || start_ms !== null || end_ms !== null)) invalid();
  if (start_ms !== null && end_ms !== null && end_ms < start_ms) invalid();
  const samples = machine_id ? parseSystemHistory({ machine_id, resolution: "detail", samples: data.samples, hours: [], next_cursor: null }, machine_id, "detail").samples : [];
  if (!machine_id && array(data.samples, 1024).length) invalid();
  if (samples.some(sample => start_ms === null || sample.observed_at_ms < start_ms || (end_ms !== null && sample.observed_at_ms >= end_ms))) invalid();
  const inventories = array(data.inventories, 1024).map(value => {
    const record = object(value), revision = integer(record.revision);
    if (!machine_id || !revision) invalid();
    const profile = parseSystemMachine({ machine_id, inventory_revision: revision, inventory: record.inventory, last_sample_at_ms: null, last_received_at_ms: null });
    return { revision, inventory: profile.inventory };
  });
  if (new Set(inventories.map(record => record.revision)).size !== inventories.length) invalid();
  const summaries = Object.entries(object(data.metric_summaries));
  if (summaries.length > 256) invalid();
  const metric_summaries = Object.fromEntries(summaries.map(([name, value]) => {
    if (!/^[A-Za-z0-9/_\-.:]{1,256}$/.test(name)) invalid();
    const summary = object(value), count = integer(summary.count);
    if (summary.kind !== "gauge" && summary.kind !== "counter_delta") invalid();
    const min = nullableFinite(summary.min), max = nullableFinite(summary.max), last = nullableFinite(summary.last), mean = nullableFinite(summary.mean);
    if (count === 0 ? min !== null || max !== null || last !== null || mean !== null : min === null || max === null || last === null || min > max || last < min || last > max) invalid();
    return [name, { kind: summary.kind, count, min, max, last, mean, unit: summary.unit == null ? null : text(summary.unit, 64) } as RunSystemSummary];
  }));
  const sample_count = integer(data.sample_count), next_cursor = nullableInteger(data.next_cursor);
  if (sample_count < samples.length || (next_cursor !== null && next_cursor > sample_count)) invalid();
  if (data.status === "unavailable" && (sample_count !== 0 || summaries.length !== 0 || next_cursor !== null)) invalid();
  return { run_id, capture_id, machine_id, start_ms, end_ms, status: data.status as RunSystemData["status"], inventories, samples, metric_summaries, sample_count, next_cursor, warnings: array(data.warnings, 100).map(value => text(value, 512)) };
}
export function runSystemItem(snapshot: RunSystemSnapshot, run_id: string): RunSystemItem | undefined {
  return Object.hasOwn(snapshot.items, run_id) ? snapshot.items[run_id] : undefined;
}
export function runSystemEnd(data: RunSystemData, now: number): number {
  const start = data.start_ms ?? 0, last = data.samples.reduce((latest, sample) => Math.max(latest, sample.observed_at_ms), start);
  return Math.max(start, data.end_ms ?? (data.status === "recording" ? Math.max(now, last) : last));
}

export class RunSystemMonitor {
  private active = false;
  private generation = 0;
  private selection_set = false;
  private state: RunSystemSnapshot = { source_id: "", run_ids: [], focused_run_id: "", items: {}, errors: {}, loading: false, metric_names: [], selected_metrics: [], x_axis: "run_elapsed", reduction: "last" };
  constructor(private readonly foreground: RequestLane, private readonly quiet: RequestLane, private readonly on_change: () => void) {}
  snapshot(): RunSystemSnapshot { return this.state; }
  private change(values: Partial<RunSystemSnapshot>): void { this.state = { ...this.state, ...values }; this.on_change(); }
  cancel(): void { this.generation++; this.foreground.cancel(); this.quiet.cancel(); if (this.state.loading) this.change({ loading: false }); }
  setContext(source_id: string, run_ids: string[], active: boolean): void {
    const changed = source_id !== this.state.source_id || JSON.stringify(run_ids) !== JSON.stringify(this.state.run_ids);
    if (changed) {
      this.cancel(); this.selection_set = false;
      this.change({ source_id, run_ids: [...run_ids], focused_run_id: run_ids[0] ?? "", items: {}, errors: {}, metric_names: [], selected_metrics: [] });
    }
    const entered = active && !this.active;
    this.active = active;
    if (!active) this.cancel();
    else if ((changed || entered) && run_ids.length) void this.refresh();
  }
  focusRun(run_id: string): void {
    if (!this.state.run_ids.includes(run_id) || run_id === this.state.focused_run_id) return;
    this.cancel(); this.change({ focused_run_id: run_id }); void this.refresh(false, [run_id]);
  }
  axis(x_axis: RunSystemAxis): void { if (x_axis !== this.state.x_axis) this.change({ x_axis }); }
  reduction(reduction: RunSystemReduction): void { if (reduction !== this.state.reduction) this.change({ reduction }); }
  metric(name: string, selected: boolean): void {
    if (selected ? !this.state.metric_names.includes(name) || this.state.selected_metrics.length >= 6 : !this.state.selected_metrics.includes(name)) return;
    this.selection_set = true;
    this.change({ selected_metrics: selected ? [...new Set([...this.state.selected_metrics, name])] : this.state.selected_metrics.filter(metric => metric !== name) });
  }
  private reconcileMetrics(): void {
    const names = new Set<string>();
    for (const item of Object.values(this.state.items)) for (const name of Object.keys(item.data.metric_summaries)) names.add(name);
    const order = (name: string) => name === "cpu/usage_percent" ? 0 : name === "memory/used_bytes" ? 1 : name.startsWith("gpu/") ? 2 : 3;
    const metric_names = [...names].sort((a, b) => order(a) - order(b) || a.localeCompare(b));
    this.change({ metric_names, selected_metrics: this.selection_set ? this.state.selected_metrics : metric_names.slice(0, 4) });
  }
  async refresh(quiet = false, requested_ids = this.state.run_ids): Promise<"success" | "failure" | "cancelled"> {
    if (!this.active || !requested_ids.length) return "cancelled";
    this.foreground.cancel(); this.quiet.cancel();
    const generation = ++this.generation, source = this.state.source_id, lane = quiet ? this.quiet : this.foreground;
    const current = () => this.active && generation === this.generation && source === this.state.source_id;
    if (!quiet) this.change({ loading: true });
    let outcome: "success" | "failure" = "success";
    for (const run_id of requested_ids) {
      if (!this.state.run_ids.includes(run_id)) continue;
      try {
        let previous = runSystemItem(this.state, run_id);
        const full = run_id === this.state.focused_run_id;
        let cursor = full && previous && !previous.preview_truncated ? previous.cursor : 0;
        let retained = full && previous ? [...previous.data.samples] : [];
        const inventories = new Map((previous?.data.inventories ?? []).map(record => [record.revision, record]));
        let pages = 0, restarts = 0, result: RunSystemData | undefined, preview_truncated = false;
        do {
          const summary_only = !full || previous?.preview_truncated === true;
          const limit = summary_only ? 1 : Math.min(1024, MAX_PREVIEW_SAMPLES - retained.length);
          const value = await lane.runJson<unknown>(apiUrl("/api/run-system", { source, run_id, cursor, limit: Math.max(1, limit) }), READ_LIMIT, READ_LIMIT_MESSAGE);
          if (value === undefined || !current()) return "cancelled";
          const page = parseRunSystem(value, run_id);
          if (previous && (page.capture_id !== previous.data.capture_id || page.start_ms !== previous.data.start_ms || page.machine_id !== previous.data.machine_id || page.sample_count < previous.cursor)) {
            previous = undefined; retained = []; inventories.clear();
            if (cursor > 0 || summary_only) {
              if (++restarts > 1) invalid();
              cursor = 0; result = undefined; continue;
            }
          }
          if (result && (page.capture_id !== result.capture_id || page.machine_id !== result.machine_id || page.start_ms !== result.start_ms || page.sample_count < result.sample_count)) invalid();
          if (page.next_cursor !== null && (page.next_cursor <= cursor || page.next_cursor > page.sample_count)) invalid();
          for (const record of page.inventories) {
            const known = inventories.get(record.revision);
            if (known && JSON.stringify(known.inventory) !== JSON.stringify(record.inventory)) invalid();
            inventories.set(record.revision, record);
          }
          if (inventories.size > 4096) invalid();
          result = page;
          const identities = new Set(retained.map(sample => `${sample.session_id}:${sample.sequence}`));
          for (const sample of page.samples) if (!identities.has(`${sample.session_id}:${sample.sequence}`)) {
            if (retained.length >= MAX_PREVIEW_SAMPLES) { preview_truncated = true; break; }
            identities.add(`${sample.session_id}:${sample.sequence}`); retained.push(sample);
          }
          cursor = page.next_cursor ?? page.sample_count;
          if (summary_only) { preview_truncated = full; break; }
          preview_truncated ||= page.next_cursor !== null && (retained.length >= MAX_PREVIEW_SAMPLES || ++pages >= MAX_PAGES);
          if (preview_truncated || page.next_cursor === null) break;
        } while (current());
        if (!result || !current()) return "cancelled";
        if (previous?.preview_truncated && full && previous.data.capture_id === result.capture_id) { retained = previous.data.samples; preview_truncated = true; }
        retained = retained.filter(sample => result!.start_ms !== null && sample.observed_at_ms >= result!.start_ms && (result!.end_ms === null || sample.observed_at_ms < result!.end_ms));
        const data = { ...result, samples: retained, inventories: [...inventories.values()].sort((a, b) => a.revision - b.revision) };
        const errors = { ...this.state.errors }; delete errors[run_id];
        this.change({ items: { ...this.state.items, [run_id]: { data, cursor, preview_truncated, series: systemHistorySeries({ resolution: "detail", samples: data.samples, hours: [] }) } }, errors });
      } catch (error) {
        if (!current()) return "cancelled";
        outcome = "failure";
        this.change({ errors: { ...this.state.errors, [run_id]: error instanceof RequestError && error.status === 404 ? "Retained system readings are unavailable on this backend or for this run." : errorText(error) } });
      }
    }
    if (!current()) return "cancelled";
    this.reconcileMetrics();
    if (this.state.loading) this.change({ loading: false });
    return outcome;
  }
}
