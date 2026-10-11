import test from "node:test";
import assert from "node:assert/strict";
import { artifactAssetIds, assetRunUrl, consumedAssets, outputAssetReference, storageCatalogValid } from "./.test/app.js";

test("output references use only the repo-relative service and preserve exact encoded paths", () => {
  const producer = { origin: "worker-1", run_id: "run-abc123", path: "outputs/best.pt" };
  assert.equal(outputAssetReference(producer), "expri://./worker-1/run-abc123/outputs/best.pt");
  assert.equal(outputAssetReference({ ...producer, path: "outputs/model #1+λ.pt" }), "expri://./worker-1/run-abc123/outputs/model%20%231%2B%CE%BB.pt");
  for (const [path, encoded] of [
    ["outputs/best(1)*.pt", "outputs/best%281%29%2A.pt"],
    ["outputs/model's!.pt", "outputs/model%27s%21.pt"],
  ]) {
    const reference = outputAssetReference({ ...producer, path });
    assert.equal(reference, `expri://./worker-1/run-abc123/${encoded}`);
    assert.equal(reference.split("/").slice(5).map(decodeURIComponent).join("/"), path);
  }
  for (const patch of [{ origin: "../other" }, { run_id: "." }, { path: "outputs/../secret" }, { path: "inputs/file" }, { path: "outputs/a\\b" }])
    assert.equal(outputAssetReference({ ...producer, ...patch }), null);
  const url = new URL(assetRunUrl("example-project", producer), "https://service.example.net");
  assert.equal(url.pathname, "/");
  assert.deepEqual(Object.fromEntries(url.searchParams), { project_id: "example-project", origin: "worker-1", run_id: "run-abc123" });
  assert.equal(assetRunUrl("../project", producer), null);
});

test("consumed asset previews omit malformed entries and cap displayed relationships", () => {
  const producer = { origin: "worker-1", run_id: "run-1", path: "outputs/best.pt" };
  const asset = { path: "models/base.pt", source: { kind: "expri", input_id: "base-v1" }, producer };
  assert.deepEqual(consumedAssets({ state: { assets: [asset] } }), [{ path: "models/base.pt", source: "base-v1", producer }]);
  assert.equal(consumedAssets({ state: { assets: Array.from({ length: 64 }, () => asset) } }).length, 16);
  assert.deepEqual(consumedAssets({ state: { assets: [{ ...asset, path: "../secret" }, null] } }), []);
  assert.equal(consumedAssets({ state: { assets: [{ ...asset, producer: { ...producer, path: "outputs/../secret" } }] } })[0].producer, null);
  assert.equal(consumedAssets({ state: { assets: [{ path: "data/audio.tar.zst", source: { kind: "local" }, availability: { input_id: "audio-v1" } }] } })[0].source, "audio-v1");
});

test("storage lineage is bounded and refuses malformed producer or consumer identity", () => {
  const producer = { origin: "worker-1", run_id: "run-1", path: "outputs/best.pt", run_available: true };
  const consumer = { origin: "worker-2", run_id: "run-2", path: "models/base.pt" };
  const input = { input_id: "base-v1", size: 10, download_url: null, producer, used_by: [consumer], used_by_count: 1 };
  const catalog = { project_id: "example-project", kind: "input", items: [input], total_count: 1, next_offset: null };
  const valid = (item) => storageCatalogValid({ ...catalog, items: [item] }, "example-project", "input", 0, 100);
  assert.equal(valid(input), true);
  assert.equal(valid({ ...input, producer: { ...producer, origin: "../secret" } }), false);
  assert.equal(valid({ ...input, used_by: [{ ...consumer, path: "../secret" }] }), false);
  assert.equal(valid({ ...input, used_by: [{ ...consumer, origin: 1 }] }), false);
  assert.equal(valid({ ...input, used_by: Array.from({ length: 17 }, () => consumer), used_by_count: 17 }), false);
  assert.equal(valid({ ...input, used_by_count: 0 }), false);
});

test("run outputs show only bounded reusable asset identifiers", () => {
  assert.deepEqual(artifactAssetIds({ assets: [{ input_id: "base-v1" }, { input_id: "base-v1" }, { input_id: "../secret" }] }), ["base-v1"]);
  assert.deepEqual(artifactAssetIds({ assets: Array.from({ length: 17 }, () => ({ input_id: "base-v1" })) }), []);
});
