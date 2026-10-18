import {
  formatNumber,
  type Run,
  type RunColumn,
  type RunSort,
  type RunTableValue,
} from "./dashboard_model";

export const MAX_RUN_COLUMNS = 8;

export function runColumnIdentity(column: RunColumn): string {
  return JSON.stringify([column.kind, column.key]);
}

export function runColumnSortKey(column: RunColumn): string {
  return `${column.kind}:${column.key}`;
}

export function runColumnCount(column: RunColumn): number | undefined {
  return typeof column.run_count === "number" && Number.isSafeInteger(column.run_count) && column.run_count >= 0
    ? column.run_count : undefined;
}

/** Keep chosen columns available even when bounded discovery no longer includes them. */
export function runColumnChoices(available: RunColumn[], selected: RunColumn[]): RunColumn[] {
  const choices = new Map<string, RunColumn>();
  for (const column of selected) {
    const retained = { ...column };
    delete retained.run_count;
    choices.set(runColumnIdentity(column), retained);
  }
  // Fresh discovery owns prevalence; a saved selection may come from another
  // source or an earlier sample and must not overwrite the current counts.
  for (const column of available)
    choices.set(runColumnIdentity(column), column);
  return [...choices.values()].sort(
    (first, second) => first.kind.localeCompare(second.kind)
      || (runColumnCount(second) ?? -1) - (runColumnCount(first) ?? -1)
      || first.label.localeCompare(second.label)
      || first.key.localeCompare(second.key),
  );
}

export function runColumnValue(run: Run, column: RunColumn): RunTableValue | undefined {
  const values = column.kind === "param" ? run.table_values?.params : run.table_values?.metrics;
  return values && Object.hasOwn(values, column.key) ? values[column.key] : undefined;
}

function boundedLabel(value: string, maximum: number): string {
  const characters = Array.from(value);
  return characters.length > maximum ? `${characters.slice(0, maximum - 1).join("")}…` : value;
}

export function formatRunTableValue(value: unknown): {
  text: string;
  title: string | undefined;
  numeric: boolean;
} {
  if (
    value === null ||
    value === undefined ||
    !["boolean", "number", "string"].includes(typeof value) ||
    (typeof value === "number" && !Number.isFinite(value))
  )
    return { text: "—", title: undefined, numeric: false };
  if (typeof value === "number")
    return { text: formatNumber(value), title: String(value), numeric: true };
  const full = String(value);
  return {
    text: full === "" ? '""' : boundedLabel(full, 80),
    title: full === "" ? "Empty string" : boundedLabel(full, 1024),
    numeric: false,
  };
}

export function runSortIsDefault(sort: RunSort): boolean {
  return sort.key === "started_at" && sort.direction === "desc";
}

export function runSortLabel(sort: RunSort, columns: RunColumn[]): string {
  if (runSortIsDefault(sort)) return "Newest first";
  const builtins: Record<string, string> = {
    started_at: "Start time",
    run_id: "Run",
    status: "Status",
    origin: "Machine",
  };
  const column = columns.find((item) => runColumnSortKey(item) === sort.key);
  const name = builtins[sort.key] ?? column?.label ?? "Column";
  return `${name} · ${sort.direction === "asc" ? "ascending" : "descending"}`;
}
