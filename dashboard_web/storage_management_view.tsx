import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { formatFileSize } from "./dashboard_files";
import { StorageManagementController, type DeletePreview, type StorageStats } from "./storage_management";

function Usage({ stats }: { stats: StorageStats }) {
  const s3_bytes = stats.s3_storage_bytes ?? stats.object_bytes + stats.retained_object_bytes;
  const s3_objects = stats.s3_object_count ?? stats.object_count + stats.retained_object_count;
  return (
    <>
      <dl className="storage-usage-grid">
        <div><dt>S3 storage</dt><dd>{formatFileSize(s3_bytes)}</dd>
          <dd className="storage-usage-description">{s3_objects.toLocaleString()} stored objects, including retained uploads</dd></div>
        <div><dt>Local storage</dt><dd>{stats.local_storage_bytes === undefined ? "Unavailable" : formatFileSize(stats.local_storage_bytes)}</dd>
          <dd className="storage-usage-description">Tracking files and result ZIPs on the server</dd></div>
      </dl>
      <dl className="storage-usage-details">
        <div><dt>Pending uploads</dt><dd>{formatFileSize(stats.pending_upload_bytes)} declared · {stats.pending_upload_count.toLocaleString()} uploads</dd></div>
      </dl>
      <p className="muted storage-usage-note">
        Shared inputs and outputs count once in S3 storage. Totals cover recorded project data;
        historical S3 versions and shared database overhead are excluded.
      </p>
      {stats.local_storage_bytes === undefined && <p className="muted storage-usage-note">
        Upgrade the server to see local storage usage.
      </p>}
    </>
  );
}

function Preview({ preview }: { preview: DeletePreview }) {
  return (
    <div id="delete-project-preview">
      <p>Delete <strong>{preview.project_id}</strong> from this hosted service?</p>
      <p>This removes all project runs, metadata, metrics, logs, private inputs, and run outputs on the server.
        Files on workers and your laptop remain. Objects referenced by another project remain available there.
        This project ID cannot be reused on this service.</p>
      <dl className="delete-preview-counts">
        <div><dt>Runs</dt><dd>{preview.run_count.toLocaleString()}</dd></div>
        <div><dt>Objects to reclaim</dt><dd>{preview.stats.reclaimable_object_count.toLocaleString()}</dd></div>
        <div><dt>S3 storage to reclaim</dt><dd>{formatFileSize(preview.stats.reclaimable_object_bytes)}</dd></div>
        {preview.stats.local_storage_bytes !== undefined && <div><dt>Local storage to remove</dt><dd>{formatFileSize(preview.stats.local_storage_bytes)}</dd></div>}
      </dl>
      {preview.stats.pending_upload_count > 0 && <p className="notice">
        {preview.stats.pending_upload_count.toLocaleString()} pending uploads will be aborted.
      </p>}
    </div>
  );
}

export function ProjectStorageManagement({
  project_id,
  active,
  auto_enabled,
  on_project_removed,
}: {
  project_id: string | null;
  active: boolean;
  auto_enabled: boolean;
  on_project_removed: () => void;
}) {
  const [controller] = useState(() => {
    let storage: Storage | null = null;
    try { storage = window.sessionStorage; } catch { /* Private browser storage can be unavailable. */ }
    return new StorageManagementController({ storage });
  });
  const state = useSyncExternalStore(controller.subscribe, controller.getSnapshot);
  const dialog = useRef<HTMLDialogElement>(null);
  const form = useRef<HTMLFormElement>(null);
  const notified_projects = useRef(new Set<string>());
  const [confirmation, setConfirmation] = useState("");
  const [password_present, setPasswordPresent] = useState(false);
  const is_open = state.phase !== "closed";
  useEffect(() => {
    controller.setContext(project_id, active, auto_enabled);
  }, [controller, project_id, active, auto_enabled]);
  useEffect(() => () => controller.dispose(), [controller]);
  useEffect(() => {
    const removed_project = state.deletion?.project_id;
    if (!removed_project || removed_project !== project_id || notified_projects.current.has(removed_project)) return;
    notified_projects.current.add(removed_project);
    let current = true;
    queueMicrotask(() => { if (current) on_project_removed(); });
    return () => { current = false; };
  }, [state.deletion, project_id, on_project_removed]);
  useEffect(() => {
    const node = dialog.current;
    if (!node) return;
    if (is_open) {
      if (!node.open) {
        if (typeof node.showModal === "function") node.showModal();
        else node.setAttribute("open", "");
      }
    } else {
      form.current?.reset();
      setConfirmation("");
      setPasswordPresent(false);
      if (node.open) {
        if (typeof node.close === "function") node.close();
        else node.removeAttribute("open");
      }
    }
  }, [is_open]);
  useEffect(() => {
    form.current?.reset();
    setConfirmation("");
    setPasswordPresent(false);
  }, [state.preview, state.phase]);
  const close = () => {
    form.current?.reset();
    setConfirmation("");
    setPasswordPresent(false);
    controller.close();
  };
  const pending = state.phase === "submitting" || state.checking_status;
  const current_stats = state.stats?.project_id === project_id ? state.stats : null;
  return (
    <>
      {active && project_id && (
        <section id="storage-usage" className="card storage-usage" aria-labelledby="storage-usage-heading" aria-busy={state.stats_loading}>
          <div className="section-heading">
            <div><h2 id="storage-usage-heading">Storage usage</h2><p>Project <code>{project_id}</code></p></div>
            <div className="storage-management-actions">
              <button id="refresh-storage-usage" type="button" className="text-button" disabled={state.stats_loading}
                onClick={() => { void controller.refreshStats(); }}>Refresh usage</button>
              {state.delete_enabled && <button id="delete-project-button" type="button" className="button danger secondary"
                disabled={is_open} onClick={() => { void controller.openPreview(); }}>Delete project</button>}
            </div>
          </div>
          {state.stats_loading && !current_stats && <p className="muted" role="status">Loading storage usage…</p>}
          {state.stats_error && <p id="storage-usage-error" className="notice error" role="alert">{state.stats_error}</p>}
          {current_stats && <Usage stats={current_stats} />}
          {current_stats && !state.delete_enabled && <p className="muted storage-usage-note">
            Project deletion is disabled on this dashboard.
          </p>}
        </section>
      )}
      {state.deletion_project_id && state.phase === "closed" && (state.deletion || state.error) && (
        <div id="project-cleanup-summary" className="notice" role="status">
          <span>{state.deletion?.status === "deleted" ? "Deleted" : state.deletion?.status === "needs_attention"
            ? "Needs attention: cleanup for" : "Cleanup status for"} project <strong>{state.deletion_project_id}</strong>.</span>
          <button type="button" className="text-button" onClick={() => controller.showStatus()}>View cleanup status</button>
        </div>
      )}
      <dialog ref={dialog} id="delete-project-dialog" className="project-delete-dialog"
        aria-labelledby="delete-project-heading" aria-describedby="delete-project-scope"
        onCancel={(event) => { event.preventDefault(); close(); }}>
        <div className="section-heading">
          <h2 id="delete-project-heading">{state.phase === "status" ? "Project cleanup" : "Delete project"}</h2>
          <button id="delete-project-close" type="button" className="text-button" disabled={state.phase === "submitting"}
            aria-label="Close project deletion" onClick={close}>Close</button>
        </div>
        <p id="delete-project-scope" className="muted">This action applies to the whole hosted project and cannot be undone.</p>
        {state.phase === "preview_loading" && <p role="status">Preparing deletion preview…</p>}
        {state.error && <p id="delete-project-error" className="notice error" role="alert">{state.error}</p>}
        {(state.phase === "review" || state.phase === "submitting") && (
          <form ref={form} onSubmit={(event) => {
            event.preventDefault();
            const node = event.currentTarget;
            const confirmation_input = node.elements.namedItem("confirmation") as HTMLInputElement | null;
            const password_input = node.elements.namedItem("password") as HTMLInputElement | null;
            const name = confirmation_input?.value ?? "";
            const password = password_input?.value ?? "";
            node.reset();
            setConfirmation("");
            setPasswordPresent(false);
            void controller.submit(name, password);
          }}>
            {state.preview && <Preview preview={state.preview} />}
            {state.stale ? (
              <button id="refresh-delete-preview" type="button" className="button secondary"
                onClick={() => { void controller.openPreview(); }}>Review fresh preview</button>
            ) : state.preview && (
              <fieldset disabled={state.phase === "submitting"} className="delete-confirmation-fields">
                <label className="field">Type <strong>{state.preview.project_id}</strong> to confirm
                  <input id="delete-project-confirmation" name="confirmation" type="text" autoComplete="off" required
                    value={confirmation} onChange={(event) => setConfirmation(event.currentTarget.value)} />
                </label>
                <label className="field">Dashboard password
                  <input id="delete-project-password" name="password" type="password" autoComplete="current-password" required
                    onChange={(event) => setPasswordPresent(event.currentTarget.value.length > 0)} />
                </label>
                <p className="muted">Stop or reconfigure workers that publish to this project before deleting it.</p>
                <button id="delete-project-submit" type="submit" className="button danger"
                  disabled={confirmation !== state.preview.project_id || !password_present}>
                  {state.phase === "submitting" ? "Requesting deletion…" : "Delete project permanently"}
                </button>
              </fieldset>
            )}
            <button id="delete-project-cancel" type="button" className="button secondary" disabled={state.phase === "submitting"}
              onClick={close}>Cancel</button>
          </form>
        )}
        {state.phase === "status" && (
          <div id="project-cleanup-status" role="status" aria-live="polite">
            {state.deletion && <p><strong id="project-cleanup-label">{state.deletion.status === "needs_attention"
              ? "Needs attention" : state.deletion.status === "deleted" ? "Completed"
              : state.deletion.last_error ? "Retrying cleanup" : "Cleanup in progress"}</strong></p>}
            <p><strong>{state.deletion_project_id}</strong>: {state.deletion?.status === "deleted"
              ? "Project deleted. Cleanup completed."
              : state.deletion?.status === "needs_attention" ? "Project removed from the catalog. Storage cleanup needs attention."
              : state.deletion ? "Project removed from the catalog. Storage cleanup is in progress." : "Deletion status needs to be checked."}</p>
            {state.deletion && <dl className="delete-preview-counts">
              <div><dt>Pending cleanup tasks</dt><dd>{state.deletion.pending_tasks.toLocaleString()}</dd></div>
              <div><dt>Objects deleted</dt><dd>{state.deletion.deleted_objects.toLocaleString()}</dd></div>
              <div><dt>Uploads aborted</dt><dd>{state.deletion.aborted_uploads.toLocaleString()}</dd></div>
            </dl>}
            {state.deletion?.last_error && state.deletion.status !== "deleted" && <p id="project-cleanup-error" className="notice error">
              {state.deletion.status === "pending" && "Cleanup is retrying. "}{state.deletion.last_error}
            </p>}
            {state.deletion?.status === "needs_attention" && <p className="muted">
              Resolve the reported cause. The server rechecks automatically. Check status again after correcting the problem.
            </p>}
            {state.deletion && state.deletion.status !== "deleted" && <p className="muted">
              Cleanup continues on the server if you leave this page or restart the service. Check status again if progress stops updating.
            </p>}
            <button id="check-project-cleanup" type="button" className="button secondary" disabled={pending}
              onClick={() => { void controller.checkStatus(); }}>{state.checking_status ? "Checking…" : "Check status"}</button>
          </div>
        )}
      </dialog>
    </>
  );
}
