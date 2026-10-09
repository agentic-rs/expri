export type RefreshOutcome = "success" | "failure" | "cancelled";
export type RefreshAvailability = "ready" | "hidden" | "offline" | "busy";
export type RefreshState = {
  enabled: boolean;
  availability: RefreshAvailability;
  running: boolean;
  interval_ms: number;
  retry_ms: number;
  failed: boolean;
};
export type RefreshClock = {
  now: () => number;
  set_timeout: (callback: () => void, delay_ms: number) => number;
  clear_timeout: (timer: number) => void;
};
type RefreshOptions = {
  run: () => Promise<RefreshOutcome>;
  availability: () => RefreshAvailability;
  cancel: () => void;
  on_state: (state: RefreshState) => void;
  clock?: RefreshClock;
  interval_ms?: number;
};

/** A completion-scheduled timer keeps slow polls from overlapping one another. */
export class AutoRefresh {
  private readonly clock: RefreshClock;
  private timer: number | null = null;
  private enabled = true;
  private disposed = false;
  private running = false;
  private generation = 0;
  private interval_ms: number;
  private retry_ms: number;
  private failed = false;
  private resume_immediately = false;
  private hint_pending = false;
  private previous_availability: RefreshAvailability;

  constructor(private readonly options: RefreshOptions) {
    this.clock = options.clock ?? {
      now: () => Date.now(),
      set_timeout: (callback, delay) => setTimeout(callback, delay),
      clear_timeout: (timer) => clearTimeout(timer),
    };
    this.interval_ms = options.interval_ms ?? 5_000;
    this.retry_ms = this.interval_ms;
    this.previous_availability = options.availability();
  }
  start(): void {
    this.schedule(this.interval_ms);
  }
  requestRefresh(): void {
    const availability = this.options.availability();
    if (this.disposed || !this.enabled || availability === "hidden" || availability === "offline")
      return;
    this.hint_pending = true;
    this.resumeHint();
  }
  /** Foreground completion may release a queued hint without overlapping work. */
  resumeHint(): void {
    if (
      this.disposed ||
      !this.enabled ||
      this.running ||
      !this.hint_pending ||
      this.options.availability() !== "ready"
    )
      return;
    this.hint_pending = false;
    this.schedule(0);
  }
  setEnabled(enabled: boolean): void {
    if (this.disposed || this.enabled === enabled) return;
    this.enabled = enabled;
    this.interrupt();
    if (enabled && this.running) this.resume_immediately = true;
    if (enabled && !this.running) this.schedule(0);
  }
  setInterval(delay_ms: number): void {
    this.interval_ms = delay_ms;
    if (!this.failed) this.retry_ms = delay_ms;
  }
  interrupt(): void {
    this.generation++;
    this.hint_pending = false;
    this.clearTimer();
    this.options.cancel();
    if (!this.running) this.schedule(this.retry_ms);
    else this.publish();
  }
  availabilityChanged(): void {
    const availability = this.options.availability();
    const was_paused =
      this.previous_availability === "hidden" || this.previous_availability === "offline";
    this.previous_availability = availability;
    if (availability === "hidden" || availability === "offline") this.interrupt();
    else if (was_paused) {
      this.resume_immediately = true;
      this.clearTimer();
      if (!this.running) {
        this.resume_immediately = false;
        this.schedule(0);
      } else this.publish();
    } else this.publish();
  }
  refreshCompleted(success: boolean): void {
    if (success) {
      this.failed = false;
      this.retry_ms = this.interval_ms;
    }
    this.clearTimer();
    if (!this.running && this.hint_pending && this.options.availability() === "ready")
      this.resumeHint();
    else if (!this.running) this.schedule(this.retry_ms);
    else this.publish();
  }
  dispose(): void {
    this.disposed = true;
    this.enabled = false;
    this.generation++;
    this.clearTimer();
    this.options.cancel();
  }
  private clearTimer(): void {
    if (this.timer !== null) this.clock.clear_timeout(this.timer);
    this.timer = null;
  }
  private publish(): void {
    this.options.on_state({
      enabled: this.enabled,
      availability: this.options.availability(),
      running: this.running,
      interval_ms: this.interval_ms,
      retry_ms: this.retry_ms,
      failed: this.failed,
    });
  }
  private schedule(delay_ms: number): void {
    this.clearTimer();
    if (this.disposed) return;
    const availability = this.options.availability();
    this.previous_availability = availability;
    if (this.enabled && !this.running && availability !== "hidden" && availability !== "offline")
      this.timer = this.clock.set_timeout(() => {
        this.timer = null;
        void this.tick();
      }, delay_ms);
    this.publish();
  }
  private async tick(): Promise<void> {
    if (this.disposed || !this.enabled || this.running) return;
    if (this.options.availability() !== "ready") {
      this.schedule(this.retry_ms);
      return;
    }
    this.running = true;
    const generation = this.generation;
    this.publish();
    let outcome: RefreshOutcome;
    try {
      outcome = await this.options.run();
    } catch {
      outcome = "failure";
    }
    this.running = false;
    if (this.disposed) return;
    if (generation === this.generation) {
      if (outcome === "success") {
        this.failed = false;
        this.retry_ms = this.interval_ms;
      } else if (outcome === "failure") {
        this.failed = true;
        this.retry_ms = Math.min(60_000, Math.max(this.interval_ms, this.retry_ms * 2));
      }
    }
    const delay = this.resume_immediately || this.hint_pending ? 0 : this.retry_ms;
    this.hint_pending = false;
    this.resume_immediately = false;
    this.schedule(delay);
  }
}
