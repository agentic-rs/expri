import test from "node:test";
import assert from "node:assert/strict";
import { JSDOM } from "jsdom";
import { act, createElement } from "react";
import { createRoot } from "react-dom/client";
import {
  DASHBOARD_THEME_STORAGE_KEY,
  ThemeControl,
  applyThemePreference,
  readThemePreference,
  saveThemePreference,
} from "./.test/app.js";

globalThis.IS_REACT_ACT_ENVIRONMENT = true;

function themeFixture(t, { stored = null, storage_unavailable = false } = {}) {
  const dom = new JSDOM("<!doctype html><html><body><main id='root'></main></body></html>", {
    url: "http://localhost/",
  });
  const browser_globals = ["window", "document", "HTMLElement", "Node", "Event"];
  const previous = new Map(browser_globals.map((name) => [name, Object.getOwnPropertyDescriptor(globalThis, name)]));
  for (const name of browser_globals) Object.defineProperty(globalThis, name, {
    configurable: true,
    writable: true,
    value: dom.window[name],
  });
  if (stored !== null) dom.window.localStorage.setItem(DASHBOARD_THEME_STORAGE_KEY, stored);
  if (storage_unavailable) Object.defineProperty(dom.window, "localStorage", {
    get() { throw new Error("Browser storage unavailable"); },
  });
  const original_fetch = globalThis.fetch;
  let request_count = 0;
  globalThis.fetch = () => {
    request_count += 1;
    throw new Error("Appearance must not request dashboard data");
  };
  const root = createRoot(dom.window.document.getElementById("root"));
  t.after(async () => {
    await act(async () => root.unmount());
    assert.equal(request_count, 0);
    dom.window.close();
    for (const [name, descriptor] of previous) {
      if (descriptor) Object.defineProperty(globalThis, name, descriptor);
      else Reflect.deleteProperty(globalThis, name);
    }
    globalThis.fetch = original_fetch;
  });
  return { root, document: dom.window.document, window: dom.window };
}

async function choose(fixture, theme) {
  const input = fixture.document.querySelector(`input[value="${theme}"]`);
  assert.ok(input);
  await act(async () => {
    input.focus();
    input.click();
  });
  assert.equal(input.checked, true);
}

test("theme preferences validate stored values and tolerate unavailable storage", () => {
  const values = new Map();
  const storage = {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value),
  };
  assert.equal(readThemePreference(storage), "light");
  for (const theme of ["light", "dark", "system"]) {
    saveThemePreference(theme, storage);
    assert.equal(readThemePreference(storage), theme);
  }
  values.set(DASHBOARD_THEME_STORAGE_KEY, "unknown");
  assert.equal(readThemePreference(storage), "light");
  assert.equal(readThemePreference(null), "light");
  const unavailable = {
    getItem() { throw new Error("Denied"); },
    setItem() { throw new Error("Denied"); },
  };
  assert.equal(readThemePreference(unavailable), "light");
  assert.doesNotThrow(() => saveThemePreference("dark", unavailable));
});

test("theme application delegates system appearance to CSS", () => {
  const dom = new JSDOM("<!doctype html><html></html>");
  const root = dom.window.document.documentElement;
  for (const theme of ["light", "dark", "system"]) {
    applyThemePreference(theme, root);
    assert.equal(root.dataset.theme, theme);
    assert.equal(root.style.colorScheme, theme === "system" ? "light dark" : theme);
  }
  dom.window.close();
});

test("native appearance choices restore and persist without requesting data", async (t) => {
  const fixture = themeFixture(t, { stored: "dark" });
  await act(async () => fixture.root.render(createElement(ThemeControl, { compact: true })));
  const controls = [...fixture.document.querySelectorAll('input[type="radio"]')];
  assert.equal(fixture.document.querySelector("fieldset legend").textContent, "Appearance");
  assert.deepEqual(controls.map((control) => control.value), ["light", "dark", "system"]);
  assert.equal(new Set(controls.map((control) => control.name)).size, 1);
  assert.equal(controls.find((control) => control.checked).value, "dark");
  assert.equal(fixture.document.documentElement.dataset.theme, "dark");
  await choose(fixture, "system");
  assert.equal(fixture.window.localStorage.getItem(DASHBOARD_THEME_STORAGE_KEY), "system");
  assert.equal(fixture.document.documentElement.style.colorScheme, "light dark");
  await choose(fixture, "light");
  assert.equal(fixture.window.localStorage.getItem(DASHBOARD_THEME_STORAGE_KEY), "light");
  assert.equal(fixture.document.documentElement.dataset.theme, "light");
});

test("appearance remains editable when browser storage access throws", async (t) => {
  const fixture = themeFixture(t, { storage_unavailable: true });
  await act(async () => fixture.root.render(createElement(ThemeControl)));
  assert.equal(fixture.document.querySelector('input[value="light"]').checked, true);
  await choose(fixture, "dark");
  assert.equal(fixture.document.documentElement.dataset.theme, "dark");
});
