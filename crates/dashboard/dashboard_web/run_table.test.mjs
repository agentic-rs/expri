import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";

execFileSync("./node_modules/.bin/esbuild", [
  "run_table.ts", "--bundle", "--format=esm", "--target=es2022", "--outfile=.test/run_table.js",
], { cwd: new URL(".", import.meta.url), stdio: "pipe" });
const {
  formatRunTableValue, runColumnChoices, runColumnIdentity, runColumnSortKey,
  runColumnValue, runSortIsDefault, runSortLabel,
} = await import("./.test/run_table.js");

test("table cells preserve zero, false and empty strings while distinguishing unavailable values", () => {
  assert.deepEqual(formatRunTableValue(undefined), { text: "—", title: undefined, numeric: false });
  for (const value of [null, NaN, Infinity, {}, [1]])
    assert.deepEqual(formatRunTableValue(value), { text: "—", title: undefined, numeric: false });
  assert.deepEqual(formatRunTableValue(0), { text: "0", title: "0", numeric: true });
  assert.deepEqual(formatRunTableValue(false), { text: "false", title: "false", numeric: false });
  assert.deepEqual(formatRunTableValue(""), { text: '""', title: "Empty string", numeric: false });
  assert.equal(formatRunTableValue(1.23456789).title, "1.23456789");
});

test("table string previews and tooltips are bounded without splitting Unicode characters", () => {
  const value = "🧪".repeat(1500);
  const formatted = formatRunTableValue(value);
  assert.equal(Array.from(formatted.text).length, 80);
  assert.equal(formatted.text, "🧪".repeat(79) + "…");
  assert.equal(Array.from(formatted.title).length, 1024);
  assert.equal(formatRunTableValue("<script>alert(1)</script>").text, "<script>alert(1)</script>");
});

test("exact parameter paths and metric names cannot alias inherited properties or each other", () => {
  const run = {
    table_values: {
      params: JSON.parse('{"/optimizer/lr":0.01,"/literal~1slash":0,"__proto__":false}'),
      metrics: { "/optimizer/lr": 0.5 },
    },
  };
  const parameter = { kind: "param", key: "/optimizer/lr", label: "optimizer / lr" };
  const metric = { ...parameter, kind: "metric" };
  assert.equal(runColumnValue(run, parameter), 0.01);
  assert.equal(runColumnValue(run, metric), 0.5);
  assert.equal(runColumnValue(run, { ...parameter, key: "/literal~1slash" }), 0);
  assert.equal(runColumnValue(run, { ...parameter, key: "__proto__" }), false);
  assert.equal(runColumnValue(run, { ...parameter, key: "toString" }), undefined);
  assert.notEqual(runColumnIdentity(parameter), runColumnIdentity(metric));
  assert.equal(runColumnSortKey(parameter), "param:/optimizer/lr");
  assert.equal(runColumnSortKey(metric), "metric:/optimizer/lr");
});

test("column discovery retains selected columns outside the preview and sort labels describe the active order", () => {
  const selected = [{ kind: "param", key: "/learning_rate", label: "learning_rate" }];
  const available = [
    ...selected,
    { kind: "metric", key: "loss", label: "loss" },
  ];
  assert.equal(runColumnChoices(available, selected).length, 2);
  assert.deepEqual(runColumnChoices([], selected), selected);
  assert.equal(runSortIsDefault({ key: "started_at", direction: "desc" }), true);
  assert.equal(runSortIsDefault({ key: "started_at", direction: "asc" }), false);
  assert.equal(runSortLabel({ key: "started_at", direction: "desc" }, selected), "Newest first");
  assert.equal(runSortLabel({ key: "param:/learning_rate", direction: "asc" }, selected), "learning_rate · ascending");
  assert.equal(runSortLabel({ key: "status", direction: "desc" }, []), "Status · descending");
});

test("common columns lead each group, with deterministic alphabetical ties and fresh discovery counts", () => {
  const available = [
    { kind: "param", key: "/rare", label: "aaa_rare", run_count: 1 },
    { kind: "param", key: "/common", label: "zzz_common", run_count: 20 },
    { kind: "param", key: "/batch", label: "batch_size", run_count: 10 },
    { kind: "param", key: "/same-label-z", label: "batch_size", run_count: 10 },
    { kind: "metric", key: "rare", label: "aaa_rare", run_count: 2 },
    { kind: "metric", key: "loss", label: "loss", run_count: 20 },
  ];
  const selected = [
    { ...available[0], run_count: 500 },
    { kind: "param", key: "/outside", label: "outside", run_count: 500 },
  ];
  const choices = runColumnChoices(available, selected);
  assert.deepEqual(choices.filter(item => item.kind === "param").map(item => item.key), ["/common", "/batch", "/same-label-z", "/rare", "/outside"]);
  assert.deepEqual(choices.filter(item => item.kind === "metric").map(item => item.key), ["loss", "rare"]);
  assert.equal(choices.find(item => item.key === "/rare").run_count, 1, "saved selections cannot override current frequency");
  assert.equal(choices.find(item => item.key === "/outside").run_count, undefined, "retained choices cannot claim prevalence from a different sample");
  assert.equal(selected[0].run_count, 500, "sorting leaves saved selection metadata untouched");
});

test("older backends without prevalence retain alphabetical ordering and invalid counts cannot outrank known columns", () => {
  const legacy = [
    { kind: "metric", key: "z", label: "z" },
    { kind: "metric", key: "a", label: "a" },
  ];
  assert.deepEqual(runColumnChoices(legacy, []).map(item => item.key), ["a", "z"]);
  const mixed = [
    { ...legacy[1], run_count: NaN },
    { ...legacy[0], run_count: 0 },
    { kind: "metric", key: "negative", label: "negative", run_count: -1 },
  ];
  assert.deepEqual(runColumnChoices(mixed, []).map(item => item.key), ["z", "a", "negative"]);
});
