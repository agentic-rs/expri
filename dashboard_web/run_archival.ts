import { apiUrl, parseRunDeepLink, parseProjectRunKey, type Run, type RunArchival, type RunDeepLink, type Source } from "./dashboard_model";

export type ArchivalView = "active" | "archived";
export type ArchivalAction = "archive" | "restore";

export function runArchivalMatchesScope(value: unknown, scope: RunDeepLink): value is RunArchival {
  if (!value || typeof value !== "object") return false;
  const record = value as Partial<RunArchival>;
  const selected = record.scope;
  const valid_date = (date: unknown) => date === null ||
    (typeof date === "string" && Number.isFinite(Date.parse(date)));
  return !!selected && !!parseRunDeepLink(apiUrl("", selected)) &&
    selected.project_id === scope.project_id && selected.origin === scope.origin && selected.run_id === scope.run_id &&
    ["active", "archived", "deleting", "needs_attention", "deleted"].includes(record.status ?? "") &&
    valid_date(record.archived_at) && valid_date(record.delete_after) &&
    Number.isSafeInteger(record.pending_tasks) && (record.pending_tasks ?? -1) >= 0 &&
    (record.last_error === null || typeof record.last_error === "string") &&
    (record.status === "active" || (record.archived_at !== null && record.delete_after !== null));
}

export function runArchivalScope(run: Run, source: Source | undefined): RunDeepLink | null {
  const scope = run.archival?.scope;
  if (!scope || !source || !runArchivalMatchesScope(run.archival, scope)) return null;
  if (source.kind === "hosted_project") {
    const identity = parseProjectRunKey(run.run_key ?? "");
    return source.project_id === scope.project_id && identity?.origin === scope.origin &&
      identity.run_id === scope.run_id ? scope : null;
  }
  if (source.kind !== "service") return null;
  return source.project_id === scope.project_id && source.origin === scope.origin &&
    run.run_id === scope.run_id ? scope : null;
}

export function runIsArchived(run: Run): boolean {
  return !!run.archival && run.archival.status !== "active";
}

export function canArchiveRun(run: Run): boolean {
  return run.archival?.status === "active" &&
    ["completed", "failed", "cancelled", "lost"].includes(run.status);
}

export function canRestoreRun(run: Run, now = Date.now()): boolean {
  return run.archival?.status === "archived" && run.archival.delete_after !== null &&
    Date.parse(run.archival.delete_after) > now;
}
