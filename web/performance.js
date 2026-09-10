export function clampPosition(x, y, width, height, viewportWidth, viewportHeight) {
  return {
    x: Math.max(8, Math.min(Number.isFinite(x) ? x : viewportWidth - width - 16, Math.max(8, viewportWidth - width - 8))),
    y: Math.max(8, Math.min(Number.isFinite(y) ? y : 84, Math.max(8, viewportHeight - height - 8))),
  };
}
export function formatPercent(value) {
  return typeof value === 'number' && Number.isFinite(value) ? `${value.toFixed(1)}%` : '—';
}
export function formatBytes(value) {
  if (typeof value !== 'number' || !Number.isFinite(value)) return '—';
  return value >= 1073741824 ? `${(value / 1073741824).toFixed(2)} GiB` : `${(value / 1048576).toFixed(0)} MiB`;
}
export function formatMs(value) {
  return typeof value === 'number' && Number.isFinite(value) ? `${value.toFixed(0)} ms` : '—';
}
export function graphPoints(values, width = 300, height = 36) {
  const valid = values.filter(value => typeof value === 'number' && Number.isFinite(value));
  const ceiling = Math.max(100, ...valid);
  return values.map((value, index) => {
    if (typeof value !== 'number' || !Number.isFinite(value)) return null;
    return `${(index * width / Math.max(1, values.length - 1)).toFixed(1)},${(height - Math.min(ceiling, Math.max(0, value)) / ceiling * height).toFixed(1)}`;
  }).filter(Boolean).join(' ');
}

function primaryGpuEngine(gpu) {
  const engines = Object.entries(gpu.engines || {});
  return engines.find(([name]) => name === 'SM' || name === 'render') || engines.find(([,value]) => value != null) || engines[0];
}

function gpuActivityNote(gpu) {
  return {
    sampled: 'Fresh driver activity sample',
    no_new_sample: 'No new process activity sample from NVML; unavailable is not a measured zero',
    warming_up: 'Waiting for a sample after identifying this process',
    unsupported: 'Per-process activity is not supported by this driver/device',
    unavailable: 'Per-process activity query failed',
  }[gpu.activity_status] || '';
}

export function processGpuDisplay(process) {
  if (!process.active || !process.gpus?.length) return { text: '—\n—', detail: 'No GPU measurement attributed to this process' };
  const detail = process.gpus.map(gpu => `${gpu.device} · ${gpu.source}: ${Object.entries(gpu.engines || {}).map(([engine, value]) => `${engine} ${formatPercent(value)}`).join(', ')}; ${gpu.memory_kind}: ${formatBytes(gpu.memory_bytes)}${gpu.activity_status ? `; ${gpuActivityNote(gpu)}` : ''}`).join(' | ');
  if (process.gpus.length > 1) return { text: `${process.gpus.length} GPUs\nSee details`, detail };
  const gpu = process.gpus[0];
  const primary = primaryGpuEngine(gpu);
  return { text: `${primary ? `${primary[0]} ${formatPercent(primary[1])}` : '—'}\n${formatBytes(gpu.memory_bytes)}`, detail };
}

// Keep row order and reuse inactive slots when short-lived helpers restart.
export function mergeProcessRows(previous, current) {
  const remaining = new Set(current);
  const rows = previous.map(old => {
    const process = current.find(item => remaining.has(item) && item.pid === old.pid && item.name === old.name && item.role === old.role);
    if (process) { remaining.delete(process); return { ...process, active: true }; }
    return { ...old, active: false, cpu_percent: null, rss_bytes: null, gpus: [] };
  });
  for (const process of remaining) {
    const slot = rows.findIndex(row => !row.active && row.name === process.name && row.role === process.role);
    const next = { ...process, active: true };
    if (slot === -1) rows.push(next);
    else rows[slot] = next;
  }
  return rows;
}

export function sortProcessRows(rows, column, direction = 'desc') {
  if (!['process', 'cpu', 'memory', 'gpu'].includes(column)) return [...rows];
  const value = row => {
    if (column === 'process') return `${row.role} · ${row.name}`;
    if (column === 'cpu') return row.cpu_percent;
    if (column === 'memory') return row.rss_bytes;
    const engines = (row.gpus || []).map(gpu => primaryGpuEngine(gpu)?.[1]).filter(value => typeof value === 'number' && Number.isFinite(value));
    return engines.length ? Math.max(...engines) : null;
  };
  const known = value => column === 'process' || (typeof value === 'number' && Number.isFinite(value));
  const gpuMemory = row => {
    const values = (row.gpus || []).map(gpu => gpu.memory_bytes).filter(value => typeof value === 'number' && Number.isFinite(value));
    return values.length ? values.reduce((sum, value) => sum + value, 0) : null;
  };
  return rows.map((row, index) => ({ row, index })).sort((a, b) => {
    if (column !== 'process' && a.row.active !== b.row.active) return a.row.active ? -1 : 1;
    const left = value(a.row), right = value(b.row);
    if (known(left) !== known(right)) return known(left) ? -1 : 1;
    const comparison = !known(left) ? 0 : column === 'process' ? left.localeCompare(right, 'en', { numeric: true }) : left - right;
    const sign = direction === 'asc' ? 1 : -1;
    if (comparison) return comparison * sign;
    if (column === 'gpu') {
      const leftMemory = gpuMemory(a.row), rightMemory = gpuMemory(b.row);
      if (known(leftMemory) !== known(rightMemory)) return known(leftMemory) ? -1 : 1;
      if (known(leftMemory) && leftMemory !== rightMemory) return (leftMemory - rightMemory) * sign;
    }
    return a.index - b.index;
  }).map(item => item.row);
}

const WORKER_STAGES = [
  ['decode', 'Decode / resize'], ['face', 'Face'], ['hands', 'Hands / gestures'],
  ['pose', 'Pose'], ['segmentation', 'Person mask'],
  ['observations_publish', 'Send observations'], ['mask_publish', 'Send mask'],
  ['avatar_tracking', 'Avatar tracking'], ['avatar_render', 'Avatar render'],
  ['avatar_publish', 'Send avatar'], ['depth', 'Depth pipeline'],
  ['depth_refine', 'Depth mask refinement'], ['depth_publish', 'Send depth'],
];

export function workerStageRows(sample, now = Date.now()) {
  const fresh = sample && sample.interval_ms > 0 && now - sample.received_at_ms <= 6000;
  return WORKER_STAGES.map(([name, label]) => {
    const stage = fresh ? sample.stages?.[name] : null;
    return {
      label, active: Boolean(stage?.calls),
      rate: stage ? `${(stage.calls * 1000 / sample.interval_ms).toFixed(1)}/s` : '—',
      mean: stage?.calls ? formatMs(stage.total_ms / stage.calls) : '—',
      max: stage?.calls ? formatMs(stage.max_ms) : '—',
    };
  });
}

export function installPerformancePanel() {
  const toggle = document.querySelector('#performance-toggle');
  if (!toggle) return;
  const key = 'tarsier.performance.v1';
  let saved = {};
  try { saved = JSON.parse(localStorage.getItem(key) || '{}') || {}; } catch { /* Storage is optional. */ }
  const panel = document.createElement('section');
  panel.id = 'performance-panel';
  panel.className = 'performance-panel';
  panel.setAttribute('aria-labelledby', 'performance-title');
  panel.innerHTML = `
    <div class="performance-header">
      <button type="button" class="performance-drag" aria-label="Move performance panel. Drag or use arrow keys; Home resets position."><span aria-hidden="true">⠿</span> <span id="performance-title">Performance</span></button>
      <button type="button" class="performance-close" aria-label="Hide performance panel" title="Hide performance panel">×</button>
    </div>
    <div class="performance-body">
      <p class="performance-status" role="status">Waiting for measurements…</p>
      <div class="performance-summary">
        <div><span>Tarsier CPU</span><strong data-metric="cpu">—</strong></div>
        <div><span>Process memory</span><strong data-metric="memory">—</strong></div>
        <div><span>Machine CPU</span><strong data-metric="host">—</strong></div>
        <div><span>Video</span><strong data-metric="fps">—</strong></div>
      </div>
      <svg class="performance-chart" viewBox="0 0 300 36" role="img" aria-label="Recent Tarsier CPU usage"><polyline fill="none" stroke="currentColor" stroke-width="2" /></svg>
      <p class="performance-note">CPU: 100% = one core. <span data-metric="cores"></span> Memory sums process RSS; shared pages can be counted twice.</p>
      <table class="performance-processes"><caption>Processes · inactive rows retained</caption><thead><tr>${[['process','Process'],['cpu','CPU'],['memory','Memory'],['gpu','GPU']].map(([column,label]) => `<th scope="col" data-sort-header="${column}" aria-sort="none"><button type="button" class="performance-sort" data-sort="${column}">${label}</button></th>`).join('')}</tr></thead><tbody></tbody></table>
      <div class="performance-gpus"></div>
      <details class="performance-stages" open><summary>Worker stages</summary><p class="performance-note" data-stage-status></p><div class="performance-stage-scroll"><table><thead><tr><th>Stage</th><th>Calls/s</th><th>Mean</th><th>Max</th></tr></thead><tbody data-stage-rows></tbody></table></div><p class="performance-note">Elapsed time per call, including waits; not CPU time. Parallel stages overlap. No calls means idle or not yet completed. Depth includes preprocessing and GPU readback.</p></details>
      <dl class="performance-latencies"><div><dt>Perception inference</dt><dd data-metric="perception">—</dd></div><div><dt>Last voice inference / pipeline</dt><dd data-metric="voice">—</dd></div></dl>
      <p class="performance-note">Browser CPU is excluded from Tarsier totals. Machine CPU includes all applications. GPU card summaries cover the entire device. GPU process cells show attributed engine activity and GPU buffers; hover for details. Shared DRM buffers or clients may appear in several processes.</p>
    </div>`;
  document.body.append(panel);
  const status = panel.querySelector('.performance-status');
  const handle = panel.querySelector('.performance-drag');
  let visible = saved.visible !== false;
  let position = { x: saved.x, y: saved.y };
  let timer = null;
  let controller = null;
  let generation = 0;
  let lastSample = null;
  let lastAdvance = Date.now();
  let history = [];
  let processRows = [];
  const sort = {
    column: ['process', 'cpu', 'memory', 'gpu'].includes(saved.sort?.column) ? saved.sort.column : null,
    direction: saved.sort?.direction === 'asc' ? 'asc' : 'desc',
  };
  function persist() {
    try { localStorage.setItem(key, JSON.stringify({ ...position, visible, sort })); } catch { /* Keep controls usable without storage. */ }
  }
  function place() {
    if (!visible) return;
    position = clampPosition(position.x, position.y, panel.offsetWidth, panel.offsetHeight, innerWidth, innerHeight);
    panel.style.left = `${position.x}px`;
    panel.style.top = `${position.y}px`;
  }
  function metric(name, text) { panel.querySelector(`[data-metric="${name}"]`).textContent = text; }
  function stale(message) {
    status.textContent = message;
    panel.classList.add('performance-stale');
  }
  function renderProcessRows() {
    const rows = sortProcessRows(processRows, sort.column, sort.direction).map(process => {
      const row = document.createElement('tr');
      row.className = process.active ? '' : 'performance-process-inactive';
      row.title = `${process.role} · ${process.name} (${process.pid})${process.active ? '' : ' · Not present in the latest sample'}`;
      for (const text of [`${process.role} · ${process.name} (${process.pid})`, process.active ? formatPercent(process.cpu_percent) : 'Inactive', formatBytes(process.rss_bytes)]) {
        const cell = document.createElement('td'); cell.textContent = text; row.append(cell);
      }
      const gpu = processGpuDisplay(process);
      const gpuCell = document.createElement('td');
      gpuCell.className = 'performance-process-gpu';
      gpuCell.textContent = gpu.text; gpuCell.title = gpu.detail; row.append(gpuCell);
      return row;
    });
    panel.querySelector('tbody').replaceChildren(...rows);
  }
  function updateSortHeaders() {
    for (const [column, label] of [['process','Process'],['cpu','CPU'],['memory','Memory'],['gpu','GPU']]) {
      const selected = sort.column === column;
      const button = panel.querySelector(`[data-sort="${column}"]`);
      button.textContent = `${label}${selected ? sort.direction === 'asc' ? ' ↑' : ' ↓' : ''}`;
      button.title = column === 'gpu' ? 'Sort by displayed GPU activity, then GPU memory when activity is equal or unavailable; unavailable values stay last' : `Sort by ${label.toLowerCase()}`;
      panel.querySelector(`[data-sort-header="${column}"]`).setAttribute('aria-sort', selected ? sort.direction === 'asc' ? 'ascending' : 'descending' : 'none');
    }
  }
  for (const column of ['process','cpu','memory','gpu']) {
    panel.querySelector(`[data-sort="${column}"]`).addEventListener('click', () => {
      sort.direction = sort.column === column ? sort.direction === 'asc' ? 'desc' : 'asc' : column === 'process' ? 'asc' : 'desc';
      sort.column = column;
      updateSortHeaders(); renderProcessRows(); persist();
    });
  }
  updateSortHeaders();
  function render(data) {
    const resources = data.resources || {};
    if (!resources.sampled_at_ms) { stale('Waiting for the first resource sample…'); return; }
    if (resources.sampled_at_ms !== lastSample) {
      lastSample = resources.sampled_at_ms;
      lastAdvance = Date.now();
      history.push(resources.cpu_percent);
      history = history.slice(-60);
    }
    if (resources.error || Date.now() - lastAdvance > 8000) {
      stale(resources.error || 'Measurements are stale');
      return;
    }
    panel.classList.remove('performance-stale');
    status.textContent = resources.partial ? 'Sampled every 2 s · some processes inaccessible' : 'CPU and GPU sampled every 2 s';
    metric('cpu', formatPercent(resources.cpu_percent));
    metric('memory', formatBytes(resources.rss_bytes));
    metric('host', formatPercent(resources.host_cpu_percent));
    metric('fps', data.video_running ? `${Number(data.video_fps || 0).toFixed(1)} fps` : 'Stopped');
    metric('cores', `${resources.logical_cpus || '—'} logical cores.`);
    metric('perception', data.perception_age_ms == null || data.perception_age_ms > 5000 ? 'No recent sample' : formatMs(data.perception_latency_ms));
    metric('voice', `${formatMs(data.voice_inference_ms)} / ${formatMs(data.voice_pipeline_ms)}`);
    panel.querySelector('polyline').setAttribute('points', graphPoints(history));
    const sample = data.worker_stages;
    const stageFresh = sample && Date.now() - sample.received_at_ms <= 6000;
    panel.querySelector('[data-stage-status]').textContent = stageFresh
      ? `PID ${sample.pid} · last ${(sample.interval_ms / 1000).toFixed(1)} s`
      : 'No recent worker stage sample';
    panel.querySelector('[data-stage-rows]').replaceChildren(...workerStageRows(sample).map(stage => {
      const row = document.createElement('tr');
      if (!stage.active) row.className = 'performance-process-inactive';
      for (const text of [stage.label, stage.rate, stage.mean, stage.max]) {
        const cell = document.createElement('td'); cell.textContent = text; row.append(cell);
      }
      return row;
    }));
    processRows = mergeProcessRows(processRows, resources.processes || []);
    renderProcessRows();
    const gpus = (resources.gpus || []).map(gpu => {
      const block = document.createElement('div');
      const title = document.createElement('strong'); title.textContent = gpu.name;
      const detail = document.createElement('p');
      detail.textContent = `Device load ${formatPercent(gpu.busy_percent)} · VRAM ${formatBytes(gpu.memory_used_bytes)} / ${formatBytes(gpu.memory_total_bytes)}`;
      const note = document.createElement('p'); note.className = 'performance-note';
      note.textContent = `${gpu.source}${gpu.note ? ` · ${gpu.note}` : ''}`;
      block.append(title, detail, note);
      return block;
    });
    const processNote = document.createElement('p');
    processNote.className = 'performance-note';
    processNote.textContent = resources.process_gpu_status === 'available' ? 'Process GPU sampling active · — means no available measurement, not zero.' : 'Process GPU counters unavailable for the current processes / driver.';
    gpus.push(processNote);
    if (!(resources.gpus || []).length) { const note = document.createElement('p'); note.textContent = 'GPU telemetry unavailable on this system.'; gpus.push(note); }
    panel.querySelector('.performance-gpus').replaceChildren(...gpus);
    place();
  }
  async function poll() {
    if (!visible || document.hidden) return;
    const current = generation;
    controller = new AbortController();
    const requestController = controller;
    const timeout = setTimeout(() => requestController.abort(), 4000);
    try {
      const response = await fetch('/api/v1/telemetry', { cache: 'no-store', signal: requestController.signal });
      if (!response.ok) throw new Error(response.status === 401 ? 'Authentication required' : `Telemetry unavailable (HTTP ${response.status})`);
      const data = await response.json();
      if (current === generation) render(data);
    } catch (error) {
      if (current === generation) stale(error.name === 'AbortError' ? 'Telemetry request timed out' : error.message);
    } finally {
      clearTimeout(timeout);
      if (current === generation) {
        controller = null;
        if (visible && !document.hidden) timer = setTimeout(poll, 2000);
      }
    }
  }
  function refreshPolling() {
    generation++;
    clearTimeout(timer);
    timer = null;
    controller?.abort();
    controller = null;
    if (visible && !document.hidden) { stale('Refreshing measurements…'); void poll(); }
  }
  function setVisible(next, focus = false) {
    visible = next;
    panel.hidden = !visible;
    toggle.setAttribute('aria-expanded', String(visible));
    toggle.setAttribute('aria-pressed', String(visible));
    if (visible) { place(); if (focus) handle.focus(); }
    else if (focus) toggle.focus();
    persist(); refreshPolling();
  }
  toggle.addEventListener('click', () => setVisible(!visible, true));
  panel.querySelector('.performance-close').addEventListener('click', () => setVisible(false, true));
  panel.addEventListener('keydown', event => {
    if (event.key === 'Escape') { event.preventDefault(); setVisible(false, true); }
  });
  let drag = null;
  handle.addEventListener('pointerdown', event => {
    if (event.button !== 0) return;
    drag = { id: event.pointerId, x: event.clientX - position.x, y: event.clientY - position.y };
    handle.setPointerCapture(event.pointerId);
  });
  handle.addEventListener('pointermove', event => {
    if (!drag || drag.id !== event.pointerId) return;
    position = { x: event.clientX - drag.x, y: event.clientY - drag.y }; place();
  });
  function endDrag() { if (drag) { drag = null; persist(); } }
  handle.addEventListener('pointerup', endDrag);
  handle.addEventListener('pointercancel', endDrag);
  handle.addEventListener('lostpointercapture', endDrag);
  handle.addEventListener('keydown', event => {
    const delta = event.shiftKey ? 40 : 10;
    const moves = { ArrowLeft: [-delta, 0], ArrowRight: [delta, 0], ArrowUp: [0, -delta], ArrowDown: [0, delta] };
    if (event.key === 'Home') { position = {}; }
    else if (moves[event.key]) { position.x += moves[event.key][0]; position.y += moves[event.key][1]; }
    else return;
    event.preventDefault(); place(); persist();
  });
  window.addEventListener('resize', () => { place(); persist(); });
  new ResizeObserver(place).observe(panel);
  document.addEventListener('visibilitychange', refreshPolling);
  setVisible(visible);
}

if (typeof document !== 'undefined') installPerformancePanel();
