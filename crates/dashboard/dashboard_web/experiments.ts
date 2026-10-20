import { apiUrl, parseProjectRunKey, parseRunDeepLink, runIdentity, type Run, type RunDeepLink, type Source } from "./dashboard_model";

export const MAX_EXPERIMENT_MEMBERS = 256;
export const MAX_EXPERIMENT_EDGES = 512;
export type LineageKind = "continued" | "restarted" | "split";
export type ExperimentLineage = {
  parent: RunDeepLink;
  child: RunDeepLink;
  kind: LineageKind;
  checkpoint: string | null;
  step: string | null;
};
export type Experiment = {
  experiment_id: string;
  name: string;
  description: string;
  revision: string;
  members: RunDeepLink[];
  lineage: ExperimentLineage[];
  created_at: string;
  updated_at: string;
  unavailable_runs: RunDeepLink[];
  membership_count: number;
};
export type ExperimentCatalog = { project_id: string; revision: string; experiments: Experiment[] };
export type ExperimentDraft = Pick<Experiment, "name" | "description" | "members" | "lineage">;
export type ExperimentSummary = Pick<Experiment, "experiment_id" | "name" | "revision">;
export type ExperimentState = {
  supported: boolean;
  management_enabled: boolean;
  loading: boolean;
  busy: boolean;
  error: string | null;
  catalog: ExperimentCatalog | null;
  filter: string;
};

export function experimentDisplayName(experiment: ExperimentSummary, experiments: ExperimentSummary[]): string {
  const duplicates = experiments.filter((item) => item.name === experiment.name && item.experiment_id !== experiment.experiment_id);
  if (!duplicates.length) return experiment.name;
  let length = Math.min(8, experiment.experiment_id.length);
  while (length < experiment.experiment_id.length && duplicates.some((item) => item.experiment_id.endsWith(experiment.experiment_id.slice(-length)))) length++;
  return `${experiment.name} · ${experiment.experiment_id.slice(-length)}`;
}

export function scopeIdentity(scope: RunDeepLink): string {
  return JSON.stringify([scope.project_id, scope.origin, scope.run_id]);
}
export function sameScope(first: RunDeepLink, second: RunDeepLink): boolean {
  return scopeIdentity(first) === scopeIdentity(second);
}
export function experimentProject(source: Source | undefined): string | null {
  return source && ["hosted_project", "service"].includes(source.kind) && source.project_id ? source.project_id : null;
}
export function experimentRunScope(run: Pick<Run, "run_id" | "run_key" | "origin" | "queue_only">, source: Source | undefined): RunDeepLink | null {
  const project_id = experimentProject(source);
  if (!project_id || run.queue_only) return null;
  const identity = source?.kind === "hosted_project" ? parseProjectRunKey(runIdentity(run)) :
    source?.kind === "service" && source.origin ? { origin: source.origin, run_id: run.run_id } : null;
  if (!identity || identity.run_id !== run.run_id || (run.origin && run.origin !== identity.origin)) return null;
  const scope = { project_id, ...identity };
  return parseRunDeepLink(apiUrl("", scope)) ? scope : null;
}
function text(value: unknown, name: string, maximum: number, multiline = false, maximum_chars = maximum): string {
  if (typeof value !== "string" || (multiline ? /[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/u : /\p{Cc}/u).test(value) ||
    new TextEncoder().encode(value).length > maximum || Array.from(value).length > maximum_chars)
    throw new Error(`The ${name} is invalid. Refresh before trying again.`);
  return value;
}
function scope(value: unknown, project_id: string): RunDeepLink {
  if (!value || typeof value !== "object") throw new Error("The experiment has an invalid run identity.");
  const item = value as RunDeepLink;
  if (item.project_id !== project_id || !parseRunDeepLink(apiUrl("", item)))
    throw new Error("The experiment contains a run from a different project.");
  return { project_id, origin: item.origin, run_id: item.run_id };
}
export function checkpointStep(value: string): string | null {
  if (!value.trim()) return null;
  if (!/^\d{1,20}$/.test(value.trim()) || BigInt(value.trim()) > 18_446_744_073_709_551_615n)
    throw new Error("Checkpoint step must be a whole number from 0 to 18446744073709551615.");
  return BigInt(value.trim()).toString();
}
export function parseExperiment(value: unknown, project_id: string): Experiment {
  if (!value || typeof value !== "object") throw new Error("The experiment could not be read.");
  const item = value as Experiment;
  if (!/^[A-Za-z0-9._-]{1,96}$/.test(item.experiment_id) || !item.revision || typeof item.revision !== "string" ||
    !Array.isArray(item.members) || item.members.length > MAX_EXPERIMENT_MEMBERS ||
    !Array.isArray(item.lineage) || item.lineage.length > MAX_EXPERIMENT_EDGES || !Array.isArray(item.unavailable_runs))
    throw new Error("The experiment response is invalid. Refresh before trying again.");
  const members = item.members.map((member) => scope(member, project_id));
  const identities = new Set(members.map(scopeIdentity));
  if (identities.size !== members.length || item.membership_count !== members.length)
    throw new Error("The experiment member count does not match its recorded membership.");
  const lineage = item.lineage.map((edge) => {
    const parent = scope(edge.parent, project_id), child = scope(edge.child, project_id);
    if (!identities.has(scopeIdentity(parent)) || !identities.has(scopeIdentity(child)) || sameScope(parent, child) ||
      !["continued", "restarted", "split"].includes(edge.kind)) throw new Error("The experiment lineage is invalid.");
    const step = edge.step === null ? null : checkpointStep(String(edge.step));
    if (step !== edge.step) throw new Error("The checkpoint step is invalid.");
    return { parent, child, kind: edge.kind, checkpoint: edge.checkpoint === null ? null : text(edge.checkpoint, "checkpoint reference", 4096, false, 1024), step };
  });
  const unavailable_runs = item.unavailable_runs.map((member) => scope(member, project_id));
  if (unavailable_runs.some((member) => !identities.has(scopeIdentity(member)))) throw new Error("An unavailable run is outside this experiment.");
  return { ...item, name: text(item.name, "experiment name", 512, false, 128), description: text(item.description, "experiment description", 8192, true, 2048), members, lineage, unavailable_runs };
}
export function parseExperimentCatalog(value: unknown, project_id: string): ExperimentCatalog {
  if (!value || typeof value !== "object") throw new Error("Experiments could not be read.");
  const catalog = value as ExperimentCatalog;
  if (catalog.project_id !== project_id || typeof catalog.revision !== "string" || !Array.isArray(catalog.experiments) || catalog.experiments.length > 128)
    throw new Error("The experiment catalog does not match the selected project.");
  const experiments = catalog.experiments.map((item) => parseExperiment(item, project_id));
  const experiment_ids = new Set(experiments.map((item) => item.experiment_id));
  const member_ids = experiments.flatMap((item) => item.members.map(scopeIdentity));
  if (experiment_ids.size !== experiments.length || new Set(member_ids).size !== member_ids.length)
    throw new Error("The experiment catalog contains duplicate membership.");
  return { project_id, revision: catalog.revision, experiments };
}
export function memberExperiment(catalog: ExperimentCatalog | null, member: RunDeepLink | null): Experiment | null {
  return member ? catalog?.experiments.find((item) => item.members.some((scope) => sameScope(scope, member))) ?? null : null;
}
export function removeExperimentMember(experiment: Experiment, member: RunDeepLink): ExperimentDraft {
  return { name: experiment.name, description: experiment.description,
    members: experiment.members.filter((scope) => !sameScope(scope, member)),
    lineage: experiment.lineage.filter((edge) => !sameScope(edge.parent, member) && !sameScope(edge.child, member)) };
}
