import {
  apiUrl,
  runScope,
  parseRunDeepLink,
  type Run,
  type RunAnnotations,
  type RunDeepLink,
  type Source,
} from "./dashboard_model";

export const MAX_RUN_NICKNAME_CHARS = 128;
export const MAX_RUN_NICKNAME_BYTES = 512;
export const MAX_RUN_TAGS = 16;
export const MAX_RUN_TAG_CHARS = 48;
export const MAX_RUN_TAG_BYTES = 192;

export type RunAnnotationValues = Pick<RunAnnotations, "nickname" | "tags">;

function annotationText(value: unknown, label: string, max_chars: number, max_bytes: number): string {
  if (typeof value !== "string") throw new Error(`${label} must be text.`);
  if (/\p{Cc}/u.test(value)) throw new Error(`${label} cannot contain control characters.`);
  // Rust str::trim uses Unicode White_Space; ECMAScript trim also strips U+FEFF.
  const text = value.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, "");
  if (Array.from(text).length > max_chars || new TextEncoder().encode(text).length > max_bytes)
    throw new Error(`${label} must be at most ${max_chars} characters (${max_bytes} UTF-8 bytes).`);
  return text;
}

/** Match the service's normalization before submitting or accepting annotation replies. */
export function normalizeRunAnnotations(nickname: unknown, tags: unknown): RunAnnotationValues {
  const name = nickname === null ? "" : annotationText(nickname, "Nickname", MAX_RUN_NICKNAME_CHARS, MAX_RUN_NICKNAME_BYTES);
  if (!Array.isArray(tags)) throw new Error("Tags must be a list.");
  if (tags.length > MAX_RUN_TAGS) throw new Error(`Use at most ${MAX_RUN_TAGS} tags.`);
  const normalized_tags = [...new Set(tags.map((value) => {
    const tag = annotationText(value, "Tag", MAX_RUN_TAG_CHARS, MAX_RUN_TAG_BYTES);
    if (!tag) throw new Error("Tags cannot be empty.");
    return tag;
  }))];
  return { nickname: name || null, tags: normalized_tags };
}

export function runAnnotationsMatchesScope(value: unknown, scope: RunDeepLink): value is RunAnnotations {
  if (!value || typeof value !== "object") return false;
  const record = value as Partial<RunAnnotations>;
  const selected = record.scope;
  if (!selected || !parseRunDeepLink(apiUrl("", selected)) ||
    selected.project_id !== scope.project_id || selected.origin !== scope.origin || selected.run_id !== scope.run_id) return false;
  try {
    const normalized = normalizeRunAnnotations(record.nickname, record.tags);
    return normalized.nickname === record.nickname && JSON.stringify(normalized.tags) === JSON.stringify(record.tags);
  } catch {
    return false;
  }
}

/** Editing requires an explicit capability from the service for this exact run. */
export function runAnnotationsScope(run: Run, source: Source | undefined): RunDeepLink | null {
  const scope = run.annotations?.scope;
  if (!scope || !source || !runAnnotationsMatchesScope(run.annotations, scope)) return null;
  const identity = runScope(run, source);
  return identity?.project_id === scope.project_id && identity.origin === scope.origin && identity.run_id === scope.run_id ? scope : null;
}
