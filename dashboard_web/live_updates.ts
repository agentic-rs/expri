export type LiveUpdatesState = "unavailable" | "connecting" | "connected" | "reconnecting";
export type EventSourceConnection = {
  readonly readyState: number;
  addEventListener: (type: string, listener: EventListener) => void;
  removeEventListener: (type: string, listener: EventListener) => void;
  close: () => void;
};
export type EventSourceFactory = (url: string) => EventSourceConnection | null;

/** A notification carries only a bounded revision hint, never run contents. */
export function parseUpdateHint(data: unknown): string | null {
  if (typeof data !== "string" || data.length > 256) return null;
  try {
    const revision = JSON.parse(data)?.catalog_revision;
    return typeof revision === "string" && /^[A-Za-z0-9._:-]{1,128}$/.test(revision)
      ? revision
      : null;
  } catch {
    return null;
  }
}

/** Native EventSource handles reconnection; polling remains the recovery path. */
export class LiveUpdates {
  private url: string | null = null;
  private connection: EventSourceConnection | null = null;
  private last_revision: string | null = null;
  private cleanup: (() => void) | null = null;
  private disposed = false;
  constructor(
    private readonly options: {
      create: EventSourceFactory;
      on_hint: () => void;
      on_state: (state: LiveUpdatesState) => void;
    },
  ) {}

  setUrl(url: string | null): void {
    if (this.disposed || this.url === url) return;
    this.close();
    this.url = url;
    this.last_revision = null;
    if (!url) {
      this.options.on_state("unavailable");
      return;
    }
    let connection: EventSourceConnection | null;
    try {
      connection = this.options.create(url);
    } catch {
      connection = null;
    }
    if (!connection) {
      this.options.on_state("unavailable");
      return;
    }
    this.connection = connection;
    const current = () => !this.disposed && this.connection === connection && this.url === url;
    const opened: EventListener = () => {
      if (!current()) return;
      this.options.on_state("connected");
      // A reconnect can follow missed notifications even when its revision repeats.
      this.options.on_hint();
    };
    const updated: EventListener = (event) => {
      if (!current()) return;
      const revision = parseUpdateHint((event as MessageEvent).data);
      if (!revision || revision === this.last_revision) return;
      this.last_revision = revision;
      this.options.on_hint();
    };
    const failed: EventListener = () => {
      if (!current()) return;
      if (connection.readyState === 2) {
        this.close();
        this.options.on_state("unavailable");
      } else {
        this.options.on_state("reconnecting");
      }
    };
    connection.addEventListener("open", opened);
    connection.addEventListener("updates", updated);
    connection.addEventListener("error", failed);
    this.cleanup = () => {
      connection.removeEventListener("open", opened);
      connection.removeEventListener("updates", updated);
      connection.removeEventListener("error", failed);
      connection.close();
    };
    this.options.on_state("connecting");
  }
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.close();
  }
  private close(): void {
    this.connection = null;
    this.cleanup?.();
    this.cleanup = null;
  }
}
