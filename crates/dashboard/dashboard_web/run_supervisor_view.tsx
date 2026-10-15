import { parseRunSupervisor, supervisorWorkerLabel } from "./run_supervisor";

function eventTime(value: string) {
  return <time dateTime={value}>{new Date(value).toLocaleString()}</time>;
}

/** This history belongs to the existing run; a supervisor handoff is not a new run. */
export function RunSupervisorHistory({ state }: { state: unknown }) {
  const supervisor_value = state !== null && typeof state === "object" && !Array.isArray(state)
    ? (state as Record<string, unknown>).supervisor : null;
  const supervisor = parseRunSupervisor(supervisor_value);
  if (!supervisor) return null;
  return <details className="detail-details supervisor-history">
    <summary>
      <span>Supervisor</span>
      <span className="supervisor-summary">Generation {supervisor.generation} · {supervisor.total_count} restart {supervisor.total_count === 1 ? "attempt" : "attempts"}</span>
    </summary>
    {supervisor.events.length ? <ol className="supervisor-events">
      {[...supervisor.events].reverse().map((event, index) => <li key={`${event.request_id}:${index}`}>
        <div className="supervisor-event-heading">
          <strong>Target generation {event.generation}</strong>
          <span className={`supervisor-event-status supervisor-${event.status}`}>{event.status === "restarting" ? "Restarting" : event.status === "completed" ? "Completed" : "Failed"}</span>
          {eventTime(event.status === "completed" ? event.completed_at ?? event.requested_at : event.status === "failed" ? event.failed_at ?? event.requested_at : event.requested_at)}
        </div>
        <p className="supervisor-reason">{event.reason}</p>
        <p className="muted supervisor-event-context">Live · {event.worker_restarted === false && "Task kept running · "}{supervisorWorkerLabel(event)}</p>
        {event.error !== null && <p className="supervisor-error">{event.error}</p>}
      </li>)}
    </ol> : <p className="muted supervisor-empty">{supervisor.total_count === 0 ? "No supervisor restart attempts recorded." : "No valid restart details are available in this preview."}</p>}
    {supervisor.truncated && <p className="muted supervisor-history-note">Showing {supervisor.events.length} of {supervisor.total_count} restart attempts. The run record retains the full history.</p>}
  </details>;
}
