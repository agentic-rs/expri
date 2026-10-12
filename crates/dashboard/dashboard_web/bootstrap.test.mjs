import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { Script } from "node:vm";
import { build } from "esbuild";
import { JSDOM, VirtualConsole } from "jsdom";

const pages = {
  dashboard: readFileSync(new URL("./index.html", import.meta.url), "utf8"),
  login: readFileSync(new URL("./login.html", import.meta.url), "utf8"),
};
const { outputFiles } = await build({
  entryPoints: [new URL("./app.tsx", import.meta.url).pathname],
  bundle: true,
  format: "iife",
  target: "es2022",
  jsx: "automatic",
  define: { "process.env.NODE_ENV": '"production"' },
  write: false,
  logLevel: "silent",
});
const production_bundle = outputFiles[0].text;
const storage_key = "expri.dashboard.theme";

function reply(value, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => value,
    text: async () => JSON.stringify(value),
  };
}

function browserFixture(t, { stored = null, storage_unavailable = false } = {}) {
  const errors = [];
  const virtual_console = new VirtualConsole();
  virtual_console.on("jsdomError", (error) => errors.push(error));
  const dom = new JSDOM("<!doctype html><html><head></head><body></body></html>", {
    url: "http://localhost/",
    pretendToBeVisual: true,
    runScripts: "outside-only",
    virtualConsole: virtual_console,
  });
  const requests = [];
  const source = { source_id: "local", kind: "local", label: "Local", target_name: null };
  dom.window.fetch = async (url) => {
    requests.push(url);
    const path = new URL(url, "http://localhost").pathname;
    if (path === "/api/catalog") return reply({
      project_name: "Bootstrap fixture",
      access_mode: "local",
      sources: [source],
      initial_source: source.source_id,
      warnings: [],
    });
    if (path === "/api/run-columns") return reply({
      available_columns: { params: [], metrics: [], truncated: false },
      warnings: [],
    });
    if (path === "/api/runs") return reply({
      source,
      runs: [],
      offset: 0,
      limit: 100,
      total_count: 0,
      next_offset: null,
      warnings: [],
    });
    return reply({ error: "Unavailable endpoint" }, 404);
  };
  if (stored !== null) dom.window.localStorage.setItem(storage_key, stored);
  if (storage_unavailable) Object.defineProperty(dom.window, "localStorage", {
    get() { throw new Error("Browser storage unavailable"); },
  });
  t.after(() => {
    dom.window.close();
    assert.deepEqual(errors, []);
  });
  return {
    document: dom.window.document,
    window: dom.window,
    requests,
    run() { dom.window.eval(production_bundle); },
    parseBody(page) {
      const template = new JSDOM(pages[page]);
      dom.window.document.body.className = template.window.document.body.className;
      dom.window.document.body.innerHTML = template.window.document.body.innerHTML;
      template.window.close();
    },
    ready() { dom.window.document.dispatchEvent(new dom.window.Event("DOMContentLoaded")); },
  };
}

async function settled(predicate) {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  assert.fail("dashboard bootstrap did not settle");
}

test("production loads as a blocking classic script before the dashboard stylesheet", () => {
  const package_config = JSON.parse(readFileSync(new URL("./package.json", import.meta.url), "utf8"));
  assert.match(package_config.scripts.build, /--format=iife\b/);
  assert.match(package_config.scripts["build:test"], /--format=esm\b/);
  assert.doesNotThrow(() => new Script(production_bundle));
  const dom = new JSDOM(pages.dashboard);
  const scripts = [...dom.window.document.querySelectorAll("script")];
  assert.equal(scripts.length, 1);
  assert.equal(scripts[0].getAttribute("src"), "/app.js");
  assert.equal(scripts[0].textContent, "");
  for (const attribute of ["type", "async", "defer"]) assert.equal(scripts[0].hasAttribute(attribute), false);
  assert.ok(pages.dashboard.indexOf('<script src="/app.js"') < pages.dashboard.indexOf('<link rel="stylesheet"'));
  dom.window.close();
});

test("head bootstrap restores appearance before parsing the dashboard and mounts once when ready", async (t) => {
  const fixture = browserFixture(t, { stored: "dark" });
  assert.equal(fixture.document.readyState, "loading");
  fixture.run();
  assert.equal(fixture.document.documentElement.dataset.theme, "dark");
  assert.equal(fixture.document.documentElement.style.colorScheme, "dark");
  assert.equal(fixture.document.getElementById("dashboard-root"), null);
  assert.deepEqual(fixture.requests, []);
  fixture.parseBody("dashboard");
  fixture.ready();
  await settled(() => fixture.document.getElementById("refresh-button")?.disabled === false);
  const chart = fixture.document.getElementById("chart-preview");
  assert.ok(chart);
  assert.equal(fixture.requests.filter((url) => new URL(url, "http://localhost").pathname === "/api/catalog").length, 1);
  fixture.ready();
  await Promise.resolve();
  assert.equal(fixture.document.getElementById("chart-preview"), chart);
  assert.equal(fixture.requests.filter((url) => new URL(url, "http://localhost").pathname === "/api/catalog").length, 1);
});

test("head bootstrap defaults to light when browser storage is unavailable", (t) => {
  const fixture = browserFixture(t, { storage_unavailable: true });
  fixture.run();
  assert.equal(fixture.document.documentElement.dataset.theme, "light");
  assert.deepEqual(fixture.requests, []);
});

test("login remains scriptless and keeps password authentication in the native POST form", () => {
  const dom = new JSDOM(pages.login);
  assert.equal(dom.window.document.querySelector("script"), null);
  assert.doesNotMatch(pages.login, /localStorage|Bearer\s|type="hidden"/i);
  const form = dom.window.document.querySelector("form");
  assert.equal(form.method, "post");
  assert.equal(form.getAttribute("action"), "/login");
  assert.equal(form.querySelector('input[name="password"]').autocomplete, "current-password");
  assert.equal(form.querySelector('input[name="password"]').value, "");
  dom.window.close();
});

test("an imported test bundle cannot mount a later document on an earlier document's readiness event", async (t) => {
  const initial = new JSDOM("<!doctype html><html><body></body></html>", { url: "http://localhost/" });
  const later = new JSDOM(pages.dashboard, { url: "http://localhost/" });
  Object.defineProperty(initial.window.document, "readyState", { configurable: true, value: "loading" });
  const names = ["window", "document", "fetch"];
  const previous = new Map(names.map((name) => [name, Object.getOwnPropertyDescriptor(globalThis, name)]));
  let request_count = 0;
  Object.defineProperty(globalThis, "fetch", {
    configurable: true,
    writable: true,
    value: () => { request_count += 1; throw new Error("Unexpected dashboard mount"); },
  });
  for (const name of ["window", "document"]) Object.defineProperty(globalThis, name, {
    configurable: true,
    writable: true,
    value: name === "window" ? initial.window : initial.window.document,
  });
  t.after(() => {
    initial.window.close();
    later.window.close();
    for (const [name, descriptor] of previous) {
      if (descriptor) Object.defineProperty(globalThis, name, descriptor);
      else Reflect.deleteProperty(globalThis, name);
    }
  });
  await import("./.test/app.js?bootstrap-document-lifetime");
  globalThis.window = later.window;
  globalThis.document = later.window.document;
  initial.window.document.dispatchEvent(new initial.window.Event("DOMContentLoaded"));
  await Promise.resolve();
  assert.equal(request_count, 0);
  assert.equal(later.window.document.getElementById("dashboard-root").children.length, 0);
});
