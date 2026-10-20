import test from "node:test";
import assert from "node:assert/strict";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { JSDOM } from "jsdom";
import { diskQuantityText, parseDiskQuantity, parseResourceRequirements, parseResourceStatus, requirementsFromDraft, resourceBlockLabel, ResourceStatusSummary, RunResourceBadge } from "./.test/app.js";

const requirements = { disk: [{ path: "outputs", min_free_bytes: 20 * 1024 ** 3, resume_free_bytes: 30 * 1024 ** 3 }], recovery_seconds: 30 };
const status = { phase: "blocked", blocked: true, checked_at: "2026-10-20T01:00:00Z", recovery_started_at: null,
  disk: [{ ...requirements.disk[0], available_bytes: 12 * 1024 ** 3, phase: "blocked", error: null }] };
function view(Component, props) {
  const dom = new JSDOM(renderToStaticMarkup(createElement(Component, props)));
  return { dom, document: dom.window.document, text: dom.window.document.body.textContent };
}

test("disk quantities keep exact byte limits through friendly-unit editing", () => {
  assert.equal(parseDiskQuantity("20 GiB"), 20 * 1024 ** 3);
  assert.equal(parseDiskQuantity("1.5 MiB"), 1.5 * 1024 ** 2);
  assert.equal(parseDiskQuantity(" 500 mb "), 500_000_000);
  assert.equal(parseDiskQuantity("1.001 KB"), 1001);
  assert.equal(parseDiskQuantity("9007199254740991 B"), Number.MAX_SAFE_INTEGER);
  for (const value of [0, 1, 1024, 2 ** 30, 123456789, Number.MAX_SAFE_INTEGER]) {
    if (value > 0) assert.equal(parseDiskQuantity(diskQuantityText(value)), value);
  }
  for (const text of ["", "-1 GiB", "0", "1e6", "Infinity", "1 PB", "0.1 B", "9007199254740992 B", "999999999999999999999999999999999999999999999 B"]) assert.throws(() => parseDiskQuantity(text));
});

test("disk requirements enforce limits, absolute worker paths and distinct hysteresis", () => {
  assert.deepEqual(parseResourceRequirements(requirements), requirements);
  assert.deepEqual(requirementsFromDraft([{ path: " outputs ", min_free: "20 GiB", resume_free: "30 GiB" }], 30), requirements);
  assert.deepEqual(parseResourceRequirements({ disk: [], recovery_seconds: 0 }), { disk: [], recovery_seconds: 0 });
  for (const patch of [
    { recovery_seconds: -1 }, { recovery_seconds: 3601 }, { recovery_seconds: 1.5 }, { disk: Array(17).fill(requirements.disk[0]) },
    { disk: [requirements.disk[0], requirements.disk[0]] },
    ...["relative", "/data/../secret", "outputs/subdir", "/data/./file", "/data\nfile", "/" + "é".repeat(2048)].map(path => ({ disk: [{ ...requirements.disk[0], path }] })),
    ...[0, -1, Number.MAX_SAFE_INTEGER + 1, 0.5].map(min_free_bytes => ({ disk: [{ ...requirements.disk[0], min_free_bytes }] })),
    { disk: [{ ...requirements.disk[0], resume_free_bytes: requirements.disk[0].min_free_bytes }] },
  ]) assert.throws(() => parseResourceRequirements({ ...requirements, ...patch }));
  assert.equal(parseResourceRequirements({ ...requirements, disk: [{ ...requirements.disk[0], path: "/data/models" }] }).disk[0].path, "/data/models");
});

test("resource observations preserve zero free space and reject malformed worker evidence", () => {
  assert.deepEqual(parseResourceStatus(status), status);
  assert.equal(parseResourceStatus({ ...status, disk: [{ ...status.disk[0], available_bytes: 0 }] }).disk[0].available_bytes, 0);
  for (const available_bytes of [2 ** 53, 2 ** 63, Number(18_446_744_073_709_551_615n)]) {
    assert.equal(parseResourceStatus({ ...status, disk: [{ ...status.disk[0], available_bytes }] }).disk[0].available_bytes, available_bytes,
      "large physical filesystems remain displayable without weakening exact policy limits");
  }
  for (const patch of [{ phase: "unknown" }, { blocked: "yes" }, { checked_at: "tomorrow" }, { recovery_started_at: 0 },
    { disk: [{ ...status.disk[0], phase: "paused" }] }, ...[-1, 0.5, Infinity, NaN, 2 ** 65].map(available_bytes => ({ disk: [{ ...status.disk[0], available_bytes }] })),
    { disk: [{ ...status.disk[0], error: {} }] }]) {
    assert.throws(() => parseResourceStatus({ ...status, ...patch }));
  }
  assert.equal(resourceBlockLabel({ ...status, blocked: false, phase: "ready" }), null);
});

test("resource hold explains the observed path, thresholds and automatic versus manual intent", () => {
  const f = view(ResourceStatusSummary, { status, requirements, manual_paused: false, task_status: "pending" });
  try {
    assert.match(f.text, /Low disk space.*Waiting to start/s);
    assert.match(f.text, /outputs.*12 GiB free.*Minimum 20 GiB · recover at 30 GiB/s);
    assert.match(f.text, /starts automatically.*30 seconds/);
    assert.equal(f.document.querySelector('[role="status"]')?.id, "run-resource-status");
  } finally { f.dom.window.close(); }
  const manual = view(ResourceStatusSummary, { status, requirements, manual_paused: true, task_status: "paused" });
  try { assert.match(manual.text, /Manual pause is active.*Resume clears your hold/s); assert.doesNotMatch(manual.text, /resumes automatically/); }
  finally { manual.dom.window.close(); }
  for (const task_status of ["running", "completed", "cancelled", "failed"]) {
    const finished = view(ResourceStatusSummary, { status, requirements, manual_paused: false, task_status });
    try { assert.equal(finished.text, "", "old resource evidence cannot claim a task will resume"); } finally { finished.dom.window.close(); }
  }
});

test("table resource evidence stays a compact badge and escapes worker error text", () => {
  const f = view(RunResourceBadge, { status: { ...status, phase: "unavailable", disk: [{ ...status.disk[0], phase: "unavailable", available_bytes: null, error: "<img src=x>" }] } });
  try {
    assert.equal(f.text, "Disk check unavailable");
    assert.match(f.document.querySelector("span").title, /outputs: free space unavailable/);
    assert.equal(f.document.querySelector("img"), null);
  } finally { f.dom.window.close(); }
  for (const value of [null, {}, { ...status, blocked: false, phase: "ready" }]) {
    const hidden = view(RunResourceBadge, { status: value });
    try { assert.equal(hidden.text, ""); } finally { hidden.dom.window.close(); }
  }
});
