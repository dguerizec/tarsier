const tracks = new Map();
const container = document.querySelector('#audio-tracks');
const status = document.querySelector('#audio-status');
let enabledSources = null;
let reservations = {};
let releasedSources = new Set();

const applicationsDialog = document.querySelector('#audio-applications-dialog');
const applicationsStatus = document.querySelector('#audio-applications-status');
const applicationsList = document.querySelector('#audio-applications-list');
let inspectedSource = null;
let applicationsRequest = null;
let applicationsTimer = null;
let applicationKillPending = false;
let applicationsAutoRefresh = false;
const applicationActionStatus = document.querySelector('#audio-applications-action-status');

async function killApplication(app) {
  if (applicationKillPending || !app.process || !app.next_signal) return;
  applicationKillPending = true;
  applicationsAutoRefresh = true;
  applicationsRequest?.abort();
  clearTimeout(applicationsTimer);
  applicationsList.querySelectorAll('button').forEach((button) => { button.disabled = true; });
  const source = inspectedSource;
  applicationActionStatus.classList.remove('error');
  applicationActionStatus.textContent = `Sending SIG${app.next_signal === 15 ? 'TERM' : 'KILL'} to ${app.name}…`;
  try {
    const response = await fetch('/api/v1/audio/applications/kill', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ source, process: app.process, signal: app.next_signal }),
    });
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || 'Could not signal this application');
    if (inspectedSource === source) applicationActionStatus.textContent = app.next_signal === 15
      ? `SIGTERM sent to ${app.name}. If it remains connected, click Kill -9 to force it to exit.`
      : `SIGKILL sent to ${app.name}.`;
  } catch (error) {
    if (inspectedSource === source) {
      applicationActionStatus.classList.add('error');
      applicationActionStatus.textContent = error.message;
    }
  } finally {
    applicationKillPending = false;
    void refreshApplications();
  }
}

async function refreshApplications() {
  if (!applicationsDialog.open || !inspectedSource || applicationKillPending) return;
  clearTimeout(applicationsTimer);
  applicationsRequest?.abort();
  const request = new AbortController();
  applicationsRequest = request;
  try {
    const url = new URL('/api/v1/audio/applications', location.href);
    url.searchParams.set('source', inspectedSource);
    const response = await fetch(url, { signal: request.signal, cache: 'no-store' });
    const data = await response.json();
    if (!response.ok) throw new Error(data.error || 'Could not list connected applications');
    if (applicationsRequest !== request || !applicationsDialog.open) return;
    applicationsList.replaceChildren();
    for (const app of data.applications) {
      const row = document.createElement('li');
      const name = document.createElement('strong');
      name.textContent = app.name;
      const detail = document.createElement('span');
      detail.textContent = [app.binary, app.pid ? `PID ${app.pid}` : null,
        app.streams > 1 ? `${app.streams} capture streams` : null].filter(Boolean).join(' · ');
      const info = document.createElement('div');
      info.append(name, detail);
      const kill = document.createElement('button');
      kill.type = 'button';
      kill.className = 'secondary compact audio-application-kill';
      kill.textContent = app.next_signal === 9 ? 'Kill -9' : 'Kill -15';
      kill.disabled = !app.process || !app.next_signal || applicationKillPending;
      kill.title = app.process ? `Send ${app.next_signal === 9 ? 'SIGKILL' : 'SIGTERM'} to ${app.name}` : 'No verifiable local process is available';
      kill.onclick = () => void killApplication(app);
      row.append(info, kill);
      applicationsList.append(row);
    }
    applicationsStatus.classList.remove('error');
    applicationsStatus.textContent = !data.available ? 'This microphone is no longer available.'
      : data.applications.length ? (applicationsAutoRefresh ? 'Connected applications · Updates every 2 seconds' : 'Connected applications')
      : 'No applications are connected. The input may be unavailable for another reason.';
  } catch (error) {
    if (request.signal.aborted) return;
    applicationsList.replaceChildren();
    applicationsStatus.classList.add('error');
    applicationsStatus.textContent = error.message;
  } finally {
    if (applicationsRequest === request && applicationsDialog.open && applicationsAutoRefresh) {
      applicationsTimer = setTimeout(refreshApplications, 2000);
    }
  }
}

function showApplications(track) {
  applicationsAutoRefresh = false;
  inspectedSource = track.id;
  document.querySelector('#audio-applications-source').textContent = track.name;
  applicationsList.replaceChildren();
  applicationActionStatus.textContent = '';
  applicationsStatus.textContent = 'Loading…';
  applicationsStatus.classList.remove('error');
  applicationsDialog.showModal();
  void refreshApplications();
}
document.querySelector('#audio-applications-refresh').onclick = () => void refreshApplications();
applicationsDialog.addEventListener('close', () => {
  clearTimeout(applicationsTimer);
  applicationsRequest?.abort();
  applicationsAutoRefresh = false;
  inspectedSource = null;
});
applicationsDialog.addEventListener('click', (event) => {
  if (event.target === applicationsDialog) applicationsDialog.close();
});

function renderReservation(track) {
  if (track.id === virtualId) return;
  const released = releasedSources.has(track.id);
  const reservation = reservations[track.id];
  const transitioning = track.reservationPending
    || (released && reservation?.status === 'held')
    || (!released && reservation?.status === 'released');
  const state = transitioning ? 'pending' : released ? 'unlocked'
    : reservation?.status === 'held' ? 'locked'
    : reservation?.status === 'unavailable' ? 'shared'
    : reservation?.status === 'disconnected' ? 'disconnected' : 'pending';
  const labels = { locked: 'Locked', unlocked: 'Unlocked', shared: 'Shared',
    pending: released ? 'Unlocking…' : 'Locking…', disconnected: 'Disconnected' };
  const actions = { locked: 'Unlock this microphone', unlocked: 'Lock this microphone',
    shared: 'Show applications and Kill controls. The input is busy or unavailable.' };
  const button = track.reserveButton;
  button.textContent = labels[state];
  button.dataset.state = state;
  button.disabled = state === 'pending' || state === 'disconnected';
  button.title = actions[state] || labels[state];
  button.setAttribute('aria-label', `${labels[state]} · ${track.name}. ${actions[state] || ''}`);
  if (state === 'shared') button.setAttribute('aria-haspopup', 'dialog');
  else button.removeAttribute('aria-haspopup');
  track.reservationMessage.textContent = track.reservationError || '';
  track.reservationMessage.hidden = !track.reservationError;
}

async function setExclusive(track) {
  if (track.reservationPending) return;
  const exclusive = releasedSources.has(track.id);
  track.reservationPending = true;
  track.reservationError = null;
  renderReservation(track);
  try {
    const response = await fetch('/api/v1/audio/exclusive', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ source: track.id, exclusive }),
    });
    if (!response.ok) throw new Error('Could not update microphone reservation');
  } catch (error) {
    track.reservationError = error.message;
  } finally {
    track.reservationPending = false;
    renderReservation(track);
  }
}
const virtualId = 'tarsier_microphone';
const outputSource = document.querySelector('#audio-output-source');
const outputStatus = document.querySelector('#audio-output-status');
const outputError = document.querySelector('#audio-output-error');
let virtualState = { enabled: false, muted: false, source: null, running: false };
let virtualPending = false;
let availableSources = [];

function renderOutput() {
  const selected = virtualState.source || '';
  if (selected && ![...outputSource.options].some((option) => option.value === selected)) {
    outputSource.add(new Option('Disconnected microphone', selected));
  }
  outputSource.value = selected;
  outputSource.disabled = virtualPending;
  document.querySelectorAll('[data-audio-output]').forEach((button) => {
    const on = button.dataset.audioOutput === 'true';
    button.setAttribute('aria-pressed', String(virtualState.enabled === on));
    button.disabled = virtualPending || (on && !selected);
  });
  document.querySelectorAll('[data-audio-mute]').forEach((button) => {
    button.setAttribute('aria-pressed', String(virtualState.muted === (button.dataset.audioMute === 'true')));
    button.disabled = virtualPending;
  });
  outputStatus.textContent = !virtualState.enabled ? 'Virtual microphone off'
    : virtualState.error ? virtualState.error
    : !virtualState.running ? 'Starting virtual microphone…'
    : virtualState.muted ? 'Output muted · Sending silence'
    : !availableSources.some((s) => s.id === selected) ? 'Input disconnected · Sending silence'
    : !enabledSources?.has(selected) ? 'Input capture off · Sending silence'
    : 'Tarsier Microphone is ready';
  const track = tracks.get(virtualId);
  if (track) syncTrack(track);
}

async function updateOutput(patch) {
  if (virtualPending) return;
  virtualPending = true;
  outputError.hidden = true;
  renderOutput();
  try {
    const response = await fetch('/api/v1/audio/virtual', {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(patch),
    });
    if (!response.ok) {
      const body = await response.json();
      throw new Error(body.error || 'Could not update virtual microphone');
    }
    // Shared runtime events update all clients, including this one.
  } catch (error) {
    outputError.textContent = error.message;
    outputError.hidden = false;
  } finally {
    virtualPending = false;
    renderOutput();
  }
}
outputSource.onchange = () => { if (outputSource.value) void updateOutput({ source: outputSource.value }); };
document.querySelectorAll('[data-audio-output]').forEach((button) => {
  button.onclick = () => void updateOutput({ enabled: button.dataset.audioOutput === 'true' });
});
document.querySelectorAll('[data-audio-mute]').forEach((button) => {
  button.onclick = () => void updateOutput({ muted: button.dataset.audioMute === 'true' });
});

export function syncAudioCapture(sources, output, currentReservations, released) {
  reservations = currentReservations || {};
  releasedSources = new Set(released || []);
  if (output) virtualState = output;
  enabledSources = new Set(sources);
  for (const track of tracks.values()) syncTrack(track);
  renderOutput();
}

function syncTrack(track) {
  renderReservation(track);
  track.enabled = track.id === virtualId ? virtualState.enabled : enabledSources?.has(track.id) ?? track.enabled;
  track.buttons.forEach((button, index) => {
    button.setAttribute('aria-pressed', String(index === (track.enabled ? 0 : 1)));
    button.disabled = track.pending || (index === 0 && track.unavailable);
  });
  if (!track.enabled) {
    stop(track, track.unavailable ? 'Disconnected' : 'Input off');
  } else if (!track.socket && Date.now() >= track.retryAt && !track.unavailable) {
    start(track);
  }
}

async function setCapture(track, enabled) {
  if (track.pending) return;
  track.pending = true;
  track.retryAt = 0;
  syncTrack(track);
  try {
    const response = await fetch('/api/v1/audio/capture', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ source: track.id, enabled }),
    });
    if (!response.ok) throw new Error('Could not update audio capture');
    // The shared state stream is authoritative, including concurrent changes.
  } catch (error) {
    track.label.textContent = error.message;
  } finally {
    track.pending = false;
    syncTrack(track);
  }
}

function stop(track, label = 'Input off') {
  const socket = track.socket;
  track.socket = null;
  socket?.close();
  track.label.textContent = label;
  track.buttons.forEach((button, index) => button.setAttribute('aria-pressed', String(index === (track.enabled ? 0 : 1))));
  track.level = null;
}

function start(track) {
  stop(track);
  track.retryAt = Date.now() + 5000;
  track.label.textContent = 'Connecting…';
  track.buttons.forEach((button, index) => button.setAttribute('aria-pressed', String(index === 0)));
  const url = new URL('/api/v1/audio/meter', location.href);
  url.protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
  url.searchParams.set('source', track.id);
  const socket = new WebSocket(url);
  track.socket = socket;
  socket.onmessage = ({ data }) => {
    if (track.socket !== socket) return;
    const level = JSON.parse(data);
    if (level.error) { stop(track, level.error); return; }
    const now = performance.now();
    track.level = { ...level, time: now };
    track.history.push(track.level);
    if (track.history.length > 500) track.history.splice(0, track.history.length - 500);
    if (level.clipped) track.clipUntil = now + 1500;
    track.label.textContent = track.muted ? 'Source muted' : 'Capturing';
  };
  socket.onclose = () => {
    if (track.socket === socket) stop(track, 'Capture stopped · Enable to retry');
  };
  socket.onerror = () => {
    if (track.socket === socket) stop(track, 'Capture unavailable · Enable to retry');
  };
}

function create(source) {
  const row = document.createElement('div');
  row.className = 'audio-track';
  row.innerHTML = `<div class="audio-track-header"><strong></strong><span class="audio-track-state"></span>
    <div class="segmented-control" role="group"><button type="button" class="secondary compact" aria-pressed="false">On</button><button type="button" class="secondary compact" aria-pressed="true">Off</button></div></div>
    <div class="audio-visual"><canvas role="img"></canvas><div class="audio-meters">
    <div><span>L</span><meter min="-60" max="0" low="-18" high="-6" optimum="-24" value="-60"></meter></div>
    <div><span>R</span><meter min="-60" max="0" low="-18" high="-6" optimum="-24" value="-60"></meter></div>
    <span class="audio-peak">−∞ dBFS</span></div></div>`;
  row.querySelector('strong').textContent = source.name;
  const track = {
    id: source.id, name: source.name, row, history: [], level: null, socket: null, clipUntil: 0,
    enabled: source.enabled === true, pending: false, unavailable: false, retryAt: 0,
    label: row.querySelector('.audio-track-state'), canvas: row.querySelector('canvas'),
    buttons: [...row.querySelectorAll('button')], meters: [...row.querySelectorAll('meter')],
    peak: row.querySelector('.audio-peak'),
  };
  row.querySelector('[role="group"]').setAttribute('aria-label', `${source.name} capture`);
  track.canvas.setAttribute('aria-label', `${source.name}: amplitude envelope over the last 10 seconds`);
  track.meters.forEach((meter, index) => meter.setAttribute('aria-label', `${source.name} ${index ? 'right' : 'left'} peak in dBFS`));
  track.buttons[0].onclick = () => void setCapture(track, true);
  track.buttons[1].onclick = () => void setCapture(track, false);
  stop(track);
  if (source.id !== virtualId) {
    const reservation = document.createElement('div');
    reservation.className = 'audio-reservation';
    reservation.innerHTML = '<button type="button" class="audio-reservation-status secondary compact"></button><span class="error" role="status" hidden></span>';
    track.reserveButton = reservation.querySelector('button');
    track.reservationMessage = reservation.querySelector('[role="status"]');
    track.reserveButton.onclick = () => {
      if (track.reserveButton.dataset.state === 'shared') showApplications(track);
      else void setExclusive(track);
    };
    row.append(reservation);
    renderReservation(track);
  }
  if (source.id === virtualId) {
    row.querySelector('[role="group"]').hidden = true;
    document.querySelector('#audio-output-track').append(row);
  } else {
    container.append(row);
  }
  tracks.set(source.id, track);
  return track;
}

async function refresh() {
  try {
    const response = await fetch('/api/v1/audio/sources', { cache: 'no-store' });
    const sources = await response.json();
    if (!response.ok) throw new Error(sources.error || 'Audio discovery unavailable');
    status.textContent = sources.length ? '' : 'No microphones detected. Connect an audio input to get started.';
    status.hidden = sources.length > 0;
    availableSources = sources;
    outputSource.replaceChildren(new Option('Select a microphone', ''));
    for (const source of sources) outputSource.add(new Option(source.name, source.id));
    renderOutput();
    for (const source of sources) {
      const track = tracks.get(source.id) || create(source);
      track.row.querySelector('strong').textContent = source.name;
      track.muted = source.muted;
      track.unavailable = false;
      if (enabledSources == null) track.enabled = source.enabled === true;
      syncTrack(track);
    }
    for (const [id, track] of tracks) {
      if (id !== virtualId && !sources.some((source) => source.id === id)) {
        track.unavailable = true;
        stop(track, 'Disconnected');
        syncTrack(track);
      }
    }
  } catch (error) {
    status.hidden = false;
    status.textContent = error.message;
    for (const track of tracks.values()) stop(track, 'Audio server unavailable');
  }
}

function draw(now) {
  for (const track of tracks.values()) {
    track.history = track.history.filter((level) => now - level.time < 10000);
    const canvas = track.canvas;
    const width = Math.max(1, Math.round(canvas.clientWidth * devicePixelRatio));
    const height = Math.max(1, Math.round(canvas.clientHeight * devicePixelRatio));
    if (canvas.width !== width || canvas.height !== height) { canvas.width = width; canvas.height = height; }
    const context = canvas.getContext('2d');
    context.clearRect(0, 0, width, height);
    context.strokeStyle = '#28352f';
    context.lineWidth = devicePixelRatio;
    context.beginPath();
    context.moveTo(0, height / 2); context.lineTo(width, height / 2);
    for (let second = 1; second < 10; second++) {
      context.moveTo(width * second / 10, 0); context.lineTo(width * second / 10, height);
    }
    context.stroke();
    for (const level of track.history) {
      const x = width * (1 - (now - level.time) / 10000);
      context.fillStyle = level.clipped ? '#f37878' : '#94dba7';
      context.fillRect(x, height / 2 - level.max * height * 0.46,
        Math.max(devicePixelRatio, width / 500), Math.max(devicePixelRatio, (level.max - level.min) * height * 0.46));
    }
    const live = track.level && now - track.level.time < 500 ? track.level : null;
    const db = (value) => value > 0 ? Math.max(-60, 20 * Math.log10(value)) : -60;
    track.meters.forEach((meter, index) => { meter.value = live ? db(live.peak[index]) : -60; });
    const peak = live ? Math.max(...live.peak) : 0;
    track.peak.textContent = track.clipUntil > now ? 'CLIP' : peak > 0 ? `${(20 * Math.log10(peak)).toFixed(1)} dBFS` : '−∞ dBFS';
    track.peak.classList.toggle('clipping', track.clipUntil > now);
    if (track.socket && track.level && !live) track.label.textContent = 'Waiting for audio…';
  }
  requestAnimationFrame(draw);
}

create({ id: virtualId, name: 'Tarsier Microphone · Output' });
refresh();
const refreshTimer = setInterval(refresh, 5000);
requestAnimationFrame(draw);
window.addEventListener('pagehide', () => {
  clearInterval(refreshTimer);
  for (const track of tracks.values()) stop(track);
});
