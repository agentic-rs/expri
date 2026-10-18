import { runIdentity, runLabel, type Run, type Warning } from "./dashboard_model";

type DiagnosticSeverity = "info" | "warning" | "error";

/** Older servers return message-only diagnostics, so recognize only known notices. */
export function warningSeverity(item: Warning): DiagnosticSeverity {
  if (item.severity === "info" || item.severity === "warning" || item.severity === "error") return item.severity;
  if (
    /^metrics\.jsonl line [1-9]\d*: steps decreased; last values use recorded order$/.test(item.message) ||
    item.message === "outputs/metrics.jsonl is missing; no metrics have been recorded" ||
    item.message === "outputs/metrics.jsonl was absent from the current remote selection" ||
    /^metric is not recorded: .+$/.test(item.message) ||
    /^requested metric is not recorded in any selected run: .+$/.test(item.message) ||
    item.message === "Hosted chart curves are sampled to bound memory; last/min/max summaries use every recorded point."
  ) return "info";
  if (
    /^metrics\.jsonl line [1-9]\d*(?:: .+| exceeds the 1 MiB limit); skipped$/.test(item.message) ||
    item.message === "metrics.jsonl has an incomplete final row; not indexed yet"
  ) return "warning";
  return "error";
}

/** Informational metric notes belong in run details, rather than the run list. */
export function visibleWarnings(items: Warning[]): Warning[] {
  return items.filter((item) => warningSeverity(item) !== "info");
}

function distinctWarnings(items: Warning[]): Warning[] {
  const unique = new Map<string, Warning>();
  const rank = { info: 0, warning: 1, error: 2 };
  for (const item of items) {
    const previous = unique.get(item.message);
    if (!previous || (item.severity !== undefined && (
      previous.severity === undefined || rank[warningSeverity(item)] > rank[warningSeverity(previous)]
    ))) unique.set(item.message, item);
  }
  return [...unique.values()];
}

function severityWarnings(items: Warning[], severity: DiagnosticSeverity): Warning[] {
  const messages = distinctWarnings(items).filter((item) => warningSeverity(item) === severity);
  return messages.length <= 100 ? messages : [
    ...messages.slice(0, 100),
    { message: "Additional data issues omitted; inspect records with expri runs show.", severity },
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
  const errors = severityWarnings(items, "error"), warnings = severityWarnings(items, "warning");
  if (!errors.length && !warnings.length) return null;
  return (
    <div className="run-diagnostics-group">
      {([
        { severity: "error", label: "Errors", messages: errors },
        { severity: "warning", label: "Incomplete metrics", messages: warnings },
      ] as const).filter((group) => group.messages.length).map((group) => (
        <details key={group.severity} className={`run-diagnostics ${group.severity}`} aria-label={`${group.label} for ${runLabel(run)}`}>
          <summary><span className="run-diagnostics-label">{group.label} ({group.messages.length})</span></summary>
          <ul>{group.messages.map((item) => <li key={item.message}>{item.message}</li>)}</ul>
        </details>
      ))}
    </div>
  );
}

export function RunDetailDiagnostics({ id, items }: { id: string; items: Warning[] }) {
  const errors = severityWarnings(items, "error"), warnings = severityWarnings(items, "warning"), notes = severityWarnings(items, "info");
  return (
    <>
      <section id={id} className="notice error run-diagnostics-detail" aria-label="Run data errors" hidden={!errors.length}>
        <strong>Run data errors</strong>
        <ul>{errors.map((item) => <li key={item.message}>{item.message}</li>)}</ul>
      </section>
      <section id={`${id}-warnings`} className="notice run-diagnostics-detail" aria-label="Incomplete metrics" hidden={!warnings.length}>
        <strong>Incomplete metrics</strong>
        <ul>{warnings.map((item) => <li key={item.message}>{item.message}</li>)}</ul>
      </section>
      <details id={`${id}-notes`} className="run-metric-notes run-diagnostics-detail" hidden={!notes.length}>
        <summary>Metric notes <span className="muted">({notes.length})</span></summary>
        <ul>{notes.map((item) => <li key={item.message}>{item.message}</li>)}</ul>
      </details>
    </>
  );
}
