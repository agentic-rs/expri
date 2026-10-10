import test from "node:test";
import assert from "node:assert/strict";
import { canArchiveRun, canRestoreRun, runArchivalMatchesScope, runArchivalScope, runIsArchived } from "./.test/app.js";

const scope = { project_id: "vision", origin: "gpu-a", run_id: "run" };
function record(status = "active") {
  return { scope, status, archived_at: status === "active" ? null : "2026-10-10T00:00:00Z",
    delete_after: status === "active" ? null : "2026-10-25T00:00:00Z", pending_tasks: 0, last_error: null };
}
function run(status = "completed", archival = record()) {
  return { run_id: "run", run_key: "gpu-a:run", origin: "gpu-a", status, archival };
}

test("only known finished training states can archive an active run", () => {
  for (const status of ["completed", "failed", "cancelled", "lost"]) assert.equal(canArchiveRun(run(status)), true);
  for (const status of ["preparing", "running", "unknown", "invented"]) assert.equal(canArchiveRun(run(status)), false);
  assert.equal(canArchiveRun(run("completed", record("archived"))), false);
  assert.equal(canArchiveRun(run("completed", null)), false);
});

test("restore is available strictly before the deadline and before deletion starts", () => {
  const deadline = Date.parse("2026-10-25T00:00:00Z");
  assert.equal(canRestoreRun(run("completed", record("archived")), deadline - 1), true);
  assert.equal(canRestoreRun(run("completed", record("archived")), deadline), false);
  assert.equal(canRestoreRun(run("completed", record("archived")), deadline + 1), false);
  for (const status of ["active", "deleting", "needs_attention", "deleted"]) {
    assert.equal(canRestoreRun(run("completed", record(status)), deadline - 1), false);
    assert.equal(runIsArchived(run("completed", record(status))), status !== "active");
  }
});

test("mutation replies must preserve the exact project, worker and run with valid retention data", () => {
  assert.equal(runArchivalMatchesScope(record(), scope), true);
  assert.equal(runArchivalMatchesScope(record("archived"), scope), true);
  for (const field of ["project_id", "origin", "run_id"]) {
    assert.equal(runArchivalMatchesScope({ ...record(), scope: { ...scope, [field]: "other" } }, scope), false);
  }
  for (const invalid of [null, {}, { ...record(), status: "finished" },
    { ...record(), pending_tasks: -1 }, { ...record(), pending_tasks: 0.5 },
    { ...record("archived"), delete_after: null }, { ...record("archived"), delete_after: "invalid" },
    { ...record("archived"), archived_at: null }, { ...record(), last_error: 12 }]) {
    assert.equal(runArchivalMatchesScope(invalid, scope), false);
  }
});

test("archive action scopes respect project run keys and scoped worker sources", () => {
  const project = { source_id: "hosted-project:vision", project_id: "vision", kind: "hosted_project" };
  const worker = { source_id: "hosted:vision:gpu-a", project_id: "vision", origin: "gpu-a", kind: "service" };
  assert.deepEqual(runArchivalScope(run(), project), scope);
  assert.deepEqual(runArchivalScope(run(), worker), scope);
  assert.equal(runArchivalScope({ ...run(), run_key: "gpu-b:run" }, project), null);
  assert.equal(runArchivalScope(run(), { ...worker, origin: "gpu-b" }), null);
  assert.equal(runArchivalScope(run(), { ...project, project_id: "other" }), null);
  assert.equal(runArchivalScope(run(), { source_id: "local", kind: "local" }), null);
});
