import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const source = await readFile(new URL('../src/webview_bridge.js', import.meta.url), 'utf8');
const calls = [];
const timers = new Map();
const events = new Map();
let timerId = 0;
const window = {
  __rsAPI__(command, payload, success, failure) {
    calls.push({ command, payload: JSON.parse(payload), success, failure });
  },
  addEventListener(name, callback) {
    events.set(name, callback);
  },
};
vm.runInNewContext(source, {
  window,
  setTimeout(callback) {
    timers.set(++timerId, callback);
    return timerId;
  },
  clearTimeout(id) {
    timers.delete(id);
  },
});

const result = window.lintel.invoke('echo', { value: 42 });
const first = calls.at(-1);
assert.equal(first.command, 'echo');
assert.equal(first.payload.__lintel, 'request-v1');
assert.deepEqual(first.payload.input, { value: 42 });
first.success('42');
assert.equal(await result, 42);
assert.equal(timers.size, 0);

const controller = new AbortController();
const cancelled = window.lintel.invoke('wait', null, { signal: controller.signal });
const running = calls.at(-1);
controller.abort();
await assert.rejects(cancelled, error => error.code === 'CANCELLED');
assert.equal(calls.at(-1).command, '__lintel.cancel');
assert.equal(calls.at(-1).payload.id, running.payload.id);
running.success('"late reply"');
assert.equal(timers.size, 0);

const timeout = window.lintel.invoke('wait');
for (const fire of Array.from(timers.values())) fire();
await assert.rejects(timeout, error => error.code === 'TIMEOUT');
await assert.rejects(window.lintel.invoke('wait', null, { timeoutMs: 0 }), error => error.code === 'INVALID_INPUT');

const pending = Array.from({ length: 64 }, () => window.lintel.invoke('wait'));
const completed = Promise.allSettled(pending);
await assert.rejects(window.lintel.invoke('overflow'), error => error.code === 'BUSY');
events.get('pagehide')();
assert.ok((await completed).every(result => result.status === 'rejected' && result.reason.code === 'CANCELLED'));
assert.equal(timers.size, 0);
console.log('bridge JS passed: typed envelope, cancellation, late replies, timeout, limits and page teardown');
