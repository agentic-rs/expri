import { useEffect, useRef, useState, type FormEvent } from "react";
import { apiUrl, dateText, errorText, jsonObject, parseRunDeepLink, projectRunKey, RequestError, RequestLane, runIdentity, runStatusClass, type Detail, type Run, type Source } from "./dashboard_model";
import { lifecycleActions, lifecyclePending, lifecycleScope, parseLifecycleArguments, parseLifecycleReason, parseRunLifecycle, type RunLifecycle, type RunLifecycleAction } from "./run_lifecycle";

const LABELS: Record<RunLifecycleAction, string> = { pause: "Pause", resume: "Resume", stop: "Stop", continue: "Continue", restart: "Restart" };
type Draft = { action: "stop" | "continue" | "restart"; arguments: string; checkpoint: string; reason: string; grace_seconds: number;
  expected_attempt: number; expected_generation: number; binding_id: string };

export function RunLifecyclePanel({ run, source, detail, enabled, visible, on_applied, on_open_run }: {
  run: Run;
  source: Source | undefined;
  detail: Detail | null;
  enabled: boolean;
  visible: boolean;
  on_applied: () => void;
  on_open_run: (id: string) => void;
}) {
  const scope = lifecycleScope(run, source);
  const scope_key = scope ? JSON.stringify(scope) : "";
  const queued = run.queue != null || run.queue_only === true;
  const [control, setControl] = useState<RunLifecycle | null>(null);
  const [loading, setLoading] = useState(false);
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [checkpoint_search, setCheckpointSearch] = useState("");
  const lane = useRef(new RequestLane());
  const posting = useRef(false);
  const alive = useRef(false);
  const applied = useRef<string | null>(null);
  const initialized = useRef(false);
  const submission = useRef<{ key: string; request_id: string } | null>(null);
  const callbacks = useRef({ on_applied, on_open_run });
  callbacks.current = { on_applied, on_open_run };

  const accept = (value: unknown) => {
    const next = parseRunLifecycle(value);
    setControl(next);
    setUnavailable(false);
    setError(null);
    if (next.request?.status === "applied" && applied.current !== next.request.request_id) {
      applied.current = next.request.request_id;
      setDraft(null);
      if (initialized.current || next.request.request_id === submission.current?.request_id) callbacks.current.on_applied();
    }
    initialized.current = true;
    if (next.request && !lifecyclePending(next.request) && next.request.request_id === submission.current?.request_id) submission.current = null;
    return next;
  };

  useEffect(() => {
    alive.current = true;
    if (!scope || queued || !visible) return () => { alive.current = false; };
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const schedule = () => {
      if (timer !== null) clearTimeout(timer);
      if (!stopped) timer = setTimeout(() => void load(), 2000);
    };
    const load = async () => {
      timer = null;
      if (stopped) return;
      if (document.visibilityState === "hidden" || posting.current) { schedule(); return; }
      let repeat = true;
      try {
        const value = await lane.current.run<unknown>(apiUrl("/api/runs/control", scope));
        if (value === undefined || stopped) return;
        const next = accept(value);
        repeat = next.supported;
      } catch (failure) {
        if (stopped) return;
        if (failure instanceof RequestError && [404, 405, 501].includes(failure.status)) { setUnavailable(true); repeat = false; }
        else setError(errorText(failure));
      } finally {
        if (!stopped) { setLoading(false); if (repeat) schedule(); }
      }
    };
    const visibilityChanged = () => {
      if (document.visibilityState !== "hidden" && timer === null) void load();
    };
    setLoading(true);
    void load();
    document.addEventListener("visibilitychange", visibilityChanged);
    return () => {
      stopped = true;
      alive.current = false;
      if (timer !== null) clearTimeout(timer);
      lane.current.cancel();
      document.removeEventListener("visibilitychange", visibilityChanged);
    };
  }, [scope_key, queued, visible]);

  const pending = lifecyclePending(control?.request);
  const available = control && control.supported ? lifecycleActions(control) : [];
  const disabled = !enabled || !visible || sending || pending || !control?.online;
  const send = async (action: RunLifecycleAction, form: Draft | null = null) => {
    if (disabled || posting.current || !scope || !control || !available.includes(action)) return;
    let arguments_value: string[] | null = null;
    try {
      if (form && (form.expected_attempt !== control.attempt || form.expected_generation !== control.generation || form.binding_id !== control.binding_id)) {
        throw new Error("The task changed while this form was open. Cancel and reopen the action to review its current state.");
      }
      if (action === "continue" || action === "restart") {
        arguments_value = parseLifecycleArguments(form?.arguments ?? "[]");
        if (control.command.length === 0) throw new Error("The recorded command is unavailable. Upgrade the worker service before trying again.");
      }
      if (action === "continue" && (!form?.checkpoint || !control.checkpoints.includes(form.checkpoint))) throw new Error("Choose a finalized checkpoint.");
      if (form && (!Number.isInteger(form.grace_seconds) || form.grace_seconds < 1 || form.grace_seconds > 3600)) throw new Error("Grace period must be between 1 and 3,600 seconds.");
      const body = {
        ...scope, expected_attempt: control.attempt,
        expected_generation: control.generation, binding_id: control.binding_id, action,
        arguments: arguments_value, checkpoint: action === "continue" ? form!.checkpoint : null,
        reason: parseLifecycleReason(form?.reason ?? ""), grace_seconds: form?.grace_seconds ?? 30,
      };
      const key = JSON.stringify(body);
      if (submission.current?.key !== key) submission.current = { key, request_id: crypto.randomUUID() };
      const payload = { ...body, request_id: submission.current.request_id };
      if (new TextEncoder().encode(JSON.stringify(payload)).length > 48 * 1024) throw new Error("The encoded lifecycle request exceeds 48 KiB. Use shorter task arguments.");
      posting.current = true;
      setSending(true);
      setError(null);
      const result = await lane.current.post<unknown>("/api/runs/control", payload);
      if (result !== undefined && alive.current) accept(result);
    } catch (failure) {
      if (alive.current) setError(errorText(failure));
    } finally {
      posting.current = false;
      if (alive.current) setSending(false);
    }
  };
  const choose = (action: RunLifecycleAction) => {
    if (disabled) return;
    if (action === "pause" || action === "resume") { void send(action); return; }
    setError(null);
    setDraft({ action, arguments: JSON.stringify(control?.command.slice(1) ?? [], null, 2),
      checkpoint: control?.checkpoints[0] ?? "", reason: "", grace_seconds: 30,
      expected_attempt: control!.attempt, expected_generation: control!.generation, binding_id: control!.binding_id });
  };
  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (draft) void send(draft.action, draft);
  };
  const request = control?.request;
  const current_detail = detail && detail.source.source_id === source?.source_id && detail.run.run_id === run.run_id &&
    runIdentity(detail.run) === runIdentity(run) && (!scope || detail.run.origin === undefined || detail.run.origin === scope.origin) ? detail : null;
  const source_record = current_detail ? jsonObject(current_detail.snapshot) : {};
  const source_label = typeof source_record.git_head === "string" ? source_record.git_head :
    typeof source_record.commit === "string" ? source_record.commit : "Recorded source snapshot";
  const explanation = queued ? "Website lifecycle controls currently support direct runs. Queued runs remain managed by the scheduler." :
    !scope ? "Website lifecycle controls require a published run and a connected Linux worker service." :
      unavailable ? "Lifecycle controls are unavailable on this server. Upgrade the server and worker service." :
        control?.supported === false ? "This run has no supported worker binding. Lifecycle controls require a current native Linux worker service." : null;
  const paused = (control?.status ?? run.status) === "paused";
  const new_id = request?.status === "applied" ? request.new_run_id : null;
  const new_key = new_id && scope ? source?.kind === "hosted_project" ? projectRunKey(scope.origin, new_id) : new_id : null;
  const restarted_from = current_detail ? jsonObject(current_detail.state).restarted_from : null;
  const parent_scope = scope && typeof restarted_from === "string" && restarted_from !== run.run_id
    ? parseRunDeepLink(apiUrl("", { ...scope, run_id: restarted_from })) : null;
  const parent_key = parent_scope ? source?.kind === "hosted_project"
    ? projectRunKey(parent_scope.origin, parent_scope.run_id) : parent_scope.run_id : null;
  const checkpoint_matches = control?.checkpoints.filter(checkpoint => checkpoint.toLocaleLowerCase().includes(checkpoint_search.toLocaleLowerCase())) ?? [];
  const checkpoint_choices = checkpoint_matches.slice(0, 100);
  return <section id="run-lifecycle-panel" className="run-lifecycle" aria-label="Run lifecycle" aria-busy={sending || pending}>
    <div className="lifecycle-bar">
      <span className="lifecycle-label">Task controls</span>
      {control?.supported && control.status !== run.status && <span id="run-lifecycle-task-status" className={`status ${runStatusClass(control.status)}`}
        title={`Worker reported ${dateText(new Date(control.last_seen_at_ms).toISOString())}`}>
        {control.online ? "Task" : "Last worker state:"} {control.status}
      </span>}
      {!explanation && loading && !control && <span className="muted">Checking worker…</span>}
      {!explanation && available.map(action => <button key={action} id={`run-lifecycle-${action}`} type="button"
        className={`button ${action === "stop" ? "lifecycle-stop" : "secondary"}`} disabled={disabled}
        onClick={() => choose(action)}>{LABELS[action]}</button>)}
      {scope && !queued && !enabled && <span className="muted">Read only on this dashboard</span>}
      {control?.supported && !control.online && <span className="lifecycle-offline" title={`Last seen ${dateText(new Date(control.last_seen_at_ms).toISOString())}`}>Worker offline</span>}
      {request && <span className={`lifecycle-request ${request.status === "failed" || request.status === "expired" ? "error" : "muted"}`} role="status">
        {LABELS[request.action]} {request.status === "requested" ? "requested · waiting for worker" : request.status}
      </span>}
      {new_key && new_id && <button id="run-lifecycle-open-result" type="button" className="text-button"
        onClick={() => callbacks.current.on_open_run(new_key)}>Open new run <code>{new_id}</code></button>}
      {parent_key && parent_scope && <button id="run-lifecycle-open-parent" type="button" className="text-button"
        onClick={() => callbacks.current.on_open_run(parent_key)}>Restarted from <code>{parent_scope.run_id}</code></button>}
    </div>
    {explanation && <p className="muted lifecycle-note">{explanation}</p>}
    {paused && <p className="muted lifecycle-note">Task processes are suspended in memory. Allocated resources are retained; elapsed time includes this pause. A worker reboot ends the in-memory pause.</p>}
    {request?.error && <p className="notice error" role="alert">{request.error}</p>}
    {error && <p id="run-lifecycle-error" className="notice error" role="alert">{error}</p>}
    {draft && <form id="run-lifecycle-form" className="lifecycle-form" onSubmit={submit}>
      <h3>{LABELS[draft.action]} task</h3>
      <p className="muted">{draft.action === "stop" ? "Request graceful termination. Outputs and checkpoints are retained, and this attempt ends as cancelled." :
        draft.action === "continue" ? "Start another attempt from a checkpoint, retaining this run ID and all metric history." :
          "Start the recorded task from scratch in a new linked run with fresh metrics."}</p>
      {draft.action !== "stop" && ["running", "waiting", "paused"].includes(control?.status ?? run.status) &&
        <p className="lifecycle-replacement">The current task will stop after the worker validates the replacement.</p>}
      {draft.action !== "stop" && <>
        <dl className="lifecycle-recorded"><dt>Task</dt><dd>{run.task ?? "Not recorded"}</dd><dt>Source</dt><dd><code>{source_label}</code></dd>
          <dt>Executable</dt><dd><code>{control?.command[0] ?? "Not recorded"}</code></dd></dl>
        {draft.action === "continue" && <fieldset className="lifecycle-checkpoints" disabled={disabled}>
          <legend>Finalized checkpoint</legend>
          {(control?.checkpoints.length ?? 0) > 8 && <label className="field" htmlFor="run-lifecycle-checkpoint-search"><span className="sr-only">Find checkpoint</span>
            <input id="run-lifecycle-checkpoint-search" type="search" value={checkpoint_search} placeholder="Find checkpoint"
              onChange={event => setCheckpointSearch(event.currentTarget.value)} /></label>}
          {control?.checkpoints.length ? <div className="lifecycle-checkpoint-list">{checkpoint_choices.map(checkpoint => <label className="choice-tag" key={checkpoint}>
            <input className="sr-only" type="radio" name="lifecycle-checkpoint" value={checkpoint} checked={draft.checkpoint === checkpoint}
              onChange={() => setDraft({ ...draft, checkpoint })} /><span>{checkpoint}</span>
          </label>)}</div> : <p className="muted">No finalized checkpoints are available. Record a checkpoint before continuing.</p>}
          {checkpoint_matches.length > checkpoint_choices.length && <p className="muted">Showing {checkpoint_choices.length} of {checkpoint_matches.length} matching checkpoints. Refine the search to find another.</p>}
          {control?.checkpoints.length !== 0 && checkpoint_choices.length === 0 && <p className="muted">No checkpoints match this search.</p>}
          {draft.checkpoint && <p className="muted">Selected: <code>{draft.checkpoint}</code></p>}
        </fieldset>}
        <label className="field" htmlFor="run-lifecycle-arguments"><span>Task arguments</span>
          <textarea id="run-lifecycle-arguments" rows={4} value={draft.arguments} disabled={disabled} spellCheck={false}
            aria-describedby="run-lifecycle-arguments-hint" onChange={event => setDraft({ ...draft, arguments: event.currentTarget.value })} /></label>
        <p id="run-lifecycle-arguments-hint" className="muted">A JSON array of argument strings. The recorded executable and source stay fixed. No shell expansion is performed.</p>
      </>}
      <div className="lifecycle-fields">
        <label className="field" htmlFor="run-lifecycle-reason"><span>Reason <span className="muted">optional</span></span>
          <input id="run-lifecycle-reason" type="text" maxLength={1024} value={draft.reason} disabled={disabled}
            onChange={event => setDraft({ ...draft, reason: event.currentTarget.value })} /></label>
        <label className="field lifecycle-grace" htmlFor="run-lifecycle-grace"><span>Stop grace · seconds</span>
          <input id="run-lifecycle-grace" type="number" min={1} max={3600} value={draft.grace_seconds} disabled={disabled}
            onChange={event => setDraft({ ...draft, grace_seconds: event.currentTarget.valueAsNumber })} /></label>
      </div>
      <div className="lifecycle-form-actions"><button id="run-lifecycle-confirm" type="submit" className={`button ${draft.action === "stop" ? "danger" : "primary"}`}
        disabled={disabled || draft.action === "continue" && !draft.checkpoint || draft.action !== "stop" && !control?.command.length}>
        {sending ? "Sending…" : draft.action === "restart" ? "Create fresh run" : LABELS[draft.action]}</button>
        <button type="button" className="text-button" disabled={sending || pending} onClick={() => setDraft(null)}>Cancel</button></div>
    </form>}
  </section>;
}

export function lifecyclePanelKey(source: string, run: Run): string {
  return `${source}:${runIdentity(run)}`;
}
