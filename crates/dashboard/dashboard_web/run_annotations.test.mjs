import test from "node:test";
import assert from "node:assert/strict";
import {
  normalizeRunAnnotations,
  runAnnotationsMatchesScope,
  runAnnotationsScope,
  runIdentity,
  runLabel,
  runNickname,
  runTags,
} from "./.test/app.js";

const scope = { project_id: "vision", origin: "gpu-a", run_id: "run" };
const annotations = { scope, nickname: "Baseline", tags: ["baseline", "gpu"] };
const run = { run_id: "run", run_key: "gpu-a:run", origin: "gpu-a", nickname: "Old name", tags: ["old"], annotations };

test("annotations trim text, clear nicknames and deduplicate exact tags in order", () => {
  assert.deepEqual(normalizeRunAnnotations("  Baseline  ", [" gpu ", "gpu", "GPU", "baseline"]), {
    nickname: "Baseline", tags: ["gpu", "GPU", "baseline"],
  });
  assert.deepEqual(normalizeRunAnnotations("  ", []), { nickname: null, tags: [] });
  assert.deepEqual(normalizeRunAnnotations(null, []), { nickname: null, tags: [] });
  assert.deepEqual(normalizeRunAnnotations("\u2003Name\u202f", ["\u3000tag\u00a0"]), { nickname: "Name", tags: ["tag"] });
  assert.deepEqual(normalizeRunAnnotations("\ufeffName\ufeff", ["\ufefftag\ufeff"]), { nickname: "\ufeffName\ufeff", tags: ["\ufefftag\ufeff"] });
  assert.deepEqual(normalizeRunAnnotations("\ufeff", ["new-tag"]), { nickname: "\ufeff", tags: ["new-tag"] });
});

test("annotation bounds count Unicode characters and reject controls or empty tags", () => {
  const emoji = "😀";
  assert.equal(normalizeRunAnnotations(emoji.repeat(128), [emoji.repeat(48)]).nickname.length, 256);
  assert.throws(() => normalizeRunAnnotations(emoji.repeat(129), []), /128 characters/);
  assert.throws(() => normalizeRunAnnotations(null, [emoji.repeat(49)]), /48 characters/);
  assert.throws(() => normalizeRunAnnotations("bad\u0000name", []), /control characters/);
  assert.throws(() => normalizeRunAnnotations("\tname\n", []), /control characters/);
  assert.throws(() => normalizeRunAnnotations(null, ["bad\u0085tag"]), /control characters/);
  assert.throws(() => normalizeRunAnnotations(null, [" "]), /cannot be empty/);
  assert.throws(() => normalizeRunAnnotations(12, []), /must be text/);
  assert.throws(() => normalizeRunAnnotations(null, "tag"), /must be a list/);
  assert.throws(() => normalizeRunAnnotations(null, Array.from({ length: 17 }, (_, index) => `tag-${index}`)), /16 tags/);
  assert.throws(() => normalizeRunAnnotations(null, Array(17).fill("one")), /16 tags/);
});

test("annotation replies must be normalized and match the exact service scope", () => {
  assert.equal(runAnnotationsMatchesScope(annotations, scope), true);
  for (const field of ["project_id", "origin", "run_id"]) {
    assert.equal(runAnnotationsMatchesScope({ ...annotations, scope: { ...scope, [field]: "other" } }, scope), false);
  }
  for (const invalid of [null, {}, { ...annotations, nickname: undefined }, { ...annotations, nickname: " " },
    { ...annotations, tags: ["gpu", "gpu"] }, { ...annotations, tags: [" gpu "] }, { ...annotations, tags: [null] },
    { ...annotations, scope: { ...scope, origin: "../gpu-a" } }]) {
    assert.equal(runAnnotationsMatchesScope(invalid, scope), false);
  }
});

test("annotation capabilities match project identities or scoped worker sources", () => {
  const project = { source_id: "hosted-project:vision", project_id: "vision", kind: "hosted_project" };
  const worker = { source_id: "hosted:vision:gpu-a", project_id: "vision", origin: "gpu-a", kind: "service" };
  assert.deepEqual(runAnnotationsScope(run, project), scope);
  assert.deepEqual(runAnnotationsScope(run, worker), scope);
  assert.equal(runAnnotationsScope({ ...run, run_key: "gpu-b:run" }, project), null);
  assert.equal(runAnnotationsScope(run, { ...project, project_id: "other" }), null);
  assert.equal(runAnnotationsScope(run, { ...worker, origin: "gpu-b" }), null);
  assert.equal(runAnnotationsScope({ ...run, annotations: null }, worker), null);
  assert.equal(runAnnotationsScope(run, { source_id: "local", kind: "local" }), null);
});

test("nicknames improve labels without changing run identities or hiding original IDs", () => {
  assert.equal(runIdentity(run), "gpu-a:run");
  assert.equal(runLabel(run), "Baseline (run · gpu-a)");
  assert.equal(runNickname(run), "Baseline");
  assert.deepEqual(runTags(run), ["baseline", "gpu"]);
  assert.equal(runNickname({ ...run, annotations: { ...annotations, nickname: null } }), null);
  assert.deepEqual(runTags({ ...run, annotations: { ...annotations, tags: [] } }), []);
  assert.deepEqual(runTags({ ...run, annotations: {} }), []);
  assert.deepEqual(runTags({ ...run, annotations: { tags: [null, 42, "safe"] } }), ["safe"]);
  assert.equal(runLabel({ run_id: "run", nickname: "Simple" }), "Simple (run)");
  assert.equal(runLabel({ run_id: "run", origin: "gpu-a" }), "run · gpu-a");
});
