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
let gainState = { gain_db: 0, speech: false };

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
      row.append(info);
      if (audioConfig.audio.reserve_inputs) row.append(kill);
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
  if (!audioConfig.audio.reserve_inputs) {
    const button = track.reserveButton;
    button.innerHTML = `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${reservationIcons.shared}</svg>`;
    button.dataset.state = 'shared';
    button.disabled = false;
    button.title = 'Shared input · Show applications. Reservation is disabled in Settings.';
    button.setAttribute('aria-label', `Shared input · ${track.name}. Show applications`);
    button.setAttribute('aria-haspopup', 'dialog');
    return;
  }
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
const audioConfig = await fetch('/api/v1/config').then((response) => {
  if (!response.ok) throw new Error('Could not load audio policy');
  return response.json();
});
const virtualId = audioConfig.audio.virtual_source;
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
    if (track.sourceButton) {
      track.sourceButton.setAttribute('aria-pressed', String(track.id === selected));
      track.sourceButton.disabled = virtualPending || track.unavailable;
    }
  }
  document.querySelectorAll('[data-audio-output]').forEach((button) => {
    const on = virtualState.enabled;
    button.setAttribute('aria-pressed', String(on));
    button.textContent = on ? 'On' : 'Off';
    button.disabled = !audioConfig.audio.virtual_output_enabled || virtualPending || (!on && !selected);
    button.title = audioConfig.audio.virtual_output_enabled ? '' : 'Output unavailable in this development profile';
  });
  document.querySelectorAll('[data-audio-mute]').forEach((button) => {
    button.setAttribute('aria-pressed', String(virtualState.muted));
    const label = virtualState.muted ? 'Unmute Tarsier Microphone' : 'Mute Tarsier Microphone';
    button.title = label;
    button.setAttribute('aria-label', label);
    if (button.dataset.muted !== String(virtualState.muted)) {
      button.replaceChildren(createElement(virtualState.muted ? MicOff : Mic, { width: 16, height: 16, 'aria-hidden': 'true', focusable: 'false' }));
      button.dataset.muted = String(virtualState.muted);
    }
    button.disabled = virtualPending;
  });
  const track = tracks.get(virtualId);
  if (track) {
    syncTrack(track);
    const automatic = virtualState.auto_gain !== false;
    track.autoGainButton.textContent = automatic ? 'On' : 'Off';
    track.autoGainButton.setAttribute('aria-pressed', String(automatic));
    track.autoGainButton.disabled = virtualPending;
    const gain = automatic && virtualState.running ? (gainState.gain_db || 0) : 0;
    track.gain.textContent = `Gain ${gain >= 0 ? '+' : ''}${gain.toFixed(1)} dB${automatic ? virtualState.muted ? ' · Muted' : !virtualState.running ? ' · Output off' : gainState.speech ? ' · Voice' : ' · Waiting for speech' : ''}`;
    track.gain.title = 'Automatic level adjustment during speech. Applies to calls and recordings. No calibration needed.';
  }
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

document.querySelector('#audio-output-applications').onclick = () => showApplications(tracks.get(virtualId));
document.querySelector('#preview-audio-mute').onclick = () => void updateOutput({ muted: !virtualState.muted });

export function syncAudioCapture(sources, output, currentReservations, released, busy, outputApplications = 0, currentGain = {}, currentVoice = {}, currentScreencast = {}, currentSatellite = {}) {
  document.querySelector('#audio-output-applications').textContent = `${outputApplications} ${outputApplications === 1 ? 'app' : 'apps'}`;
  screencastState = currentScreencast;
  satelliteState = currentSatellite;
  renderScreencast();
  gainState = currentGain;
  voiceState = currentVoice;
  reservations = currentReservations || {};
  releasedSources = new Set(released || []);
  busySources = new Set(busy || []);
  if (output) virtualState = output;
  renderVoice();
  enabledSources = new Set(sources);
  for (const track of tracks.values()) syncTrack(track);
  renderOutput();
  renderSatellite();
  renderNoise();
}

function syncTrack(track) {
  renderReservation(track);
  if (track.sourceButton) {
    track.sourceButton.setAttribute('aria-pressed', String(virtualState.source === track.id));
    track.sourceButton.disabled = virtualPending || track.unavailable;
  }
  track.enabled = track.id === virtualId ? virtualState.enabled : enabledSources?.has(track.id) ?? track.enabled;
  track.buttons.forEach((button) => {
    button.setAttribute('aria-pressed', String(track.enabled));
    button.textContent = track.enabled ? 'On' : 'Off';
    button.disabled = track.id === virtualId
      ? !audioConfig.audio.virtual_output_enabled || virtualPending || (!virtualState.enabled && !virtualState.source)
      : track.pending || (!track.enabled && track.unavailable);
  });
  if (!track.enabled) {
    stop(track, track.unavailable ? 'Disconnected' : '');
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

function stop(track, label = '') {
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
    track.maxPeak = track.maxPeak.map((peak, i) => Math.max(peak, level.peak[i]));
    const mutedOutput = track.id === virtualId && virtualState.muted;
    const input = mutedOutput ? tracks.get(virtualState.source) : null;
    const reference = input?.enabled && input.level && now - input.level.time < 500
      ? input.level : track.level;
    if (level.spectrum) track.spectra.push({ spectrum: level.spectrum, time: now });
    track.history.push(mutedOutput ? { ...reference, time: now, muted: true } : track.level);
    if (track.history.length > 500) track.history.splice(0, track.history.length - 500);
    if (level.clipped) track.clipUntil = now + 1500;
    track.label.textContent = track.muted ? 'Source muted' : '';
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
    <div><span>L</span><span class="audio-meter-wrap"><meter min="-60" max="0" low="-18" high="-6" optimum="-24" value="-60"></meter><i class="audio-max-marker" hidden></i></span></div>
    <div><span>R</span><span class="audio-meter-wrap"><meter min="-60" max="0" low="-18" high="-6" optimum="-24" value="-60"></meter><i class="audio-max-marker" hidden></i></span></div>
    <span class="audio-peak">−∞ dBFS</span><button type="button" class="audio-max" title="Maximum since reset · Click to reset both channels">Max −∞ dBFS</button></div>
    <div class="audio-track-controls segmented-control" role="group"><button type="button" class="secondary compact" aria-pressed="false">On</button></div></div>`;
  row.querySelector('strong').textContent = source.name;
  row.querySelector('strong').title = source.name;
  const track = {
    id: source.id, name: source.name, row, history: [], spectra: [], level: null, socket: null, clipUntil: 0, maxPeak: [0, 0],
    enabled: source.enabled === true, pending: false, unavailable: false, retryAt: 0,
    label: row.querySelector('.audio-track-state'), canvas: row.querySelector('canvas'),
    buttons: [...row.querySelectorAll('[role=group] > button')], meters: [...row.querySelectorAll('meter')],
    peak: row.querySelector('.audio-peak'),
  };
  row.querySelector('[role="group"]').setAttribute('aria-label', `${source.name} capture`);
  track.canvas.setAttribute('aria-label', `${source.name}: logarithmic amplitude envelope over the last 10 seconds, minus 60 to 0 dBFS`);
  track.canvas.title = 'Last 10 seconds · Logarithmic amplitude · −60 to 0 dBFS, matching the peak meters';
  const viewToggle = document.createElement('button');
  viewToggle.type = 'button';
  viewToggle.className = 'audio-view-toggle secondary compact';
  let mode = 'spectrogram';
  try { mode = localStorage.getItem('tarsier.audio.view') || mode; } catch {}
  function setView(value) {
    track.view = value === 'waveform' ? 'waveform' : 'spectrogram';
    track.lastDraw = -Infinity;
    const spectrogram = track.view === 'spectrogram';
    viewToggle.innerHTML = spectrogram
      ? '<svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.7" aria-hidden="true"><path d="M2 12h3l2-7 3 14 4-14 3 14 2-7h3"/></svg>'
      : '<svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M4 4v16M8 9v8M12 3v18M16 7v12M20 5v11"/></svg>';
    viewToggle.title = spectrogram ? 'Show waveform' : 'Show spectrogram';
    viewToggle.setAttribute('aria-label', `${viewToggle.title} · ${track.name}`);
    track.canvas.setAttribute('aria-label', `${track.name}: ${spectrogram ? 'spectrogram, low frequencies at bottom, high at top' : 'waveform'} over the last 10 seconds`);
    track.canvas.title = spectrogram
      ? 'Last 10 seconds · 50 Hz (bottom) to 20 kHz (top) · Dark: quiet, bright: loud · Actual signal, including output mute'
      : 'Last 10 seconds · −60 to 0 dBFS · Gray output waveform: input before mute';
  }
  track.setView = setView;
  setView(mode);
  viewToggle.onclick = () => {
    const next = track.view === 'spectrogram' ? 'waveform' : 'spectrogram';
    for (const item of tracks.values()) item.setView(next);
    try { localStorage.setItem('tarsier.audio.view', next); } catch {}
  };
  row.querySelector('.audio-waveform').append(viewToggle);
  track.meters.forEach((meter, index) => meter.setAttribute('aria-label', `${source.name} ${index ? 'right' : 'left'} peak in dBFS`));
  track.buttons[0].onclick = () => void setCapture(track, !track.enabled);
  stop(track);
  if (source.id !== virtualId) {
    const choose = document.createElement('button');
    choose.type = 'button';
    choose.className = 'audio-source-choice';
    choose.title = `Use ${source.name} for the virtual microphone`;
    choose.setAttribute('aria-label', choose.title);
    choose.setAttribute('aria-pressed', 'false');
    choose.onclick = () => {
      if (virtualState.source !== source.id) void updateOutput({ source: source.id });
    };
    row.querySelector('.audio-waveform').append(choose);
    track.sourceButton = choose;
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
  track.maxLabel = row.querySelector('.audio-max');
  track.maxMarkers = [...row.querySelectorAll('.audio-max-marker')];
  track.maxLabel.onclick = () => { track.maxPeak = [0, 0]; };
  if (source.id === virtualId) {
    const controls = document.createElement('div');
    controls.className = 'audio-gain-controls';
    controls.innerHTML = '<span>Auto gain</span><div class="segmented-control" role="group" aria-label="Automatic microphone gain"><button type="button" class="secondary compact" aria-pressed="true">On</button></div><span class="audio-gain" role="status"></span>';
    track.autoGainButton = controls.querySelector('button');
    track.autoGainButton.onclick = () => void updateOutput({ auto_gain: !virtualState.auto_gain });
    track.gain = controls.querySelector('.audio-gain');
    row.append(controls);
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
    renderSatellite();
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

const spectralColors = Array.from({ length: 91 }, (_, value) => {
  const t = value / 90;
  return `rgb(${Math.round(12 + 220 * t ** 3)},${Math.round(20 + 215 * t ** 1.4)},${Math.round(27 + 90 * Math.sin(t * Math.PI))})`;
});
function drawSpectrogram(context, spectra, now, width, height) {
  context.fillStyle = '#0c141b';
  context.fillRect(0, 0, width, height);
  for (const column of spectra) {
    const x = width * (1 - (now - column.time) / 10000);
    column.spectrum.forEach((db, band) => {
      context.fillStyle = spectralColors[Math.round(Math.max(0, Math.min(90, db + 90)))];
      context.fillRect(x, height * (1 - (band + 1) / 80), Math.max(1, width / 100), Math.ceil(height / 80));
    });
  }
  context.font = `${10 * devicePixelRatio}px system-ui`;
  context.fillStyle = '#dbe8e0';
  context.fillText('20k', width - 28 * devicePixelRatio, 11 * devicePixelRatio);
  context.fillText('50 Hz', 4 * devicePixelRatio, height - 4 * devicePixelRatio);
}

function draw(now) {
  for (const track of tracks.values()) {
    track.history = track.history.filter((level) => now - level.time < 10000);
    track.spectra = track.spectra.filter((level) => now - level.time < 10000);
    const canvas = track.canvas;
    const width = Math.max(1, Math.round(canvas.clientWidth * devicePixelRatio));
    const height = Math.max(1, Math.round(canvas.clientHeight * devicePixelRatio));
    if (canvas.width !== width || canvas.height !== height) { canvas.width = width; canvas.height = height; }
    if (now - (track.lastDraw ?? -Infinity) >= 100 && canvas.clientWidth && canvas.clientHeight) {
      track.lastDraw = now;
      const context = canvas.getContext('2d');
      context.clearRect(0, 0, width, height);
      if (track.view === 'spectrogram') {
        drawSpectrogram(context, track.spectra, now, width, height);
      } else {
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
          context.fillStyle = level.muted ? '#7c8580' : level.clipped ? '#f37878' : '#94dba7';
          const top = waveformAmplitude(level.max);
          const bottom = waveformAmplitude(level.min);
          context.fillRect(x, height / 2 - top * height * 0.46,
            Math.max(devicePixelRatio, width / 500), Math.max(devicePixelRatio, (top - bottom) * height * 0.46));
        }
      }
    }
    const live = track.level && now - track.level.time < 500 ? track.level : null;
    track.meters.forEach((meter, index) => { meter.value = live ? amplitudeDb(live.peak[index]) : -60; });
    const peak = live ? Math.max(...live.peak) : 0;
    track.peak.textContent = track.clipUntil > now ? 'CLIP' : peak > 0 ? `${(20 * Math.log10(peak)).toFixed(1)} dBFS` : '−∞ dBFS';
    track.peak.classList.toggle('clipping', track.clipUntil > now);
    const maximum = Math.max(...track.maxPeak);
    track.maxLabel.textContent = maximum > 0 ? `Max ${(20 * Math.log10(maximum)).toFixed(1)} dBFS` : 'Max −∞ dBFS';
    track.maxLabel.classList.toggle('clipping', maximum >= 32767 / 32768);
    track.maxMarkers.forEach((marker, i) => {
      marker.hidden = track.maxPeak[i] === 0;
      marker.style.left = `${(amplitudeDb(track.maxPeak[i]) + 60) / 60 * 100}%`;
    });
    if (track.socket && track.level && !live) track.label.textContent = 'Waiting for audio…';
  }
  requestAnimationFrame(draw);
}

create({ id: virtualId, name: `${virtualId.replaceAll('_', ' ')} · Output` });
let noiseState = null;
let noisePending = false;
let noisePollPending = false;
let noiseRevision = 0;
let noiseError = '';
const noiseControls = document.createElement('div');
noiseControls.className = 'audio-noise-controls';
noiseControls.innerHTML = `<div class="audio-gain-controls"><span>Noise reduction</span>
  <button type="button" class="secondary compact" data-noise-analyze>Analyze ambient noise</button>
  <button type="button" class="secondary compact" data-noise-toggle aria-pressed="false">Reduce this noise</button>
  <label class="audio-noise-strength" hidden>Strength <input type="range" min="0" max="100" value="65" step="5" aria-label="Noise reduction strength"><output>65%</output></label></div>
  <progress max="1" value="0" aria-label="Ambient noise analysis progress" hidden></progress>
  <p class="audio-noise-status" role="status"></p>`;
tracks.get(virtualId).row.append(noiseControls);
const noiseAnalyze = noiseControls.querySelector('[data-noise-analyze]');
const noiseToggle = noiseControls.querySelector('[data-noise-toggle]');
const noiseStrength = noiseControls.querySelector('input');
function renderNoise() {
  const current = noiseState?.source === virtualState.source ? noiseState : null;
  const analyzing = !!current?.analyzing;
  noiseAnalyze.textContent = analyzing ? 'Cancel analysis' : current?.ready ? 'Analyze again' : 'Analyze ambient noise';
  noiseAnalyze.disabled = noisePending || !virtualState.running || !virtualState.enabled;
  noiseToggle.textContent = current?.enabled ? 'On' : 'Reduce this noise';
  noiseToggle.setAttribute('aria-pressed', String(!!current?.enabled));
  noiseToggle.disabled = noisePending || !current?.ready;
  noiseToggle.title = 'Compare with and without noise reduction. Applies to Tarsier Microphone calls and recordings.';
  noiseControls.querySelector('label').hidden = !current?.ready;
  noiseStrength.disabled = noisePending;
  if (document.activeElement !== noiseStrength) noiseStrength.value = Math.round((current?.strength ?? 0.65) * 100);
  noiseControls.querySelector('output').textContent = `${noiseStrength.value}%`;
  const progress = noiseControls.querySelector('progress');
  progress.hidden = !analyzing;
  progress.value = current?.progress || 0;
  noiseControls.querySelector('[role="status"]').textContent = noiseError || current?.error || (analyzing
    ? `Stay silent… ${Math.ceil(3 * (1 - current.progress))} s remaining`
    : !virtualState.enabled || !virtualState.running ? 'Turn on audio output to analyze the selected microphone.'
    : current?.ready ? (current.enabled ? 'Reducing steady background noise · Lower strength if the voice sounds distorted.' : 'Noise profile ready · Enable reduction to compare.')
    : 'Stay silent for 3 seconds to measure steady background noise. The profile is kept until the microphone changes or Tarsier restarts.');
}
async function pollNoise() {
  if (noisePending || noisePollPending || document.hidden) return;
  noisePollPending = true;
  const revision = noiseRevision;
  try {
    const response = await fetch('/api/v1/audio/noise', { cache: 'no-store' });
    if (!response.ok) throw new Error('Noise reduction unavailable');
    const result = await response.json();
    if (revision === noiseRevision) { noiseState = result; noiseError = ''; }
  } catch (error) { noiseError = error.message; }
  finally { noisePollPending = false; renderNoise(); }
}
async function updateNoise(action, strength) {
  if (noisePending) return;
  noiseRevision++;
  noisePending = true; noiseError = ''; renderNoise();
  try {
    const response = await fetch('/api/v1/audio/noise', { method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ source: virtualState.source, action, ...(strength === undefined ? {} : { strength }) }) });
    const body = await response.json();
    if (!response.ok) throw new Error(body.error || 'Could not update noise reduction');
    noiseState = body;
  } catch (error) { noiseError = error.message; }
  finally { noisePending = false; renderNoise(); }
}
noiseAnalyze.onclick = () => void updateNoise(noiseState?.source === virtualState.source && noiseState.analyzing ? 'cancel' : 'analyze');
noiseToggle.onclick = () => void updateNoise(noiseState?.enabled ? 'disable' : 'enable');
noiseStrength.oninput = () => { noiseControls.querySelector('output').textContent = `${noiseStrength.value}%`; };
noiseStrength.onchange = () => void updateNoise('strength', Number(noiseStrength.value) / 100);
renderNoise();
void pollNoise();
const noiseTimer = setInterval(pollNoise, 500);
window.addEventListener('pagehide', () => clearInterval(noiseTimer));
refresh();
const refreshTimer = setInterval(refresh, 5000);
requestAnimationFrame(draw);
window.addEventListener('pagehide', () => {
  clearInterval(refreshTimer);
  for (const track of tracks.values()) stop(track);
});


let voiceState = {};
let voicePending = false;
let voiceLibraryRevision = null;
let voiceModelsPending = false;
const voiceModel = document.querySelector('#voice-model');
const voiceImportStatus = document.querySelector('#voice-import-status');
async function refreshVoiceModels() {
  const response = await fetch('/api/v1/audio/voice/models');
  const body = await response.json();
  if (!response.ok) throw new Error(body.error || 'Could not list voice models');
  document.querySelector('#voice-model-controls').hidden = !body.available;
  voiceModel.replaceChildren(new Option('Choose a voice', ''), ...body.models.filter(model => model.enabled !== false).map(model => new Option(model.name, model.id)));
  voiceModel.options[0].disabled = true;
  renderVoice();
}
const voiceFold = document.querySelector('#voice-fold');
voiceFold.prepend(createElement(ChevronDown, { width: 18, height: 18, 'aria-hidden': 'true', focusable: 'false' }));
function foldVoice(folded) {
  document.querySelector('#voice-settings').hidden = folded;
  voiceFold.setAttribute('aria-expanded', String(!folded));
  voiceFold.title = folded ? 'Show voice conversion settings' : 'Hide voice conversion settings';
  try { localStorage.setItem('tarsier.voice.folded', String(folded)); } catch {}
}
try { foldVoice(localStorage.getItem('tarsier.voice.folded') === 'true'); } catch {}
voiceFold.onclick = () => foldVoice(voiceFold.getAttribute('aria-expanded') === 'true');
const voiceToggle = document.querySelector('#voice-toggle');
const voicePitch = document.querySelector('#voice-pitch');
const voiceStatus = document.querySelector('#voice-status');
document.querySelector('#voice-conversion').hidden = !audioConfig.audio.voice_worker?.length;
function renderVoice() {
  document.querySelector('#voice-conversion').hidden = !audioConfig.audio.voice_worker?.length || voiceState.show_controls === false;
  if (voiceLibraryRevision !== (voiceState.library_revision ?? 0) && !voiceModelsPending) {
    voiceLibraryRevision = voiceState.library_revision ?? 0;
    voiceModelsPending = true;
    refreshVoiceModels().catch(error => { voiceImportStatus.textContent = error.message; }).finally(() => { voiceModelsPending = false; });
  }
  voiceToggle.textContent = voiceState.enabled ? 'On' : 'Off';
  voiceToggle.setAttribute('aria-pressed', String(!!voiceState.enabled));
  voiceToggle.disabled = voicePending || (!voiceState.enabled && ![...voiceModel.options].some(option => option.value && option.value === voiceState.model));
  voicePitch.disabled = voicePending;
  voiceModel.disabled = voicePending;
  voiceModel.value = [...voiceModel.options].some(option => option.value === voiceState.model) ? voiceState.model : '';
  const credit = document.querySelector('#voice-model-credit');
  const credits = {
    'FrenchWoman.pth': ['French Woman — DantSu', 'https://github.com/DantSu/RVC-french-woman-model'],
    'Shigure.pth': ['Shigure Tokina — CV: Marukoro · Managed by Bindume · Trained by yasyune', 'https://huggingface.co/yasyune/Shigure_Tokina_RVC'],
  };
  const metadata = credits[voiceState.model || 'FrenchWoman.pth'];
  credit.replaceChildren();
  if (metadata) {
    const link = document.createElement('a');
    link.textContent = metadata[0]; link.href = metadata[1]; link.target = '_blank'; link.rel = 'noopener';
    credit.append(link);
  } else { credit.textContent = voiceState.model || ''; }
  if (document.activeElement !== voicePitch) voicePitch.value = voiceState.pitch || 0;
  document.querySelector('#voice-pitch-value').textContent = `${voicePitch.value} st`;
  voiceStatus.textContent = voiceState.error || (!voiceState.enabled ? (voiceState.ready ? 'Off · Model loaded' : 'Off') : !virtualState.enabled
    ? 'Turn on audio output to start conversion.' : !voiceState.ready ? 'Loading voice model…'
    : `Ready${virtualState.muted ? ' · Output muted' : ''} · Inference ${Math.round(voiceState.inference_ms || 0)} ms · Pipeline delay ${voiceState.pipeline_ms ?? '–'} ms · Dropped chunks ${voiceState.dropped_chunks || 0}`);
  voiceStatus.title = 'Pipeline delay measures daemon queue and processing time. It excludes filter alignment, hardware and application buffering.';
}
async function updateVoice(enabled, pitch, model) {
  if (voicePending) return;
  voicePending = true; renderVoice();
  try {
    const response = await fetch('/api/v1/audio/voice', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({enabled, pitch, ...(model ? {model} : {})})});
    const body = await response.json();
    if (!response.ok) throw new Error(body.error || 'Could not change voice conversion');
    voiceState = body;
  } catch (error) { voiceState.error = error.message; }
  finally { voicePending = false; renderVoice(); }
}
voiceToggle.onclick = () => updateVoice(!voiceState.enabled, Number(voicePitch.value));
voicePitch.oninput = () => { document.querySelector('#voice-pitch-value').textContent = `${voicePitch.value} st`; };
voicePitch.onchange = () => updateVoice(!!voiceState.enabled, Number(voicePitch.value));

voiceModel.onchange = () => updateVoice(!!voiceState.enabled, Number(voicePitch.value), voiceModel.value);
renderVoice();


const screencastFold = document.querySelector('#screencast-fold');
screencastFold.prepend(createElement(ChevronDown, { width: 18, height: 18, 'aria-hidden': 'true', focusable: 'false' }));
function foldScreencast(folded) {
  document.querySelector('#screencast-settings').hidden = folded;
  screencastFold.setAttribute('aria-expanded', String(!folded));
  screencastFold.title = folded ? 'Show screencast settings' : 'Hide screencast settings';
  try { localStorage.setItem('tarsier.screencast.folded', String(folded)); } catch {}
}
try { foldScreencast(localStorage.getItem('tarsier.screencast.folded') === 'true'); } catch {}
screencastFold.onclick = () => foldScreencast(screencastFold.getAttribute('aria-expanded') === 'true');

let screencastState = {};
let screencastPending = false;
let screencastError = '';
const screencastToggle = document.querySelector('#screencast-toggle');
function renderScreencast() {
  const settings = screencastState.settings || {};
  screencastToggle.textContent = settings.enabled ? 'On' : 'Off';
  screencastToggle.setAttribute('aria-pressed', String(!!settings.enabled));
  screencastToggle.disabled = screencastPending || !audioConfig.audio.virtual_output_enabled;
  for (const channel of ['microphone', 'system']) {
    const slider = document.querySelector(`#screencast-${channel}-volume`);
    const mute = document.querySelector(`#screencast-${channel}-mute`);
    if (document.activeElement !== slider) slider.value = settings[`${channel}_volume`] ?? 70;
    document.querySelector(`#screencast-${channel}-value`).textContent = `${slider.value}%`;
    slider.disabled = screencastPending;
    mute.disabled = screencastPending;
    mute.setAttribute('aria-pressed', String(!!settings[`${channel}_muted`]));
    mute.textContent = `${settings[`${channel}_muted`] ? 'Unmute' : 'Mute'} ${channel === 'system' ? 'system audio' : 'microphone'}`;
  }
  document.querySelector('#screencast-status').textContent = screencastError || (!settings.enabled ? 'Off' : screencastState.error ||
    (screencastState.running ? 'Ready · Following the default playback device' : 'Starting…'));
}
async function updateScreencast(patch) {
  if (screencastPending) return;
  screencastPending = true;
  screencastError = '';
  renderScreencast();
  try {
    const response = await fetch('/api/v1/audio/screencast', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({...screencastState.settings, ...patch}),
    });
    const body = await response.json();
    if (!response.ok) throw new Error(body.error || 'Could not update screencast audio');
    // The shared state stream remains authoritative for all open clients.
  } catch (error) { screencastError = error.message; }
  finally { screencastPending = false; renderScreencast(); }
}
screencastToggle.onclick = () => updateScreencast({enabled: !screencastState.settings?.enabled});
for (const channel of ['microphone', 'system']) {
  const slider = document.querySelector(`#screencast-${channel}-volume`);
  slider.oninput = () => { document.querySelector(`#screencast-${channel}-value`).textContent = `${slider.value}%`; };
  slider.onchange = () => updateScreencast({[`${channel}_volume`]: Number(slider.value)});
  document.querySelector(`#screencast-${channel}-mute`).onclick = () => updateScreencast({[`${channel}_muted`]: !screencastState.settings?.[`${channel}_muted`]});
}
renderScreencast();


let satelliteState = {};
let satellitePending = false;
let satelliteError = '';
const satelliteSource = document.querySelector('#satellite-source');
const satelliteToggle = document.querySelector('#satellite-toggle');
const satelliteMute = document.querySelector('#satellite-mute');
function renderSatellite() {
  const sources = [...tracks.values()].filter(track => track.id !== virtualId && !track.unavailable);
  const options = sources.map(track => [track.id, track.name]);
  if (satelliteState.source && !options.some(([id]) => id === satelliteState.source)) {
    options.push([satelliteState.source, 'Selected input · Disconnected']);
  }
  const key = JSON.stringify(options);
  if (satelliteSource.dataset.options !== key) {
    satelliteSource.replaceChildren(new Option('No input selected', ''), ...options.map(([id, name]) => new Option(name, id)));
    satelliteSource.dataset.options = key;
  }
  satelliteSource.value = satelliteState.source || '';
  satelliteSource.disabled = satellitePending;
  satelliteToggle.textContent = satelliteState.enabled ? 'On' : 'Off';
  satelliteToggle.setAttribute('aria-pressed', String(!!satelliteState.enabled));
  satelliteToggle.disabled = satellitePending;
  satelliteMute.setAttribute('aria-pressed', String(!!satelliteState.muted));
  satelliteMute.textContent = satelliteState.muted ? 'Unmute' : 'Mute';
  satelliteMute.disabled = satellitePending;
  const active = sources.some(track => track.id === satelliteState.source && track.enabled);
  document.querySelector('#satellite-status').textContent = satelliteError || (!satelliteState.enabled ? 'Off'
    : satelliteState.muted ? 'Muted · Channel connected'
    : !active ? 'Silent · Select and enable an available input' : 'Ready · tarsier_satellites');
}
async function updateSatellite(patch) {
  if (satellitePending) return;
  satellitePending = true;
  satelliteError = '';
  renderSatellite();
  try {
    const response = await fetch('/api/v1/audio/satellite', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({enabled: !!satelliteState.enabled, source: satelliteState.source || null, muted: !!satelliteState.muted, ...patch}),
    });
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || 'Could not update satellite microphone');
    // Shared state updates remain authoritative across browser tabs.
  } catch (error) { satelliteError = error.message; }
  finally { satellitePending = false; renderSatellite(); }
}
satelliteSource.onchange = () => updateSatellite({source: satelliteSource.value || null});
satelliteToggle.onclick = () => updateSatellite({enabled: !satelliteState.enabled});
satelliteMute.onclick = () => updateSatellite({muted: !satelliteState.muted});
