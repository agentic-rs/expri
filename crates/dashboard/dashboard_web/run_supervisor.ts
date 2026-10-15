export type SupervisorEvent = {
  generation: number;
  request_id: string;
  reason: string;
  mode: "live";
  status: "restarting" | "completed" | "failed";
  requested_at: string;
  completed_at: string | null;
  failed_at: string | null;
  worker_restarted: boolean | null;
  error: string | null;
};
export type RunSupervisor = {
  protocol_version: 1;
  generation: number;
  mode: "live";
  events: SupervisorEvent[];
  total_count: number;
  truncated: boolean;
};

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
}
function integer(value: unknown, minimum: number): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= minimum;
}
function text(value: unknown, limit: number): value is string {
  return typeof value === "string" && value.length > 0 && value.length <= limit * 2 &&
    Array.from(value).length <= limit && !/\p{Cc}/u.test(value);
}
function timestamp(value: unknown): value is string {
  return typeof value === "string" && value.length <= 64 &&
    /^\d{4}-\d{2}-\d{2}[Tt]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:[Zz]|[+-]\d{2}:\d{2})$/.test(value) &&
    Number.isFinite(Date.parse(value));
}
function optionalTimestamp(value: unknown): value is string | null {
  return value === null || timestamp(value);
}
function parseEvent(value: unknown): SupervisorEvent | null {
  const event = record(value);
  if (!event || !integer(event.generation, 1) || !text(event.request_id, 96) ||
    !text(event.reason, 512) || event.mode !== "live" ||
    (event.status !== "restarting" && event.status !== "completed" && event.status !== "failed") ||
    !timestamp(event.requested_at) || !optionalTimestamp(event.completed_at) ||
    !optionalTimestamp(event.failed_at) || !(event.error === null || event.error === "" || text(event.error, 512))) return null;
  return {
    generation: event.generation,
    request_id: event.request_id,
    reason: event.reason,
    mode: "live",
    status: event.status,
    requested_at: event.requested_at,
    completed_at: event.completed_at,
    failed_at: event.failed_at,
    worker_restarted: typeof event.worker_restarted === "boolean" ? event.worker_restarted : null,
    error: event.error,
  };
}

/** Read only the bounded public history; never retain process or handoff details. */
export function parseRunSupervisor(value: unknown): RunSupervisor | null {
  const supervisor = record(value);
  if (!supervisor || supervisor.protocol_version !== 1 || !integer(supervisor.generation, 1) ||
    supervisor.mode !== "live" || !Array.isArray(supervisor.events) ||
    !integer(supervisor.total_count, supervisor.events.length) || typeof supervisor.truncated !== "boolean") return null;
  const events = supervisor.events.slice(-16).map(parseEvent).filter(event => event !== null);
  return {
    protocol_version: 1,
    generation: supervisor.generation,
    mode: "live",
    events,
    total_count: supervisor.total_count,
    truncated: supervisor.truncated || events.length < supervisor.total_count,
  };
}

export function supervisorWorkerLabel(event: SupervisorEvent): string {
  if (event.worker_restarted === true) return "Worker restarted";
  if (event.worker_restarted === false) return "Worker not restarted";
  return "Worker restart not recorded";
}
