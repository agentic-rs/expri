import test from "node:test";
import assert from "node:assert/strict";
import {
  apiUrl,
  boundedStorageSearch,
  storageCatalogValid,
  storageInputDownloadUrl,
  storageOutputDownloadUrl,
  storageUrl,
} from "./.test/app.js";

const project_id = "vision";
const input = { input_id: "dataset-v1", size: 1024,
  download_url: apiUrl("/api/input", { project_id, input_id: "dataset-v1" }) };
const output = { origin: "gpu-a", run_id: "run-1", path: "outputs/checkpoint.pt", size: 2048,
  download_url: apiUrl("/api/artifact", { source: "hosted-project:vision", run_id: "gpu-a:run-1", path: "outputs/checkpoint.pt" }) };

test("storage download links bind exact project and object identity on the same origin", () => {
  assert.equal(storageInputDownloadUrl(input, project_id), input.download_url);
  assert.equal(storageOutputDownloadUrl(output, project_id), output.download_url);
  for (const download_url of [
    "https://evil.test/input", "//evil.test/api/input?project_id=vision&input_id=dataset-v1",
    "/api/input?project_id=other&input_id=dataset-v1",
    "/api/input?project_id=vision&input_id=other",
    input.download_url + "&input_id=other", input.download_url + "#fragment",
  ]) assert.equal(storageInputDownloadUrl({ ...input, download_url }, project_id), null, download_url);
  for (const download_url of [
    "https://evil.test/artifact", output.download_url + "&path=outputs/other.pt",
    apiUrl("/api/artifact", { source: "hosted-project:other", run_id: "gpu-a:run-1", path: output.path }),
    apiUrl("/api/artifact", { source: "hosted-project:vision", run_id: "gpu-b:run-1", path: output.path }),
    apiUrl("/api/artifact", { source: "hosted-project:vision", run_id: "gpu-a:run-1", path: "outputs/other.pt" }),
  ]) assert.equal(storageOutputDownloadUrl({ ...output, download_url }, project_id), null, download_url);
  assert.equal(storageOutputDownloadUrl({ ...output, path: "inputs/private" }, project_id), null);
  assert.equal(storageInputDownloadUrl({ ...input, input_id: "../secret" }, project_id), null);
});

test("storage catalogs and queries stay bound to one project, kind and bounded page", () => {
  const catalog = { project_id, kind: "input", items: [input], total_count: 1, next_offset: null };
  assert.equal(storageCatalogValid(catalog, project_id, "input", 0, 100), true);
  assert.equal(storageCatalogValid({ ...catalog, project_id: "other" }, project_id, "input", 0, 100), false);
  assert.equal(storageCatalogValid({ ...catalog, kind: "output" }, project_id, "input", 0, 100), false);
  assert.equal(storageCatalogValid({ ...catalog, items: [{ ...input, input_id: "../secret" }] }, project_id, "input", 0, 100), false);
  assert.equal(storageCatalogValid({ ...catalog, items: [null] }, project_id, "input", 0, 100), false);
  assert.equal(storageCatalogValid({ ...catalog, next_offset: 0 }, project_id, "input", 0, 100), false);
  assert.equal(storageCatalogValid({ ...catalog, items: Array.from({ length: 101 }, () => input) }, project_id, "input", 0, 100), false);
  const query = new URL(storageUrl(project_id, "output", 100, "checkpoint & v2"), "http://localhost");
  assert.deepEqual(Object.fromEntries(query.searchParams), { project_id, kind: "output", limit: "100", offset: "100", search: "checkpoint & v2" });
  const bounded = boundedStorageSearch("字".repeat(100));
  assert.equal(new TextEncoder().encode(bounded).length, 255);
  assert.equal(boundedStorageSearch("📦".repeat(100)).length, 128);
});
