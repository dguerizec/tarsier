const tracks = new Map();
const container = document.querySelector('#audio-tracks');
const status = document.querySelector('#audio-status');

function stop(track, label = 'Capture off') {
  const socket = track.socket;
  track.socket = null;
  socket?.close();
  track.label.textContent = label;
  track.buttons.forEach((button, index) => button.setAttribute('aria-pressed', String(index === 1)));
  track.level = null;
}

function start(track) {
  stop(track);
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
    if (track.history.length > 200) track.history.splice(0, track.history.length - 200);
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
  const track = {
    id: source.id, row, history: [], level: null, socket: null, clipUntil: 0,
    label: row.querySelector('.audio-track-state'), canvas: row.querySelector('canvas'),
    buttons: [...row.querySelectorAll('button')], meters: [...row.querySelectorAll('meter')],
    peak: row.querySelector('.audio-peak'),
  };
  row.querySelector('[role="group"]').setAttribute('aria-label', `${source.name} capture`);
  track.canvas.setAttribute('aria-label', `${source.name}: amplitude envelope over the last 10 seconds`);
  track.meters.forEach((meter, index) => meter.setAttribute('aria-label', `${source.name} ${index ? 'right' : 'left'} peak in dBFS`));
  track.buttons[0].onclick = () => start(track);
  track.buttons[1].onclick = () => stop(track);
  stop(track);
  container.append(row);
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
      track.row.querySelector('strong').textContent = source.name;
      track.muted = source.muted;
      track.buttons[0].disabled = false;
      if (!track.socket) track.label.textContent = source.muted ? 'Source muted · Capture off' : 'Capture off';
    }
    for (const [id, track] of tracks) {
      if (!sources.some((source) => source.id === id)) {
        stop(track, 'Disconnected');
        track.buttons[0].disabled = true;
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
        Math.max(devicePixelRatio, width / 200), Math.max(devicePixelRatio, (level.max - level.min) * height * 0.46));
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

refresh();
const refreshTimer = setInterval(refresh, 5000);
requestAnimationFrame(draw);
window.addEventListener('pagehide', () => {
  clearInterval(refreshTimer);
  for (const track of tracks.values()) stop(track);
});
