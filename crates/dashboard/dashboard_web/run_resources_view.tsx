import { useState, type FormEvent } from "react";
import { formatFileSize } from "./dashboard_files";
import { dateText } from "./dashboard_model";
import { diskQuantityText, parseResourceStatus, requirementsFromDraft, resourceBlockLabel, type DiskRequirementDraft, type ResourceRequirements, type ResourceStatus } from "./run_resources";

export function RunResourceBadge({ status }: { status: ResourceStatus | null | undefined }) {
  let value: ResourceStatus;
  try { value = parseResourceStatus(status); } catch { return null; }
  const label = resourceBlockLabel(value);
  if (!label) return null;
  const details = value.disk.filter(disk => disk.phase !== "ready").map(disk =>
    `${disk.path}: ${disk.available_bytes === null ? "free space unavailable" : `${formatFileSize(disk.available_bytes)} free`} · minimum ${formatFileSize(disk.min_free_bytes)} · recover at ${formatFileSize(disk.resume_free_bytes)}${disk.error ? ` · ${disk.error}` : ""}`);
  return <span className="run-resource-badge" title={details.join("\n")}>{label}</span>;
}

export function ResourceStatusSummary({ status, requirements, manual_paused, task_status }: {
  status: ResourceStatus | null | undefined;
  requirements: ResourceRequirements | null | undefined;
  manual_paused: boolean;
  task_status: string;
}) {
  if (!status?.blocked || !["pending", "paused"].includes(task_status)) return null;
  const recovering = status.phase === "recovering";
  const label = resourceBlockLabel(status);
  const recovery_seconds = requirements?.recovery_seconds ?? 30;
  return <div id="run-resource-status" className="resource-status" role="status">
    <div className="resource-status-heading"><strong>{label}</strong>
      <span className="muted">{task_status === "pending" ? "Waiting to start" : task_status === "paused" ? "Task suspended in memory" : "Worker resource hold"}</span>
    </div>
    <ul>{status.disk.filter(disk => disk.phase !== "ready").map(disk => <li key={disk.path}>
      <code>{disk.path}</code><span>{disk.available_bytes === null ? "Free space unavailable" : `${formatFileSize(disk.available_bytes)} free`}</span>
      <span className="muted">Minimum {formatFileSize(disk.min_free_bytes)} · recover at {formatFileSize(disk.resume_free_bytes)}</span>
      {disk.error && <span className="resource-check-error">{disk.error}</span>}
    </li>)}</ul>
    <p className="muted">{manual_paused ? "Manual pause is active. Resume clears your hold; the task still waits for its disk requirements." :
      recovering ? `Free space must stay above every recovery limit for ${recovery_seconds} seconds before ${task_status === "pending" ? "starting" : "resuming"} automatically.` :
        `The task ${task_status === "pending" ? "starts" : "resumes"} automatically after every recovery limit is met${recovery_seconds ? ` for ${recovery_seconds} seconds` : ""}.`}
      {status.recovery_started_at && <span> Recovery began {dateText(status.recovery_started_at)}.</span>}</p>
  </div>;
}

export function ResourceRequirementsEditor({ requirements, disabled, sending, readonly, on_save, on_cancel }: {
  requirements: ResourceRequirements | null | undefined;
  disabled: boolean;
  sending: boolean;
  readonly: boolean;
  on_save: (requirements: ResourceRequirements) => void;
  on_cancel: () => void;
}) {
  const [disk, setDisk] = useState<DiskRequirementDraft[]>(() => (requirements?.disk ?? []).map(entry =>
    ({ path: entry.path, min_free: diskQuantityText(entry.min_free_bytes), resume_free: diskQuantityText(entry.resume_free_bytes) })));
  const [recovery_seconds, setRecoverySeconds] = useState(requirements?.recovery_seconds ?? 30);
  const [error, setError] = useState<string | null>(null);
  const update = (index: number, patch: Partial<DiskRequirementDraft>) => setDisk(current => current.map((entry, position) => position === index ? { ...entry, ...patch } : entry));
  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (disabled || readonly) return;
    try { const value = requirementsFromDraft(disk, recovery_seconds); setError(null); on_save(value); }
    catch (failure) { setError(failure instanceof Error ? failure.message : String(failure)); }
  };
  return <form id="run-resource-form" className="lifecycle-form resource-form" onSubmit={submit}>
    <h3>Resource requirements</h3>
    <p className="muted">Check free disk space before starting and while running. A low disk pauses task processes in memory. Set a higher recovery limit to avoid repeated pauses.</p>
    <fieldset disabled={disabled || readonly}>
      <legend className="sr-only">Disk space requirements</legend>
      {disk.length === 0 && <p id="run-resource-disabled" className="muted">No disk requirements. Add a path to enable automatic protection.</p>}
      {disk.map((entry, index) => <div className="resource-disk-row" key={index}>
        <label className="field resource-path" htmlFor={`run-resource-path-${index}`}><span>Worker path</span>
          <input id={`run-resource-path-${index}`} type="text" value={entry.path} placeholder="outputs or /data" maxLength={4096}
            onChange={event => update(index, { path: event.currentTarget.value })} /></label>
        <label className="field" htmlFor={`run-resource-min-${index}`}><span>Minimum free</span>
          <input id={`run-resource-min-${index}`} type="text" value={entry.min_free} placeholder="20 GiB" autoComplete="off"
            onChange={event => update(index, { min_free: event.currentTarget.value })} /></label>
        <label className="field" htmlFor={`run-resource-resume-${index}`}><span>Recover at</span>
          <input id={`run-resource-resume-${index}`} type="text" value={entry.resume_free} placeholder="30 GiB" autoComplete="off"
            onChange={event => update(index, { resume_free: event.currentTarget.value })} /></label>
        <button id={`run-resource-remove-${index}`} type="button" className="text-button resource-remove" aria-label={`Remove disk requirement ${index + 1}`}
          onClick={() => setDisk(current => current.filter((_, position) => position !== index))}>Remove</button>
      </div>)}
      <div className="resource-options">
        <button id="run-resource-add" type="button" className="text-button" disabled={disk.length >= 16}
          onClick={() => setDisk(current => [...current, { path: current.length === 0 ? "outputs" : "", min_free: "", resume_free: "" }])}>Add disk path</button>
        <label className="field resource-recovery" htmlFor="run-resource-recovery"><span>Stable recovery · seconds</span>
          <input id="run-resource-recovery" type="number" min={0} max={3600} step={1} value={recovery_seconds}
            onChange={event => setRecoverySeconds(event.currentTarget.valueAsNumber)} /></label>
      </div>
    </fieldset>
    <p className="muted resource-path-note"><code>outputs</code> checks this run’s output filesystem. Absolute paths refer to the worker. Removing all paths disables protection. Changes apply only after saving.</p>
    {error && <p id="run-resource-error" className="notice error" role="alert">{error}</p>}
    <div className="lifecycle-form-actions">
      {!readonly && <button id="run-resource-save" type="submit" className="button primary" disabled={disabled}>{sending ? "Saving…" : "Save requirements"}</button>}
      {readonly && <span className="muted">Read only</span>}
      <button id="run-resource-cancel" type="button" className="text-button" disabled={sending} onClick={on_cancel}>{readonly ? "Close" : "Cancel"}</button>
    </div>
  </form>;
}
