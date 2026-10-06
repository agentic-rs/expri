import test from "node:test";
import assert from "node:assert/strict";
import { AutoRefresh } from "./app.js";

async function flush() { for (let index = 0; index < 12; index++) await Promise.resolve(); }
function deferred() { let resolve; const promise = new Promise(accept => { resolve = accept; }); return { promise, resolve }; }
class Clock {
  time = 0;
  sequence = 0;
  timers = new Map();
  now = () => this.time;
  set_timeout = (callback, delay_ms) => { const id = ++this.sequence; this.timers.set(id, { callback, due: this.time + delay_ms }); return id; };
  clear_timeout = timer => { this.timers.delete(timer); };
  async advance(delay_ms) {
    const target = this.time + delay_ms;
    for (;;) {
      const next = [...this.timers].filter(([, timer]) => timer.due <= target).sort((first, second) => first[1].due - second[1].due)[0];
      if (!next) break;
      this.time = next[1].due; this.timers.delete(next[0]); next[1].callback(); await flush();
    }
    this.time = target; await flush();
  }
}
function fixture(t, run = async () => "success") {
  const model = { clock: new Clock(), availability: "ready", states: [], calls: 0, cancellations: 0 };
  model.scheduler = new AutoRefresh({
    clock: model.clock,
    availability: () => model.availability,
    run: async () => { model.calls++; return run(); },
    cancel: () => { model.cancellations++; },
    on_state: state => model.states.push(state),
  });
  t.after(() => model.scheduler.dispose()); model.scheduler.start(); return model;
}

test("slow polls schedule their next tick after completion and never overlap", async t => {
  const pending = deferred(), model = fixture(t, () => pending.promise);
  await model.clock.advance(5_000);
  assert.equal(model.calls, 1);
  await model.clock.advance(80_000);
  assert.equal(model.calls, 1);
  assert.equal(model.clock.timers.size, 0);
  pending.resolve("success"); await flush();
  await model.clock.advance(4_999); assert.equal(model.calls, 1);
  await model.clock.advance(1); assert.equal(model.calls, 2);
});

test("hidden and offline pages stop polling and resume with one immediate tick", async t => {
  const model = fixture(t);
  for (const availability of ["hidden", "offline"]) {
    model.availability = availability; model.scheduler.availabilityChanged();
    const before = model.calls;
    await model.clock.advance(90_000); assert.equal(model.calls, before);
    assert.equal(model.clock.timers.size, 0);
    model.availability = "ready"; model.scheduler.availabilityChanged();
    await model.clock.advance(0); assert.equal(model.calls, before + 1);
    assert.equal(model.clock.timers.size, 1);
  }
});

test("resume waits for a cancelled slow poll to finish instead of starting overlapping work", async t => {
  const pending = deferred(), model = fixture(t, () => pending.promise);
  await model.clock.advance(5_000);
  model.availability = "hidden"; model.scheduler.availabilityChanged();
  model.availability = "ready"; model.scheduler.availabilityChanged();
  await model.clock.advance(0); assert.equal(model.calls, 1);
  pending.resolve("cancelled"); await flush();
  await model.clock.advance(0); assert.equal(model.calls, 2);
});

test("quick toggle resume waits for old work and then checks immediately", async t => {
  const pending = deferred(), model = fixture(t, () => pending.promise);
  await model.clock.advance(5_000);
  model.scheduler.setEnabled(false); model.scheduler.setEnabled(true);
  await model.clock.advance(0); assert.equal(model.calls, 1);
  pending.resolve("cancelled"); await flush();
  await model.clock.advance(0); assert.equal(model.calls, 2);
});

test("errors back off to one minute and a successful or manual refresh resets the delay", async t => {
  let outcome = "failure";
  const model = fixture(t, async () => outcome);
  for (const [delay, retry] of [[5_000, 10_000], [10_000, 20_000], [20_000, 40_000], [40_000, 60_000], [60_000, 60_000]]) {
    await model.clock.advance(delay);
    assert.equal(model.states.at(-1).retry_ms, retry);
    assert.equal(model.states.at(-1).failed, true);
  }
  outcome = "success";
  await model.clock.advance(60_000); assert.equal(model.states.at(-1).retry_ms, 5_000);
  assert.equal(model.states.at(-1).failed, false);
  outcome = "failure";
  await model.clock.advance(5_000); assert.equal(model.states.at(-1).retry_ms, 10_000);
  model.scheduler.refreshCompleted(true); assert.equal(model.states.at(-1).retry_ms, 5_000);
});

test("foreground work and a disabled toggle do not launch background requests", async t => {
  const model = fixture(t);
  model.availability = "busy";
  await model.clock.advance(20_000); assert.equal(model.calls, 0);
  model.availability = "ready";
  model.scheduler.setEnabled(false);
  await model.clock.advance(90_000); assert.equal(model.calls, 0);
  model.scheduler.setEnabled(true);
  await model.clock.advance(0); assert.equal(model.calls, 1);
  model.scheduler.dispose();
  await model.clock.advance(90_000); assert.equal(model.calls, 1);
});

test("changing to the bounded fallback cadence is retained after successful checks", async t => {
  const model = fixture(t);
  model.scheduler.setInterval(30_000);
  await model.clock.advance(5_000);
  assert.equal(model.calls, 1);
  assert.equal(model.states.at(-1).interval_ms, 30_000);
  await model.clock.advance(29_999); assert.equal(model.calls, 1);
  await model.clock.advance(1); assert.equal(model.calls, 2);
});
