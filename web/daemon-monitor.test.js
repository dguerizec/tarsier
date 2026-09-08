import test from 'node:test';
import assert from 'node:assert/strict';
import { createDaemonMonitor } from './daemon-monitor.js';

const health = value => ({ ok: true, json: async () => ({ started_at_ms: value }) });

test('reloads once after a restart, retaining the instance across outages', async () => {
  const replies = [health(100), new Error('offline'), { ok: false }, health(null), health(100), health(200)];
  let reloads = 0;
  const monitor = createDaemonMonitor({
    fetchHealth: async () => {
      const response = replies.shift();
      if (response instanceof Error) throw response;
      return response;
    },
    reload: () => reloads++,
  });
  for (let i = 0; i < 5; i++) await monitor.check();
  assert.equal(reloads, 0);
  await monitor.check();
  monitor.observe(300);
  await monitor.check();
  assert.equal(reloads, 1);
});

test('state observations seed detection and overlapping polls are skipped', async () => {
  let resolve;
  let requests = 0;
  let reloads = 0;
  const monitor = createDaemonMonitor({
    fetchHealth: () => { requests++; return new Promise(done => { resolve = done; }); },
    reload: () => reloads++,
  });
  monitor.observe(100);
  const pending = monitor.check();
  await monitor.check();
  assert.equal(requests, 1);
  resolve(health(200));
  await pending;
  assert.equal(reloads, 1);
});
