import test from "node:test";
import assert from "node:assert/strict";
import {
  StorageManagementController, storageStatsValid, deletionPreviewValid, projectDeletionValid,
} from "./.test/app.js";

const project_id = "vision";
function stats(project = project_id, revision = "1") {
  return { project_id: project, revision, file_count: 4, logical_bytes: 16_384,
    object_count: 2, object_bytes: 8_192, shared_reference_count: 2,
    retained_object_count: 1, retained_object_bytes: 1_024, pending_upload_count: 1,
    pending_upload_bytes: 512, tracking_bytes: 256,
    reclaimable_object_count: 3, reclaimable_object_bytes: 9_216 };
}
function preview(project = project_id, revision = "1") {
  return { project_id: project, revision, run_count: 2, stats: stats(project, revision) };
}
function deletion(project = project_id, status = "pending") {
  return { project_id: project, status, pending_tasks: status === "deleted" ? 0 : 2,
    deleted_objects: status === "deleted" ? 3 : 1, aborted_uploads: 1, last_error: null };
}
function response(value, status = 200) {
  return { ok: status >= 200 && status < 300, status, json: async () => value };
}
async function settled() { for (let index = 0; index < 32; index++) await Promise.resolve(); }
function fixture(t, override = () => null, options = {}) {
  const requests = [], timers = new Map(), stored = new Map();
  let next_timer = 0;
  const controller = new StorageManagementController({
    request: async (url, init) => {
      const parsed = new URL(url, "https://expri.test");
      requests.push({ url: parsed, ...init });
      const custom = override(parsed, init);
      if (custom) return custom;
      const project = parsed.searchParams.get("project_id");
      if (parsed.pathname === "/api/storage/stats") return response({ stats: stats(project), delete_enabled: true });
      if (parsed.pathname === "/api/projects/delete-preview") return response(preview(project));
      if (parsed.pathname === "/api/projects/delete") return response(deletion(JSON.parse(init.body).project_id), 202);
      if (parsed.pathname === "/api/projects/deletion") return response(deletion(project));
      assert.fail(`Unexpected request: ${url}`);
    },
    schedule: (callback, delay) => { const key = ++next_timer; timers.set(key, { callback, delay }); return key; },
    cancel: (key) => { timers.delete(key); },
    storage: { getItem: key => stored.get(key) ?? null, setItem: (key, value) => stored.set(key, value), removeItem: key => stored.delete(key) },
    ...options,
  });
  t.after(() => controller.dispose());
  return { controller, requests, timers, stored,
    async tick(delay = 5_000) {
      const jobs = [...timers.entries()].filter(([, item]) => item.delay === delay);
      for (const [key, item] of jobs) { timers.delete(key); item.callback(); }
      await settled();
    },
    posts() { return requests.filter(item => item.method === "POST"); },
  };
}

test("storage management validates scope and all counters without treating file totals as object bytes", () => {
  assert.equal(storageStatsValid(stats(), project_id), true);
  assert.equal(storageStatsValid(stats("other"), project_id), false);
  assert.equal(storageStatsValid({ ...stats(), object_bytes: -1 }, project_id), false);
  assert.equal(storageStatsValid({ ...stats(), tracking_bytes: Number.MAX_SAFE_INTEGER + 1 }, project_id), false);
  assert.equal(storageStatsValid({ ...stats(), retained_object_count: undefined }, project_id), false);
  assert.equal(deletionPreviewValid({ ...preview(), stats: stats("other") }, project_id), false);
  assert.equal(projectDeletionValid(deletion(), project_id), true);
  assert.equal(projectDeletionValid(deletion(project_id, "needs_attention"), project_id), true);
  assert.equal(projectDeletionValid({ ...deletion(), status: "failed" }, project_id), false);
});

test("preview and cancel are read-only; disabled dashboards never request a preview", async t => {
  const f = fixture(t);
  f.controller.setContext(project_id, true, true);
  await settled();
  await f.controller.openPreview();
  assert.equal(f.controller.getSnapshot().preview.stats.shared_reference_count, 2);
  f.controller.close();
  assert.equal(f.controller.getSnapshot().phase, "closed");
  assert.equal(f.posts().length, 0);
  assert.equal(f.stored.size, 0);
  f.controller.setContext(null, false, true);
  await f.controller.openPreview();
  assert.equal(f.requests.filter(item => item.url.pathname.endsWith("delete-preview")).length, 1);
  const disabled = fixture(t, parsed => parsed.pathname === "/api/storage/stats"
    ? response({ stats: stats(), delete_enabled: false }) : null);
  disabled.controller.setContext(project_id, true, true);
  await settled();
  await disabled.controller.openPreview();
  assert.equal(disabled.requests.length, 1);
  assert.equal(disabled.controller.getSnapshot().phase, "closed");
});

test("explicit deletion pins preview and confirmation; status survives catalog removal and polls only reads", async t => {
  let done = false;
  const f = fixture(t, parsed => parsed.pathname === "/api/projects/deletion"
    ? response(deletion(project_id, done ? "deleted" : "pending")) : null);
  f.controller.setContext(project_id, true, true);
  await settled();
  await f.controller.openPreview();
  await f.controller.submit("other", "secret");
  await f.controller.submit(project_id, "");
  assert.equal(f.posts().length, 0);
  await f.controller.submit(project_id, "secret");
  assert.deepEqual(JSON.parse(f.posts()[0].body), { project_id, revision: "1", confirmation: project_id, password: "secret" });
  assert.equal(f.posts()[0].credentials, "same-origin");
  assert.equal(JSON.stringify(f.controller.getSnapshot()).includes("secret"), false);
  assert.deepEqual([...f.stored.values()], [project_id]);
  f.controller.setContext(null, false, true);
  assert.equal(f.controller.getSnapshot().deletion.project_id, project_id);
  f.controller.close();
  await f.tick();
  assert.equal(f.controller.getSnapshot().deletion.pending_tasks, 2);
  done = true;
  await f.tick();
  assert.equal(f.controller.getSnapshot().deletion.status, "deleted");
  assert.equal(f.stored.size, 0);
  assert.equal(f.posts().length, 1);
});

test("stale previews and ambiguous network failures require review or a status read, never deletion replay", async t => {
  let failure = "stale", revision = "1";
  const f = fixture(t, parsed => {
    if (parsed.pathname === "/api/projects/delete-preview") return response(preview(project_id, revision));
    if (parsed.pathname === "/api/projects/delete") {
      if (failure === "stale") return response({ error: "changed" }, 409);
      throw new Error("network disconnected after sending");
    }
    return null;
  });
  f.controller.setContext(project_id, true, true);
  await settled();
  await f.controller.openPreview();
  await f.controller.submit(project_id, "secret");
  assert.equal(f.controller.getSnapshot().stale, true);
  await f.controller.submit(project_id, "secret");
  await f.tick();
  assert.equal(f.posts().length, 1);
  revision = "2";
  await f.controller.openPreview();
  failure = "network";
  await f.controller.submit(project_id, "secret");
  assert.equal(f.controller.getSnapshot().phase, "status");
  assert.match(f.controller.getSnapshot().error, /Check its status/);
  f.controller.close();
  await f.controller.openPreview();
  assert.equal(f.controller.getSnapshot().phase, "status", "an uncertain request cannot be replaced by a new preview");
  await f.controller.checkStatus();
  await f.tick();
  assert.equal(f.posts().length, 2);
});

test("source switches discard late stats and deletion previews and cannot retarget a confirmation", async t => {
  let finish;
  const f = fixture(t, parsed => parsed.pathname === "/api/projects/delete-preview"
    ? new Promise(resolve => { finish = resolve; }) : null);
  f.controller.setContext(project_id, true, true);
  await settled();
  const first = f.controller.openPreview();
  f.controller.setContext("other", true, true);
  finish(response(preview()));
  await first;
  await settled();
  assert.equal(f.controller.getSnapshot().phase, "closed");
  assert.equal(f.controller.getSnapshot().preview, null);
  assert.equal(f.controller.getSnapshot().stats.project_id, "other");
  await f.controller.submit(project_id, "secret");
  assert.equal(f.posts().length, 0);
});

test("cleanup polling is bounded, status failures are actionable, and manual checks never resubmit", async t => {
  const f = fixture(t);
  f.controller.setContext(project_id, true, false);
  await settled();
  await f.controller.openPreview();
  await f.controller.submit(project_id, "secret");
  for (let index = 0; index < 15; index++) await f.tick();
  assert.equal(f.requests.filter(item => item.url.pathname === "/api/projects/deletion").length, 12);
  await f.controller.checkStatus();
  assert.equal(f.requests.filter(item => item.url.pathname === "/api/projects/deletion").length, 13);
  assert.equal(f.posts().length, 1);
  const failed = fixture(t, parsed => parsed.pathname === "/api/projects/deletion"
    ? response({ error: "service restarting" }, 503) : null);
  failed.controller.setContext(project_id, true, false);
  await settled();
  await failed.controller.openPreview();
  await failed.controller.submit(project_id, "secret");
  await failed.tick();
  assert.match(failed.controller.getSnapshot().error, /Check status when/);
  await failed.tick();
  assert.equal(failed.requests.filter(item => item.url.pathname === "/api/projects/deletion").length, 1);
  assert.equal(failed.posts().length, 1);
});

test("pending cleanup identity survives a page restart without saving or replaying credentials", async t => {
  const f = fixture(t, parsed => parsed.pathname === "/api/projects/deletion"
    ? response(deletion(project_id, "deleted")) : null);
  f.stored.set("expri.project_deletion", project_id);
  f.controller.setContext(null, false, false);
  await settled();
  assert.equal(f.controller.getSnapshot().phase, "status");
  assert.equal(f.controller.getSnapshot().deletion.status, "deleted");
  assert.equal(f.posts().length, 0);
  assert.equal(f.requests.length, 1);
  assert.equal(f.stored.size, 0);
});

test("cleanup needing attention stays unfinished, polls only reads, and recovers without repeating deletion", async t => {
  let status = "needs_attention";
  const error = "S3 denied cleanup. Grant delete permission to the service credentials.";
  const f = fixture(t, parsed => {
    if (parsed.pathname === "/api/projects/delete" || parsed.pathname === "/api/projects/deletion")
      return response({ ...deletion(project_id, status), last_error: status === "needs_attention" ? error : null });
    return null;
  });
  f.controller.setContext(project_id, true, false);
  await settled();
  await f.controller.openPreview();
  await f.controller.submit(project_id, "secret");
  assert.equal(f.controller.getSnapshot().deletion.status, "needs_attention");
  assert.equal(f.controller.getSnapshot().deletion.last_error, error);
  assert.deepEqual([...f.stored.values()], [project_id]);
  f.controller.close();
  await f.controller.openPreview();
  assert.equal(f.controller.getSnapshot().phase, "status");
  assert.equal(f.requests.filter(item => item.url.pathname === "/api/projects/delete-preview").length, 1,
    "an unfinished cleanup cannot obtain another destructive preview");
  for (let index = 0; index < 15; index++) await f.tick();
  assert.equal(f.requests.filter(item => item.url.pathname === "/api/projects/deletion").length, 13,
    "attention status uses the same bounded twelve read polls after the manual status check");
  assert.deepEqual([...f.stored.values()], [project_id]);
  status = "pending";
  await f.controller.checkStatus();
  assert.equal(f.controller.getSnapshot().deletion.status, "pending");
  assert.equal(f.controller.getSnapshot().deletion.last_error, null);
  assert.deepEqual([...f.stored.values()], [project_id]);
  status = "deleted";
  await f.controller.checkStatus();
  assert.equal(f.controller.getSnapshot().deletion.status, "deleted");
  assert.equal(f.stored.size, 0);
  assert.equal(f.posts().length, 1);
});

test("a page restart recovers cleanup needing attention through reads without storing credentials", async t => {
  let status = "needs_attention", visible = true;
  const f = fixture(t, parsed => parsed.pathname === "/api/projects/deletion"
    ? response({ ...deletion(project_id, status), last_error: status === "needs_attention" ? "S3 access denied." : null })
    : null, { visible: () => visible });
  f.stored.set("expri.project_deletion", project_id);
  f.controller.setContext(null, false, false);
  await settled();
  assert.equal(f.controller.getSnapshot().phase, "status");
  assert.equal(f.controller.getSnapshot().deletion.status, "needs_attention");
  assert.deepEqual([...f.stored.entries()], [["expri.project_deletion", project_id]]);
  visible = false;
  await f.tick();
  assert.equal(f.requests.length, 1, "hidden recovery pauses status reads");
  visible = true;
  status = "deleted";
  await f.tick();
  assert.equal(f.controller.getSnapshot().deletion.status, "deleted");
  assert.equal(f.controller.getSnapshot().deletion.last_error, null);
  assert.equal(f.requests.length, 2);
  assert.equal(f.posts().length, 0);
  assert.equal(f.stored.size, 0);
});

test("invalid dashboard sessions navigate to login and disable destructive controls", async t => {
  const previous_location = globalThis.location, redirects = [];
  globalThis.location = { assign: value => redirects.push(value) };
  t.after(() => {
    if (previous_location === undefined) delete globalThis.location;
    else globalThis.location = previous_location;
  });
  const f = fixture(t, () => response({ error: "not authenticated" }, 401));
  f.controller.setContext(project_id, true, false);
  await settled();
  assert.deepEqual(redirects, ["/login"]);
  assert.equal(f.controller.getSnapshot().delete_enabled, false);
  await f.controller.openPreview();
  assert.equal(f.requests.length, 1);
  assert.equal(f.posts().length, 0);
});

test("late statistics cannot overwrite the usage of another project", async t => {
  let finish;
  const f = fixture(t, parsed => parsed.pathname === "/api/storage/stats" && parsed.searchParams.get("project_id") === project_id
    ? new Promise(resolve => { finish = resolve; }) : null);
  f.controller.setContext(project_id, true, true);
  f.controller.setContext("other", true, true);
  await settled();
  finish(response({ stats: stats(project_id), delete_enabled: true }));
  await settled();
  assert.equal(f.controller.getSnapshot().stats.project_id, "other");
  assert.equal(f.controller.getSnapshot().stats_loading, false);
});

test("usage polling pauses while hidden or disabled and backs off when the connection fails", async t => {
  let visible = true, failing = false;
  const f = fixture(t, parsed => parsed.pathname === "/api/storage/stats" && failing
    ? response({ error: "network unavailable" }, 503) : null, { visible: () => visible });
  f.controller.setContext(project_id, true, true);
  await settled();
  visible = false;
  await f.tick();
  assert.equal(f.requests.length, 1);
  visible = true;
  failing = true;
  await f.tick();
  assert.equal(f.requests.length, 2);
  assert.equal(f.controller.getSnapshot().delete_enabled, false);
  await f.tick();
  assert.equal(f.requests.length, 2, "failure no longer checks every five seconds");
  await f.tick(10_000);
  assert.equal(f.requests.length, 3);
  failing = false;
  await f.tick(20_000);
  assert.equal(f.requests.length, 4);
  await f.tick();
  assert.equal(f.requests.length, 5, "recovery restores the five-second cadence");
  f.controller.setContext(project_id, true, false);
  await f.tick();
  assert.equal(f.requests.length, 5);
});

test("timed-out statistics, previews and cleanup reads leave actionable controls", async t => {
  const stall = init => new Promise((_, reject) => {
    init.signal.addEventListener("abort", () => reject(new Error("aborted")), { once: true });
  });
  const usage = fixture(t, (parsed, init) => parsed.pathname === "/api/storage/stats" ? stall(init) : null);
  usage.controller.setContext(project_id, true, true);
  await usage.tick(30_000);
  assert.equal(usage.controller.getSnapshot().stats_loading, false);
  assert.match(usage.controller.getSnapshot().stats_error, /timed out/);
  assert.equal(usage.controller.getSnapshot().delete_enabled, false);
  const preview = fixture(t, (parsed, init) => parsed.pathname === "/api/projects/delete-preview" ? stall(init) : null);
  preview.controller.setContext(project_id, true, false);
  await settled();
  const pending = preview.controller.openPreview();
  await preview.tick(30_000);
  await pending;
  assert.equal(preview.controller.getSnapshot().phase, "review");
  assert.equal(preview.controller.getSnapshot().stale, true);
  assert.match(preview.controller.getSnapshot().error, /timed out/);
  const cleanup = fixture(t, (parsed, init) => parsed.pathname === "/api/projects/deletion" ? stall(init) : null);
  cleanup.controller.setContext(project_id, true, false);
  await settled();
  await cleanup.controller.openPreview();
  await cleanup.controller.submit(project_id, "secret");
  await cleanup.tick();
  await cleanup.tick(30_000);
  assert.equal(cleanup.controller.getSnapshot().checking_status, false);
  assert.match(cleanup.controller.getSnapshot().error, /timed out/);
  assert.equal(cleanup.posts().length, 1);
});
