import { createContext, useContext, useEffect, useState, type ReactNode } from "react";
import type { RefreshClock } from "./auto_refresh";
import { formatDuration, runInProgress, type Run } from "./dashboard_model";

const DurationTime = createContext<number | null>(null);

/** One display clock updates duration consumers without refreshing the workspace. */
export function RunDurationProvider({ clock, active, children }: { clock: RefreshClock; active: boolean; children: ReactNode }) {
  const [now_ms, setNow] = useState(clock.now);
  useEffect(() => {
    setNow(clock.now());
    if (!active) return;
    const page_document = document;
    let timer: number | null = null;
    const clear = () => {
      if (timer !== null) clock.clear_timeout(timer);
      timer = null;
    };
    const schedule = () => {
      if (page_document.visibilityState === "hidden") return;
      timer = clock.set_timeout(() => {
        timer = null;
        setNow(clock.now());
        schedule();
      }, 1000);
    };
    const visibilityChanged = () => {
      clear();
      setNow(clock.now());
      schedule();
    };
    schedule();
    page_document.addEventListener("visibilitychange", visibilityChanged);
    return () => {
      clear();
      page_document.removeEventListener("visibilitychange", visibilityChanged);
    };
  }, [active, clock]);
  return <DurationTime.Provider value={now_ms}>{children}</DurationTime.Provider>;
}

function durationValue(run: Run, value: string) {
  const live = runInProgress(run);
  const label = live ? "Elapsed time" : "Duration";
  return <span className="run-duration" aria-live="off" data-live={live ? "true" : "false"}
    title={value === "—" ? `${label} unavailable` : live ? "Elapsed since the recorded run start" : "Recorded run duration"}
    aria-label={`${label}: ${value === "—" ? "unavailable" : value}`}>{value}</span>;
}

export function RunDuration({ run, show_unknown = true }: { run: Run; show_unknown?: boolean }) {
  const now_ms = useContext(DurationTime);
  const value = formatDuration(run, now_ms ?? Date.now());
  return value === "—" && !show_unknown ? null : durationValue(run, value);
}

export function RunStatusTiming({ run }: { run: Run }) {
  const now_ms = useContext(DurationTime);
  const value = formatDuration(run, now_ms ?? Date.now());
  const show_duration = value !== "—" || runInProgress(run);
  const show_exit = run.exit_code !== null && run.exit_code !== 0;
  if (!show_duration && !show_exit) return null;
  return <div className="run-meta run-timing">
    {show_duration && durationValue(run, value)}
    {show_duration && show_exit && " · "}
    {show_exit && <span>exit {run.exit_code}</span>}
  </div>;
}
