import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { artifactCanSelect, artifactDownloadUrl, artifactLabels, artifactPullCommand, artifactScopeMatchesRun, artifactSyncError, artifactSyncStatus, apiUrl, formatFileSize, parseProjectRunKey, runIdentity, safeArtifactPath } from "./.test/app.js";

const scope = { project_id: "vision", origin: "gpu-1", run_id: "run-123" };
const source_id = "hosted:vision:gpu-1";
function file(path, fields = {}) { return { path, size: 1024, cloud: true, local: null, worker: true, download_url: apiUrl("/api/artifact", { source: source_id, run_id: scope.run_id, path }), ...fields }; }

test("artifact links remain same-origin and bind the exact catalog source, run and output path", () => {
  const item = file("outputs/model 'two' $HOME.pt");
  assert.equal(artifactDownloadUrl(item, source_id, scope.run_id), item.download_url);
  for (const download_url of [
    "https://evil.test/download", "javascript:alert(1)", "//evil.test/api/artifact", "/api/artifact-extra?path=x",
    item.download_url + "#other", item.download_url + "&path=outputs/other.pt", item.download_url + "&extra=anything",
    apiUrl("/api/artifact", { source: source_id, run_id: "run-other", path: item.path }),
    apiUrl("/api/artifact", { source: "hosted:another", run_id: scope.run_id, path: item.path }),
    apiUrl("/api/artifact", { source: source_id, run_id: scope.run_id, path: "outputs/different.pt" }),
  ]) assert.equal(artifactDownloadUrl({ ...item, download_url }, source_id, scope.run_id), null, download_url);
  for (const path of ["code/train.py", "outputs/../private", "outputs//model.pt", "outputs/./model.pt", "outputs/a\0b", "outputs/.expri-artifacts.json", "outputs/nested/.private/model.pt", "outputs/node_modules/model.pt", "outputs/__pycache__/model.pt", "outputs/cache/model.pt", "outputs/nested/cache/model.pt", "outputs/a\\b.pt", "outputs/a\nb.pt", "outputs/a\u0085b.pt", "outputs/" + "字".repeat(400)]) {
    assert.equal(safeArtifactPath(path), false);
    assert.equal(artifactDownloadUrl(file(path), source_id, scope.run_id), null);
  }
  assert.equal(artifactDownloadUrl({ ...item, download_url: null }, source_id, scope.run_id), null);
});

test("cached cloud files can be selected for the CLI without a native download URL", () => {
  const item = file("outputs/not-pulled.pt", { local: false, download_url: null });
  assert.equal(artifactCanSelect(item, scope, source_id, scope.run_id), true);
  assert.equal(artifactCanSelect(item, null, source_id, scope.run_id), false);
  assert.equal(artifactCanSelect(item, { ...scope, run_id: "different" }, source_id, scope.run_id), false);
  assert.equal(artifactCanSelect({ ...item, cloud: false }, scope, source_id, scope.run_id), false);
  assert.equal(artifactCanSelect({ ...item, download_url: "https://evil.test/file" }, scope, source_id, scope.run_id), false);
});

test("resumable pull commands preserve literal shell arguments and include only selected cloud files", () => {
  const path = "outputs/model 'two' $(printf unsafe) `echo unsafe` $HOME.pt";
  const files = [file(path), file("outputs/local.pt", { cloud: false, local: true }), file("outputs/unselected.pt")];
  const config = "/private/client's $(printf unsafe) $HOME.toml";
  const command = artifactPullCommand(scope, files, [path, "outputs/local.pt", path], config);
  assert.ok(command);
  const output = execFileSync("/bin/sh", ["-c", "expri() { printf '%s\\0' \"$@\"; }\n" + command]);
  const args = output.toString().split("\0").slice(0, -1);
  assert.deepEqual(args, ["service", "pull", "--config", config, "--project-id", "vision", "--origin", "gpu-1", "--run-id", "run-123", "--repo", ".", "--artifact", path]);
});

test("pull commands require a client path and valid scope and bound artifact selection", () => {
  const files = Array.from({ length: 65 }, (_, index) => file(`outputs/${index}.pt`));
  assert.equal(artifactPullCommand(scope, files, [files[0].path], ""), null);
  assert.equal(artifactPullCommand(scope, files, [files[0].path], "client\0.toml"), null);
  assert.equal(artifactPullCommand(null, files, [files[0].path], "owner.toml"), null);
  assert.equal(artifactPullCommand({ ...scope, origin: "../worker" }, files, [files[0].path], "owner.toml"), null);
  assert.equal(artifactPullCommand(scope, files, files.map(file => file.path), "owner.toml"), null);
  assert.equal(artifactPullCommand(scope, [file("outputs/../secret")], ["outputs/../secret"], "owner.toml"), null);
  assert.equal(artifactPullCommand(scope, [file("outputs/local.pt", { cloud: false })], ["outputs/local.pt"], "owner.toml"), null);
});

test("file sizes distinguish unknown values and binary units", () => {
  assert.equal(formatFileSize(0), "0 B");
  assert.equal(formatFileSize(1024), "1 KiB");
  assert.equal(formatFileSize(2 ** 30), "1 GiB");
  assert.equal(formatFileSize(-1), "Unknown");
  assert.equal(formatFileSize(Infinity), "Unknown");
});

test("sync states distinguish worker reports from confirmed cloud and local receipts", () => {
  const item = file("outputs/model.pt", { cloud: false, sync_status: "registered" });
  assert.equal(artifactSyncStatus(item), "Registered");
  assert.equal(artifactSyncStatus({ ...item, sync_status: "uploading" }), "Uploading");
  assert.equal(artifactSyncStatus({ ...item, sync_status: "needs_attention" }), "Needs attention");
  assert.equal(artifactSyncStatus({ ...item, sync_error: "Upload will retry" }), "Retrying upload");
  assert.equal(artifactSyncStatus({ ...item, sync_status: "cloud" }), "Awaiting cloud confirmation");
  assert.equal(artifactSyncStatus({ ...item, cloud: true, sync_status: "uploading" }), "Uploading");
  assert.equal(artifactSyncStatus({ ...item, cloud: true, sync_status: "cloud" }), "Available in cloud");
  assert.equal(artifactSyncStatus({ ...item, cloud: true, sync_status: "cloud", downloaded: true }), "Downloaded locally");
  assert.equal(artifactSyncStatus({ ...item, cloud: true, downloaded: true }), "Registered");
  assert.equal(artifactSyncError({ ...item, cloud: true, sync_status: "cloud", sync_error: "stale error" }), null);
  for (const sync_error of ["x".repeat(513), "line\nbreak", "\0private", 5])
    assert.equal(artifactSyncError({ ...item, sync_error }), null);
  assert.equal(artifactSyncError({ ...item, sync_status: "future", sync_error: "future error" }), null);
  assert.deepEqual(artifactLabels({ ...item, labels: ["best", "latest"] }), ["best", "latest"]);
  assert.deepEqual(artifactLabels({ ...item, labels: ["best", "best"] }), ["best"]);
  assert.deepEqual(artifactLabels({ ...item, labels: ["<script>", "latest"] }), ["latest"]);
  assert.deepEqual(artifactLabels({ ...item, labels: ["best", "latest", "unknown"] }), []);
});

test("project artifacts bind the recorded machine and actual run ID before browser or CLI selection", () => {
  const source = "hosted-project:vision", key = "gpu-1:run-123";
  const item = file("outputs/model.pt", { download_url: apiUrl("/api/artifact", { source, run_id: key, path: "outputs/model.pt" }) });
  assert.equal(runIdentity({ run_id: "run-123", run_key: key }), key);
  assert.deepEqual(parseProjectRunKey(key), { origin: "gpu-1", run_id: "run-123" });
  assert.equal(artifactScopeMatchesRun(scope, source, key), true);
  assert.equal(artifactDownloadUrl(item, source, key, scope), item.download_url);
  assert.equal(artifactCanSelect(item, scope, source, key), true);
  for (const invalid of [null, { ...scope, origin: "gpu-2" }, { ...scope, project_id: "other" }, { ...scope, run_id: key }]) {
    assert.equal(artifactScopeMatchesRun(invalid, source, key), false);
    assert.equal(artifactDownloadUrl(item, source, key, invalid), null);
    assert.equal(artifactCanSelect(item, invalid, source, key), false);
  }
  for (const invalid of ["run-123", "gpu-1:run-123:extra", "../gpu:run-123", "gpu-1:", "gpu-1:.."]) {
    assert.equal(parseProjectRunKey(invalid), null);
    assert.equal(artifactScopeMatchesRun(scope, source, invalid), false);
  }
  assert.equal(artifactDownloadUrl(item, source, key), null, "project downloads require their actual stored scope");
});
