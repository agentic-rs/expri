import { useEffect, useId, useState } from "react";

export type DashboardTheme = "light" | "dark" | "system";
type ThemeStorage = Pick<Storage, "getItem" | "setItem">;

export const DASHBOARD_THEME_STORAGE_KEY = "expri.dashboard.theme";
const THEMES: { value: DashboardTheme; label: string }[] = [
  { value: "light", label: "Light" },
  { value: "dark", label: "Dark" },
  { value: "system", label: "System" },
];

function themeStorage(): ThemeStorage | null {
  try {
    return typeof window === "undefined" ? null : window.localStorage;
  } catch {
    return null;
  }
}

export function readThemePreference(storage: ThemeStorage | null = themeStorage()): DashboardTheme {
  try {
    const value = storage?.getItem(DASHBOARD_THEME_STORAGE_KEY);
    return value === "dark" || value === "system" ? value : "light";
  } catch {
    return "light";
  }
}

export function saveThemePreference(theme: DashboardTheme, storage: ThemeStorage | null = themeStorage()): void {
  try {
    storage?.setItem(DASHBOARD_THEME_STORAGE_KEY, theme);
  } catch {
    // The current page can change appearance when browser storage is unavailable.
  }
}

export function applyThemePreference(theme: DashboardTheme, root: HTMLElement = document.documentElement): void {
  root.dataset.theme = theme;
  root.style.colorScheme = theme === "system" ? "light dark" : theme;
}

export function ThemeControl({ compact = false }: { compact?: boolean }) {
  const [theme, setTheme] = useState(readThemePreference);
  const control_id = useId();

  useEffect(() => {
    applyThemePreference(theme);
  }, [theme]);

  return (
    <fieldset className={`theme-control${compact ? " compact" : ""}`}>
      <legend>Appearance</legend>
      <div className="theme-choices">
        {THEMES.map((option) => (
          <label className="theme-choice" key={option.value}>
            <input
              className="sr-only"
              id={`${control_id}-${option.value}`}
              type="radio"
              name={`${control_id}-theme`}
              value={option.value}
              checked={theme === option.value}
              onChange={() => {
                setTheme(option.value);
                saveThemePreference(option.value);
              }}
            />
            <span>{option.label}</span>
          </label>
        ))}
      </div>
    </fieldset>
  );
}
