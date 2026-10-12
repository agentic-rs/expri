export type IconName = "runs" | "storage" | "charts" | "overview" | "logs" | "files" | "refresh" | "compare" | "close" | "search";

const PATHS: Record<IconName, string> = {
  runs: "M4 5h16M4 12h16M4 19h16M7 3v4M7 10v4M7 17v4",
  storage: "M4 6c0-2 16-2 16 0s-16 2-16 0Zm0 0v6c0 2 16 2 16 0V6M4 12v6c0 2 16 2 16 0v-6",
  charts: "M4 4v16h16M7 15l4-5 4 3 5-7",
  overview: "M9 4h11M9 12h11M9 20h11M4 4h.01M4 12h.01M4 20h.01",
  logs: "m5 6 5 6-5 6M13 18h6",
  files: "M14 3H5v18h14V8l-5-5Zm0 0v5h5",
  refresh: "M20 7v5h-5M4 17v-5h5M5.5 8a7 7 0 0 1 11.6-3L20 8M4 16l2.9 3A7 7 0 0 0 18.5 16",
  compare: "M8 3v18M16 3v18M4 7h8M12 17h8",
  close: "m6 6 12 12M6 18 18 6",
  search: "M11 18a7 7 0 1 0 0-14 7 7 0 0 0 0 14Zm5-2 5 5",
};

export function Icon({ name }: { name: IconName }) {
  return <svg className="ui-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false"><path d={PATHS[name]} /></svg>;
}
