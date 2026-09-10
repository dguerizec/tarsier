import test from 'node:test';
import assert from 'node:assert/strict';
import { clampPosition, formatPercent, formatBytes, graphPoints } from './performance.js';
test('panel stays reachable after resize and invalid saved coordinates', () => {
  assert.deepEqual(clampPosition(900, 800, 390, 500, 400, 600), { x: 8, y: 92 });
  assert.deepEqual(clampPosition(NaN, undefined, 200, 100, 800, 600), { x: 584, y: 84 });
  assert.deepEqual(clampPosition(-200, -80, 390, 900, 320, 400), { x: 8, y: 8 });
});
test('unavailable resources never display as zero', () => {
  assert.equal(formatPercent(null), '—');
  assert.equal(formatPercent(NaN), '—');
  assert.equal(formatPercent(250), '250.0%');
  assert.equal(formatBytes(undefined), '—');
  assert.equal(formatBytes(1073741824), '1.00 GiB');
});
test('CPU graph scales above one core and filters invalid measurements', () => {
  assert.equal(graphPoints([0, 100, 200]), '0.0,36.0 150.0,18.0 300.0,0.0');
  assert.equal(graphPoints([null, NaN]), '');
});

test('panel hides without polling, persists drag position, and resumes when shown', async () => {
  const { installPerformancePanel } = await import('./performance.js');
  class Element {
    constructor() { this.listeners = new Map(); this.nodes = new Map(); this.style = {}; this.attributes = {}; this.offsetWidth = 390; this.offsetHeight = 500; this.children = []; this.classList = { add() {}, remove() {} }; }
    querySelector(selector) { if (!this.nodes.has(selector)) this.nodes.set(selector, new Element()); return this.nodes.get(selector); }
    setAttribute(name, value) { this.attributes[name] = value; }
    addEventListener(name, callback) { this.listeners.set(name, callback); }
    emit(name, event = {}) { this.listeners.get(name)?.(event); }
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = children; }
    setPointerCapture() {}
    focus() { this.focused = true; }
  }
  const savedGlobals = new Map();
  const replace = (key, value) => { savedGlobals.set(key, Object.getOwnPropertyDescriptor(globalThis, key)); Object.defineProperty(globalThis, key, { value, configurable: true, writable: true }); };
  const document = new Element();
  document.body = new Element(); document.hidden = false; document.createElement = () => new Element();
  const store = new Map();
  const timers = new Map(); let next = 0; let requests = 0;
  const flush = () => new Promise(resolve => setImmediate(resolve));
  try {
    replace('document', document); replace('window', new Element());
    replace('innerWidth', 1000); replace('innerHeight', 800);
    replace('localStorage', { getItem: key => store.get(key), setItem: (key, value) => store.set(key, value) });
    replace('ResizeObserver', class { observe() {} });
    replace('setTimeout', (callback, delay) => { timers.set(++next, { callback, delay }); return next; });
    replace('clearTimeout', id => timers.delete(id));
    replace('fetch', async () => { requests++; return { ok: true, json: async () => ({ resources: { sampled_at_ms: Date.now(), logical_cpus: 8, cpu_percent: 123, processes: [], gpus: [] } }) }; });
    installPerformancePanel(); await flush();
    const toggle = document.querySelector('#performance-toggle');
    const panel = document.body.children[0];
    const drag = panel.querySelector('.performance-drag');
    assert.equal(requests, 1);
    drag.emit('pointerdown', { button: 0, pointerId: 1, clientX: 600, clientY: 100 });
    drag.emit('pointermove', { pointerId: 1, clientX: 100, clientY: 200 });
    drag.emit('pointerup');
    const stored = JSON.parse(store.get('tarsier.performance.v1'));
    assert.equal(stored.x, 94); assert.equal(stored.y, 184);
    panel.querySelector('.performance-close').emit('click');
    assert.equal(panel.hidden, true); assert.equal(toggle.attributes['aria-expanded'], 'false');
    assert.equal(timers.size, 0); assert.equal(JSON.parse(store.get('tarsier.performance.v1')).visible, false);
    toggle.emit('click'); await flush();
    assert.equal(requests, 2); assert.equal(panel.hidden, false);
    document.hidden = true; document.emit('visibilitychange');
    assert.equal(timers.size, 0);
    document.hidden = false; document.emit('visibilitychange'); await flush();
    assert.equal(requests, 3);
    panel.emit('keydown', { key: 'Escape', preventDefault() {} });
    assert.equal(panel.hidden, true); assert.equal(timers.size, 0);
  } finally {
    for (const [key, descriptor] of savedGlobals) { if (descriptor) Object.defineProperty(globalThis, key, descriptor); else delete globalThis[key]; }
  }
});

test('departed helpers stay gray-ready with cleared readings and stable ordering', async () => {
  const { mergeProcessRows } = await import('./performance.js');
  const daemon = { pid: 1, name: 'tarsier', role: 'Daemon', cpu_percent: 80, rss_bytes: 100 };
  const helper = { pid: 2, name: 'pw-dump', role: 'Worker', cpu_percent: 10, rss_bytes: 50 };
  let rows = mergeProcessRows([], [daemon, helper]);
  rows = mergeProcessRows(rows, [daemon]);
  assert.deepEqual(rows.map(row => row.pid), [1, 2]);
  assert.equal(rows[1].active, false);
  assert.equal(rows[1].cpu_percent, null);
  assert.equal(rows[1].rss_bytes, null);
  for (let pid = 3; pid < 20; pid++) {
    rows = mergeProcessRows(rows, [{ ...helper, pid }, daemon]);
    assert.deepEqual(rows.map(row => row.pid), [1, pid]);
    assert.equal(rows[1].active, true);
    rows = mergeProcessRows(rows, [daemon]);
  }
  assert.equal(rows.length, 2);
});

test('concurrent helpers retain distinct rows and exact PID matches take precedence', async () => {
  const { mergeProcessRows } = await import('./performance.js');
  const helper = { name: 'pw-dump', role: 'Worker', cpu_percent: 5, rss_bytes: 50 };
  let rows = mergeProcessRows([], [{ ...helper, pid: 1 }, { ...helper, pid: 2 }]);
  rows = mergeProcessRows(rows, [{ ...helper, pid: 3 }, { ...helper, pid: 2 }]);
  assert.deepEqual(rows.map(row => row.pid), [3, 2]);
  assert.ok(rows.every(row => row.active));
});

test('GPU display distinguishes engines, unknown readings, inactive rows and multiple GPUs', async () => {
  const { processGpuDisplay } = await import('./performance.js');
  const gpu = {device:'NVIDIA GPU 0',source:'pmon',engines:{SM:25,encode:null},memory_bytes:104857600,memory_kind:'framebuffer memory'};
  assert.equal(processGpuDisplay({active:true,gpus:[gpu]}).text, 'SM 25.0%\n100 MiB');
  assert.match(processGpuDisplay({active:true,gpus:[gpu]}).detail, /encode —/);
  assert.equal(processGpuDisplay({active:false,gpus:[gpu]}).text, '—\n—');
  assert.equal(processGpuDisplay({active:true,gpus:[gpu,gpu]}).text, '2 GPUs\nSee details');
});
