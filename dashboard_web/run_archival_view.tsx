import { dateText, localTimeZoneLabel } from "./dashboard_model";
import { canArchiveRun, canRestoreRun, runArchivalScope, runIsArchived } from "./run_archival";
import type { DashboardActions, DashboardSnapshot } from "./dashboard_view";

export function RunArchivalPanel({ snapshot: s, actions: a }: {
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
}) {
  const run = s.detail?.run;
  const source = s.sources.find((item) => item.source_id === s.source_id);
  if (!run || !run.archival || !runArchivalScope(run, source)) return null;
  const archival = run.archival;
  const archived = runIsArchived(run);
  const can_archive = canArchiveRun(run);
  const can_restore = canRestoreRun(run);
  const enabled = s.run_management_enabled && !s.review_loading;
  return <section id="run-archival-panel" className="run-archival-panel" aria-label="Run archive" aria-busy={s.archival_busy}>
    {archived && <div id="run-archival-summary" className="notice" role="status">
      <strong>{archival.status === "archived" ? "Archived run" :
        archival.status === "needs_attention" ? "Deletion needs attention" :
          archival.status === "deleted" ? "Run deleted" : "Deleting run"}</strong>
      <p>Archived {dateText(archival.archived_at)}. Scheduled deletion {dateText(archival.delete_after)} ({localTimeZoneLabel()}).</p>
      {archival.status === "archived" && <p>{can_restore ?
        "Restore this run before the deletion date to keep its hosted data." :
        "The retention period has ended. Automatic deletion is due."}</p>}
      {(archival.status === "deleting" || archival.status === "needs_attention") && <p>
        Hosted data cleanup is in progress{archival.pending_tasks > 0 ? ` (${archival.pending_tasks} tasks remaining)` : ""}.
      </p>}
      {archival.last_error && <p>{archival.last_error.slice(0, 512)}</p>}
      <p>Local copies and project private inputs are kept.</p>
    </div>}
    {s.archival_error && <p id="run-archival-error" className="notice error" role="alert">{s.archival_error}</p>}
    {enabled && !archived && !can_archive && <p className="muted archival-hint">Only finished runs can be archived.</p>}
    {enabled && can_archive && !s.archival_confirming && <button id="archive-run-button" type="button"
      className="text-button" disabled={s.archival_busy} onClick={a.archival_confirm}>Archive run</button>}
    {enabled && can_archive && s.archival_confirming && <div id="archive-run-confirmation" className="notice">
      <strong>Archive this run?</strong>
      <p>Its hosted metadata, logs, and outputs will be automatically deleted after 15 days. You can restore it before then. Local copies and project private inputs are kept.</p>
      <div className="archival-actions">
        <button id="confirm-archive-run" className="button danger" type="button" disabled={s.archival_busy}
          onClick={() => a.archival_mutate("archive")}>{s.archival_busy ? "Archiving…" : "Archive run"}</button>
        <button id="cancel-archive-run" className="text-button" type="button" disabled={s.archival_busy}
          onClick={a.archival_cancel}>Cancel</button>
      </div>
    </div>}
    {enabled && can_restore && <button id="restore-run-button" className="button secondary" type="button"
      disabled={s.archival_busy} onClick={() => a.archival_mutate("restore")}>{s.archival_busy ? "Restoring…" : "Restore run"}</button>}
    {!s.run_management_enabled && archived && <p className="muted archival-hint">Run management is disabled on this dashboard.</p>}
  </section>;
}
