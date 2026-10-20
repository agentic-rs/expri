export type DiskRequirement = { path: string; min_free_bytes: number; resume_free_bytes: number };
export type ResourceRequirements = { disk: DiskRequirement[]; recovery_seconds: number };
export type ResourcePhase = "ready" | "blocked" | "recovering" | "unavailable";
export type DiskResourceStatus = DiskRequirement & { available_bytes: number | null; phase: ResourcePhase; error: string | null };
export type ResourceStatus = {
  phase: ResourcePhase;
  blocked: boolean;
  checked_at: string;
  recovery_started_at: string | null;
  disk: DiskResourceStatus[];
};
export type DiskRequirementDraft = { path: string; min_free: string; resume_free: string };
const PHASES: readonly string[] = ["ready", "blocked", "recovering", "unavailable"];
function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : null;
}
function integer(value: unknown, minimum = 0): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= minimum;
}
/** Worker observations are display-only u64 values; policy thresholds remain exact safe integers. */
function observedBytes(value: unknown): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= Number(18_446_744_073_709_551_615n);
}
function timestamp(value: unknown): value is string {
  return typeof value === "string" && value.length <= 64 &&
    /^\d{4}-\d{2}-\d{2}[Tt]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:[Zz]|[+-]\d{2}:\d{2})$/.test(value) && Number.isFinite(Date.parse(value));
}
function diskPath(value: unknown): value is string {
  return typeof value === "string" && new TextEncoder().encode(value).length <= 4096 && !/\p{Cc}/u.test(value) &&
    (value === "outputs" || value.startsWith("/") && value.split("/").every(part => part !== "." && part !== ".."));
}
export function parseResourceRequirements(value: unknown): ResourceRequirements {
  const item = record(value);
  if (!item || !Array.isArray(item.disk) || item.disk.length > 16 || !integer(item.recovery_seconds) || item.recovery_seconds > 3600) {
    throw new Error("Disk requirements allow up to 16 paths and a recovery period from 0 to 3,600 seconds.");
  }
  const disk = item.disk.map(value => {
    const entry = record(value);
    if (!entry || !diskPath(entry.path) || !integer(entry.min_free_bytes, 1) || !integer(entry.resume_free_bytes, 1) || entry.resume_free_bytes <= entry.min_free_bytes) {
      throw new Error("Each disk requirement needs outputs or an absolute worker path, a positive free-space limit, and a higher recovery limit.");
    }
    return { path: entry.path, min_free_bytes: entry.min_free_bytes, resume_free_bytes: entry.resume_free_bytes };
  });
  if (new Set(disk.map(entry => entry.path)).size !== disk.length) throw new Error("Each disk path can appear only once.");
  return { disk, recovery_seconds: item.recovery_seconds };
}
export function parseResourceStatus(value: unknown): ResourceStatus {
  const item = record(value);
  if (!item || typeof item.phase !== "string" || !PHASES.includes(item.phase) || typeof item.blocked !== "boolean" ||
    !timestamp(item.checked_at) || !(item.recovery_started_at === null || timestamp(item.recovery_started_at)) || !Array.isArray(item.disk) || item.disk.length > 16) {
    throw new Error("Invalid resource status.");
  }
  const requirements = parseResourceRequirements({ disk: item.disk, recovery_seconds: 0 });
  const disk = item.disk.map((value, index) => {
    const entry = record(value)!;
    if (!(entry.available_bytes === null || observedBytes(entry.available_bytes)) || typeof entry.phase !== "string" || !PHASES.includes(entry.phase) ||
      !(entry.error === null || typeof entry.error === "string" && entry.error.length <= 4096)) throw new Error("Invalid disk resource status.");
    return { ...requirements.disk[index]!, available_bytes: entry.available_bytes, phase: entry.phase as ResourcePhase, error: entry.error };
  });
  return { phase: item.phase as ResourcePhase, blocked: item.blocked, checked_at: item.checked_at, recovery_started_at: item.recovery_started_at, disk };
}

/** Quantities remain exact integer bytes in requests, including edited existing limits. */
export function parseDiskQuantity(text: string): number {
  const match = /^\s*(\d+(?:\.\d+)?)\s*(B|KiB|MiB|GiB|TiB|KB|MB|GB|TB)?\s*$/i.exec(text);
  if (!match) throw new Error("Enter a disk size such as 20 GiB, 500 MB, or 1024 B.");
  const units: Record<string, bigint> = { b: 1n, kib: 1024n, mib: 1024n ** 2n, gib: 1024n ** 3n, tib: 1024n ** 4n,
    kb: 1000n, mb: 1000n ** 2n, gb: 1000n ** 3n, tb: 1000n ** 4n };
  const [whole, fraction = ""] = match[1]!.split(".");
  if (whole!.length + fraction.length > 32) throw new Error("Disk sizes must fit a positive, exact byte count.");
  const scale = 10n ** BigInt(fraction.length);
  const quantity = BigInt(whole! + fraction) * units[(match[2] ?? "B").toLowerCase()]!;
  if (quantity % scale !== 0n || quantity < scale || quantity / scale > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error("Disk sizes must fit a positive, exact byte count.");
  }
  return Number(quantity / scale);
}
export function diskQuantityText(bytes: number): string {
  for (const [unit, factor] of [["TiB", 1024 ** 4], ["GiB", 1024 ** 3], ["MiB", 1024 ** 2], ["KiB", 1024]] as const) {
    if (bytes >= factor && bytes % factor === 0) return `${bytes / factor} ${unit}`;
  }
  return `${bytes} B`;
}
export function requirementsFromDraft(disk: DiskRequirementDraft[], recovery_seconds: number): ResourceRequirements {
  return parseResourceRequirements({ disk: disk.map(entry => ({ path: entry.path.trim(), min_free_bytes: parseDiskQuantity(entry.min_free), resume_free_bytes: parseDiskQuantity(entry.resume_free) })), recovery_seconds });
}
export function resourceBlockLabel(status: ResourceStatus | null | undefined): string | null {
  if (!status || !status.blocked) return null;
  return status.phase === "recovering" ? "Disk recovering" : status.phase === "unavailable" ? "Disk check unavailable" : "Low disk space";
}
