import { apiUrl, parseRunDeepLink, type RunDeepLink } from "./dashboard_model";
import { experimentDisplayName, type ExperimentCatalog } from "./experiments";

export type ExperimentSelection = { kind: "existing"; experiment_id: string } | { kind: "new"; name: string } |
  { kind: "join_run"; run_id: string; origin: string };
export type RestartExperimentDraft = {
  kind: "ungrouped" | ExperimentSelection["kind"];
  experiment_id: string;
  name: string;
  run_id: string;
  origin: string;
};

export function restartExperimentDraft(scope: RunDeepLink | null): RestartExperimentDraft {
  return { kind: "ungrouped", experiment_id: "", name: "", run_id: scope?.run_id ?? "", origin: scope?.origin ?? "" };
}

export function restartExperimentSelection(draft: RestartExperimentDraft, project_id: string,
  catalog: ExperimentCatalog | null | undefined): ExperimentSelection | null {
  if (draft.kind === "ungrouped") return null;
  if (draft.kind === "existing") {
    if (catalog?.project_id !== project_id || !catalog.experiments.some(item => item.experiment_id === draft.experiment_id)) {
      throw new Error("Choose an available experiment in this project.");
    }
    return { kind: "existing", experiment_id: draft.experiment_id };
  }
  if (draft.kind === "new") {
    const name = draft.name.trim();
    if (!name || /\p{Cc}/u.test(name) || Array.from(name).length > 128 || new TextEncoder().encode(name).length > 512) {
      throw new Error("Experiment name must contain 1 to 128 characters without control characters (at most 512 bytes).");
    }
    return { kind: "new", name };
  }
  const run_id = draft.run_id.trim(), origin = draft.origin.trim();
  if (!parseRunDeepLink(apiUrl("", { project_id, origin, run_id }))) {
    throw new Error("Enter a valid machine origin and run ID from this project.");
  }
  return { kind: "join_run", run_id, origin };
}

export function RestartExperimentEditor({ draft, catalog, project_id, disabled, on_change }: {
  draft: RestartExperimentDraft;
  catalog: ExperimentCatalog | null | undefined;
  project_id: string;
  disabled: boolean;
  on_change: (draft: RestartExperimentDraft) => void;
}) {
  const experiments = catalog?.project_id === project_id ? catalog.experiments : [];
  return <fieldset id="restart-experiment" className="choice-group" disabled={disabled}>
    <legend>Experiment</legend>
    <div className="choice-tags">{([['ungrouped', 'Ungrouped'], ['existing', 'Existing experiment'], ['new', 'New experiment'], ['join_run', 'Join a run']] as const).map(([kind, label]) =>
      <label className="choice-tag" key={kind}><input id={`restart-experiment-${kind}`} className="sr-only" type="radio"
        name="restart-experiment" value={kind} checked={draft.kind === kind}
        onChange={() => on_change({ ...draft, kind })} /><span>{label}</span></label>)}</div>
    {draft.kind === "existing" && <label className="field">Choose experiment<select id="restart-experiment-id" value={draft.experiment_id}
      onChange={event => on_change({ ...draft, experiment_id: event.currentTarget.value })}>
      <option value="">{experiments.length ? "Choose an experiment" : "No experiments available"}</option>
      {[...experiments].sort((a, b) => a.name.localeCompare(b.name)).map(item => <option key={item.experiment_id} value={item.experiment_id}>
        {experimentDisplayName(item, experiments)}</option>)}
    </select></label>}
    {draft.kind === "new" && <label className="field">Experiment name<input id="restart-experiment-name" value={draft.name} maxLength={256}
      autoComplete="off" onChange={event => on_change({ ...draft, name: event.currentTarget.value })} /></label>}
    {draft.kind === "join_run" && <>
      <div className="lifecycle-fields"><label className="field">Machine origin<input id="restart-experiment-origin" value={draft.origin} maxLength={96}
        autoComplete="off" onChange={event => on_change({ ...draft, origin: event.currentTarget.value })} /></label>
      <label className="field">Run ID<input id="restart-experiment-run-id" value={draft.run_id} maxLength={96}
        autoComplete="off" onChange={event => on_change({ ...draft, run_id: event.currentTarget.value })} /></label></div>
      <p className="muted">Join this run’s experiment. If it is ungrouped, create an experiment containing both runs.</p>
    </>}
    {draft.kind !== "ungrouped" && <p className="muted">The worker saves this choice and retries assignment when publishing is available. Each run keeps its own metrics.</p>}
  </fieldset>;
}
