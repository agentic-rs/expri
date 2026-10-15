import { attemptRestartGap, attemptWorkerLabel, parseTaskAttempts } from "./run_attempts";

function eventTime(label: string, value: string | null) {
  return value === null ? null : <span>{label} <time dateTime={value}>{new Date(value).toLocaleString()}</time></span>;
}

export function RunAttemptHistory({ state }: { state: unknown }) {
  const history = parseTaskAttempts(state !== null && typeof state === "object" && !Array.isArray(state) ? (state as Record<string, unknown>).task_attempts : null);
  if (!history) return null;
  return <details className="detail-details task-attempt-history">
    <summary>Task attempts<span className="supervisor-summary">Current {history.current_attempt} · {history.total_count} {history.total_count === 1 ? "record" : "records"}</span></summary>
    <ol className="supervisor-events">
      {[...history.attempts].reverse().map((attempt, index) => {
        const revision = attempt.source.files_sha256 ?? attempt.source.checkout_manifest_sha256 ?? attempt.source.sync_state_sha256 ?? attempt.source.git_head;
        const gap = attemptRestartGap(attempt);
        return <li key={`${attempt.attempt}:${index}`}>
          <div className="supervisor-event-heading"><strong>Attempt {attempt.attempt}{attempt.attempt === history.current_attempt ? " · Current" : ""}</strong><span className={attempt.status === "failed" ? "supervisor-failed" : "muted"}>{attempt.status}</span>{attempt.exit_code !== null && <span>Exit {attempt.exit_code}</span>}</div>
          {attempt.reason !== null && <p className="supervisor-reason">{attempt.reason}</p>}
          {attempt.error !== null && <p className="supervisor-failed supervisor-reason">{attempt.error} <span className="muted">Inspect <code>expri runs status</code> for details.</span></p>}
          <div className="attempt-dates muted">{eventTime("Requested", attempt.requested_at)}{eventTime("Started", attempt.started_at)}{eventTime("Finished", attempt.finished_at)}</div>
          <dl className="attempt-provenance">
            <dt>Code</dt><dd>{revision ? <code title={revision}>{revision.slice(0, 12)}</code> : "Not recorded"}{attempt.source.git_head && attempt.source.git_head !== revision && <small title={attempt.source.git_head}> · Git {attempt.source.git_head.slice(0, 12)}</small>}</dd>
            <dt>Checkpoint</dt><dd>{attempt.checkpoint ? <><code>{attempt.checkpoint.relative_path ?? attempt.checkpoint.name}</code>{attempt.checkpoint.sha256 && <small title={attempt.checkpoint.sha256}> · {attempt.checkpoint.sha256.slice(0, 12)}</small>}</> : attempt.attempt === 1 ? "Initial execution" : "Not recorded"}</dd>
            <dt>Global metric base</dt><dd>{attempt.attempt === 1 ? "Original steps" : <code>{attempt.metric_step_offset}</code>}{attempt.first_local_step !== null && <small> · Local origin {attempt.first_local_step}</small>}</dd>
            <dt>SDK parameters</dt><dd><code>{attempt.attempt === 1 ? "outputs/params.json" : `outputs/attempts/${attempt.attempt}/params.json`}</code></dd>
          </dl>
          <p className="muted supervisor-event-context">{attemptWorkerLabel(attempt)}{gap !== null && ` · Restart gap ${gap}`}</p>
          {attempt.previous_worker_stopped_at !== null && <div className="attempt-dates muted">{eventTime("Previous worker stopped", attempt.previous_worker_stopped_at)}</div>}
        </li>;
      })}
    </ol>
    {history.attempts.length === 0 && <p className="muted supervisor-empty">No valid attempt details are available in this preview.</p>}
    <p className="muted supervisor-history-note">Run elapsed time includes gaps between attempts. Metrics and logs retain their earlier history.</p>
    {history.truncated && <p className="muted supervisor-history-note">Showing {history.attempts.length} of {history.total_count} attempt records. The run record retains the full history.</p>}
  </details>;
}
