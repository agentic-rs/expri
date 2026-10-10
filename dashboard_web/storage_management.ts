import { apiUrl, errorText, RequestError } from "./dashboard_model";

export type StorageStats = {
  project_id: string;
  revision: string;
  file_count: number;
  logical_bytes: number;
  object_count: number;
  object_bytes: number;
  shared_reference_count: number;
  retained_object_count: number;
  retained_object_bytes: number;
  pending_upload_count: number;
  pending_upload_bytes: number;
  tracking_bytes: number;
  reclaimable_object_count: number;
  reclaimable_object_bytes: number;
};
export type DeletePreview = {
  project_id: string;
  revision: string;
  run_count: number;
  stats: StorageStats;
};
export type ProjectDeletion = {
  project_id: string;
  status: "pending" | "deleted";
  pending_tasks: number;
  deleted_objects: number;
  aborted_uploads: number;
  last_error: string | null;
};
export type StorageManagementState = {
  project_id: string | null;
  stats: StorageStats | null;
  delete_enabled: boolean;
  stats_loading: boolean;
  stats_error: string | null;
  phase: "closed" | "preview_loading" | "review" | "submitting" | "status";
  deletion_project_id: string | null;
  preview: DeletePreview | null;
  deletion: ProjectDeletion | null;
  error: string | null;
  stale: boolean;
  checking_status: boolean;
};

const COMPONENT = /^[A-Za-z0-9._-]{1,96}$/;
function validProject(value: unknown): value is string {
  return typeof value === "string" && COMPONENT.test(value) && value !== "." && value !== "..";
}
function count(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}
const STATS_COUNTS = [
  "file_count", "logical_bytes", "object_count", "object_bytes", "shared_reference_count",
  "retained_object_count", "retained_object_bytes", "pending_upload_count", "pending_upload_bytes",
  "tracking_bytes", "reclaimable_object_count", "reclaimable_object_bytes",
] as const;
export function storageStatsValid(value: unknown, project_id: string): value is StorageStats {
  if (value === null || typeof value !== "object") return false;
  const stats = value as StorageStats;
  return validProject(project_id) && stats.project_id === project_id &&
    typeof stats.revision === "string" && stats.revision.length > 0 &&
    STATS_COUNTS.every((key) => count(stats[key]));
}
export function deletionPreviewValid(value: unknown, project_id: string): value is DeletePreview {
  if (value === null || typeof value !== "object") return false;
  const preview = value as DeletePreview;
  return preview.project_id === project_id && typeof preview.revision === "string" &&
    preview.revision.length > 0 && count(preview.run_count) &&
    storageStatsValid(preview.stats, project_id);
}
export function projectDeletionValid(value: unknown, project_id: string): value is ProjectDeletion {
  if (value === null || typeof value !== "object") return false;
  const deletion = value as ProjectDeletion;
  return deletion.project_id === project_id &&
    (deletion.status === "pending" || deletion.status === "deleted") &&
    count(deletion.pending_tasks) && count(deletion.deleted_objects) && count(deletion.aborted_uploads) &&
    (deletion.last_error === null || typeof deletion.last_error === "string");
}

type Timer = ReturnType<typeof setTimeout>;
type ControllerOptions = {
  request?: typeof fetch;
  visible?: () => boolean;
  schedule?: (callback: () => void, delay: number) => Timer;
  cancel?: (timer: Timer) => void;
  storage?: Pick<Storage, "getItem" | "setItem" | "removeItem"> | null;
};
const PENDING_PROJECT_KEY = "expri.project_deletion";

/** Read polls may retry; the destructive POST is always an explicit user action. */
export class StorageManagementController {
  private state: StorageManagementState = {
    project_id: null, stats: null, delete_enabled: false, stats_loading: false, stats_error: null,
    phase: "closed", deletion_project_id: null, preview: null, deletion: null,
    error: null, stale: false, checking_status: false,
  };
  private readonly listeners = new Set<() => void>();
  private readonly request: typeof fetch;
  private readonly visible: () => boolean;
  private readonly schedule: (callback: () => void, delay: number) => Timer;
  private readonly cancel: (timer: Timer) => void;
  private readonly storage: ControllerOptions["storage"];
  private stats_request: AbortController | null = null;
  private preview_request: AbortController | null = null;
  private deletion_request: AbortController | null = null;
  private stats_timer: Timer | null = null;
  private deletion_timer: Timer | null = null;
  private context_generation = 0;
  private dialog_generation = 0;
  private polling_attempts = 0;
  private stats_interval = 5_000;
  private active = false;
  private auto_enabled = true;
  private recovered = false;
  private unresolved_submission = false;
  private disposed = false;

  constructor(options: ControllerOptions = {}) {
    this.request = options.request ?? ((...args) => fetch(...args));
    this.visible = options.visible ?? (() =>
      (typeof document === "undefined" || document.visibilityState !== "hidden") &&
      (typeof navigator === "undefined" || navigator.onLine !== false));
    this.schedule = options.schedule ?? ((callback, delay) => setTimeout(callback, delay));
    this.cancel = options.cancel ?? ((timer) => clearTimeout(timer));
    this.storage = options.storage;
  }
  getSnapshot = (): StorageManagementState => this.state;
  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };
  private update(values: Partial<StorageManagementState>): void {
    if (this.disposed) return;
    this.state = { ...this.state, ...values };
    for (const listener of this.listeners) listener();
  }
  setContext(project_id: string | null, active: boolean, auto_enabled: boolean): void {
    project_id = validProject(project_id) ? project_id : null;
    const changed = this.state.project_id !== project_id;
    this.active = active && project_id !== null;
    this.auto_enabled = auto_enabled;
    if (this.stats_timer !== null) this.cancel(this.stats_timer);
    this.stats_timer = null;
    if (changed) {
      this.context_generation++;
      this.stats_interval = 5_000;
      this.stats_request?.abort();
      this.stats_request = null;
      if (this.state.phase === "preview_loading" || this.state.phase === "review") this.close();
      this.update({ project_id, stats: null, delete_enabled: false, stats_error: null, stats_loading: false });
    }
    if (this.active && (changed || !this.state.stats)) void this.refreshStats();
    else this.scheduleStats();
    if (!this.recovered) {
      this.recovered = true;
      let pending: string | null = null;
      try { pending = this.storage?.getItem(PENDING_PROJECT_KEY) ?? null; } catch { /* Storage can be unavailable. */ }
      if (validProject(pending)) {
        this.unresolved_submission = true;
        this.update({ deletion_project_id: pending, phase: "status" });
        void this.checkStatus();
      }
    }
  }
  private scheduleStats(): void {
    if (this.stats_timer !== null) this.cancel(this.stats_timer);
    this.stats_timer = null;
    if (!this.active || !this.auto_enabled || this.disposed) return;
    this.stats_timer = this.schedule(() => {
      this.stats_timer = null;
      if (this.visible()) void this.refreshStats();
      else this.scheduleStats();
    }, this.stats_interval);
  }
  async refreshStats(): Promise<void> {
    const project_id = this.state.project_id;
    if (!project_id || !this.active || this.stats_request || this.disposed) return;
    const generation = this.context_generation;
    const controller = new AbortController();
    this.stats_request = controller;
    this.update({ stats_loading: true });
    try {
      const value = await this.json(apiUrl("/api/storage/stats", { project_id }), controller);
      if (generation !== this.context_generation || this.disposed) return;
      const result = value as { stats?: unknown; delete_enabled?: unknown };
      if (!storageStatsValid(result?.stats, project_id) || typeof result.delete_enabled !== "boolean")
        throw new Error("Storage usage does not match this project.");
      this.stats_interval = 5_000;
      this.update({ stats: result.stats, delete_enabled: result.delete_enabled, stats_error: null });
    } catch (error) {
      if (generation === this.context_generation && !this.disposed) {
        this.stats_interval = Math.min(60_000, this.stats_interval * 2);
        this.update({ delete_enabled: false, stats_error: error instanceof RequestError && error.status === 404
          ? "Storage usage is unavailable on this server. Upgrade expri to see usage and manage projects."
          : `Could not read storage usage. ${errorText(error)}` });
      }
    } finally {
      if (this.stats_request === controller) this.stats_request = null;
      if (generation === this.context_generation) {
        this.update({ stats_loading: false });
        this.scheduleStats();
      }
    }
  }
  async openPreview(): Promise<void> {
    const project_id = this.state.project_id;
    if (!this.state.delete_enabled || !project_id || this.state.phase === "submitting") return;
    if (this.unresolved_submission || this.state.deletion?.status === "pending") {
      this.update({ phase: "status" });
      await this.checkStatus();
      return;
    }
    this.preview_request?.abort();
    const generation = ++this.dialog_generation;
    const controller = new AbortController();
    this.preview_request = controller;
    this.update({ phase: "preview_loading", deletion_project_id: project_id,
      preview: null, deletion: null, error: null, stale: false });
    try {
      const value = await this.json(apiUrl("/api/projects/delete-preview", { project_id }), controller);
      if (generation !== this.dialog_generation || project_id !== this.state.project_id || this.disposed) return;
      if (!deletionPreviewValid(value, project_id)) throw new Error("The deletion preview does not match this project.");
      this.update({ phase: "review", preview: value });
    } catch (error) {
      if (generation === this.dialog_generation && !this.disposed)
        this.update({ phase: "review", error: `Could not preview project deletion. ${errorText(error)}`, stale: true });
    } finally {
      if (this.preview_request === controller) this.preview_request = null;
    }
  }
  async submit(confirmation: string, password: string): Promise<void> {
    const preview = this.state.preview;
    if (this.state.phase !== "review" || this.state.stale || !preview ||
      preview.project_id !== this.state.project_id || !this.state.delete_enabled) return;
    if (confirmation !== preview.project_id || !password) {
      this.update({ error: "Enter the exact project name and your dashboard password." });
      return;
    }
    const project_id = preview.project_id;
    const controller = new AbortController();
    this.deletion_request = controller;
    this.update({ phase: "submitting", error: null });
    this.unresolved_submission = true;
    this.rememberProject(project_id);
    try {
      const value = await this.json("/api/projects/delete", controller, {
        method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify({ project_id, revision: preview.revision, confirmation, password }),
      });
      if (this.disposed) return;
      if (!projectDeletionValid(value, project_id)) throw new Error("The deletion response does not match this project.");
      this.unresolved_submission = false;
      this.update({ phase: "status", deletion: value, preview: null });
      this.polling_attempts = 0;
      this.scheduleDeletion();
    } catch (error) {
      if (this.disposed) return;
      if (error instanceof RequestError && (error.status === 409 || error.status === 403 || error.status === 400)) {
        this.unresolved_submission = false;
        this.rememberProject(null);
        this.update({ phase: "review", stale: true,
          error: error.status === 409
            ? "The project changed since this preview. Review a fresh preview before deleting."
            : `Deletion was not accepted. Review a fresh preview before trying again. ${errorText(error)}` });
      } else {
        this.update({ phase: "status", error: `Could not confirm the deletion request. Check its status before trying again. ${errorText(error)}` });
      }
    } finally {
      if (this.deletion_request === controller) this.deletion_request = null;
    }
  }
  private scheduleDeletion(): void {
    if (this.deletion_timer !== null) this.cancel(this.deletion_timer);
    this.deletion_timer = null;
    if (this.state.deletion?.status === "deleted") {
      this.rememberProject(null);
      return;
    }
    if (this.state.deletion?.status !== "pending" || this.polling_attempts >= 12 || this.disposed) return;
    this.deletion_timer = this.schedule(() => {
      this.deletion_timer = null;
      if (this.visible()) {
        this.polling_attempts++;
        void this.checkStatus();
      } else this.scheduleDeletion();
    }, 5_000);
  }
  async checkStatus(): Promise<void> {
    const project_id = this.state.deletion_project_id;
    if (!project_id || this.deletion_request || this.disposed) return;
    if (this.deletion_timer !== null) this.cancel(this.deletion_timer);
    this.deletion_timer = null;
    const controller = new AbortController();
    this.deletion_request = controller;
    this.update({ checking_status: true });
    try {
      const value = await this.json(apiUrl("/api/projects/deletion", { project_id }), controller);
      if (this.disposed || project_id !== this.state.deletion_project_id) return;
      if (!projectDeletionValid(value, project_id)) throw new Error("The cleanup status does not match this project.");
      this.unresolved_submission = false;
      this.update({ deletion: value, error: null, preview: null });
      this.scheduleDeletion();
    } catch (error) {
      if (this.disposed) return;
      if (error instanceof RequestError && error.status === 404) {
        this.unresolved_submission = false;
        this.rememberProject(null);
        this.update({ deletion: null, stale: true,
          error: "No deletion request was found. Close this dialog and review a fresh preview before trying again." });
      } else this.update({ error: `Could not read cleanup status. Check status when the connection is available. ${errorText(error)}` });
    } finally {
      if (this.deletion_request === controller) this.deletion_request = null;
      this.update({ checking_status: false });
    }
  }
  close(): void {
    if (this.state.phase === "submitting") return;
    const keep_status = !!this.state.deletion || (this.state.phase === "status" && !!this.state.error);
    this.dialog_generation++;
    this.preview_request?.abort();
    this.preview_request = null;
    this.update({ phase: "closed", preview: null, stale: false,
      ...(keep_status ? {} : { deletion_project_id: null, error: null }) });
  }
  showStatus(): void { this.update({ phase: "status" }); }
  private rememberProject(project_id: string | null): void {
    try {
      if (project_id) this.storage?.setItem(PENDING_PROJECT_KEY, project_id);
      else this.storage?.removeItem(PENDING_PROJECT_KEY);
    } catch { /* Status remains available for the current page without browser storage. */ }
  }
  private async json(url: string, controller: AbortController, init: RequestInit = {}): Promise<unknown> {
    const timeout = this.schedule(() => controller.abort(), 30_000);
    try {
      const response = await this.request(url, { ...init, signal: controller.signal,
        cache: "no-store", credentials: "same-origin" });
      if (response.status === 401 && typeof location !== "undefined") {
        location.assign("/login");
        throw new RequestError("Your dashboard session expired. Sign in again.", 401);
      }
      if (!response.ok) {
        let message = `Request failed (${response.status})`;
        try {
          const body = await response.json() as { error?: unknown };
          if (typeof body.error === "string") message = body.error;
        } catch { /* Proxy failures may not be JSON. */ }
        throw new RequestError(message, response.status);
      }
      return await response.json();
    } catch (error) {
      if (controller.signal.aborted) throw new Error("The request timed out. Check the connection and try again.");
      throw error;
    } finally { this.cancel(timeout); }
  }
  dispose(): void {
    this.disposed = true;
    this.stats_request?.abort();
    this.preview_request?.abort();
    this.deletion_request?.abort();
    if (this.stats_timer !== null) this.cancel(this.stats_timer);
    if (this.deletion_timer !== null) this.cancel(this.deletion_timer);
    this.listeners.clear();
  }
}
