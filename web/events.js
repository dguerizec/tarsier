// One semantic event connection per page, shared by visible components.
export function createEventClient({
  openSocket = () => new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/api/v1/events`),
  schedule = callback => setTimeout(callback, 1000),
  cancel = timer => clearTimeout(timer),
  authorize = async () => {
    const response = await fetch('/api/v1/auth/status', {cache: 'no-store'});
    const auth = await response.json();
    if (auth.enabled && !auth.admin) { location.assign('/login'); return false; }
    return true;
  },
} = {}) {
  const listeners = new Map();
  let socket = null;
  let retry = null;
  let generation = 0;
  let reconnecting = false;
  let perceptionDemand = null;
  const sendDemand = () => {
    if (socket?.readyState === 1 && perceptionDemand) socket.send(JSON.stringify(perceptionDemand));
  };
  const emit = (type, value) => {
    for (const callback of [...(listeners.get(type) || [])]) {
      try { callback(value); } catch (error) { console.error(error); }
    }
  };
  function connect() {
    if (socket || retry !== null || reconnecting || !listeners.size) return;
    const current = openSocket();
    socket = current;
    current.addEventListener('open', () => { if (socket === current) { sendDemand(); emit('connected'); } });
    current.addEventListener('message', ({data}) => {
      if (socket !== current) return;
      let message;
      try { message = JSON.parse(data); } catch { return; }
      if (!message || typeof message !== 'object') return;
      emit(message.type, message.data);
      if (message?.type === 'event' && message.data?.kind) emit(`event:${message.data.kind}`, message.data);
    });
    current.addEventListener('close', async () => {
      if (socket !== current) return;
      socket = null;
      const version = generation;
      reconnecting = true;
      emit('disconnected');
      let allowed = true;
      try { allowed = await authorize(); } catch { /* Retry transient outages. */ }
      if (version !== generation) return;
      reconnecting = false;
      if (allowed && listeners.size) {
        retry = schedule(() => { retry = null; connect(); });
      }
    });
  }
  function subscribe(type, callback) {
    if (!listeners.has(type)) listeners.set(type, new Set());
    listeners.get(type).add(callback);
    connect();
    if (type === 'connected' && socket?.readyState === 1) callback();
    return () => {
      listeners.get(type)?.delete(callback);
      if (!listeners.get(type)?.size) listeners.delete(type);
      if (!listeners.size) {
        generation++;
        reconnecting = false;
        if (retry !== null) cancel(retry);
        retry = null;
        const previous = socket;
        socket = null;
        previous?.close();
      }
    };
  }
  function setPerceptionDemand(models = {}, events = []) {
    const next = {type: 'perception.subscribe', models, events};
    if (JSON.stringify(next) === JSON.stringify(perceptionDemand)) return;
    perceptionDemand = next;
    sendDemand();
  }
  return {subscribe, setPerceptionDemand};
}

export const events = createEventClient();

// Lifecycle includes hidden ancestors (settings tabs/dialogs) and browser tabs.
export function observeVisible(element, mount) {
  let cleanup = null;
  let suspended = false;
  const update = () => {
    const visible = !suspended && !document.hidden && !element.closest('[hidden]')
      && (!element.closest('dialog') || element.closest('dialog').open);
    if (visible && !cleanup) cleanup = mount() || (() => {});
    if (!visible && cleanup) { cleanup(); cleanup = null; }
  };
  const observer = new MutationObserver(update);
  for (let node = element; node; node = node.parentElement) {
    observer.observe(node, {attributes: true, attributeFilter: ['hidden', 'open']});
  }
  const hide = () => { suspended = true; update(); };
  const show = () => { suspended = false; update(); };
  window.addEventListener('pagehide', hide);
  window.addEventListener('pageshow', show);
  document.addEventListener('visibilitychange', update);
  update();
  return () => {
    observer.disconnect();
    document.removeEventListener('visibilitychange', update);
    window.removeEventListener('pagehide', hide);
    window.removeEventListener('pageshow', show);
    cleanup?.();
  };
}

export function subscribeVisibleRefresh(element, type, refresh, onError, client = events) {
  return observeVisible(element, () => {
    const controller = new AbortController();
    let loading = false;
    let dirty = false;
    const update = async () => {
      if (controller.signal.aborted) return;
      if (loading) { dirty = true; return; }
      loading = true;
      try { await refresh(controller.signal); }
      catch (error) { if (!controller.signal.aborted) onError(error); }
      finally {
        loading = false;
        if (dirty) { dirty = false; void update(); }
      }
    };
    const offChanged = client.subscribe(type, update);
    const offConnected = client.subscribe('connected', update);
    void update();
    return () => { controller.abort(); offChanged(); offConnected(); };
  });
}
