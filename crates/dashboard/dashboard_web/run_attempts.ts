export type TaskAttempt = {
  attempt: number;
  request_id: string | null;
  reason: string | null;
  status: "pending" | "preparing" | "running" | "waiting" | "paused" | "completed" | "failed" | "cancelled" | "lost" | "continued";
  error: string | null;
  requested_at: string | null;
  started_at: string | null;
  finished_at: string | null;
  previous_worker_stopped_at: string | null;
  exit_code: number | null;
  worker_restarted: boolean | null;
  source: { kind?: string; git_head?: string; checkout_manifest_sha256?: string; sync_state_sha256?: string; files_sha256?: string };
  checkpoint: { name: string; relative_path: string | null; sha256: string | null; size: string | null } | null;
  metric_step_offset: string;
  first_local_step: string | null;
};
export type TaskAttempts = {
  protocol_version: 1;
  current_attempt: number;
  attempts: TaskAttempt[];
  total_count: number;
  truncated: boolean;
};

const STATUSES = ["pending", "preparing", "running", "waiting", "paused", "completed", "failed", "cancelled", "lost", "continued"];
function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : null;
}
function integer(value: unknown, minimum: number): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= minimum;
}
function text(value: unknown, limit: number): value is string {
  return typeof value === "string" && value.length > 0 && value.length <= limit * 2 && Array.from(value).length <= limit && !/\p{Cc}/u.test(value);
}
function optionalText(value: unknown, limit: number): value is string | null {
  return value === null || text(value, limit);
}
function timestamp(value: unknown): value is string | null {
  return value === null || typeof value === "string" && value.length <= 64 &&
    /^\d{4}-\d{2}-\d{2}[Tt]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:[Zz]|[+-]\d{2}:\d{2})$/.test(value) && Number.isFinite(Date.parse(value));
}
function decimal(value: unknown): value is string {
  return typeof value === "string" && /^(?:0|[1-9][0-9]{0,19})$/.test(value) && BigInt(value) <= 18_446_744_073_709_551_615n;
}
function digest(value: unknown, git_head: boolean): value is string {
  return typeof value === "string" && (git_head ? /^(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})$/ : /^[0-9a-fA-F]{64}$/).test(value);
}
function checkpoint(value: unknown): TaskAttempt["checkpoint"] {
  const item = record(value);
  if (!item || !text(item.name, 128) || /[/\\]/.test(item.name) ||
    !(item.relative_path === null || text(item.relative_path, 1024) && item.relative_path.startsWith("outputs/") &&
      !item.relative_path.includes("\\") && item.relative_path.split("/").every(part => part !== "" && part !== "." && part !== "..")) ||
    !(item.sha256 === null || typeof item.sha256 === "string" && /^[0-9a-fA-F]{64}$/.test(item.sha256)) ||
    !(item.size === null || decimal(item.size))) return null;
  return { name: item.name, relative_path: item.relative_path, sha256: item.sha256, size: item.size };
}
function parseAttempt(value: unknown): TaskAttempt | null {
  const item = record(value);
  if (!item || !integer(item.attempt, 1) || !optionalText(item.request_id, 96) || !optionalText(item.reason, 512) ||
    typeof item.status !== "string" || !STATUSES.includes(item.status) ||
    !timestamp(item.requested_at) || !timestamp(item.started_at) || !timestamp(item.finished_at) || !timestamp(item.previous_worker_stopped_at) ||
    !(item.exit_code === null || typeof item.exit_code === "number" && Number.isInteger(item.exit_code) && item.exit_code >= -2_147_483_648 && item.exit_code <= 2_147_483_647) ||
    !decimal(item.metric_step_offset) || !(item.first_local_step === null || decimal(item.first_local_step))) return null;
  const source_value = record(item.source), source: TaskAttempt["source"] = {};
  if (source_value) {
    if (source_value.kind === "git" || source_value.kind === "synced_checkout") source.kind = source_value.kind;
    for (const field of ["git_head", "checkout_manifest_sha256", "sync_state_sha256", "files_sha256"] as const) {
      if (digest(source_value[field], field === "git_head")) source[field] = source_value[field];
    }
  }
  return {
    attempt: item.attempt, request_id: item.request_id, reason: item.reason,
    status: item.status as TaskAttempt["status"], error: item.status === "failed" && text(item.error, 512) ? item.error : null,
    requested_at: item.requested_at, started_at: item.started_at,
    finished_at: item.finished_at, previous_worker_stopped_at: item.previous_worker_stopped_at,
    exit_code: item.exit_code, worker_restarted: typeof item.worker_restarted === "boolean" ? item.worker_restarted : null,
    source, checkpoint: checkpoint(item.checkpoint), metric_step_offset: item.metric_step_offset, first_local_step: item.first_local_step,
  };
}
export function parseTaskAttempts(value: unknown): TaskAttempts | null {
  const history = record(value);
  if (!history || history.protocol_version !== 1 || !integer(history.current_attempt, 1) || !Array.isArray(history.attempts) ||
    !integer(history.total_count, history.attempts.length) || typeof history.truncated !== "boolean") return null;
  const attempts = history.attempts.slice(-16).map(parseAttempt).filter(value => value !== null);
  return { protocol_version: 1, current_attempt: history.current_attempt, attempts, total_count: history.total_count, truncated: history.truncated || attempts.length < history.total_count };
}
export function attemptWorkerLabel(attempt: TaskAttempt): string {
  if (attempt.worker_restarted === true) return "Worker restarted";
  if (attempt.worker_restarted === false) return attempt.attempt === 1 ? "Original task" : attempt.previous_worker_stopped_at === null ? "Worker not restarted" : "Previous worker stopped; replacement not started";
  return "Worker restart not recorded";
}
export function attemptRestartGap(attempt: TaskAttempt): string | null {
  if (attempt.worker_restarted !== true || attempt.started_at === null || attempt.previous_worker_stopped_at === null) return null;
  const difference = Date.parse(attempt.started_at) - Date.parse(attempt.previous_worker_stopped_at);
  return difference >= 0 ? `${new Intl.NumberFormat(undefined, { maximumFractionDigits: 3 }).format(difference / 1000)} s` : null;
}
