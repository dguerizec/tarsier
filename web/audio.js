import { createElement, ChevronDown, Mic, MicOff } from "/assets/lucide.js";

const audioFold = document.querySelector('#audio-fold');
audioFold.prepend(createElement(ChevronDown, { width: 18, height: 18, 'aria-hidden': 'true', focusable: 'false' }));
function foldAudio(folded) {
  document.querySelector('#audio-inputs').hidden = folded;
  document.querySelector('#audio-folded-source').hidden = !folded;
  audioFold.setAttribute('aria-expanded', String(!folded));
  audioFold.title = folded ? 'Show audio inputs' : 'Hide audio inputs';
  try { localStorage.setItem('tarsier.audio.folded', String(folded)); } catch {}
}
try { foldAudio(localStorage.getItem('tarsier.audio.folded') === 'true'); } catch {}
audioFold.onclick = () => foldAudio(audioFold.getAttribute('aria-expanded') === 'true');

const reservationIcons = {
  locked: '<rect x="5" y="10" width="14" height="11" rx="2"/><path d="M8 10V6a4 4 0 0 1 8 0v4"/>',
  unlocked: '<rect x="5" y="10" width="14" height="11" rx="2"/><path d="M8 10V6a4 4 0 0 1 8 0"/>',
  shared: '<circle cx="9" cy="7" r="3"/><path d="M3 21v-3a6 6 0 0 1 12 0v3M16 4a3 3 0 0 1 0 6M21 21v-3a6 6 0 0 0-4-5"/>',
  pending: '<path d="M20 12a8 8 0 1 1-8-8"/>',
  disconnected: '<path d="m4 4 16 16M9 9l-3 3a4 4 0 0 0 6 6l3-3M15 15l3-3a4 4 0 0 0-6-6"/>',
};
const tracks = new Map();
const container = document.querySelector('#audio-tracks');
const status = document.querySelector('#audio-status');
let enabledSources = null;
let reservations = {};
let releasedSources = new Set();
let busySources = new Set();

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
      : 'No external applications are connected. Tarsier capture is excluded.';
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
  const state = transitioning ? 'pending' : released ? (busySources.has(track.id) ? 'shared' : 'unlocked')
    : reservation?.status === 'held' ? 'locked'
    : reservation?.status === 'unavailable' ? 'shared'
    : reservation?.status === 'disconnected' ? 'disconnected' : 'pending';
  const labels = { locked: 'Locked', unlocked: 'Unlocked', shared: 'Shared',
    pending: released ? 'Unlocking…' : 'Locking…', disconnected: 'Disconnected' };
  const actions = { locked: 'Unlock this microphone', unlocked: 'Lock this microphone',
    shared: 'Show applications and Kill controls. The input is busy or unavailable.' };
  const button = track.reserveButton;
  if (button.dataset.state !== state) button.innerHTML = `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${reservationIcons[state]}</svg>`;
  button.dataset.state = state;
  button.disabled = state === 'pending' || state === 'disconnected';
  button.title = `${labels[state]} · ${actions[state] || labels[state]}`;
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
const outputError = document.querySelector('#audio-output-error');
let virtualState = { enabled: false, muted: false, source: null, running: false };
let virtualPending = false;

function renderOutput() {
  const selected = virtualState.source || '';
  const sourceLabel = document.querySelector('#audio-folded-source');
  const input = tracks.get(selected);
  sourceLabel.textContent = selected ? `${input?.name || selected}${input?.unavailable ? ' · Disconnected' : ''}` : 'No input selected';
  sourceLabel.title = sourceLabel.textContent;
  for (const track of tracks.values()) {
    if (track.outputRadio) {
      track.outputRadio.checked = track.id === selected;
      track.outputRadio.disabled = virtualPending || track.unavailable;
    }
  }
  document.querySelectorAll('[data-audio-output]').forEach((button) => {
    const on = virtualState.enabled;
    button.setAttribute('aria-pressed', String(on));
    button.textContent = on ? 'On' : 'Off';
    button.disabled = virtualPending || (!on && !selected);
  });
  document.querySelectorAll('[data-audio-mute]').forEach((button) => {
    button.setAttribute('aria-pressed', String(virtualState.muted));
    const label = virtualState.muted ? 'Unmute output' : 'Mute output';
    button.title = label;
    button.setAttribute('aria-label', label);
    if (button.dataset.muted !== String(virtualState.muted)) {
      button.replaceChildren(createElement(virtualState.muted ? MicOff : Mic, { width: 16, height: 16, 'aria-hidden': 'true', focusable: 'false' }));
      button.dataset.muted = String(virtualState.muted);
    }
    button.disabled = virtualPending;
  });
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

export function syncAudioCapture(sources, output, currentReservations, released, busy) {
  reservations = currentReservations || {};
  releasedSources = new Set(released || []);
  busySources = new Set(busy || []);
  if (output) virtualState = output;
  enabledSources = new Set(sources);
  for (const track of tracks.values()) syncTrack(track);
  renderOutput();
}

function syncTrack(track) {
  renderReservation(track);
  if (track.outputRadio) {
    track.outputRadio.checked = virtualState.source === track.id;
    track.outputRadio.disabled = virtualPending || track.unavailable;
  }
  track.enabled = track.id === virtualId ? virtualState.enabled : enabledSources?.has(track.id) ?? track.enabled;
  track.buttons.forEach((button) => {
    button.setAttribute('aria-pressed', String(track.enabled));
    button.textContent = track.enabled ? 'On' : 'Off';
    button.disabled = track.id === virtualId
      ? virtualPending || (!virtualState.enabled && !virtualState.source)
      : track.pending || (!track.enabled && track.unavailable);
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
  track.buttons[0].setAttribute('aria-pressed', String(track.enabled));
  track.buttons[0].textContent = track.enabled ? 'On' : 'Off';
  track.level = null;
}

function start(track) {
  stop(track);
  track.retryAt = Date.now() + 5000;
  track.label.textContent = 'Connecting…';
  track.buttons[0].setAttribute('aria-pressed', 'true');
  track.buttons[0].textContent = 'On';
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
  row.innerHTML = `<div class="audio-visual">
    <div class="audio-waveform"><canvas role="img"></canvas>
      <div class="audio-track-overlay"><strong></strong><span class="audio-track-state"></span></div>
    </div><div class="audio-meters">
    <div><span>L</span><meter min="-60" max="0" low="-18" high="-6" optimum="-24" value="-60"></meter></div>
    <div><span>R</span><meter min="-60" max="0" low="-18" high="-6" optimum="-24" value="-60"></meter></div>
    <span class="audio-peak">−∞ dBFS</span></div>
    <div class="audio-track-controls segmented-control" role="group"><button type="button" class="secondary compact" aria-pressed="false">On</button></div></div>`;
  row.querySelector('strong').textContent = source.name;
  row.querySelector('strong').title = source.name;
  const track = {
    id: source.id, name: source.name, row, history: [], level: null, socket: null, clipUntil: 0,
    enabled: source.enabled === true, pending: false, unavailable: false, retryAt: 0,
    label: row.querySelector('.audio-track-state'), canvas: row.querySelector('canvas'),
    buttons: [...row.querySelectorAll('button')], meters: [...row.querySelectorAll('meter')],
    peak: row.querySelector('.audio-peak'),
  };
  row.querySelector('[role="group"]').setAttribute('aria-label', `${source.name} capture`);
  track.canvas.setAttribute('aria-label', `${source.name}: logarithmic amplitude envelope over the last 10 seconds, minus 60 to 0 dBFS`);
  track.canvas.title = 'Last 10 seconds · Logarithmic amplitude · −60 to 0 dBFS, matching the peak meters';
  track.meters.forEach((meter, index) => meter.setAttribute('aria-label', `${source.name} ${index ? 'right' : 'left'} peak in dBFS`));
  track.buttons[0].onclick = () => void setCapture(track, !track.enabled);
  stop(track);
  if (source.id !== virtualId) {
    const radioLabel = document.createElement('label');
    radioLabel.className = 'audio-source-choice';
    radioLabel.title = `Use ${source.name} for the virtual microphone`;
    const radio = document.createElement('input');
    radio.type = 'radio';
    radio.name = 'audio-output-source';
    radio.value = source.id;
    radio.setAttribute('aria-label', `Use ${source.name} for the virtual microphone`);
    radio.onchange = () => { if (radio.checked) void updateOutput({ source: source.id }); };
    radioLabel.append(radio);
    row.querySelector('.audio-track-overlay').prepend(radioLabel);
    track.outputRadio = radio;
    const reservation = document.createElement('div');
    reservation.className = 'audio-reservation';
    reservation.innerHTML = '<button type="button" class="audio-reservation-status secondary compact"></button><span class="error" role="status" hidden></span>';
    track.reserveButton = reservation.querySelector('button');
    track.reservationMessage = reservation.querySelector('[role="status"]');
    track.reserveButton.onclick = () => {
      if (track.reserveButton.dataset.state === 'shared') showApplications(track);
      else void setExclusive(track);
    };
    row.querySelector('[role="group"]').append(reservation);
    row.append(track.reservationMessage);
    renderReservation(track);
  }
  if (source.id === virtualId) {
    const controls = row.querySelector('[role="group"]');
    controls.setAttribute('aria-label', 'Virtual microphone output');
    track.buttons[0].setAttribute('data-audio-output', '');
    track.buttons[0].setAttribute('aria-label', 'Virtual microphone');
    track.buttons[0].onclick = () => void updateOutput({ enabled: !virtualState.enabled });
    const mute = document.createElement('button');
    mute.type = 'button';
    mute.className = 'secondary compact audio-mute';
    mute.setAttribute('data-audio-mute', '');
    mute.onclick = () => void updateOutput({ muted: !virtualState.muted });
    controls.append(mute);
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
    for (const source of sources) {
      const track = tracks.get(source.id) || create(source);
      track.name = source.name;
      track.row.querySelector('strong').textContent = source.name;
      track.row.querySelector('strong').title = source.name;
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
    renderOutput();
  } catch (error) {
    status.hidden = false;
    status.textContent = error.message;
    for (const track of tracks.values()) stop(track, 'Audio server unavailable');
  }
}

function amplitudeDb(value) {
  return value > 0 ? Math.max(-60, Math.min(0, 20 * Math.log10(value))) : -60;
}

function waveformAmplitude(value) {
  return Math.sign(value) * (amplitudeDb(Math.abs(value)) + 60) / 60;
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
      const top = waveformAmplitude(level.max);
      const bottom = waveformAmplitude(level.min);
      context.fillRect(x, height / 2 - top * height * 0.46,
        Math.max(devicePixelRatio, width / 500), Math.max(devicePixelRatio, (top - bottom) * height * 0.46));
    }
    const live = track.level && now - track.level.time < 500 ? track.level : null;
    track.meters.forEach((meter, index) => { meter.value = live ? amplitudeDb(live.peak[index]) : -60; });
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
