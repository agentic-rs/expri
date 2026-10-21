import { useEffect, useState, type FormEvent } from "react";
import { apiUrl, errorText, parseProjectRunKey, parseRunDeepLink, runIdentity, type RunDeepLink } from "./dashboard_model";
import { RunControlPopover } from "./run_control_popover";
import { checkpointStep, experimentDisplayName, experimentProject, experimentRunScope, memberExperiment, removeExperimentMember,
  sameScope, scopeIdentity, type Experiment, type ExperimentDraft, type ExperimentGroup, type ExperimentLineage, type LineageKind } from "./experiments";
import type { DashboardActions, DashboardSnapshot } from "./dashboard_view";

const LINEAGE_LABELS: Record<LineageKind, string> = { continued: "Continued from", restarted: "Restarted from", split: "Split from" };
function memberLabel(member: RunDeepLink): string { return `${member.run_id} · ${member.origin}`; }
function MemberLink({ member }: { member: RunDeepLink }) {
  return <a href={apiUrl("/", member)} title={`${member.project_id} · ${member.origin} · ${member.run_id}`}>{memberLabel(member)}</a>;
}
function draftOf(experiment: Experiment): ExperimentDraft {
  return { name: experiment.name, description: experiment.description, members: experiment.members, lineage: experiment.lineage };
}
export function ExperimentRecordedHistory({ experiment, draft, on_remove }: {
  experiment: Experiment; draft?: ExperimentDraft; on_remove?: (member: RunDeepLink) => void;
}) {
  const [open, setOpen] = useState(false);
  const members = draft?.members ?? experiment.members, lineage = draft?.lineage ?? experiment.lineage;
  return <details className="experiment-recorded-history" onToggle={(event) => setOpen(event.currentTarget.open)}>
    <summary>Members and lineage · {members.length}</summary>
    {open && <>
      {on_remove && <p className="muted">Removing a member also removes its lineage relationships when you save. The run and metrics remain.</p>}
      <ul>{members.map((member) => {
        const unavailable = experiment.unavailable_runs.some((missing) => sameScope(missing, member));
        const incoming = lineage.filter((edge) => sameScope(edge.child, member));
        return <li key={scopeIdentity(member)}><div className="experiment-recorded-member"><span title={`${member.project_id} · ${member.origin} · ${member.run_id}`}>
          {unavailable ? <span>{memberLabel(member)}</span> : <MemberLink member={member} />}{unavailable && <small>Unavailable · provenance retained</small>}
        </span>{on_remove && <button type="button" className="text-button" aria-label={`Remove member ${memberLabel(member)}`} onClick={() => on_remove(member)}>Remove</button>}</div>
          {incoming.length > 0 && <ul className="experiment-lineage-list">{incoming.map((edge, index) => <li key={index}><span>{LINEAGE_LABELS[edge.kind]} {memberLabel(edge.parent)}</span>{edge.step !== null && <small>Checkpoint step {edge.step}</small>}{edge.checkpoint && <code>{edge.checkpoint}</code>}</li>)}</ul>}
        </li>;
      })}</ul>
      {!members.length && <p className="muted">No members yet.</p>}
    </>}
  </details>;
}

export function ExperimentFilter({ snapshot: s, actions: a }: { snapshot: DashboardSnapshot; actions: DashboardActions }) {
  const source = s.sources.find((item) => item.source_id === s.source_id);
  const state = s.experiments;
  if (!state.supported || !experimentProject(source)) return null;
  const choices = [...(state.catalog?.experiments ?? [])].sort((first, second) => first.name.localeCompare(second.name));
  return <label className="field experiment-filter">
    Experiment
    <select id="experiment-filter" value={state.filter} disabled={s.controls_disabled || state.loading || !state.catalog} onChange={(event) => a.experiment_filter(event.currentTarget.value)}>
      <option value="">All experiments</option>
      <option value="ungrouped">Ungrouped runs</option>
      {choices.map((item) => <option value={item.experiment_id} key={item.experiment_id} title={`${item.name} · ${item.experiment_id}`}>{experimentDisplayName(item, choices)} · {item.membership_count}</option>)}
    </select>
  </label>;
}

export function ExperimentViewControl({ snapshot: s, actions: a }: { snapshot: DashboardSnapshot; actions: DashboardActions }) {
  if (!s.experiments.groups_supported) return null;
  return <fieldset className="run-history-view" disabled={s.controls_disabled || s.list_busy}>
    <legend className="sr-only">Run history view</legend>
    {([['experiments', 'Experiments'], ['runs', 'Runs']] as const).map(([value, label]) => <label className="history-view-choice" key={value}>
      <input id={`history-view-${value}`} className="sr-only" type="radio" name="run_history_view" value={value}
        checked={s.experiments.view === value} onChange={() => a.experiment_view(value)} />
      <span>{label}</span>
    </label>)}
  </fieldset>;
}

export function ExperimentGroupLabel({ group, snapshot: s, actions: a }: {
  group: ExperimentGroup; snapshot: DashboardSnapshot; actions: DashboardActions;
}) {
  const experiment = group.experiment;
  if (!experiment) return null;
  const expanded = s.experiments.expanded.includes(experiment.experiment_id);
  const name = experimentDisplayName(experiment, s.experiments.catalog?.experiments ?? s.experiments.groups.flatMap((group) => group.experiment ? [group.experiment] : []));
  const counts = `${group.membership_count} retained runs · ${group.matching_count} matching · ${group.active_count} active matching · ${group.unavailable_count} unavailable`;
  return <div className="experiment-group-label">
    <button type="button" className="experiment-expand" disabled={s.list_busy} title={`${experiment.name} · ${experiment.experiment_id}`}
      aria-label={`${expanded ? "Collapse" : "Expand"} experiment ${name}`} aria-expanded={expanded}
      onClick={() => a.experiment_expand(experiment.experiment_id, !expanded)}>
      <svg viewBox="0 0 12 12" aria-hidden="true"><path d="m4 2 4 4-4 4" /></svg><strong>{name}</strong>
    </button>
    <span className="experiment-member-count" title={counts} aria-label={`${group.matching_count} matching runs of ${group.membership_count} retained runs`}>
      {group.matching_count === group.membership_count ? group.membership_count : `${group.matching_count}/${group.membership_count}`}
    </span>
    {group.active_count > 0 && <span className="experiment-active-count" title={`${group.active_count} active matching ${group.active_count === 1 ? "run" : "runs"}`} aria-label={`${group.active_count} active matching runs`}><span aria-hidden="true">●</span>{group.active_count}</span>}
  </div>;
}

export function ExperimentHistoryFooter({ group, shown, snapshot: s, actions: a, columns }: {
  group: ExperimentGroup; shown: number; snapshot: DashboardSnapshot; actions: DashboardActions; columns: number;
}) {
  const experiment = group.experiment;
  if (!experiment) return null;
  const history = s.experiments.histories[experiment.experiment_id];
  return <tr className="experiment-history-footer" data-experiment-history={experiment.experiment_id}><td colSpan={columns}>
    <div className="experiment-history-controls">
      <span className="muted">{shown} matching {shown === 1 ? "run" : "runs"} shown</span>
      {(history?.loading ?? true) && <span className="muted" role="status">Loading history…</span>}
      {history?.error && <><span className="experiment-history-error" role="alert">{history.error}</span><button id={`experiment-retry-${experiment.experiment_id}`} type="button" className="text-button" disabled={history.loading || s.list_busy} onClick={() => a.experiment_more(experiment.experiment_id)}>Retry</button></>}
      {history?.next_offset !== null && history?.next_offset !== undefined && !history.error && <button id={`experiment-more-${experiment.experiment_id}`} type="button" className="text-button" disabled={history.loading || s.list_busy} onClick={() => a.experiment_more(experiment.experiment_id)}>Load more runs</button>}
    </div>
  </td></tr>;
}

export function ExperimentManager({ snapshot: s, actions: a, open, on_change }: {
  snapshot: DashboardSnapshot; actions: DashboardActions; open: boolean; on_change: (open: boolean) => void;
}) {
  const [editing, setEditing] = useState<string | null | undefined>(undefined);
  const [draft, setDraft] = useState<ExperimentDraft>({ name: "", description: "", members: [], lineage: [] });
  const [revision, setRevision] = useState<string | null>(null);
  const [confirm_delete, setConfirmDelete] = useState(false);
  const [add_selected, setAddSelected] = useState(false);
  const [create_request_id, setCreateRequestId] = useState<string | undefined>();
  const source = s.sources.find((item) => item.source_id === s.source_id);
  const project_id = experimentProject(source), state = s.experiments;
  useEffect(() => setEditing(undefined), [s.source_id]);
  if (!state.supported || !project_id) return null;
  const catalog = state.catalog;
  const choices = [...(catalog?.experiments ?? [])].sort((a, b) => a.name.localeCompare(b.name));
  const editable = state.management_enabled && catalog !== null && !state.loading && !state.busy;
  const latest = choices.find((item) => item.experiment_id === editing);
  const stale = editing !== null && editing !== undefined && latest?.revision !== revision;
  const selected_members = s.selected.flatMap((identity): RunDeepLink[] => {
    const run = s.runs.find((item) => runIdentity(item) === identity);
    const member = run ? experimentRunScope(run, source) : source?.kind === "hosted_project" ? (() => {
      const parsed = parseProjectRunKey(identity); return parsed ? { project_id, ...parsed } : null;
    })() : source?.kind === "service" && source.origin ? { project_id, origin: source.origin, run_id: identity } : null;
    return member && parseRunDeepLink(apiUrl("", member)) ? [member] : [];
  });
  const save = async (event: FormEvent) => {
    event.preventDefault();
    if (!editable || editing === undefined || stale) return;
    const members = add_selected ? [...new Map([...draft.members, ...selected_members].map((member) => [scopeIdentity(member), member])).values()] : draft.members;
    if (await a.experiment_save(editing, { ...draft, members }, revision, editing === null ? create_request_id : undefined)) { setEditing(undefined); setConfirmDelete(false); }
  };
  return <RunControlPopover id="experiment-manager" label="Experiments" icon="runs" count={Number(state.error !== null)}
    open={open} disabled={s.controls_disabled} on_change={(value) => { on_change(value); if (value) a.experiments_refresh(); }}>
      <div className="experiment-manager">
        <div className="experiment-manager-heading"><p className="muted">Group related runs across machines.</p><button className="text-button" type="button" disabled={!editable || choices.length >= 128} onClick={() => {
          setEditing(null); setCreateRequestId(crypto.randomUUID()); setDraft({ name: "", description: "", members: [], lineage: [] }); setRevision(null); setConfirmDelete(false); setAddSelected(selected_members.length > 0);
        }}>+ New</button></div>
        {state.loading && <p className="muted" role="status">Updating experiments…</p>}
        {!state.management_enabled && <p className="muted">Experiment management is read only on this dashboard.</p>}
        <ul className="experiment-chooser" aria-label="Experiments">
          {choices.map((item) => <li key={item.experiment_id}>
            <div className="experiment-choice-row">
            <button type="button" className="experiment-choice" title={`${item.name} · ${item.experiment_id}`} onClick={() => { a.experiment_filter(item.experiment_id); on_change(false); }}>
              <span>{experimentDisplayName(item, choices)}</span><small>{item.membership_count} {item.membership_count === 1 ? "member" : "members"}{item.unavailable_runs.length ? ` · ${item.unavailable_runs.length} unavailable` : ""}</small>
            </button>
            <button type="button" className="text-button" disabled={!editable} aria-label={`Edit experiment ${item.name}`} onClick={() => {
              setEditing(item.experiment_id); setDraft(draftOf(item)); setRevision(item.revision); setConfirmDelete(false); setAddSelected(false);
            }}>Edit</button>
            </div>
            <ExperimentRecordedHistory experiment={item} />
          </li>)}
        </ul>
        {!state.loading && !choices.length && <p className="muted">No experiments yet. Create one, then add existing runs.</p>}
        {editing !== undefined && <form className="experiment-edit-form" onSubmit={(event) => { void save(event); }}>
          <h4>{editing === null ? "New experiment" : "Edit experiment"}</h4>
          {stale && <p className="notice">This experiment changed while you were editing. <button className="text-button" type="button" onClick={() => {
            if (latest) { setDraft(draftOf(latest)); setRevision(latest.revision); setConfirmDelete(false); }
            else setEditing(undefined);
          }}>Reload latest</button></p>}
          <fieldset disabled={!editable}>
            <label className="field">Name<input id="experiment-name" value={draft.name} maxLength={256} required autoComplete="off" onChange={(event) => setDraft({ ...draft, name: event.currentTarget.value })} /></label>
            <label className="field">Description<textarea id="experiment-description" rows={3} value={draft.description} onChange={(event) => setDraft({ ...draft, description: event.currentTarget.value })} /></label>
            {latest && <ExperimentRecordedHistory experiment={latest} draft={draft} on_remove={(member) => setDraft({ ...draft,
              members: draft.members.filter((item) => !sameScope(item, member)),
              lineage: draft.lineage.filter((edge) => !sameScope(edge.parent, member) && !sameScope(edge.child, member)) })} />}
            {selected_members.length > 0 && <label className="experiment-checkbox"><input type="checkbox" checked={add_selected} onChange={(event) => setAddSelected(event.currentTarget.checked)} />Add {selected_members.length} selected {selected_members.length === 1 ? "run" : "runs"}</label>}
            {add_selected && <p className="muted">Runs already in another experiment must be unassigned first.</p>}
            <div className="experiment-form-actions"><button id="save-experiment" className="button primary" type="submit" disabled={stale}>{state.busy ? "Saving…" : "Save"}</button><button type="button" className="text-button" onClick={() => setEditing(undefined)}>Cancel</button></div>
            {editing !== null && <div className="experiment-delete">
              {!confirm_delete ? <button type="button" className="text-button" onClick={() => setConfirmDelete(true)}>Remove experiment…</button> : <>
                <p>Remove this grouping and its lineage? All runs, attempts, and metrics will remain.</p>
                <div className="experiment-form-actions"><button id="delete-experiment" type="button" className="button danger" onClick={() => { void a.experiment_delete(editing, revision!).then((saved) => { if (saved) setEditing(undefined); }); }}>Remove grouping</button><button className="text-button" type="button" onClick={() => setConfirmDelete(false)}>Keep experiment</button></div>
              </>}
            </div>}
          </fieldset>
        </form>}
        {state.error && <p className="notice error" role="alert">{state.error}</p>}
      </div>
    </RunControlPopover>;
}

export function ExperimentRunPanel({ snapshot: s, actions: a }: { snapshot: DashboardSnapshot; actions: DashboardActions }) {
  const run = s.detail?.run, source = s.sources.find((item) => item.source_id === s.source_id);
  const member = run ? experimentRunScope(run, source) : null;
  if (!s.experiments.supported || !member || !run || s.review?.kind !== "run" ||
    s.review.run_ids[0] !== runIdentity(run) || s.detail?.source.source_id !== s.source_id) return null;
  return <ExperimentRunEditor key={`${s.source_id}:${runIdentity(run)}`} member={member} snapshot={s} actions={a} />;
}
function ExperimentRunEditor({ member, snapshot: s, actions: a }: { member: RunDeepLink; snapshot: DashboardSnapshot; actions: DashboardActions }) {
  const state = s.experiments, catalog = state.catalog;
  const experiment = memberExperiment(catalog, member);
  const [target, setTarget] = useState("");
  const [editing, setEditing] = useState(false);
  const [kind, setKind] = useState<LineageKind>("continued");
  const [parent, setParent] = useState("");
  const [checkpoint, setCheckpoint] = useState("");
  const [step, setStep] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [confirm_unassign, setConfirmUnassign] = useState(false);
  const [baseline, setBaseline] = useState<string | null>(null);
  const [draft_edges, setDraftEdges] = useState<ExperimentLineage[]>([]);
  const edges = experiment?.lineage.filter((edge) => sameScope(edge.child, member)) ?? [];
  const outgoing = experiment?.lineage.filter((edge) => sameScope(edge.parent, member)) ?? [];
  const disabled = !state.management_enabled || !catalog || state.loading || state.busy || s.review_loading;
  const stale = editing && experiment?.revision !== baseline;
  useEffect(() => { setEditing(false); setConfirmUnassign(false); setError(null); }, [experiment?.experiment_id]);
  const beginEdit = () => {
    setEditing(true); setBaseline(experiment!.revision); setDraftEdges(edges); setParent(""); setCheckpoint(""); setStep(""); setError(null);
  };
  const saveLineage = async (event: FormEvent) => {
    event.preventDefault();
    if (!experiment || disabled || stale) return;
    try {
      let lineage = draft_edges;
      if (parent) {
        const selected = experiment.members.find((item) => scopeIdentity(item) === parent);
        if (!selected || sameScope(selected, member)) throw new Error("Choose a different parent run in this experiment.");
        lineage = [...lineage, { parent: selected, child: member, kind, checkpoint: checkpoint.trim() || null, step: checkpointStep(step) }];
      } else if (checkpoint.trim() || step.trim()) throw new Error("Choose a parent run for the checkpoint information.");
      const values = { ...draftOf(experiment), lineage: [...experiment.lineage.filter((edge) => !sameScope(edge.child, member)), ...lineage] };
      if (await a.experiment_save(experiment.experiment_id, values, baseline)) setEditing(false);
    } catch (error) { setError(errorText(error)); }
  };
  return <details id="run-experiment-panel" className="card run-annotations-panel experiment-run-panel">
    <summary className="annotation-summary"><span className="annotation-summary-label"><span className="annotation-summary-title">{experiment?.name ?? "Experiment and lineage"}</span><span className="annotation-summary-description">{experiment ? `${experiment.membership_count} members · ${edges.length} incoming relationships` : "Group related work across machines."}</span></span>{!state.management_enabled && <span className="annotation-summary-status">Read only</span>}<span aria-hidden="true">⌄</span></summary>
    <div className="experiment-run-content">
      {experiment ? <>
        <div className="experiment-run-heading"><button type="button" className="text-button" onClick={() => a.experiment_filter(experiment.experiment_id)}>View experiment ↗</button><span className="muted">{experiment.membership_count} members{experiment.unavailable_runs.length ? ` · ${experiment.unavailable_runs.length} unavailable` : ""}</span></div>
        {experiment.description && <p className="experiment-description">{experiment.description}</p>}
        {!editing && <>
          <ul className="experiment-lineage-list" aria-label="Run lineage">{edges.map((edge, index) => <li key={index}><span>{LINEAGE_LABELS[edge.kind]}</span> <MemberLink member={edge.parent} />{edge.step !== null && <small>Checkpoint step {edge.step}</small>}{edge.checkpoint && <code>{edge.checkpoint}</code>}</li>)}</ul>
          {!edges.length && <p className="muted">No incoming lineage recorded. This run’s metrics remain independent.</p>}
          {outgoing.length > 0 && <details className="experiment-descendants"><summary>{outgoing.length} related {outgoing.length === 1 ? "child run" : "child runs"}</summary><ul>{outgoing.map((edge, index) => <li key={index}><span>{edge.kind === "continued" ? "Continued as" : edge.kind === "restarted" ? "Restarted as" : "Split into"}</span> <MemberLink member={edge.child} /></li>)}</ul></details>}
          <div className="experiment-form-actions"><button type="button" className="button secondary" disabled={disabled} onClick={beginEdit}>Edit lineage</button><button type="button" className="text-button" disabled={disabled} onClick={() => { setConfirmUnassign(true); setBaseline(experiment.revision); }}>Unassign…</button></div>
        </>}
        {confirm_unassign && <div className="experiment-confirm"><p>Remove this run from the experiment{edges.length + outgoing.length ? ` and its ${edges.length + outgoing.length} lineage relationships` : ""}? Its attempts and metrics will remain.</p><div className="experiment-form-actions"><button type="button" className="button danger" disabled={disabled} onClick={() => { void a.experiment_save(experiment.experiment_id, removeExperimentMember(experiment, member), baseline).then((saved) => { if (saved) setConfirmUnassign(false); }); }}>Unassign run</button><button type="button" className="text-button" onClick={() => setConfirmUnassign(false)}>Keep in experiment</button></div></div>}
        {editing && <form className="experiment-lineage-form" onSubmit={(event) => { void saveLineage(event); }}>
          {stale && <p className="notice">This experiment changed while you were editing. <button className="text-button" type="button" onClick={beginEdit}>Reload latest</button></p>}
          <fieldset disabled={disabled}>
          <ul className="experiment-lineage-list">{draft_edges.map((edge, index) => <li key={index}><span>{LINEAGE_LABELS[edge.kind]} {memberLabel(edge.parent)}</span><button type="button" className="text-button" aria-label={`Remove relationship from ${memberLabel(edge.parent)}`} onClick={() => setDraftEdges(draft_edges.filter((_, edge_index) => edge_index !== index))}>Remove</button></li>)}</ul>
          <label className="field">Parent run<select id="experiment-lineage-parent" value={parent} onChange={(event) => setParent(event.currentTarget.value)}><option value="">No new relationship</option>{experiment.members.filter((item) => !sameScope(item, member)).map((item) => <option value={scopeIdentity(item)} key={scopeIdentity(item)}>{memberLabel(item)}{experiment.unavailable_runs.some((missing) => sameScope(missing, item)) ? " · unavailable" : ""}</option>)}</select></label>
          <fieldset className="choice-group" disabled={!parent}><legend>Relationship</legend><div className="choice-tags">{([['continued', 'Continue'], ['restarted', 'Restart'], ['split', 'Split']] as const).map(([value, label]) => <label className="choice-tag" key={value}><input className="sr-only" type="radio" name="lineage_kind" value={value} checked={kind === value} onChange={() => setKind(value)} /><span>{label}</span></label>)}</div></fieldset>
          <div className="experiment-checkpoint-fields"><label className="field">Parent checkpoint step<input id="experiment-checkpoint-step" value={step} disabled={!parent} inputMode="numeric" placeholder="Optional" onChange={(event) => setStep(event.currentTarget.value)} /></label><label className="field">Checkpoint reference<input id="experiment-checkpoint-reference" value={checkpoint} disabled={!parent} placeholder="Optional file or label" onChange={(event) => setCheckpoint(event.currentTarget.value)} /></label></div>
          <p className="muted">Records provenance only. It does not launch a task or join metric histories.</p>
          <div className="experiment-form-actions"><button id="save-experiment-lineage" type="submit" className="button primary" disabled={stale}>Save lineage</button><button type="button" className="text-button" onClick={() => setEditing(false)}>Cancel</button></div>
        </fieldset></form>}
      </> : <form onSubmit={(event) => {
        event.preventDefault(); const chosen = catalog?.experiments.find((item) => item.experiment_id === target);
        if (!chosen || disabled) return;
        void a.experiment_save(chosen.experiment_id, { ...draftOf(chosen), members: [...chosen.members, member] }, chosen.revision);
      }}><fieldset disabled={disabled}><label className="field">Experiment<select id="run-experiment-choice" value={target} onChange={(event) => setTarget(event.currentTarget.value)}><option value="">Choose an experiment</option>{catalog?.experiments.map((item) => <option value={item.experiment_id} key={item.experiment_id} title={`${item.name} · ${item.experiment_id}`}>{experimentDisplayName(item, catalog.experiments)}</option>)}</select></label><div className="experiment-form-actions"><button id="assign-run-experiment" className="button primary" type="submit" disabled={!target}>Add run</button></div><p className="muted">Create an experiment from Experiments in the run history header.</p></fieldset></form>}
      {(error || state.error) && <p className="notice error" role="alert">{error || state.error}</p>}
      {state.loading && <p className="muted" role="status">Updating experiments…</p>}
    </div>
  </details>;
}
