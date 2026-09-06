import { createElement, Users } from '/assets/lucide.js';

const button = document.querySelector('#preview-video-applications');
const count = button.querySelector('span');
const dialog = document.querySelector('#video-applications-dialog');
const status = document.querySelector('#video-applications-status');
const list = document.querySelector('#video-applications-list');
button.prepend(createElement(Users, { width: 16, height: 16, 'aria-hidden': 'true', focusable: 'false' }));
let snapshot = null;
let error = null;
let pending = false;
let timer;

function render() {
  const total = snapshot?.applications.length;
  count.textContent = total == null ? '–' : String(total);
  button.title = total == null ? 'Virtual camera applications unavailable'
    : `${total} detected ${total === 1 ? 'application has' : 'applications have'} the virtual camera open${snapshot.partial ? '. Some processes could not be inspected.' : ''}`;
  button.setAttribute('aria-label', button.title);
  if (!dialog.open) return;
  list.replaceChildren();
  status.classList.toggle('error', Boolean(error));
  status.textContent = error || (!snapshot ? 'Loading…' : !snapshot.available ? 'The virtual camera is unavailable.'
    : snapshot.partial ? 'Some local processes could not be inspected.'
    : total ? 'Open device handles · Updates every 2 seconds'
    : 'No external applications have this camera open. Tarsier is excluded.');
  for (const app of snapshot?.applications || []) {
    const row = document.createElement('li');
    const name = document.createElement('strong');
    name.textContent = app.name;
    const detail = document.createElement('span');
    detail.textContent = [app.binary, `PID ${app.pid}`].filter(Boolean).join(' · ');
    row.append(name, detail);
    list.append(row);
  }
}

async function refresh() {
  clearTimeout(timer);
  if (pending || document.hidden) return;
  pending = true;
  try {
    const response = await fetch('/api/v1/video/applications', { cache: 'no-store', signal: AbortSignal.timeout(5000) });
    if (!response.ok) throw new Error('Could not list virtual camera applications');
    snapshot = await response.json();
    error = null;
  } catch (failure) {
    snapshot = null;
    error = failure.message;
  } finally {
    pending = false;
    render();
    timer = setTimeout(refresh, 2000);
  }
}
button.onclick = () => {
  dialog.showModal();
  render();
  void refresh();
};
document.querySelector('#video-applications-refresh').onclick = () => void refresh();
document.addEventListener('visibilitychange', () => {
  clearTimeout(timer);
  if (!document.hidden) void refresh();
});
void refresh();
