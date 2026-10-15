import test from "node:test";
import assert from "node:assert/strict";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { JSDOM } from "jsdom";
import { parseTaskAttempts, RunAttemptHistory, attemptRestartGap, attemptWorkerLabel } from "./.test/app.js";

const attempt = { attempt: 2, request_id: "continue-2", reason: "Extend training", status: "running", error: null, requested_at: "2026-10-15T03:00:00Z", previous_worker_stopped_at: "2026-10-15T03:00:01Z", started_at: "2026-10-15T03:00:02.500Z", finished_at: null, exit_code: null, worker_restarted: true, source: { kind: "git", git_head: "a".repeat(40) }, checkpoint: { name: "checkpoint.pt", relative_path: "outputs/checkpoint.pt", sha256: "b".repeat(64), size: "100" }, metric_step_offset: "18446744073709551615", first_local_step: "1000" };
const history = { protocol_version: 1, current_attempt: 2, attempts: [attempt], total_count: 1, truncated: false };
function view(value) {
  const dom = new JSDOM(renderToStaticMarkup(createElement(RunAttemptHistory, { state: value })));
  return { dom, document: dom.window.document, text: dom.window.document.body.textContent };
}

test("attempt history projects exact steps and never retains private execution metadata", () => {
  const parsed = parseTaskAttempts({ ...history, private_socket: "/private/socket", attempts: [{ ...attempt, worker_pid: 123, code_dir: "/private/code", checkpoint: { ...attempt.checkpoint, pinned_path: "/private/model.pt" }, source: { ...attempt.source, paths: ["/private/code"] } }] });
  assert.deepEqual(parsed, history);
  assert.equal(JSON.stringify(parsed).includes("private"), false);
  assert.equal(parsed.attempts[0].metric_step_offset, "18446744073709551615");
});

test("malformed attempt evidence cannot fabricate dates, status, gaps or exact step values", () => {
  for (const patch of [{ attempt: 0 }, { attempt: Infinity }, { status: "pending" }, { started_at: "invalid" }, { exit_code: 0.5 }, { reason: "bad\nreason" }, { metric_step_offset: 1 }, { metric_step_offset: "01" }, { metric_step_offset: "18446744073709551616" }, { first_local_step: "-1" }]) {
    const parsed = parseTaskAttempts({ ...history, attempts: [{ ...attempt, ...patch }] });
    assert.deepEqual(parsed.attempts, []);
    assert.equal(parsed.truncated, true);
  }
  for (const value of [null, [], {}, { ...history, protocol_version: 2 }, { ...history, total_count: 0 }]) assert.equal(parseTaskAttempts(value), null);
  assert.equal(attemptRestartGap({ ...attempt, previous_worker_stopped_at: null }), null);
  assert.equal(attemptRestartGap({ ...attempt, started_at: "2026-10-15T02:00:00Z" }), null);
  assert.equal(attemptRestartGap({ ...attempt, worker_restarted: false }), null);
});

test("worker replacement failures and preflight failures make no task-continuity claims", () => {
  assert.equal(attemptWorkerLabel({ ...attempt, worker_restarted: false, previous_worker_stopped_at: null }), "Worker not restarted");
  assert.match(attemptWorkerLabel({ ...attempt, worker_restarted: false }), /Previous worker stopped; replacement not started/);
  assert.equal(attemptWorkerLabel({ ...attempt, worker_restarted: null }), "Worker restart not recorded");
  const { dom, text } = view({ task_attempts: { ...history, current_attempt: 1, attempts: [{ ...attempt, status: "failed", started_at: null, finished_at: "2026-10-15T03:00:03Z", worker_restarted: false, previous_worker_stopped_at: null }] } });
  try {
    assert.match(text, /Current 1 · 1 record/);
    assert.match(text, /failed/);
    assert.match(text, /Worker not restarted/);
    assert.doesNotMatch(text, /Restart gap|Task kept running|Attempt 2 · Current/);
  } finally { dom.window.close(); }
});

test("failed attempts show their safe warning without dropping older records or interpreting markup", () => {
  const error = "Checkpoint preparation failed.";
  const failed = { ...attempt, status: "failed", error, started_at: null, worker_restarted: false, previous_worker_stopped_at: null };
  const { dom, document, text } = view({ task_attempts: { ...history, current_attempt: 1, attempts: [failed] } });
  try {
    assert.match(text, /Checkpoint preparation failed\./);
    assert.match(text, /Inspect expri runs status for details/);
    assert.equal(document.querySelector(".supervisor-failed.supervisor-reason").textContent.startsWith(error), true);
    assert.doesNotMatch(text, /Worker restarted|Restart gap/);
  } finally { dom.window.close(); }
  for (const invalid of [undefined, null, {}, "secret\nvalue", "a".repeat(513)]) {
    const parsed = parseTaskAttempts({ ...history, attempts: [{ ...failed, error: invalid }] });
    assert.equal(parsed.attempts.length, 1);
    assert.equal(parsed.attempts[0].error, null);
  }
  assert.equal(parseTaskAttempts({ ...history, attempts: [{ ...attempt, error }] }).attempts[0].error, null);
  const escaped = view({ task_attempts: { ...history, attempts: [{ ...failed, error: "<img src=x onerror=alert(1)>" }] } });
  try {
    assert.ok(escaped.text.includes("<img src=x onerror=alert(1)>"));
    assert.equal(escaped.document.querySelector("img, script"), null);
  } finally { escaped.dom.window.close(); }
});

test("collapsed attempt Overview shows code, checkpoint, times, parameters and actual worker gap", () => {
  const { dom, document, text } = view({ task_attempts: history });
  try {
    assert.equal(document.querySelector("details").open, false);
    assert.match(text, /Task attemptsCurrent 2 · 1 record/);
    assert.match(text, /Attempt 2 · Current/);
    assert.match(text, /outputs\/checkpoint.pt/);
    assert.match(text, /outputs\/attempts\/2\/params.json/);
    assert.match(text, /18446744073709551615/);
    assert.match(text, /Worker restarted · Restart gap 1.5 s/);
    assert.match(text, /Run elapsed time includes gaps/);
    assert.equal(document.querySelectorAll("time").length, 3);
    assert.equal(document.querySelector("button, a, input"), null);
  } finally { dom.window.close(); }
});

test("legacy metadata stays hidden and bounded history reports omitted attempt records", () => {
  const missing = view({});
  try { assert.equal(missing.document.querySelector("details"), null); } finally { missing.dom.window.close(); }
  const attempts = Array.from({ length: 20 }, (_, index) => ({ ...attempt, attempt: index + 1 }));
  const parsed = parseTaskAttempts({ ...history, current_attempt: 20, attempts, total_count: 20 });
  assert.equal(parsed.attempts.length, 16);
  assert.equal(parsed.attempts[0].attempt, 5);
  const { dom, text } = view({ task_attempts: parsed });
  try { assert.match(text, /Showing 16 of 20 attempt records/); } finally { dom.window.close(); }
});

test("untrusted attempt strings render as text and unsafe checkpoint paths stay unavailable", () => {
  const unsafe = parseTaskAttempts({ ...history, attempts: [{ ...attempt, checkpoint: { ...attempt.checkpoint, relative_path: "/private/checkpoint.pt" } }] });
  assert.equal(unsafe.attempts[0].checkpoint, null);
  const reason = "<img src=x onerror=alert(1)>";
  const { dom, document, text } = view({ task_attempts: { ...history, attempts: [{ ...attempt, reason }] } });
  try { assert.ok(text.includes(reason)); assert.equal(document.querySelector("img, script"), null); } finally { dom.window.close(); }
});

test("file manifests distinguish changed code even when attempts share a Git commit", () => {
  const first = { ...attempt, attempt: 1, source: { ...attempt.source, files_sha256: "c".repeat(64) } };
  const next = { ...attempt, source: { ...attempt.source, files_sha256: "d".repeat(64) } };
  const { dom, document } = view({ task_attempts: { ...history, attempts: [first, next], total_count: 2 } });
  try {
    const records = document.querySelectorAll("li");
    assert.equal(records[0].querySelector("dd code").textContent, "dddddddddddd");
    assert.equal(records[1].querySelector("dd code").textContent, "cccccccccccc");
    assert.match(records[0].querySelector("dd").textContent, /Git aaaaaaaaaaaa/);
    assert.equal(records[0].querySelector("dd code").title, "d".repeat(64));
  } finally { dom.window.close(); }
});
