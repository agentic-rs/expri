import { apiUrl, parseRunDeepLink, runScope, type Run, type RunDeepLink, type Source } from "./dashboard_model";
import { parseResourceRequirements, parseResourceStatus, type ResourceRequirements, type ResourceStatus } from "./run_resources";

export const RUN_LIFECYCLE_ACTIONS = ["pause", "resume", "stop", "continue", "restart", "set_requirements"] as const;
export type RunLifecycleAction = typeof RUN_LIFECYCLE_ACTIONS[number];
export type RunLifecycleRequest = {
  request_id: string;
  action: RunLifecycleAction;
  status: "requested" | "delivered" | "executing" | "applied" | "failed" | "expired";
  error: string | null;
  new_run_id: string | null;
};
export type RunLifecycle = {
  supported: boolean;
  online: boolean;
  status: string;
  attempt: number;
  generation: number;
  binding_id: string;
  capabilities: RunLifecycleAction[];
  available_actions?: RunLifecycleAction[];
  command: string[];
  checkpoints: string[];
  last_seen_at_ms: number;
  request: RunLifecycleRequest | null;
  requirements?: ResourceRequirements | null;
  resource_status?: ResourceStatus | null;
  manual_paused?: boolean;
  experiment_assignment_supported?: boolean;
};

export function lifecycleScope(run: Run, source: Source | undefined): RunDeepLink | null {
  return runScope(run, source);
}

export function lifecyclePending(request: RunLifecycleRequest | null | undefined): boolean {
  return request != null && ["requested", "delivered", "executing"].includes(request.status);
}

export function lifecycleActions(value: RunLifecycle): RunLifecycleAction[] {
  if (value.available_actions !== undefined) return value.available_actions.filter(action => action !== "set_requirements");
  // Compatibility for older servers. Current responses carry the worker/server
  // decision, so presentation does not independently interpret execution state.
  const allowed: RunLifecycleAction[] = ["running", "waiting"].includes(value.status) ? ["pause", "stop", "continue", "restart"] :
    value.status === "paused" ? [...(value.manual_paused === false && value.resource_status?.blocked ? ["pause" as const] : []), "resume", "stop", "continue", "restart"] :
      ["pending", "preparing"].includes(value.status) ? ["stop"] :
      ["completed", "failed", "cancelled", "lost"].includes(value.status) ? ["continue", "restart"] : [];
  return allowed.filter(action => value.capabilities.includes(action));
}

export function parseRunLifecycle(value: unknown): RunLifecycle {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("Invalid run control response.");
  const item = value as Record<string, unknown>;
  const strings = (value: unknown): value is string[] => Array.isArray(value) && value.length <= 4096 &&
    value.every(part => typeof part === "string" && part.length <= 65536 && !part.includes("\0"));
  const integer = (value: unknown): value is number => typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
  if (typeof item.supported !== "boolean" || typeof item.online !== "boolean" || typeof item.status !== "string" ||
    !integer(item.attempt) || !integer(item.generation) || typeof item.binding_id !== "string" ||
    !strings(item.capabilities) || !item.capabilities.every(action => RUN_LIFECYCLE_ACTIONS.includes(action as RunLifecycleAction)) ||
    !strings(item.command) || !strings(item.checkpoints) || !integer(item.last_seen_at_ms) ||
    !Number.isFinite(new Date(item.last_seen_at_ms).valueOf())) throw new Error("Invalid run control response.");
  if (item.supported && (item.attempt < 1 || !item.binding_id)) throw new Error("Run control is missing its worker binding.");
  if (item.manual_paused !== undefined && typeof item.manual_paused !== "boolean") throw new Error("Invalid manual pause state.");
  const capabilities = item.capabilities as string[];
  if (item.available_actions !== undefined && (!strings(item.available_actions) ||
    item.available_actions.length > RUN_LIFECYCLE_ACTIONS.length ||
    new Set(item.available_actions).size !== item.available_actions.length ||
    !item.available_actions.every(action => RUN_LIFECYCLE_ACTIONS.includes(action as RunLifecycleAction) && capabilities.includes(action))))
    throw new Error("Invalid available lifecycle actions.");
  if (item.experiment_assignment_supported !== undefined && typeof item.experiment_assignment_supported !== "boolean") throw new Error("Invalid experiment assignment capability.");
  const resource_fields: Pick<RunLifecycle, "requirements" | "resource_status" | "manual_paused"> = {};
  if (item.requirements !== undefined) resource_fields.requirements = item.requirements === null ? null : parseResourceRequirements(item.requirements);
  if (item.resource_status !== undefined) resource_fields.resource_status = item.resource_status === null ? null : parseResourceStatus(item.resource_status);
  if (item.manual_paused !== undefined) resource_fields.manual_paused = item.manual_paused;
  let request: RunLifecycleRequest | null = null;
  if (item.request !== null) {
    const record = item.request as Partial<RunLifecycleRequest> | undefined;
    if (!record || typeof record.request_id !== "string" || typeof record.action !== "string" ||
      !RUN_LIFECYCLE_ACTIONS.includes(record.action) ||
      !["requested", "delivered", "executing", "applied", "failed", "expired"].includes(record.status ?? "") ||
      !(record.error === null || typeof record.error === "string") ||
      !(record.new_run_id === null || typeof record.new_run_id === "string" &&
        parseRunDeepLink(apiUrl("", { project_id: "project", origin: "worker", run_id: record.new_run_id })))) {
      throw new Error("Invalid run control acknowledgement.");
    }
    request = record as RunLifecycleRequest;
  }
  return {
    supported: item.supported, online: item.online, status: item.status,
    attempt: item.attempt, generation: item.generation, binding_id: item.binding_id,
    capabilities: item.capabilities as RunLifecycleAction[], command: item.command,
    ...(item.available_actions !== undefined ? { available_actions: item.available_actions as RunLifecycleAction[] } : {}),
    checkpoints: item.checkpoints, last_seen_at_ms: item.last_seen_at_ms, request, ...resource_fields,
    ...(item.experiment_assignment_supported !== undefined ? { experiment_assignment_supported: item.experiment_assignment_supported } : {}),
  };
}

/** JSON preserves argument boundaries, empty values and whitespace without a shell parser. */
export function parseLifecycleArguments(text: string): string[] {
  let value: unknown;
  try { value = JSON.parse(text); } catch { throw new Error("Arguments must be a JSON array of strings, for example [\"--epochs\", \"20\"]."); }
  if (!Array.isArray(value) || value.length > 256 || value.some(part => typeof part !== "string" || part.includes("\0"))) {
    throw new Error("Arguments must be a JSON array of up to 256 strings without null characters.");
  }
  const sizes = value.map(part => new TextEncoder().encode(part).length);
  if (sizes.some(size => size > 8192) || sizes.reduce((total, size) => total + size, 0) > 32768) {
    throw new Error("Task arguments allow up to 8,192 UTF-8 bytes each and 32,768 bytes combined.");
  }
  return value as string[];
}

export function parseLifecycleReason(text: string): string {
  if (/\p{Cc}/u.test(text) || new TextEncoder().encode(text).length > 1024) {
    throw new Error("The reason allows up to 1,024 UTF-8 bytes and cannot contain control characters.");
  }
  return text.trim() || "Requested from website";
}
