import test from "node:test";
import assert from "node:assert/strict";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { JSDOM } from "jsdom";
import { checkpointStep, parseExperimentCatalog, experimentDisplayName, experimentRunScope, memberExperiment, removeExperimentMember,
  experimentActivity, ExperimentGroupHeader, ExperimentLineageBadge } from "./.test/app.js";

const scope = (origin, run_id = "same-run") => ({ project_id: "vision", origin, run_id });
const first = scope("gpu-a"), second = scope("gpu-b");
function experiment(patch = {}) {
  return { experiment_id: "experiment-1", name: "Translation", description: "Try a better optimizer.\nKeep provenance.\tCompare results.", revision: "opaque-1",
    members: [first, second], lineage: [{ parent: first, child: second, kind: "continued", checkpoint: "模型/".repeat(200), step: "18446744073709551615" }],
    created_at: "2026-10-20T01:00:00Z", updated_at: "2026-10-20T01:00:00Z", unavailable_runs: [first], membership_count: 2, ...patch };
}
function catalog(item = experiment()) { return { project_id: "vision", revision: "project-1", experiments: [item] }; }

test("experiment parsing preserves multiline descriptions, Unicode checkpoints and exact u64 steps", () => {
  assert.deepEqual(parseExperimentCatalog(catalog(), "vision"), catalog());
  assert.equal(checkpointStep(" 000900 "), "900");
  assert.equal(checkpointStep("0"), "0");
  assert.equal(checkpointStep(""), null);
  for (const input of ["-1", "0.5", "1e3", "18446744073709551616", "9".repeat(21)]) assert.throws(() => checkpointStep(input));
  for (const patch of [{ description: "bad\u0000value" }, { name: "x".repeat(129) }, { description: "x".repeat(2049) },
    { members: [first, first] }, { membership_count: 1 }, { members: [first, { ...second, project_id: "outside" }] },
    { lineage: [{ ...experiment().lineage[0], step: 900 }] }, { unavailable_runs: [scope("outside")] }]) {
    assert.throws(() => parseExperimentCatalog(catalog(experiment(patch)), "vision"));
  }
  assert.throws(() => parseExperimentCatalog(catalog(), "another-project"));
  assert.throws(() => parseExperimentCatalog({ ...catalog(), experiments: [experiment(), experiment({ experiment_id: "experiment-2" })] }, "vision"));
});

test("membership uses project, origin and run ID while removing a member preserves unrelated lineage", () => {
  const source = { source_id: "hosted-project:vision", kind: "hosted_project", project_id: "vision" };
  assert.deepEqual(experimentRunScope({ run_id: "same-run", run_key: "gpu-b:same-run", origin: "gpu-b" }, source), second);
  assert.equal(experimentRunScope({ run_id: "same-run", run_key: "gpu-b:same-run", origin: "gpu-a" }, source), null);
  assert.equal(experimentRunScope({ run_id: "same-run", queue_only: true, run_key: "gpu-b:same-run" }, source), null);
  assert.equal(memberExperiment(catalog(), second)?.experiment_id, "experiment-1");
  assert.equal(memberExperiment(catalog(), { ...second, project_id: "another" }), null);
  const third = scope("gpu-c", "other-run"), fourth = scope("gpu-d", "last-run");
  const unrelated = { parent: third, child: fourth, kind: "split", checkpoint: null, step: null };
  const original = experiment({ members: [first, second, third, fourth], lineage: [...experiment().lineage, unrelated], membership_count: 4 });
  const removed = removeExperimentMember(original, first);
  assert.deepEqual(removed.members, [second, third, fourth]);
  assert.deepEqual(removed.lineage, [unrelated]);
  assert.equal(original.members.length, 4, "editing does not mutate the stored catalog");
});

test("experiment headers distinguish page activity from full membership without inventing a parent status", () => {
  const runs = [{ status: "failed" }, { status: "running" }];
  assert.equal(experimentActivity(runs), "1 running · 1 failed");
  const markup = renderToStaticMarkup(createElement(ExperimentGroupHeader, { experiment: experiment({ membership_count: 25 }), runs,
    expanded: true, on_toggle() {}, on_filter() {}, columns: 5 }));
  const dom = new JSDOM(`<table><tbody>${markup}</tbody></table>`);
  assert.match(dom.window.document.body.textContent, /2 shown · 25 members.*This page: 1 running · 1 failed/s);
  assert.equal(dom.window.document.querySelector("td").colSpan, 5);
  assert.equal(dom.window.document.querySelector(".status"), null);
  assert.equal(dom.window.document.querySelector("[aria-expanded]").getAttribute("aria-expanded"), "true");
  dom.window.close();
});

test("lineage badges preserve machine identity and escape checkpoint references", () => {
  const item = experiment({ lineage: [{ parent: first, child: second, kind: "continued", step: "900", checkpoint: "<script>alert(1)</script>" }] });
  const dom = new JSDOM(renderToStaticMarkup(createElement(ExperimentLineageBadge, { experiment: item, member: second })));
  assert.match(dom.window.document.body.textContent, /Continued from same-run/);
  assert.match(dom.window.document.querySelector("div").title, /gpu-a.*step 900.*<script>/);
  assert.equal(dom.window.document.querySelector("script"), null);
  dom.window.close();
});

test("same-name experiments display distinct IDs without treating names as identities", () => {
  const first = experiment({ experiment_id: "experiment-aaaaaaaa-12345678" });
  const second = experiment({ experiment_id: "experiment-bbbbbbbb-12345678" });
  assert.equal(experimentDisplayName(first, [first]), first.name);
  assert.notEqual(experimentDisplayName(first, [first, second]), experimentDisplayName(second, [first, second]));
  assert.match(experimentDisplayName(first, [first, second]), /^Translation · /);
});
