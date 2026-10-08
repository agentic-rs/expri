import type { ChartController } from "./interactive_charts";
import type { RefreshClock } from "./auto_refresh";

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type Warning = { run_id?: string; message: string };
export type Source = {
  source_id: string;
  label: string;
  kind: string;
  target_name: string | null;
  project_id?: string;
  origin?: string;
};
export type RunDeepLink = { project_id: string; origin: string; run_id: string };

/** Public service identifiers select existing catalog entries; they are never URLs. */
export function parseRunDeepLink(search: string): RunDeepLink | null {
  if (search.length > 512) return null;
  const query = new URLSearchParams(search);
  const fields = ["project_id", "origin", "run_id"] as const;
  const values = fields.map((field) => query.getAll(field));
  if (values.some((value) => value.length !== 1)) return null;
  const valid = (value: string | undefined): value is string =>
    value !== undefined && value !== "." && value !== ".." && /^[A-Za-z0-9._-]{1,96}$/.test(value);
  const project_id = values[0]?.[0],
    origin = values[1]?.[0],
    run_id = values[2]?.[0];
  return valid(project_id) && valid(origin) && valid(run_id)
    ? { project_id, origin, run_id }
    : null;
}
export type Run = {
  run_id: string;
  task: string | null;
  status: string;
  started_at: string | null;
  finished_at: string | null;
  exit_code: number | null;
};
export type Point = { step: number; value: number; timestamp?: string };
export type Metric = { count: number; last: Point; min: Point; max: Point };
export type Catalog = {
  project_name: string;
  initial_source: string;
  sources: Source[];
  warnings: Warning[];
  access_mode?: "local" | "hosted";
};
export type RunList = {
  source: Source;
  runs: Run[];
  warnings: Warning[];
  total_count: number;
  offset: number;
  next_offset: number | null;
};
export type Detail = {
  source: Source;
  run: Run;
  state: Json;
  snapshot: Json;
  environment: Json;
  params: Json;
  params_truncated: boolean;
  metadata_truncated: boolean;
  metrics: Record<string, Metric>;
  metric_count: number;
  metrics_truncated: boolean;
  metrics_error: string | null;
  warnings: Warning[];
  cache: Json;
};
export type Log = { content: string; stream: string; missing: boolean; truncated: boolean };
export type Comparison = {
  source: Source;
  comparison: {
    reduction: string;
    metric_names: string[];
    runs: { run_id: string; run: Run; values: Record<string, Point | null> }[];
    warnings: Warning[];
  };
};
export type ArtifactScope = { project_id: string; origin: string; run_id: string };
export type ArtifactFile = {
  path: string;
  size: number;
  local: boolean | null;
  cloud: boolean | null;
  worker: boolean | null;
  download_url: string | null;
};
export type ArtifactCatalog = {
  source: Source;
  run_id: string;
  files: ArtifactFile[];
  truncated: boolean;
  warnings: Warning[];
  pull_scope: ArtifactScope | null;
  inventory_recorded_at?: string | null;
};
export type ReviewTab = "charts" | "overview" | "logs" | "files";
export type Review = {
  kind: "run" | "compare";
  origin: "inspection" | "selection";
  run_ids: string[];
  metric_names: string[];
  metric_selection_set: boolean;
  tab: ReviewTab;
  log_stream: "stdout" | "stderr";
};
export type LogView = { run_id: string; stream: string; loaded: boolean; pending: boolean };
export type RunRevision = {
  run_id: string;
  metadata_revision: string | null;
  metrics_revision: string | null;
  stdout_revision: string | null;
  stderr_revision: string | null;
  missing: boolean;
};
export type Updates = {
  catalog_revision: string | null;
  source_revision: string | null;
  runs: RunRevision[];
};
export type ChartRefreshOutcome = "applied" | "deferred" | "cancelled";
export type ComparisonReduction = "last" | "min" | "max";
export type DashboardOptions = { refresh_clock?: RefreshClock; chart_controller?: ChartController };

export function apiUrl(
  path: string,
  fields: Record<string, string | number | string[] | null>,
): string {
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
  return Number.isFinite(value)
    ? new Intl.NumberFormat(undefined, { maximumSignificantDigits: 6 }).format(value)
    : "—";
}

export function formatDuration(run: Run): string {
  if (!run.started_at || !run.finished_at) return "—";
  const seconds = Math.max(
    0,
    Math.round((Date.parse(run.finished_at) - Date.parse(run.started_at)) / 1000),
  );
  if (!Number.isFinite(seconds)) return "—";
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
}

export class RequestError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

async function boundedText(response: Response, maximum_bytes: number): Promise<string> {
  const declared = response.headers?.get("content-length");
  if (declared && Number(declared) > maximum_bytes)
    throw new Error("The chart exceeds the 2 MiB preview limit. Select fewer metrics.");
  if (!response.body?.getReader) {
    const value = await response.text();
    if (new TextEncoder().encode(value).length > maximum_bytes)
      throw new Error("The chart exceeds the 2 MiB preview limit. Select fewer metrics.");
    return value;
  }
  const reader = response.body.getReader(),
    decoder = new TextDecoder();
  let length = 0,
    value = "";
  try {
    for (;;) {
      const part = await reader.read();
      if (part.done) break;
      length += part.value.byteLength;
      if (length > maximum_bytes) {
        await reader.cancel();
        throw new Error("The chart exceeds the 2 MiB preview limit. Select fewer metrics.");
      }
      value += decoder.decode(part.value, { stream: true });
    }
    return value + decoder.decode();
  } finally {
    reader.releaseLock();
  }
}

export class RequestLane {
  constructor(private readonly login_run: () => RunDeepLink | null = () => null) {}
  private controller: AbortController | null = null;
  private generation = 0;
  get pending(): boolean {
    return this.controller !== null;
  }
  cancel(): void {
    this.controller?.abort();
    this.controller = null;
    this.generation++;
  }
  run<T>(url: string): Promise<T | undefined> {
    return this.request(url, (response) => response.json() as Promise<T>);
  }
  runText(url: string, maximum_bytes = 2 * 1024 * 1024): Promise<string | undefined> {
    return this.request(url, (response) => boundedText(response, maximum_bytes));
  }
  private async request<T>(
    url: string,
    parse: (response: Response) => Promise<T>,
  ): Promise<T | undefined> {
    this.cancel();
    const generation = this.generation;
    const controller = new AbortController();
    this.controller = controller;
    const timeout = setTimeout(() => controller.abort(), 30_000);
    try {
      const response = await fetch(url, { signal: controller.signal, cache: "no-store" });
      if (generation !== this.generation) return undefined;
      if (response.status === 401) {
        if (typeof location !== "undefined") {
          const link = this.login_run();
          location.assign(link ? apiUrl("/login", link) : "/login");
        }
        return undefined;
      }
      if (!response.ok) {
        let message = `Request failed (${response.status})`;
        try {
          const error = (await response.json()) as { error?: string };
          message = error.error ?? message;
        } catch {
          /* Error pages need not be JSON. */
        }
        if (generation !== this.generation) return undefined;
        throw new RequestError(message, response.status);
      }
      const value = await parse(response);
      return generation === this.generation ? value : undefined;
    } catch (error) {
      if (generation !== this.generation) return undefined;
      if (controller.signal.aborted)
        throw new Error("The request timed out. Try Refresh when the connection is available.");
      throw error;
    } finally {
      clearTimeout(timeout);
      if (this.controller === controller) this.controller = null;
    }
  }
}

export function dateText(value: string | null): string {
  if (!value) return "—";
  const date = new Date(value);
  return Number.isNaN(date.valueOf()) ? value : date.toLocaleString();
}
export function localTimeZoneLabel(): string {
  return new Intl.DateTimeFormat().resolvedOptions().timeZone;
}
export function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
export function jsonObject(value: Json): Record<string, Json> {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? value : {};
}
