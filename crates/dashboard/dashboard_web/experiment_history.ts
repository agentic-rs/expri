import { RequestLane, errorText, runIdentity, type Run, type RunDeepLink, type RunList } from "./dashboard_model";
import type { ExperimentGroup, ExperimentState } from "./experiments";

type HistoryOptions = {
  context: () => string;
  url: (experiment_id: string, offset: number) => string;
  publish: () => void;
  login_run?: () => RunDeepLink | null;
};
type HistoryRead = { lane: RequestLane; anchor: string; pages: number; append: boolean };

/** Member pages have their own cursor and cancellation lifetime, independent of group pages. */
export class ExperimentHistoryController {
  private readonly reads = new Map<string, HistoryRead>();
  constructor(private readonly state: ExperimentState, private readonly options: HistoryOptions) {}

  private group(id: string): ExperimentGroup | undefined {
    return this.state.groups.find((group) => group.experiment?.experiment_id === id);
  }
  private anchor(id: string): string {
    const group = this.group(id);
    return JSON.stringify([this.options.context(), group?.experiment?.revision, group?.run_key]);
  }
  reset(): void {
    for (const read of this.reads.values()) read.lane.cancel();
    this.reads.clear();
    this.state.expanded = [];
    this.state.histories = {};
  }
  expand(id: string, open: boolean): void {
    if (!open) {
      this.reads.get(id)?.lane.cancel();
      this.reads.delete(id);
      this.state.expanded = this.state.expanded.filter((value) => value !== id);
      const { [id]: removed, ...remaining } = this.state.histories;
      void removed;
      this.state.histories = remaining;
      this.options.publish();
      return;
    }
    if (!this.group(id)) return;
    if (this.state.expanded.includes(id)) {
      if (this.state.histories[id]?.error) void this.load(id, false);
      return;
    }
    this.state.expanded = [...this.state.expanded, id];
    void this.load(id, false);
  }
  more(id: string): void {
    const history = this.state.histories[id];
    if (this.state.expanded.includes(id) && !history?.loading)
      void this.load(id, history?.error ? this.reads.get(id)?.append === true : history !== undefined && history.next_offset !== null);
  }
  reconcile(): void {
    for (const id of [...this.state.expanded]) {
      if (!this.group(id)) this.expand(id, false);
      else if (!this.state.histories[id]?.loading || this.reads.get(id)?.anchor !== this.anchor(id)) void this.load(id, false);
    }
  }
  mapRuns(update: (run: Run) => Run | null): void {
    this.state.histories = Object.fromEntries(Object.entries(this.state.histories).map(([id, history]) => {
      const read = this.reads.get(id);
      read?.lane.cancel();
      if (read) this.reads.set(id, { ...read });
      return [id, { ...history, loading: false,
        runs: history.runs.flatMap((run) => { const next = update(run); return next ? [next] : []; }) }];
    }));
  }
  private async load(id: string, append: boolean): Promise<void> {
    if (!this.state.groups_supported || this.state.view !== "experiments" || !this.group(id)) return;
    const anchor = this.anchor(id), previous = this.reads.get(id);
    previous?.lane.cancel();
    const read: HistoryRead = { lane: previous?.lane ?? new RequestLane(this.options.login_run),
      anchor, pages: previous?.anchor === anchor ? previous.pages : 0, append };
    this.reads.set(id, read);
    const old = previous?.anchor === anchor ? this.state.histories[id] : undefined;
    const offset = append ? old?.next_offset : 0;
    if (offset === null || offset === undefined) return;
    const current = () => this.reads.get(id) === read && this.state.expanded.includes(id) &&
      this.state.groups_supported && this.state.view === "experiments" && this.anchor(id) === anchor;
    this.state.histories = { ...this.state.histories, [id]: { runs: old?.runs ?? [], loading: true, error: null,
      offset: old?.offset ?? 0, next_offset: old?.next_offset ?? null, total_count: old?.total_count ?? null } };
    this.options.publish();
    try {
      let cursor = offset, collected = append ? old?.runs ?? [] : [], next_offset: number | null = null;
      const pages = append ? 1 : Math.max(1, read.pages);
      let received = 0, total_count = 0;
      for (let page = 0; page < pages; page++) {
        const result = await read.lane.run<RunList>(this.options.url(id, cursor));
        if (!result || !current()) return;
        if (result.source?.source_id !== new URL(this.options.url(id, cursor), "http://expri.local").searchParams.get("source") ||
          !Array.isArray(result.runs) || result.runs.length > 100 || result.offset !== cursor ||
          !Number.isSafeInteger(result.total_count) || result.total_count < 0 || result.total_count > 256 ||
          (result.next_offset !== null && (!Number.isSafeInteger(result.next_offset) || result.next_offset <= cursor)))
          throw new Error("Experiment history returned invalid pagination. Refresh to retry.");
        const ids = new Set(collected.map(runIdentity));
        for (const run of result.runs) {
          if (run.experiment !== undefined && (!run.experiment || run.experiment.experiment_id !== id ||
            run.experiment.revision !== this.group(id)?.experiment?.revision))
            throw new Error("Experiment membership changed. Refresh to retry.");
          const key = runIdentity(run);
          if (!ids.has(key)) { collected = [...collected, run]; ids.add(key); }
        }
        if (collected.length > 256) throw new Error("Experiment history exceeds its member limit.");
        total_count = result.total_count;
        next_offset = result.next_offset;
        received++;
        if (next_offset === null) break;
        cursor = next_offset;
      }
      if (!current()) return;
      read.pages = append ? read.pages + received : received;
      this.state.histories = { ...this.state.histories, [id]: { runs: collected, loading: false, error: null,
        offset: 0, next_offset, total_count } };
    } catch (error) {
      if (current()) this.state.histories = { ...this.state.histories, [id]: {
        ...this.state.histories[id]!, loading: false, error: errorText(error) } };
    } finally {
      if (current()) this.options.publish();
    }
  }
}
