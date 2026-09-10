import test from 'node:test';
import assert from 'node:assert/strict';
import {createEventClient} from './events.js';

function fixture(authorize = async () => true) {
  const sockets = [];
  const timers = new Set();
  const client = createEventClient({
    openSocket() {
      const handlers = new Map();
      const socket = {
        readyState: 0,
        addEventListener(type, callback) { handlers.set(type, callback); },
        fire(type, value) { return handlers.get(type)?.(value); },
        close() { this.closed = true; return this.fire('close'); },
      };
      sockets.push(socket);
      return socket;
    },
    authorize,
    schedule(callback) { timers.add(callback); return callback; },
    cancel(callback) { timers.delete(callback); },
  });
  return {client, sockets, timers};
}

test('components share a connection and receive only subscribed event kinds', () => {
  const {client, sockets} = fixture();
  const seen = [];
  const off = client.subscribe('event:avatar.library.changed', event => seen.push(event.kind));
  const offState = client.subscribe('state', state => seen.push(state.value));
  assert.equal(sockets.length, 1);
  const socket = sockets[0];
  socket.fire('message', {data: 'invalid'});
  socket.fire('message', {data: 'null'});
  socket.fire('message', {data: JSON.stringify({type: 'event', data: {kind: 'other'}})});
  socket.fire('message', {data: JSON.stringify({type: 'event', data: {kind: 'avatar.library.changed'}})});
  socket.fire('message', {data: JSON.stringify({type: 'state', data: {value: 42}})});
  assert.deepEqual(seen, ['avatar.library.changed', 42]);
  off();
  assert.equal(socket.closed, undefined);
  offState();
  assert.equal(socket.closed, true);
});

test('last unsubscribe cancels a pending reconnect and stale messages are ignored', async () => {
  const {client, sockets, timers} = fixture();
  let calls = 0;
  const off = client.subscribe('state', () => calls++);
  await sockets[0].fire('close');
  assert.equal(timers.size, 1);
  off();
  assert.equal(timers.size, 0);
  const offNew = client.subscribe('state', () => calls++);
  sockets[0].fire('message', {data: '{"type":"state"}'});
  assert.equal(calls, 0);
  assert.equal(sockets.length, 2);
  offNew();
});

test('an authorization response from an old lifecycle cannot reconnect it', async () => {
  let resolve;
  const {client, sockets, timers} = fixture(() => new Promise(done => { resolve = done; }));
  const off = client.subscribe('state', () => {});
  const closing = sockets[0].fire('close');
  off();
  const offNew = client.subscribe('state', () => {});
  resolve(true);
  await closing;
  assert.equal(timers.size, 0);
  assert.equal(sockets.length, 2);
  offNew();
});

test('reconnection notifies components to reload missed state', async () => {
  const {client, sockets, timers} = fixture();
  let connected = 0;
  const off = client.subscribe('connected', () => connected++);
  sockets[0].fire('open');
  await sockets[0].fire('close');
  const callback = [...timers][0];
  timers.delete(callback);
  callback();
  sockets[1].fire('open');
  assert.equal(connected, 2);
  off();
});

test('visible lifecycle subscribes again after a hidden panel or browser page resumes', async () => {
  const {observeVisible} = await import('./events.js');
  const previous = {document: globalThis.document, window: globalThis.window, MutationObserver: globalThis.MutationObserver};
  const document = new EventTarget();
  document.hidden = false;
  globalThis.document = document;
  globalThis.window = new EventTarget();
  let changed;
  globalThis.MutationObserver = class {
    constructor(callback) { changed = callback; }
    observe() {}
    disconnect() {}
  };
  let hidden = false;
  const element = {parentElement: null, closest: selector => selector === '[hidden]' && hidden ? element : null};
  let mounts = 0;
  let unmounts = 0;
  try {
    const stop = observeVisible(element, () => { mounts++; return () => unmounts++; });
    assert.equal(mounts, 1);
    hidden = true; changed(); changed();
    assert.equal(unmounts, 1);
    hidden = false; changed();
    assert.equal(mounts, 2);
    document.hidden = true; document.dispatchEvent(new Event('visibilitychange'));
    assert.equal(unmounts, 2);
    document.hidden = false; document.dispatchEvent(new Event('visibilitychange'));
    globalThis.window.dispatchEvent(new Event('pagehide'));
    globalThis.window.dispatchEvent(new Event('pageshow'));
    assert.equal(mounts, 4);
    stop();
    assert.equal(unmounts, 4);
  } finally {
    Object.assign(globalThis, previous);
  }
});

test('an open picker refreshes on library changes and stops when closed', async () => {
  const {subscribeVisibleRefresh} = await import('./events.js');
  const previous = {document: globalThis.document, window: globalThis.window, MutationObserver: globalThis.MutationObserver};
  globalThis.document = new EventTarget();
  globalThis.document.hidden = false;
  globalThis.window = new EventTarget();
  let changed;
  globalThis.MutationObserver = class {
    constructor(callback) { changed = callback; }
    observe() {}
    disconnect() {}
  };
  const dialog = {open: false, parentElement: null, closest: selector => selector === 'dialog' ? dialog : null};
  const callbacks = new Map();
  const client = {subscribe(type, callback) { callbacks.set(type, callback); return () => callbacks.delete(type); }};
  let release;
  let calls = 0;
  let signal;
  const flush = () => new Promise(resolve => setImmediate(resolve));
  try {
    const stop = subscribeVisibleRefresh(dialog, 'event:avatar.library.changed', async value => {
      signal = value;
      calls++;
      await new Promise(resolve => { release = resolve; });
    }, assert.fail, client);
    assert.equal(calls, 0);
    dialog.open = true; changed();
    assert.equal(calls, 1);
    callbacks.get('event:avatar.library.changed')();
    callbacks.get('event:avatar.library.changed')();
    assert.equal(calls, 1);
    release(); await flush();
    assert.equal(calls, 2);
    release(); await flush();
    callbacks.get('event:avatar.library.changed')();
    assert.equal(calls, 3);
    dialog.open = false; changed();
    assert.equal(signal.aborted, true);
    assert.equal(callbacks.size, 0);
    release(); await flush();
    stop();
  } finally { Object.assign(globalThis, previous); }
});
