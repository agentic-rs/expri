import { runIdentity, runLabel, type Run, type Warning } from "./dashboard_model";

function distinctWarnings(items: Warning[]): Warning[] {
  const seen = new Set<string>();
  const unique = items.filter((item) => {
    if (seen.has(item.message)) return false;
    seen.add(item.message);
    return true;
  });
  return unique.length <= 100 ? unique : [
    ...unique.slice(0, 100),
    { message: "Additional data issues omitted; inspect records with expri runs show." },
  ];
}

/** Errors with no displayable row must still be visible, including skipped records. */
export function warningsOutsideRuns(items: Warning[], runs: Run[]): Warning[] {
  const identities = new Set(runs.map(runIdentity));
  return items.filter((item) => !item.run_id || !identities.has(item.run_id));
}

/** A project may contain the same run ID on multiple workers. Match its full key. */
export function runWarnings(run: Run, ...groups: Warning[][]): Warning[] {
  const identity = runIdentity(run);
  return distinctWarnings([
    ...(run.warnings ?? []),
    ...groups.flatMap((items) => items.filter((item) => item.run_id === identity)),
  ]);
}

export function RunRowDiagnostics({ run, items }: { run: Run; items: Warning[] }) {
  if (!items.length) return null;
  return (
    <details className="run-diagnostics" aria-label={`Data issues for ${runLabel(run)}`}>
      <summary>
        <span className="run-diagnostics-label">Data issues ({items.length})</span>
        <span className="run-diagnostics-message">{items[0]?.message}</span>
      </summary>
      {items.length > 1 && <ul>{items.slice(1).map((item) => <li key={item.message}>{item.message}</li>)}</ul>}
    </details>
  );
}

export function RunDetailDiagnostics({ id, items }: { id: string; items: Warning[] }) {
  const messages = distinctWarnings(items);
  return (
    <section id={id} className="notice error run-diagnostics-detail" aria-label="Run data issues" hidden={!messages.length}>
      <strong>Run data issues</strong>
      <ul>{messages.map((item) => <li key={item.message}>{item.message}</li>)}</ul>
    </section>
  );
}
