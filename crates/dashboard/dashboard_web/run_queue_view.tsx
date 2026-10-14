import { Fragment, type ReactNode } from "react";
import { queueContext, queueMessage, queuePhaseLabel, queuePlaceholder, queueResourceLabels, type QueueRun } from "./run_queue";

const TERMINAL_QUEUE_STATES = ["completed", "failed", "cancelled", "lost"];

function submittedTime(timestamp_ms: number): ReactNode {
  const date = new Date(timestamp_ms);
  return <time dateTime={date.toISOString()}>{date.toLocaleString()}</time>;
}

/** A scheduling record is not a substitute for worker run evidence. */
export function QueueOverview({ run }: { run: QueueRun }) {
  const queue = queueContext(run);
  if (!queue) return <section id="queue-overview" className="card" role="status">
    <h3>Queue job</h3><p className="muted">Scheduling details are unavailable. Refresh the job to retry.</p>
  </section>;
  const placeholder = queuePlaceholder(run);
  const terminal = TERMINAL_QUEUE_STATES.includes(queue.status);
  const values: [string, ReactNode][] = [
    ["Job ID", <code>{queue.job_id}</code>],
    ["Task", queue.task],
    ["Submitted", submittedTime(queue.submitted_at_ms)],
    ["Queue phase", queuePhaseLabel(queue.status)],
    ["Assigned worker", queue.worker_id ?? "Not assigned"],
  ];
  if (queue.run_id !== null) values.push(["Acknowledged run ID", <code>{queue.run_id}</code>]);
  return <div id="queue-overview" className="detail-grid queue-overview">
    <section className="card" aria-labelledby="queue-overview-title">
      <h3 id="queue-overview-title">Queue job</h3>
      <dl className="facts">
        {values.map(([label, value]) => <Fragment key={label}><dt>{label}</dt><dd>{value}</dd></Fragment>)}
      </dl>
      {queue.status === "pending" && <p className="muted queue-context-note">
        Pending means this job is waiting for the scheduler to reserve its requested resources.
      </p>}
      {placeholder && queue.status !== "pending" && <div className="notice queue-context-note" role="status">
        <strong>{terminal ? "No worker run publication" : "Awaiting worker run metadata"}</strong>
        <p>{queueMessage(run) ?? (terminal ? "The queue job has ended, but no native worker run is available in this project." :
          "The worker has not published its native run record yet.")}</p>
      </div>}
    </section>
    <section className="card" aria-labelledby="queue-resources-title">
      <h3 id="queue-resources-title">Requested resources</h3>
      <ul className="queue-resource-list">
        {queueResourceLabels(queue.resources).map(label => <li key={label}>{label}</li>)}
      </ul>
      {queue.pending_reason !== null && <p className="queue-pending-reason">{queue.pending_reason}</p>}
      <p className="muted queue-context-note">Requests are scheduling reservations. Resource usage is available after the worker publishes a run.</p>
    </section>
  </div>;
}
