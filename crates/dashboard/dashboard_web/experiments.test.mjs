import test from "node:test";
import assert from "node:assert/strict";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { JSDOM } from "jsdom";
import { checkpointStep, parseExperimentCatalog, parseExperimentGroups, experimentDisplayName, experimentRunScope, memberExperiment, removeExperimentMember,
  ExperimentFilter, ExperimentRunPanel } from "./.test/app.js";

const scope = (origin, run_id = "same-run") => ({ project_id: "vision", origin, run_id });
const first = scope("gpu-a"), second = scope("gpu-b");
function experiment(patch = {}) {
  return { experiment_id: "experiment-1", name: "Translation", description: "Try a better optimizer.\nKeep provenance.\tCompare results.", revision: "opaque-1",
    members: [first, second], lineage: [{ parent: first, child: second, kind: "continued", checkpoint: "模型/".repeat(200), step: "18446744073709551615" }],
    created_at: "2026-10-20T01:00:00Z", updated_at: "2026-10-20T01:00:00Z", unavailable_runs: [first], membership_count: 2, ...patch };
}
function catalog(item = experiment()) { return { project_id: "vision", revision: "project-1", experiments: [item] }; }

test("group summaries preserve top-level order, full representative identities and empty retained experiments", () => {
  const runs = [first, second].map((member) => ({ ...member, run_key: `${member.origin}:${member.run_id}` }));
  const summary = { experiment_id: "experiment-1", name: "Translation", revision: "1" };
  const group = { experiment: summary, run_key: runs[0].run_key, membership_count: 3, matching_count: 2, active_count: 1, unavailable_count: 1 };
  const standalone = { experiment: null, run_key: runs[1].run_key, membership_count: 1, matching_count: 1, active_count: 1, unavailable_count: 0 };
  const retained = { ...group, experiment: { ...summary, experiment_id: "retained" }, run_key: null,
    membership_count: 1, matching_count: 0, active_count: 0, unavailable_count: 1 };
  const empty = { ...retained, experiment: { ...summary, experiment_id: "empty" }, membership_count: 0, unavailable_count: 0 };
  const entries = [standalone, retained, group, empty];
  assert.deepEqual(parseExperimentGroups(entries, runs), entries,
    "group order comes from the server, while equal actual run IDs remain distinct across workers");
});

test("invalid group responses cannot invent representatives, duplicate identities or impossible activity", () => {
  const run = { run_id: first.run_id, run_key: `${first.origin}:${first.run_id}` };
  const group = { experiment: { experiment_id: "experiment-1", name: "Translation", revision: "1" }, run_key: run.run_key,
    membership_count: 2, matching_count: 2, active_count: 1, unavailable_count: 0 };
  for (const entries of [null, [null], Array(101).fill(group),
    [{ ...group, membership_count: -1 }], [{ ...group, membership_count: 257 }], [{ ...group, active_count: 1.5 }],
    [{ ...group, matching_count: 3 }], [{ ...group, active_count: 3 }], [{ ...group, unavailable_count: 3 }],
    [{ ...group, unavailable_count: 1 }],
    [{ ...group, run_key: "outside:unknown-run" }], [{ ...group, run_key: null }],
    [{ ...group, matching_count: 0, active_count: 0 }], [{ ...group, experiment: null }],
    [{ ...group, experiment: null, membership_count: 1, matching_count: 1, unavailable_count: 1 }],
    [group, group], [group, { ...group, experiment: { ...group.experiment, experiment_id: "other" } }], []]) {
    assert.throws(() => parseExperimentGroups(entries, [run]));
  }
  assert.throws(() => parseExperimentGroups([group], [run, { ...run }]));
  assert.deepEqual(parseExperimentGroups([], []), []);
});

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

test("experiment filtering keeps distinct scoped choices usable on a read-only dashboard", () => {
  const first = experiment({ name: "<img src=x onerror=alert(1)>" });
  const second = experiment({ experiment_id: "experiment-2", name: first.name, members: [], lineage: [], membership_count: 0, unavailable_runs: [] });
  const source = { source_id: "hosted-project:vision", project_id: "vision", kind: "hosted_project" };
  const snapshot = { source_id: source.source_id, sources: [source], controls_disabled: false,
    experiments: { supported: true, management_enabled: false, loading: false, filter: second.experiment_id,
      catalog: { project_id: "vision", revision: "1", experiments: [first, second] } } };
  const dom = new JSDOM(renderToStaticMarkup(createElement(ExperimentFilter, { snapshot, actions: {} })));
  const select = dom.window.document.querySelector("select");
  assert.equal(select.disabled, false);
  assert.equal(select.value, second.experiment_id);
  assert.deepEqual([...select.options].map((option) => option.value), ["", "ungrouped", first.experiment_id, second.experiment_id]);
  assert.notEqual(select.options[2].textContent, select.options[3].textContent);
  assert.match(select.options[2].title, /experiment-1/);
  assert.equal(dom.window.document.querySelector("img"), null, "names remain text rather than HTML");
  dom.window.close();
});

test("run inspection preserves exact lineage and escapes checkpoint references without adding history rows", () => {
  const item = experiment({ lineage: [{ parent: first, child: second, kind: "continued", step: "900", checkpoint: "<script>alert(1)</script>" }] });
  const source = { source_id: "hosted-project:vision", project_id: "vision", kind: "hosted_project" };
  const run = { run_id: second.run_id, run_key: `${second.origin}:${second.run_id}`, origin: second.origin };
  const snapshot = { source_id: source.source_id, sources: [source], detail: { source, run }, review: { kind: "run", run_ids: [run.run_key] },
    experiments: { supported: true, management_enabled: false, loading: false, busy: false, error: null, catalog: catalog(item) } };
  const dom = new JSDOM(renderToStaticMarkup(createElement(ExperimentRunPanel, { snapshot, actions: {} })));
  assert.match(dom.window.document.body.textContent, /Continued from same-run · gpu-a.*Checkpoint step 900.*<script>alert\(1\)<\/script>/s);
  const parent = new URL(dom.window.document.querySelector(".experiment-lineage-list a").getAttribute("href"), "http://localhost");
  assert.deepEqual(Object.fromEntries(parent.searchParams), first);
  assert.equal(dom.window.document.querySelector("script"), null);
  assert.equal(dom.window.document.querySelector("tr"), null, "lineage belongs to the inspector rather than the run table");
  dom.window.close();
});

test("same-name experiments display distinct IDs without treating names as identities", () => {
  const first = experiment({ experiment_id: "experiment-aaaaaaaa-12345678" });
  const second = experiment({ experiment_id: "experiment-bbbbbbbb-12345678" });
  assert.equal(experimentDisplayName(first, [first]), first.name);
  assert.notEqual(experimentDisplayName(first, [first, second]), experimentDisplayName(second, [first, second]));
  assert.match(experimentDisplayName(first, [first, second]), /^Translation · /);
});
