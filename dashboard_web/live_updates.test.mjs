import test from "node:test";
import assert from "node:assert/strict";
import { LiveUpdates, parseUpdateHint } from "./.test/app.js";

class FakeEventSource {
  readyState = 0;
  listeners = new Map();
  closed = 0;
  addEventListener(type, listener) { const listeners = this.listeners.get(type) ?? new Set(); listeners.add(listener); this.listeners.set(type, listeners); }
  removeEventListener(type, listener) { this.listeners.get(type)?.delete(listener); }
  emit(type, data) { for (const listener of [...this.listeners.get(type) ?? []]) listener({ type, data }); }
  close() { this.closed++; this.readyState = 2; }
}
function fixture(t, create) {
  const model = { sources: [], hints: 0, states: [] };
  model.client = new LiveUpdates({ create: create ?? (() => { const source = new FakeEventSource(); model.sources.push(source); return source; }), on_hint: () => model.hints++, on_state: state => model.states.push(state) });
  t.after(() => model.client.dispose()); return model;
}

test("live notifications accept only bounded revision hints", () => {
  assert.equal(parseUpdateHint('{"catalog_revision":"9223372036854775807"}'), "9223372036854775807");
  assert.equal(parseUpdateHint('{"catalog_revision":"legacy-1"}'), "legacy-1");
  for (const value of [null, {}, "{", "[]", '{"catalog_revision":5}', '{"catalog_revision":""}', JSON.stringify({ catalog_revision: "a".repeat(129) }), JSON.stringify({ catalog_revision: "1", metrics: "a".repeat(256) }), '{"catalog_revision":"bad\\nrevision"}']) assert.equal(parseUpdateHint(value), null);
});

test("duplicate hints coalesce and stale scope events are detached", t => {
  const model = fixture(t);
  model.client.setUrl("/api/events?source=a&run_id=run-1");
  const source = model.sources[0], stale = [...source.listeners.get("updates")][0];
  source.readyState = 1; source.emit("open");
  source.emit("updates", '{"catalog_revision":"1"}'); source.emit("updates", '{"catalog_revision":"1"}');
  assert.equal(model.hints, 2, "open probes catch-up and one distinct revision adds one hint");
  source.emit("updates", '{"catalog_revision":"2"}'); assert.equal(model.hints, 3);
  source.emit("updates", '{"catalog_revision":true}'); assert.equal(model.hints, 3);
  model.client.setUrl("/api/events?source=b");
  assert.equal(source.closed, 1); assert.equal(source.listeners.get("updates").size, 0);
  stale({ data: '{"catalog_revision":"3"}' }); assert.equal(model.hints, 3);
  model.client.dispose(); model.client.dispose(); assert.equal(model.sources[1].closed, 1);
  model.client.setUrl("/api/events?source=c"); assert.equal(model.sources.length, 2);
});

test("transient errors keep native reconnect and a permanent unsupported endpoint falls back", t => {
  const model = fixture(t);
  model.client.setUrl("/api/events?source=a"); const source = model.sources[0];
  source.emit("error"); assert.equal(source.closed, 0); assert.equal(model.states.at(-1), "reconnecting");
  source.readyState = 1; source.emit("open"); assert.equal(model.hints, 1); assert.equal(model.states.at(-1), "connected");
  source.readyState = 0; source.emit("error"); source.readyState = 1; source.emit("open"); assert.equal(model.hints, 2, "reconnect checks missed events even without a new revision");
  source.readyState = 2; source.emit("error"); assert.equal(source.closed, 1); assert.equal(model.states.at(-1), "unavailable");
  model.client.setUrl("/api/events?source=a"); assert.equal(model.sources.length, 1, "a 404-like closed stream does not reopen on every render");
  model.client.setUrl(null); model.client.setUrl("/api/events?source=a"); assert.equal(model.sources.length, 2);
});

test("missing or rejected EventSource support keeps the polling path usable", t => {
  for (const create of [() => null, () => { throw new Error("unsupported"); }]) {
    const model = fixture(t, create); model.client.setUrl("/api/events");
    assert.equal(model.states.at(-1), "unavailable"); assert.equal(model.hints, 0);
    model.client.setUrl(null); model.client.dispose();
  }
});
