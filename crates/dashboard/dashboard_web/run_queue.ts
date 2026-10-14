/** Scheduling metadata is distinct from a worker's published run record. */
export type RunQueueStatus = "pending" | "starting" | "running" | "cancel_requested" |
  "completed" | "failed" | "cancelled" | "unknown" | "lost";
export type RunQueueResources = {
  cpus: number;
  memory_bytes: number;
  gpus: number;
  gpu_ids: string[];
  gpu_model: string | null;
  gpu_memory_bytes: number | null;
};
export type RunQueue = {
  job_id: string;
  project_id: string;
  task: string;
  status: RunQueueStatus;
  resources: RunQueueResources;
  pending_reason: string | null;
  submitted_at_ms: number;
  updated_at_ms: number;
  worker_id: string | null;
  run_id: string | null;
};
export type RunRedirect = { source_id: string; run_id: string };
export type QueueRun = {
  run_id: string;
  queue?: unknown;
  queue_only?: boolean;
  queue_message?: unknown;
};

const QUEUE_STATUSES: readonly RunQueueStatus[] = [
  "pending", "starting", "running", "cancel_requested", "completed", "failed", "cancelled", "unknown", "lost",
];
const MAX_DATE_MS = 8_640_000_000_000_000;

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
}
function identifier(value: unknown): value is string {
  return typeof value === "string" && value !== "." && value !== ".." &&
    /^[A-Za-z0-9._-]{1,96}$/.test(value);
}
function boundedText(value: unknown): value is string {
  return typeof value === "string" && value.length > 0 && value.length <= 256 &&
    !/\p{Cc}/u.test(value) && new TextEncoder().encode(value).length <= 256;
}
function boundedInteger(value: unknown, min: number, max = Number.MAX_SAFE_INTEGER): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= min && value <= max;
}
function optionalIdentifier(value: unknown): value is string | null {
  return value === null || identifier(value);
}
function optionalText(value: unknown): value is string | null {
  return value === null || boundedText(value);
}
function parseResources(value: unknown): RunQueueResources | null {
  const resources = record(value);
  if (!resources || !boundedInteger(resources.cpus, 1, 1_000_000) ||
    !boundedInteger(resources.memory_bytes, 1) || !boundedInteger(resources.gpus, 0, 64) ||
    !Array.isArray(resources.gpu_ids) || resources.gpu_ids.length > 64 ||
    !resources.gpu_ids.every(boundedText) ||
    new Set(resources.gpu_ids).size !== resources.gpu_ids.length ||
    (resources.gpu_ids.length !== 0 && resources.gpu_ids.length !== resources.gpus) ||
    !optionalText(resources.gpu_model) ||
    !(resources.gpu_memory_bytes === null || boundedInteger(resources.gpu_memory_bytes, 1)) ||
    (resources.gpus === 0 && (resources.gpu_model !== null || resources.gpu_memory_bytes !== null))) return null;
  return {
    cpus: resources.cpus,
    memory_bytes: resources.memory_bytes,
    gpus: resources.gpus,
    gpu_ids: [...resources.gpu_ids],
    gpu_model: resources.gpu_model,
    gpu_memory_bytes: resources.gpu_memory_bytes,
  };
}

/** Accept only bounded scheduling fields; do not retain commands, tokens, or worker leases. */
export function parseRunQueue(value: unknown): RunQueue | null {
  const queue = record(value);
  if (!queue || !identifier(queue.job_id) || !identifier(queue.project_id) ||
    !boundedText(queue.task) || typeof queue.status !== "string" ||
    !QUEUE_STATUSES.includes((queue.status === "queued" ? "pending" : queue.status) as RunQueueStatus) ||
    !optionalText(queue.pending_reason) ||
    !boundedInteger(queue.submitted_at_ms, 0, MAX_DATE_MS) ||
    !boundedInteger(queue.updated_at_ms, queue.submitted_at_ms, MAX_DATE_MS) ||
    !optionalIdentifier(queue.worker_id) || !optionalIdentifier(queue.run_id)) return null;
  const resources = parseResources(queue.resources);
  if (!resources) return null;
  return {
    job_id: queue.job_id,
    project_id: queue.project_id,
    task: queue.task,
    status: (queue.status === "queued" ? "pending" : queue.status) as RunQueueStatus,
    resources,
    pending_reason: queue.pending_reason,
    submitted_at_ms: queue.submitted_at_ms,
    updated_at_ms: queue.updated_at_ms,
    worker_id: queue.worker_id,
    run_id: queue.run_id,
  };
}

/** Redirects select a hosted catalog entry. They must never become a URL or path. */
export function parseRunRedirect(value: unknown): RunRedirect | null {
  const redirect = record(value);
  if (!redirect || typeof redirect.source_id !== "string" || redirect.source_id.length > 200 ||
    typeof redirect.run_id !== "string" || redirect.run_id.length > 193) return null;
  const source = redirect.source_id.split(":");
  if (source[0] === "hosted-project" && source.length === 2 && identifier(source[1])) {
    const run = redirect.run_id.split(":");
    if (run.length === 2 && run.every(identifier))
      return { source_id: redirect.source_id, run_id: redirect.run_id };
  }
  if (source[0] === "hosted" && source.length === 3 && source.slice(1).every(identifier) &&
    identifier(redirect.run_id)) return { source_id: redirect.source_id, run_id: redirect.run_id };
  return null;
}

export function queueContext(run: QueueRun): RunQueue | null {
  return parseRunQueue(run.queue);
}

/** Acknowledging a native ID does not publish that native run's metadata. */
export function queuePlaceholder(run: QueueRun): boolean {
  if (typeof run.queue_only === "boolean") return run.queue_only;
  const queue = queueContext(run);
  return queue !== null && run.run_id === queue.job_id;
}

export function queueMessage(run: QueueRun): string | null {
  return boundedText(run.queue_message) ? run.queue_message : null;
}

export function queuePhaseLabel(status: RunQueueStatus): string {
  switch (status) {
    case "pending": return "Pending · Resource scheduling";
    case "starting": return "Starting";
    case "running": return "Running";
    case "cancel_requested": return "Cancellation requested";
    case "completed": return "Completed";
    case "failed": return "Failed";
    case "cancelled": return "Cancelled";
    case "unknown": return "Unknown";
    case "lost": return "Lost";
  }
}

function memoryLabel(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB", "TiB", "PiB"];
  let value = bytes / 1024;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index++;
  }
  return `${new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 }).format(value)} ${units[index]}`;
}

export function queueResourceLabels(resources: RunQueueResources): string[] {
  const labels = [
    `${resources.cpus} CPU ${resources.cpus === 1 ? "slot" : "slots"}`,
    `${memoryLabel(resources.memory_bytes)} memory`,
    `${resources.gpus} ${resources.gpus === 1 ? "GPU" : "GPUs"}`,
  ];
  if (resources.gpu_ids.length) labels.push(`GPU IDs: ${resources.gpu_ids.join(", ")}`);
  if (resources.gpu_model !== null) labels.push(`GPU model: ${resources.gpu_model}`);
  if (resources.gpu_memory_bytes !== null)
    labels.push(`Minimum GPU memory: ${memoryLabel(resources.gpu_memory_bytes)} each`);
  return labels;
}
