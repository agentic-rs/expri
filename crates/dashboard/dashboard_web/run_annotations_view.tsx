import { useEffect, useState, type FormEvent } from "react";
import { apiUrl, errorText, parseProjectRunKey, parseRunDeepLink, runIdentity, runNickname, runTags, type Run, type Source } from "./dashboard_model";
import { MAX_RUN_NICKNAME_CHARS, MAX_RUN_TAGS, normalizeRunAnnotations, runAnnotationsScope, type RunAnnotationValues } from "./run_annotations";
import type { DashboardActions, DashboardSnapshot } from "./dashboard_view";

export function RunTags({ run }: { run: Run }) {
  const tags = runTags(run);
  return tags.length ? <ul className="run-tags" aria-label="Run tags">
    {tags.map((tag) => <li className="run-tag" key={tag}>{tag}</li>)}
  </ul> : null;
}

type AnnotationDraft = {
  nickname: string;
  tags: string[];
  tag_input: string;
  baseline: RunAnnotationValues;
  submitted: RunAnnotationValues | null;
  error: string | null;
};

function newDraft(values: RunAnnotationValues): AnnotationDraft {
  return { nickname: values.nickname ?? "", tags: values.tags, tag_input: "", baseline: values, submitted: null, error: null };
}
function valuesEqual(left: RunAnnotationValues, right: RunAnnotationValues): boolean {
  return left.nickname === right.nickname && JSON.stringify(left.tags) === JSON.stringify(right.tags);
}
function draftChanged(draft: AnnotationDraft): boolean {
  return draft.nickname !== (draft.baseline.nickname ?? "") || draft.tag_input !== "" ||
    JSON.stringify(draft.tags) !== JSON.stringify(draft.baseline.tags);
}

function RunAnnotationsEditor({ run, snapshot: s, actions: a, unavailable = false }: {
  run: Run;
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
  unavailable?: boolean;
}) {
  const committed = { nickname: runNickname(run), tags: runTags(run) };
  const committed_key = JSON.stringify(committed);
  const [draft, setDraft] = useState(() => newDraft(committed));
  useEffect(() => {
    setDraft((current) => (!draftChanged(current) ||
      (current.submitted !== null && current.tag_input === "" &&
        current.nickname === (current.submitted.nickname ?? "") &&
        JSON.stringify(current.tags) === JSON.stringify(current.submitted.tags) &&
        valuesEqual(current.submitted, committed))) ? newDraft(committed) : current);
  }, [committed_key]);
  const disabled = unavailable || !s.run_management_enabled || s.review_loading || s.annotations_busy || s.archival_busy;
  const description_id = unavailable ? "run-annotations-hint run-annotations-unavailable" : "run-annotations-hint";
  const dirty = draftChanged(draft);
  const addTag = () => {
    if (disabled) return;
    try {
      const tag = normalizeRunAnnotations(null, [draft.tag_input]).tags[0]!;
      const values = normalizeRunAnnotations(null, draft.tags.includes(tag) ? draft.tags : [...draft.tags, tag]);
      setDraft({ ...draft, tags: values.tags, tag_input: "", error: null });
    } catch (error) {
      setDraft({ ...draft, error: errorText(error) });
    }
  };
  const save = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (disabled || !dirty) return;
    try {
      let tags = draft.tags;
      if (draft.tag_input !== "") {
        const tag = normalizeRunAnnotations(null, [draft.tag_input]).tags[0]!;
        if (!tags.includes(tag)) tags = [...tags, tag];
      }
      const values = normalizeRunAnnotations(draft.nickname, tags);
      if (valuesEqual(values, committed)) {
        setDraft(newDraft(committed));
        return;
      }
      setDraft({ ...draft, nickname: values.nickname ?? "", tags: values.tags, tag_input: "", error: null, submitted: values });
      a.annotations_save(values.nickname, values.tags);
    } catch (error) {
      setDraft({ ...draft, error: errorText(error) });
    }
  };
  const summary_status = unavailable ? "Unavailable" : !s.run_management_enabled ? "Read only" :
    s.annotations_busy ? "Saving…" : (draft.error || s.annotations_error) ? "Save failed" : dirty ? "Unsaved changes" : null;
  return <details id="run-annotations-panel" className="card run-annotations-panel" aria-label="Nickname and tags" aria-busy={s.annotations_busy}>
    <summary className="annotation-summary">
      <span className="annotation-summary-label">
        <span className="annotation-summary-title">Nickname and tags</span>
        <span className="annotation-summary-description">Give this run a name and add labels.</span>
      </span>
      {summary_status && <span className={`annotation-summary-status${(draft.error || s.annotations_error) ? " error" : ""}`}>{summary_status}</span>}
      <svg className="annotation-summary-chevron" viewBox="0 0 20 20" fill="none" aria-hidden="true">
        <path d="m6 8 4 4 4-4" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
      </svg>
    </summary>
    <form onSubmit={save}>
      <fieldset disabled={disabled} className="run-annotations-fields">
        <legend className="sr-only">Edit run nickname and tags</legend>
        <label className="field" htmlFor="run-nickname">
          <span>Nickname</span>
          <input id="run-nickname" type="text" value={draft.nickname} autoComplete="off"
            aria-describedby={description_id}
            onChange={(event) => setDraft({ ...draft, nickname: event.currentTarget.value, error: null })}
            placeholder="Give this run a name" />
        </label>
        <div className="run-annotations-tags">
          <span className="field-label">Tags</span>
          <ul className="run-tags" aria-label="Editable run tags">
            {draft.tags.map((tag) => <li className="run-tag editable" key={tag}>
              <span>{tag}</span>
              <button type="button" aria-label={`Remove tag ${tag}`}
                onClick={() => setDraft({ ...draft, tags: draft.tags.filter((item) => item !== tag), error: null })}>
                <span aria-hidden="true">×</span>
              </button>
            </li>)}
          </ul>
          <div className="run-tag-add">
            <label className="field" htmlFor="run-tag-input">
              <span className="sr-only">New tag</span>
              <input id="run-tag-input" type="text" value={draft.tag_input} autoComplete="off" placeholder="Add a tag"
                aria-describedby={description_id} onChange={(event) => setDraft({ ...draft, tag_input: event.currentTarget.value, error: null })}
                onKeyDown={(event) => {
                  if (event.key === "Enter" && !event.nativeEvent.isComposing) {
                    event.preventDefault();
                    if (draft.tag_input) addTag();
                  }
                }} />
            </label>
            <button id="add-run-tag" className="button secondary" type="button" disabled={!draft.tag_input} onClick={addTag}>Add tag</button>
          </div>
        </div>
        <p id="run-annotations-hint" className="muted">Optional nickname, up to {MAX_RUN_NICKNAME_CHARS} characters. Up to {MAX_RUN_TAGS} tags, 48 characters each.</p>
        <div className="run-annotations-actions">
          <button id="save-run-annotations" type="submit" className="button primary" disabled={!dirty}>{s.annotations_busy ? "Saving…" : "Save"}</button>
          <button id="cancel-run-annotations" type="button" className="text-button" disabled={!dirty} onClick={() => setDraft(newDraft(committed))}>Cancel</button>
        </div>
      </fieldset>
    </form>
    {(draft.error || s.annotations_error) && <p id="run-annotations-error" className="notice error" role="alert">{draft.error || s.annotations_error}</p>}
    {unavailable && <p id="run-annotations-unavailable" className="muted">Nickname and tags are unavailable on this server. The server needs an upgrade.</p>}
    {!unavailable && !s.run_management_enabled && <p className="muted">Run management is disabled on this dashboard.</p>}
  </details>;
}

function annotationsUnavailable(run: Run, source: Source | undefined): boolean {
  if (run.annotations !== undefined || !source || !source.project_id) return false;
  const identity = source.kind === "hosted_project" ? parseProjectRunKey(run.run_key ?? "") :
    source.kind === "service" && source.origin ? { origin: source.origin, run_id: run.run_id } : null;
  if (!identity || identity.run_id !== run.run_id || (run.origin !== undefined && run.origin !== identity.origin)) return false;
  return parseRunDeepLink(apiUrl("", { project_id: source.project_id, ...identity })) !== null;
}

export function RunAnnotationsPanel({ snapshot: s, actions: a }: {
  snapshot: DashboardSnapshot;
  actions: DashboardActions;
}) {
  const run = s.detail?.run;
  const source = s.sources.find((item) => item.source_id === s.source_id);
  if (s.access_mode !== "hosted" || s.review?.kind !== "run" || !run ||
    s.review.run_ids[0] !== runIdentity(run) || s.detail?.source.source_id !== s.source_id) return null;
  const unavailable = annotationsUnavailable(run, source);
  if (!runAnnotationsScope(run, source) && !unavailable) return null;
  return <RunAnnotationsEditor key={`${s.source_id}:${runIdentity(run)}`} run={run} snapshot={s} actions={a} unavailable={unavailable} />;
}
