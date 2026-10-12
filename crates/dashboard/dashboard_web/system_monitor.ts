import { apiUrl, errorText, RequestError, RequestLane } from "./dashboard_model";

export type SystemResolution = "detail" | "hourly";
export type SystemPeriod = "1h" | "6h" | "24h" | "72h" | "7d" | "30d";
export type SystemMetricKind = "gauge" | "counter_delta";
export type SystemDevice = { device_id: string; kind: "gpu" | "disk" | "network"; name: string; total_bytes: number | null; attributes: Record<string, string> };
export type SystemInventory = {
  observed_at_ms: number; hostname: string; os_name: string | null; os_version: string | null;
  kernel_version: string | null; architecture: string;
  cpu: { brand: string; vendor: string; logical_cpu_count: number; physical_core_count: number | null };
  memory_total_bytes: number; measurement_scope: string; devices: SystemDevice[]; warnings: string[];
};
export type SystemMachine = { machine_id: string; inventory_revision: number; inventory: SystemInventory; last_sample_at_ms: number | null; last_received_at_ms: number | null };
export type SystemSample = {
  sequence: number; session_id: string; observed_at_ms: number; interval_ms: number;
  collection_duration_ms: number; inventory_revision: number;
  metrics: Record<string, { kind: SystemMetricKind; value: number }>; counters: Record<string, string>; warnings: string[];
};
export type SystemAggregate = { kind: SystemMetricKind; sample_count: number; coverage_ms: number; sum: number; min: number; max: number; last: number; last_at_ms: number };
export type SystemHour = { hour_start_ms: number; revision: number; sample_count: number; first_sample_at_ms: number; last_sample_at_ms: number; inventory_revisions: number[]; metrics: Record<string, SystemAggregate>; warnings: string[] };
export type SystemHistory = { machine_id: string; resolution: SystemResolution; samples: SystemSample[]; hours: SystemHour[]; next_cursor: number | null };
export type SystemPoint = { observed_at_ms: number; value: number; coverage_ms: number; break_before: boolean; sample_count: number };
export type SystemSeries = { metric_name: string; kind: SystemMetricKind; points: SystemPoint[] };
export type SystemMonitorSnapshot = {
  machines: SystemMachine[]; machine_cursor: string | null; machine_id: string; profile: SystemMachine | null;
  period: SystemPeriod; resolution: SystemResolution; start_ms: number; end_ms: number;
  series: SystemSeries[]; metric_names: string[]; selected_metrics: string[];
  loading: boolean; error: string | null; unsupported: boolean; truncated: boolean; warnings: string[]; loaded_at_ms: number | null;
};

const HOUR_MS = 3_600_000;
const MAX_HISTORY_ROWS = 4096;
const MAX_MACHINES = 400;
const MAX_SELECTED_METRICS = 6;
const READ_LIMIT_MESSAGE = "The system response exceeds the 512 KiB read limit. Use the CLI to inspect oversized records.";
const PERIOD_MS: Record<SystemPeriod, number> = { "1h": HOUR_MS, "6h": 6 * HOUR_MS, "24h": 24 * HOUR_MS, "72h": 72 * HOUR_MS, "7d": 7 * 24 * HOUR_MS, "30d": 30 * 24 * HOUR_MS };
const METRIC_NAME = /^[A-Za-z0-9/_\-.:]{1,256}$/;
const COMPONENT = /^[A-Za-z0-9._-]{1,96}$/;
function invalid(): never { throw new Error("The system monitor response is invalid or exceeds its limits."); }
function object(value: unknown): Record<string, unknown> { if (!value || typeof value !== "object" || Array.isArray(value)) invalid(); return value as Record<string, unknown>; }
function text(value: unknown, maximum = 1024): string { if (typeof value !== "string" || value.length > maximum || /[\u0000-\u001f\u007f]/.test(value)) invalid(); return value; }
function nullableText(value: unknown): string | null { return value === null ? null : text(value); }
function integer(value: unknown): number { if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) invalid(); return value; }
function nullableInteger(value: unknown): number | null { return value === null ? null : integer(value); }
function finite(value: unknown): number { if (typeof value !== "number" || !Number.isFinite(value)) invalid(); return value; }
function list(value: unknown, maximum: number): unknown[] { if (!Array.isArray(value) || value.length > maximum) invalid(); return value; }
function warnings(value: unknown): string[] { return list(value ?? [], 32).map(value => text(value, 512)); }
function component(value: unknown): string { const result = text(value, 96); if (!COMPONENT.test(result) || result === "." || result === "..") invalid(); return result; }
function kind(value: unknown): SystemMetricKind { if (value !== "gauge" && value !== "counter_delta") invalid(); return value; }
function records<T>(value: unknown, parse: (value: unknown) => T): Record<string, T> {
  const data = object(value), entries = Object.entries(data);
  if (entries.length > 2048) invalid();
  return Object.fromEntries(entries.map(([name, value]) => { if (!METRIC_NAME.test(name)) invalid(); return [name, parse(value)]; }));
}
export function parseSystemMachine(value: unknown): SystemMachine {
  const data = object(value), inventory = object(data.inventory), cpu = object(inventory.cpu);
  return {
    machine_id: component(data.machine_id), inventory_revision: integer(data.inventory_revision),
    last_sample_at_ms: nullableInteger(data.last_sample_at_ms), last_received_at_ms: nullableInteger(data.last_received_at_ms),
    inventory: {
      observed_at_ms: integer(inventory.observed_at_ms), hostname: text(inventory.hostname),
      os_name: nullableText(inventory.os_name), os_version: nullableText(inventory.os_version), kernel_version: nullableText(inventory.kernel_version),
      architecture: text(inventory.architecture), memory_total_bytes: integer(inventory.memory_total_bytes),
      measurement_scope: text(inventory.measurement_scope), warnings: warnings(inventory.warnings),
      cpu: { brand: text(cpu.brand), vendor: text(cpu.vendor), logical_cpu_count: integer(cpu.logical_cpu_count), physical_core_count: nullableInteger(cpu.physical_core_count) },
      devices: list(inventory.devices, 256).map(value => {
        const device = object(value), attributes = object(device.attributes);
        if (typeof device.kind !== "string" || !["gpu", "disk", "network"].includes(device.kind) || Object.keys(attributes).length > 32) invalid();
        return { device_id: component(device.device_id), kind: device.kind as SystemDevice["kind"], name: text(device.name), total_bytes: nullableInteger(device.total_bytes), attributes: Object.fromEntries(Object.entries(attributes).map(([key, value]) => [text(key, 128), text(value)])) };
      }),
    },
  };
}
export function parseSystemMachines(value: unknown): { machines: SystemMachine[]; next_cursor: string | null } {
  const data = object(value), machines = list(data.machines, 100).map(parseSystemMachine);
  if (new Set(machines.map(machine => machine.machine_id)).size !== machines.length) invalid();
  return { machines, next_cursor: data.next_cursor === null ? null : component(data.next_cursor) };
}
export function parseSystemHistory(value: unknown, machine_id: string, resolution: SystemResolution): SystemHistory {
  const data = object(value);
  if (data.machine_id !== machine_id || data.resolution !== resolution) invalid();
  const samples = list(data.samples, 1024).map(value => {
    const row = object(value);
    if (integer(row.sequence) === 0 || integer(row.inventory_revision) === 0 || integer(row.interval_ms) > 120_000) invalid();
    return {
      sequence: integer(row.sequence), session_id: component(row.session_id), observed_at_ms: integer(row.observed_at_ms), interval_ms: integer(row.interval_ms),
      collection_duration_ms: integer(row.collection_duration_ms), inventory_revision: integer(row.inventory_revision), warnings: warnings(row.warnings),
      metrics: records(row.metrics, value => { const metric = object(value), metric_kind = kind(metric.kind), metric_value = finite(metric.value); if (metric_kind === "counter_delta" && metric_value < 0) invalid(); return { kind: metric_kind, value: metric_value }; }),
      counters: records(row.counters, value => { const result = text(value, 20); if (!/^\d{1,20}$/.test(result) || BigInt(result) > (1n << 64n) - 1n) invalid(); return result; }),
    };
  });
  const hours = list(data.hours, 1024).map(value => {
    const row = object(value);
    if (integer(row.hour_start_ms) % HOUR_MS || integer(row.revision) === 0 || integer(row.sample_count) === 0 || integer(row.first_sample_at_ms) > integer(row.last_sample_at_ms)) invalid();
    return {
      hour_start_ms: integer(row.hour_start_ms), revision: integer(row.revision), sample_count: integer(row.sample_count), first_sample_at_ms: integer(row.first_sample_at_ms), last_sample_at_ms: integer(row.last_sample_at_ms),
      inventory_revisions: list(row.inventory_revisions, 256).map(integer), warnings: warnings(row.warnings),
      metrics: records(row.metrics, value => {
        const metric = object(value);
        return { kind: kind(metric.kind), sample_count: integer(metric.sample_count), coverage_ms: integer(metric.coverage_ms), sum: finite(metric.sum), min: finite(metric.min), max: finite(metric.max), last: finite(metric.last), last_at_ms: integer(metric.last_at_ms) };
      }),
    };
  });
  if ((resolution === "detail" && hours.length) || (resolution === "hourly" && samples.length)) invalid();
  return { machine_id, resolution, samples, hours, next_cursor: nullableInteger(data.next_cursor) };
}

export function systemHistorySeries(history: Pick<SystemHistory, "samples" | "hours" | "resolution">): SystemSeries[] {
  const series = new Map<string, SystemSeries>();
  const add = (name: string, metric_kind: SystemMetricKind, point: SystemPoint) => {
    if (!Number.isFinite(point.value)) return;
    const current = series.get(name);
    if (current && current.kind !== metric_kind) return;
    if (current) current.points.push(point);
    else if (series.size < 2048) series.set(name, { metric_name: name, kind: metric_kind, points: [point] });
  };
  if (history.resolution === "detail") {
    let previous: SystemSample | undefined;
    for (const sample of history.samples) {
      for (const [name, reading] of Object.entries(sample.metrics)) {
        if (reading.kind === "counter_delta" && sample.interval_ms === 0) continue;
        const last = series.get(name)?.points.at(-1);
        const gap = !previous || previous.session_id !== sample.session_id ||
          sample.observed_at_ms <= previous.observed_at_ms ||
          sample.observed_at_ms - previous.observed_at_ms > Math.max(1000, sample.interval_ms * 1.5) ||
          last?.observed_at_ms !== previous.observed_at_ms;
        add(name, reading.kind, { observed_at_ms: sample.observed_at_ms, value: reading.kind === "counter_delta" ? reading.value * 1000 / sample.interval_ms : reading.value, coverage_ms: sample.interval_ms, break_before: gap, sample_count: 1 });
      }
      previous = sample;
    }
  } else {
    for (const hour of history.hours) for (const [name, aggregate] of Object.entries(hour.metrics)) {
      if (aggregate.coverage_ms === 0) continue;
      const last = series.get(name)?.points.at(-1);
      add(name, aggregate.kind, { observed_at_ms: hour.hour_start_ms, value: aggregate.sum * (aggregate.kind === "counter_delta" ? 1000 : 1) / aggregate.coverage_ms, coverage_ms: aggregate.coverage_ms, break_before: !last || hour.hour_start_ms - last.observed_at_ms !== HOUR_MS, sample_count: aggregate.sample_count });
    }
  }
  const priority = (name: string): number => name === "cpu/usage_percent" ? 0 : name === "memory/used_bytes" ? 1 : name.startsWith("gpu/") ? 2 : name.startsWith("cpu/") ? 3 : name.startsWith("memory/") ? 4 : 5;
  return [...series.values()].sort((a, b) => priority(a.metric_name) - priority(b.metric_name) || a.metric_name.localeCompare(b.metric_name));
}

export class SystemMonitor {
  private active = false;
  private generation = 0;
  private selection_set = false;
  private state: SystemMonitorSnapshot = { machines: [], machine_cursor: null, machine_id: "", profile: null, period: "1h", resolution: "detail", start_ms: 0, end_ms: 0, series: [], metric_names: [], selected_metrics: [], loading: false, error: null, unsupported: false, truncated: false, warnings: [], loaded_at_ms: null };
  constructor(private readonly foreground: RequestLane, private readonly quiet: RequestLane, private readonly now: () => number, private readonly on_change: () => void) {}
  snapshot(): SystemMonitorSnapshot { return this.state; }
  private change(values: Partial<SystemMonitorSnapshot>): void { this.state = { ...this.state, ...values }; this.on_change(); }
  setActive(active: boolean): void { this.active = active; if (!active) this.cancel(); else void this.refresh(); }
  cancel(): void { this.generation++; this.foreground.cancel(); this.quiet.cancel(); if (this.state.loading) this.change({ loading: false }); }
  chooseMachine(machine_id: string): void {
    if (machine_id === this.state.machine_id || !this.state.machines.some(machine => machine.machine_id === machine_id)) return;
    this.cancel(); this.selection_set = false;
    this.change({ machine_id, profile: null, series: [], metric_names: [], selected_metrics: [], error: null, warnings: [], truncated: false, start_ms: 0, end_ms: 0, loaded_at_ms: null });
    void this.refresh();
  }
  choosePeriod(period: SystemPeriod): void {
    if (!(period in PERIOD_MS) || period === this.state.period) return;
    this.cancel();
    this.change({ period, resolution: PERIOD_MS[period] > 6 * HOUR_MS ? "hourly" : "detail", series: [], warnings: [], truncated: false, start_ms: 0, end_ms: 0, loaded_at_ms: null });
    void this.refresh();
  }
  chooseResolution(resolution: SystemResolution): void {
    if (resolution === this.state.resolution) return;
    this.cancel(); this.change({ resolution, series: [], warnings: [], truncated: false, start_ms: 0, end_ms: 0, loaded_at_ms: null }); void this.refresh();
  }
  chooseMetric(metric_name: string, selected: boolean): void {
    if (selected ? !this.state.metric_names.includes(metric_name) : !this.state.selected_metrics.includes(metric_name)) return;
    const metrics = new Set(this.state.selected_metrics);
    if (selected && metrics.size >= MAX_SELECTED_METRICS) return;
    selected ? metrics.add(metric_name) : metrics.delete(metric_name);
    this.selection_set = true; this.change({ selected_metrics: [...metrics] });
  }
  async moreMachines(): Promise<void> {
    if (!this.active || this.state.loading || !this.state.machine_cursor || this.state.machines.length >= MAX_MACHINES) return;
    const generation = ++this.generation, cursor = this.state.machine_cursor;
    this.change({ loading: true, error: null });
    try {
      const value = await this.foreground.runJson<unknown>(apiUrl("/api/system-machines", { limit: 100, cursor }), 512 * 1024, READ_LIMIT_MESSAGE);
      if (value === undefined || !this.active || generation !== this.generation) return;
      const page = parseSystemMachines(value);
      if (page.next_cursor !== null && page.next_cursor <= cursor) invalid();
      const machines = new Map(this.state.machines.map(machine => [machine.machine_id, machine]));
      for (const machine of page.machines) machines.set(machine.machine_id, machine);
      this.change({ machines: [...machines.values()].slice(0, MAX_MACHINES), machine_cursor: page.next_cursor });
    } catch (error) { if (this.active && generation === this.generation) this.change({ error: errorText(error) }); }
    finally { if (this.active && generation === this.generation) this.change({ loading: false }); }
  }
  async refresh(quiet = false): Promise<"success" | "failure" | "cancelled"> {
    if (!this.active) return "cancelled";
    const generation = ++this.generation, lane = quiet ? this.quiet : this.foreground;
    const current = () => this.active && generation === this.generation;
    if (!quiet) this.change({ loading: true, error: null });
    try {
      const value = await lane.runJson<unknown>(apiUrl("/api/system-machines", { limit: 100 }), 512 * 1024, READ_LIMIT_MESSAGE);
      if (value === undefined || !current()) return "cancelled";
      const catalog = parseSystemMachines(value);
      if (!catalog.machines.length && !this.state.machine_id) {
        this.change({ machines: [], machine_cursor: catalog.next_cursor, unsupported: false, profile: null, series: [], metric_names: [], selected_metrics: [], error: null, loaded_at_ms: this.now() });
        return "success";
      }
      const machine_id = this.state.machine_id || catalog.machines[0]?.machine_id || "";
      const available = new Map(this.state.machines.map(machine => [machine.machine_id, machine]));
      for (const machine of catalog.machines) available.set(machine.machine_id, machine);
      // A failed history query must not hide the catalog or prevent changing machines.
      this.change({ machines: [...available.values()].slice(0, MAX_MACHINES), machine_cursor: catalog.next_cursor, machine_id, unsupported: false });
      const profile_value = await lane.runJson<unknown>(apiUrl("/api/system-profile", { machine_id }), 512 * 1024, READ_LIMIT_MESSAGE);
      if (profile_value === undefined || !current()) return "cancelled";
      const profile = parseSystemMachine(profile_value);
      if (profile.machine_id !== machine_id) invalid();
      this.change({ profile, unsupported: false, warnings: [...new Set([...this.state.warnings, ...profile.inventory.warnings])].slice(0, 64) });
      const end_ms = Math.max(1, Math.floor(this.now())), resolution = this.state.resolution;
      const requested_start = Math.max(0, end_ms - PERIOD_MS[this.state.period]);
      const start_ms = resolution === "hourly" ? Math.floor(requested_start / HOUR_MS) * HOUR_MS : requested_start;
      const history: SystemHistory = { machine_id, resolution, samples: [], hours: [], next_cursor: null };
      const notes = new Set(profile.inventory.warnings);
      const metric_catalog = new Set<string>();
      let cursor: number | null = null, count = 0, pages = 0, truncated = false;
      const seen = new Set<number>();
      do {
        const history_value = await lane.runJson<unknown>(apiUrl("/api/system-history", { machine_id, start_ms, end_ms, resolution, limit: Math.min(1024, MAX_HISTORY_ROWS - count), cursor }), 512 * 1024, READ_LIMIT_MESSAGE);
        if (history_value === undefined || !current()) return "cancelled";
        const page = parseSystemHistory(history_value, machine_id, resolution);
        const rows = resolution === "detail" ? page.samples : page.hours;
        let previous_key: number | null = cursor;
        for (const row of rows) {
          const key = "sequence" in row ? row.sequence : row.hour_start_ms;
          const time = "observed_at_ms" in row ? row.observed_at_ms : row.hour_start_ms;
          if (seen.has(key) || (previous_key !== null && key <= previous_key) || time < start_ms || time >= end_ms) invalid();
          previous_key = key;
          seen.add(key); for (const note of row.warnings) if (notes.size < 64) notes.add(note);
          for (const name of Object.keys(row.metrics)) {
            if (metric_catalog.size > 2048) break;
            metric_catalog.add(name);
          }
        }
        history.samples.push(...page.samples); history.hours.push(...page.hours); count += rows.length;
        if (page.next_cursor !== null && (!rows.length || page.next_cursor !== previous_key)) invalid();
        cursor = page.next_cursor;
        if (cursor !== null && (count >= MAX_HISTORY_ROWS || ++pages >= 16)) { truncated = true; break; }
      } while (cursor !== null);
      if (metric_catalog.size > 2048) notes.add("The metric catalog is limited to the first 2048 metric names observed in this period; additional transient-device metrics are omitted.");
      const series = systemHistorySeries(history), metric_names = series.map(series => series.metric_name);
      const selected_metrics = this.selection_set ? this.state.selected_metrics : metric_names.slice(0, 4);
      const machines = new Map(this.state.machines.map(machine => [machine.machine_id, machine]));
      for (const machine of catalog.machines) machines.set(machine.machine_id, machine);
      machines.set(machine_id, profile);
      this.change({ machines: [...machines.values()].slice(0, MAX_MACHINES), machine_cursor: catalog.next_cursor, machine_id, profile, start_ms, end_ms, series, metric_names, selected_metrics, unsupported: false, truncated, warnings: [...notes], error: null, loaded_at_ms: this.now() });
      return "success";
    } catch (error) {
      if (!current()) return "cancelled";
      const unsupported = error instanceof RequestError && error.status === 404 && !this.state.profile;
      this.change({ unsupported, error: unsupported ? "System monitoring is unavailable. Upgrade the backend, or start a local system collector before opening this page." : `Could not refresh system monitoring. ${errorText(error)}` });
      return "failure";
    } finally { if (current() && !quiet) this.change({ loading: false }); }
  }
}
