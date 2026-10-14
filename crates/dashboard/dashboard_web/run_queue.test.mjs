import test from "node:test";
import assert from "node:assert/strict";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { JSDOM } from "jsdom";
import {
  parseRunQueue,
  parseRunRedirect,
  queueContext,
  queueMessage,
  queuePlaceholder,
  queueResourceLabels,
  QueueOverview,
} from "./.test/app.js";

const resources = {
  cpus: 4,
  memory_bytes: 8 * 1024 ** 3,
  gpus: 1,
  gpu_ids: ["GPU-a"],
  gpu_model: "NVIDIA A100",
  gpu_memory_bytes: 40 * 1024 ** 3,
};
const queue = {
  job_id: "job-a",
  project_id: "vision",
  task: "train",
  status: "pending",
  resources,
  pending_reason: "Requested GPUs are busy",
  submitted_at_ms: Date.parse("2026-10-14T00:00:00Z"),
  updated_at_ms: Date.parse("2026-10-14T00:01:00Z"),
  worker_id: null,
  run_id: null,
};

function overview(run) {
  const dom = new JSDOM(renderToStaticMarkup(createElement(QueueOverview, { run })));
  return { dom, text: dom.window.document.body.textContent, document: dom.window.document };
}

test("dashboard queue context preserves scheduling evidence and strips private execution fields", () => {
  const raw = {
    ...queue, token: "private", command: ["train"], attempt: { fencing_token: "private" },
    resources: { ...resources, secret: "private" },
  };
  const parsed = parseRunQueue(raw);
  assert.deepEqual(parsed, queue);
  assert.notEqual(parsed, raw);
  assert.notEqual(parsed.resources, raw.resources);
  assert.notEqual(parsed.resources.gpu_ids, raw.resources.gpu_ids);
  assert.equal(JSON.stringify(parsed).includes("private"), false);
  assert.deepEqual(queueContext({ run_id: "job-a", queue: raw }), queue);
  assert.equal(queueContext({ run_id: "run-a" }), null);
  assert.equal(parseRunQueue({ ...queue, status: "queued" }).status, "pending");
});

test("native acknowledgement alone cannot turn a job placeholder into a published worker run", () => {
  const pending = { run_id: "job-a", queue };
  const acknowledged = { ...pending, queue: { ...queue, status: "running", run_id: "run-native", worker_id: "gpu-1" } };
  const native = { ...acknowledged, run_id: "run-native", queue_only: false };
  assert.equal(queuePlaceholder(pending), true);
  assert.equal(queuePlaceholder(acknowledged), true);
  assert.equal(queuePlaceholder(native), false);
  assert.equal(queuePlaceholder({ ...acknowledged, run_id: "run-native" }), false);
  assert.equal(queuePlaceholder({ ...pending, queue_only: false }), false);
  assert.equal(queuePlaceholder({ run_id: "job-a", queue_only: true, queue: null }), true);
  assert.equal(queuePlaceholder({ run_id: "job-a", queue: { ...queue, job_id: "other-job" } }), false);
  assert.equal(queuePlaceholder({ run_id: "job-a", queue: {} }), false);
});

test("malformed scheduling replies cannot fabricate valid identity, times or resource reservations", () => {
  for (const patch of [
    { job_id: "../job" }, { project_id: "." }, { worker_id: "https://worker.example" },
    { run_id: "queue:job-a" }, { status: "waiting" }, { task: "" }, { task: "bad\nname" },
    { submitted_at_ms: -1 }, { submitted_at_ms: 0.5 }, { updated_at_ms: queue.submitted_at_ms - 1 },
    { updated_at_ms: Infinity }, { updated_at_ms: 8_640_000_000_000_001 },
    { pending_reason: "bad\u0000reason" }, { pending_reason: 1 }, { worker_id: undefined },
  ]) assert.equal(parseRunQueue({ ...queue, ...patch }), null, JSON.stringify(patch));
  for (const patch of [
    { cpus: 0 }, { cpus: 1_000_001 }, { memory_bytes: 0 }, { memory_bytes: Number.MAX_SAFE_INTEGER + 1 },
    { gpus: 65 }, { gpus: 2 }, { gpu_ids: ["GPU-a", "GPU-a"], gpus: 2 },
    { gpu_ids: ["GPU-a\n"] }, { gpu_model: "" }, { gpu_memory_bytes: 0 },
    { gpus: 0, gpu_ids: [], gpu_model: null }, { gpu_ids: null },
  ]) assert.equal(parseRunQueue({ ...queue, resources: { ...resources, ...patch } }), null, JSON.stringify(patch));
  for (const value of [null, [], {}, true, "queue"]) assert.equal(parseRunQueue(value), null);
  const cpu_only = { cpus: 1, memory_bytes: 1, gpus: 0, gpu_ids: [], gpu_model: null, gpu_memory_bytes: null };
  assert.deepEqual(parseRunQueue({ ...queue, resources: cpu_only }).resources, cpu_only);
});

test("queue text and identifiers obey service byte limits rather than truncating evidence", () => {
  assert.equal(parseRunQueue({ ...queue, task: "é".repeat(128) }).task, "é".repeat(128));
  assert.equal(parseRunQueue({ ...queue, task: "é".repeat(129) }), null);
  assert.equal(parseRunQueue({ ...queue, pending_reason: "x".repeat(257) }), null);
  assert.equal(parseRunQueue({ ...queue, job_id: "x".repeat(96) }).job_id.length, 96);
  assert.equal(parseRunQueue({ ...queue, job_id: "x".repeat(97) }), null);
  assert.equal(parseRunQueue({ ...queue, resources: { ...resources, gpu_ids: ["é".repeat(129)] } }), null);
  assert.equal(queueMessage({ run_id: "job-a", queue_message: "Awaiting worker run metadata" }), "Awaiting worker run metadata");
  assert.equal(queueMessage({ run_id: "job-a", queue_message: "bad\nmessage" }), null);
  assert.equal(queueMessage({ run_id: "job-a", queue_message: "x".repeat(257) }), null);
});

test("redirects select bounded native hosted identities and never paths, URLs or queued identities", () => {
  assert.deepEqual(parseRunRedirect({ source_id: "hosted-project:vision", run_id: "gpu-1:run-native" }), {
    source_id: "hosted-project:vision", run_id: "gpu-1:run-native",
  });
  assert.deepEqual(parseRunRedirect({ source_id: "hosted-project:vision", run_id: "queue:run-native" }), {
    source_id: "hosted-project:vision", run_id: "queue:run-native",
  });
  assert.deepEqual(parseRunRedirect({ source_id: "hosted:vision:gpu-1", run_id: "run-native", token: "private" }), {
    source_id: "hosted:vision:gpu-1", run_id: "run-native",
  });
  for (const value of [
    null, {}, { source_id: "https://evil.example", run_id: "run-native" },
    { source_id: "hosted-project:../vision", run_id: "gpu-1:run-native" },
    { source_id: "hosted-project:vision", run_id: "../gpu:run-native" },
    { source_id: "hosted-project:vision", run_id: "queue::job-a" },
    { source_id: "hosted-project:vision", run_id: "run-native" },
    { source_id: "hosted:vision:gpu-1", run_id: "gpu-2:run-native" },
    { source_id: "local", run_id: "run-native" },
    { source_id: "hosted:vision:gpu-1", run_id: "https://evil.example/run" },
    { source_id: "hosted:vision:gpu-1", run_id: ".." },
    { source_id: "hosted:vision:gpu-1:extra", run_id: "run-native" },
  ]) assert.equal(parseRunRedirect(value), null, JSON.stringify(value));
});

test("queue Overview explains resource scheduling without inventing worker evidence", () => {
  const { dom, text, document } = overview({ run_id: "job-a", queue, queue_only: true });
  try {
    assert.match(text, /Job IDjob-a/);
    assert.match(text, /Tasktrain/);
    assert.match(text, /Pending · Resource scheduling/);
    assert.match(text, /Assigned workerNot assigned/);
    assert.match(text, /Requested GPUs are busy/);
    assert.match(text, /4 CPU slots/);
    assert.match(text, /8 GiB memory/);
    assert.match(text, /1 GPU/);
    assert.match(text, /Minimum GPU memory: 40 GiB each/);
    assert.equal(document.querySelector("time").dateTime, "2026-10-14T00:00:00.000Z");
    assert.doesNotMatch(text, /Started|Duration|Exit code|No parameters|No metric|run-state|warning/i);
    assert.equal(document.querySelector(".error, [role='alert']"), null);
    assert.equal(document.querySelector("a, button, input"), null);
    assert.equal(document.querySelector("[role='status']"), null);
  } finally { dom.window.close(); }
});

test("preparing and terminal queue placeholders show informational publication state", () => {
  for (const status of ["starting", "running", "cancel_requested", "completed", "failed", "cancelled", "lost"]) {
    const terminal = ["completed", "failed", "cancelled", "lost"].includes(status);
    const message = terminal ? "No published worker run metadata is available" : "Awaiting worker run metadata";
    const { dom, text, document } = overview({
      run_id: "job-a", queue: { ...queue, status, worker_id: "gpu-1", run_id: "run-native", pending_reason: null },
      queue_only: true, queue_message: message,
    });
    try {
      assert.match(text, /Assigned workergpu-1/);
      assert.match(text, /Acknowledged run IDrun-native/);
      assert.ok(text.includes(message));
      assert.match(document.querySelector("[role='status']").textContent,
        terminal ? /No worker run publication/ : /Awaiting worker run metadata/);
      assert.doesNotMatch(text, /Started|Duration|Exit code|Metric summaries|Cached results/);
      assert.equal(document.querySelector(".error, [role='alert']"), null);
    } finally { dom.window.close(); }
  }
  const native = overview({ run_id: "run-native", queue: { ...queue, status: "completed" }, queue_only: false });
  try { assert.equal(native.document.querySelector("[role='status']"), null); }
  finally { native.dom.window.close(); }
});

test("resource constraints are human readable and untrusted text is escaped", () => {
  assert.deepEqual(queueResourceLabels({ ...resources, cpus: 1, gpus: 0, gpu_ids: [], gpu_model: null, gpu_memory_bytes: null }), [
    "1 CPU slot", "8 GiB memory", "0 GPUs",
  ]);
  const { dom, document } = overview({ run_id: "job-a", queue: { ...queue, task: "<img src=x onerror=alert(1)>" } });
  try {
    assert.equal(document.querySelector("img, script"), null);
    assert.ok(document.body.textContent.includes("<img src=x onerror=alert(1)>"));
  } finally { dom.window.close(); }
});
