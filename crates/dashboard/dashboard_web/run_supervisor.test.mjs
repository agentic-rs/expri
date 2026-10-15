import test from "node:test";
import assert from "node:assert/strict";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { JSDOM } from "jsdom";
import { parseRunSupervisor, RunSupervisorHistory } from "./.test/app.js";

const event = {
  generation: 2,
  request_id: "request-a",
  reason: "Upgrade supervisor",
  mode: "live",
  status: "completed",
  requested_at: "2026-10-15T01:00:00Z",
  completed_at: "2026-10-15T01:00:01Z",
  failed_at: null,
  worker_restarted: false,
  error: null,
};
const supervisor = {
  protocol_version: 1, generation: 2, mode: "live", events: [event], total_count: 1, truncated: false,
};

function history(state) {
  const dom = new JSDOM(renderToStaticMarkup(createElement(RunSupervisorHistory, { state })));
  return { dom, document: dom.window.document, text: dom.window.document.body.textContent };
}

test("supervisor parser retains only public bounded evidence and tolerates unknown worker restart state", () => {
  const parsed = parseRunSupervisor({
    ...supervisor, control_socket: "/private/socket", task_pid: 123,
    events: [{ ...event, handoff_fd: 4, target_digest: "private", target_path: "/private/expri" }],
  });
  assert.deepEqual(parsed, supervisor);
  assert.notEqual(parsed.events[0], event);
  assert.equal(JSON.stringify(parsed).includes("private"), false);
  for (const worker_restarted of [undefined, null, "false", {}, 0]) {
    assert.equal(parseRunSupervisor({ ...supervisor, events: [{ ...event, worker_restarted }] }).events[0].worker_restarted, null);
  }
  assert.equal(parseRunSupervisor({ ...supervisor, events: [{ ...event, worker_restarted: true }] }).events[0].worker_restarted, true);
});

test("malformed supervisor metadata and nonfinite fields cannot fabricate restart history", () => {
  for (const value of [null, [], {}, "supervisor", { ...supervisor, protocol_version: 2 },
    { ...supervisor, generation: 0 }, { ...supervisor, generation: Infinity },
    { ...supervisor, generation: Number.MAX_SAFE_INTEGER + 1 }, { ...supervisor, mode: "cold" },
    { ...supervisor, events: {} }, { ...supervisor, total_count: 0 }, { ...supervisor, truncated: null }]) {
    assert.equal(parseRunSupervisor(value), null);
  }
  for (const patch of [{ generation: -1 }, { generation: 1.5 }, { request_id: "" },
    { reason: "bad\nreason" }, { reason: "x".repeat(513) }, { requested_at: "invalid" },
    { completed_at: "invalid" }, { failed_at: 1 }, { status: "running" }, { mode: "cold" },
    { error: {} }, { error: "bad\0error" }]) {
    const parsed = parseRunSupervisor({ ...supervisor, events: [{ ...event, ...patch }] });
    assert.deepEqual(parsed.events, []);
    assert.equal(parsed.total_count, 1);
    assert.equal(parsed.truncated, true);
  }
});

test("supervisor history keeps the newest sixteen events and preserves full-history counts", () => {
  const events = Array.from({ length: 20 }, (_, index) => ({ ...event, generation: index + 2, request_id: `request-${index}` }));
  const parsed = parseRunSupervisor({ ...supervisor, generation: 21, events, total_count: 20 });
  assert.equal(parsed.events.length, 16);
  assert.equal(parsed.events[0].generation, 6);
  assert.equal(parsed.events[15].generation, 21);
  assert.equal(parsed.total_count, 20);
  assert.equal(parsed.truncated, true);
});

test("supervisor Overview is compact, collapsible, and explicit about the preserved task and worker", () => {
  const { dom, document, text } = history({ supervisor });
  try {
    assert.equal(document.querySelector("details").open, false);
    assert.match(document.querySelector("summary").textContent, /SupervisorGeneration 2 · 1 restart attempt$/);
    assert.match(text, /Upgrade supervisor/);
    assert.match(text, /Completed/);
    assert.match(text, /Live · Task kept running · Worker not restarted/);
    assert.equal(document.querySelector("time").dateTime, event.completed_at);
    assert.equal(document.querySelector("button, input, a"), null);
    assert.equal(document.querySelector("[role=alert]"), null);
  } finally { dom.window.close(); }
});

test("failed and in-progress restarts preserve accurate dates, failure detail and worker evidence", () => {
  for (const worker_restarted of [true, false, null]) {
    const failed = { ...event, status: "failed", completed_at: null, failed_at: "2026-10-15T01:00:02Z", worker_restarted, error: "Handoff refused" };
    const { dom, document, text } = history({ supervisor: { ...supervisor, generation: 1, events: [failed] } });
    try {
      assert.match(document.querySelector("summary").textContent, /Generation 1 · 1 restart attempt$/);
      assert.match(text, /Target generation 2/);
      assert.match(text, /Failed/);
      assert.match(text, /Handoff refused/);
      assert.match(text, worker_restarted === true ? /Worker restarted/ : worker_restarted === false ? /Worker not restarted/ : /Worker restart not recorded/);
      if (worker_restarted === false) assert.match(text, /Task kept running/);
      else assert.doesNotMatch(text, /Task kept running|Worker not restarted/);
      assert.equal(document.querySelector("time").dateTime, failed.failed_at);
    } finally { dom.window.close(); }
  }
  const { dom, document, text } = history({ supervisor: { ...supervisor, events: [{ ...event, status: "restarting", completed_at: null }] } });
  try {
    assert.match(text, /Restarting/);
    assert.equal(document.querySelector("time").dateTime, event.requested_at);
  } finally { dom.window.close(); }
});

test("legacy runs hide supervisor history while initial and truncated records remain clear", () => {
  for (const state of [null, {}, { supervisor: null }, { supervisor: { generation: 1 } }]) {
    const { dom, document } = history(state);
    try { assert.equal(document.querySelector("details"), null); }
    finally { dom.window.close(); }
  }
  const initial = history({ supervisor: { ...supervisor, generation: 1, events: [], total_count: 0 } });
  try {
    assert.match(initial.text, /Generation 1 · 0 restart attempts/);
    assert.match(initial.text, /No supervisor restart attempts recorded/);
    assert.doesNotMatch(initial.text, /Task kept running/);
  } finally { initial.dom.window.close(); }
  const truncated = history({ supervisor: { ...supervisor, total_count: 40, truncated: true } });
  try {
    assert.match(truncated.text, /Showing 1 of 40 restart attempts/);
    assert.match(truncated.text, /run record retains the full history/);
  } finally { truncated.dom.window.close(); }
});

test("restart reasons and errors render as escaped text", () => {
  const reason = "<img src=x onerror=alert(1)>", error = "<script>alert(1)</script>";
  const { dom, document, text } = history({ supervisor: { ...supervisor, events: [{ ...event, reason, error }] } });
  try {
    assert.equal(document.querySelector("img, script"), null);
    assert.ok(text.includes(reason));
    assert.ok(text.includes(error));
  } finally { dom.window.close(); }
});
