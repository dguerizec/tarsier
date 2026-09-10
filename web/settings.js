import { subscribeVisibleRefresh } from "/assets/events.js";
import { avatarDeleteButton } from "/assets/avatar-delete.js";
import { createDaemonMonitor } from "/assets/daemon-monitor.js";
createDaemonMonitor().start();

const settingsTabs = [...document.querySelectorAll('[role="tab"]')];
function selectSettingsTab(id, focus = false) {
  const selected = settingsTabs.find(tab => tab.id === `tab-${id}`) || settingsTabs[0];
  for (const tab of settingsTabs) {
    const active = tab === selected;
    tab.setAttribute('aria-selected', String(active));
    tab.tabIndex = active ? 0 : -1;
    document.getElementById(tab.getAttribute('aria-controls')).hidden = !active;
  }
  if (selected.id !== 'tab-video') document.querySelector('#mute-media-video').pause();
  if (focus) selected.focus();
}
for (const [index, tab] of settingsTabs.entries()) {
  tab.addEventListener('click', () => {
    selectSettingsTab(tab.id.slice(4));
    history.replaceState(null, '', `#${tab.id.slice(4)}`);
  });
  tab.addEventListener('keydown', event => {
    const next = event.key === 'ArrowRight' ? (index + 1) % settingsTabs.length
      : event.key === 'ArrowLeft' ? (index + settingsTabs.length - 1) % settingsTabs.length
      : event.key === 'Home' ? 0 : event.key === 'End' ? settingsTabs.length - 1 : null;
    if (next === null) return;
    event.preventDefault();
    const id = settingsTabs[next].id.slice(4);
    selectSettingsTab(id, true);
    history.replaceState(null, '', `#${id}`);
  });
}
window.addEventListener('hashchange', () => selectSettingsTab(location.hash.slice(1)));
selectSettingsTab(location.hash.slice(1));

const form = document.querySelector("#network-form");
const options = document.querySelector("#network-options");
const save = document.querySelector("#network-save");
const status = document.querySelector("#network-status");
let current = null;
let pending = false;
const selected = () => form.elements.lan_access.value === "true";

function render() {
  options.disabled = pending || !current?.can_apply;
  save.disabled = pending || !current?.can_apply || selected() === current.lan_access;
  document.querySelector("#network-description").textContent = selected()
    ? "Listen on all IPv4 interfaces. Other devices on your network can connect using this computer's IP address."
    : "Only this computer can connect. Existing connections from other devices will be disconnected.";
  document.querySelector("#network-address").textContent = current ? `Current listener: ${current.bind}` : "";
}

async function load() {
  const response = await fetch("/api/v1/settings/network", { cache: "no-store" });
  if (!response.ok) throw new Error("Could not load network settings");
  current = await response.json();
  form.elements.lan_access.value = String(current.lan_access);
  if (!current.can_apply) status.textContent = "Network changes require the supervised Tarsier service.";
  render();
}
form.addEventListener("change", render);
form.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (pending || save.disabled) return;
  const lanAccess = selected();
  pending = true;
  render();
  status.textContent = "Saving and restarting…";
  try {
    const response = await fetch("/api/v1/settings/network", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ lan_access: lanAccess }),
    });
    if (!response.ok) throw new Error((await response.json()).error || "Could not save settings");
    if (!lanAccess && !["localhost", "127.0.0.1", "[::1]"].includes(location.hostname)) {
      status.textContent = `Saved. Remote access is now disabled. On the camera computer, open http://127.0.0.1:${current.port}/settings.`;
      return;
    }
    const previousStart = current.started_at_ms;
    const deadline = Date.now() + 30000;
    while (Date.now() < deadline) {
      await new Promise(resolve => setTimeout(resolve, 1000));
      try {
        const response = await fetch("/api/v1/settings/network", { cache: "no-store", signal: AbortSignal.timeout(2000) });
        if (response.ok && (await response.json()).started_at_ms !== previousStart) {
          pending = false;
          await load();
          status.textContent = "Saved. Tarsier has restarted.";
          return;
        }
      } catch { /* Wait for the service to reconnect. */ }
    }
    status.textContent = "Saved. The service has not reconnected yet; reload this page to check.";
  } catch (error) {
    pending = false;
    render();
    status.textContent = error.message;
  }
});
load().catch(error => { status.textContent = error.message; });


const passwordForm = document.querySelector("#password-form");
const tokenForm = document.querySelector("#token-form");
for (const name of ['api', 'mcp']) {
  const button = tokenForm.elements[name];
  button.onclick = () => button.setAttribute('aria-pressed', String(button.getAttribute('aria-pressed') !== 'true'));
}
tokenForm.addEventListener('reset', () => {
  tokenForm.elements.api.setAttribute('aria-pressed', 'true');
  tokenForm.elements.mcp.setAttribute('aria-pressed', 'false');
});
const authStatus = document.querySelector("#auth-status");
const tokenStatus = document.querySelector("#token-status");
let authState;
async function authRequest(path, body) {
  const response = await fetch(`/api/v1/auth/${path}`, {
    method: body === undefined ? "GET" : "POST",
    headers: { "Content-Type": "application/json", "X-Tarsier-Request": "1" },
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: "no-store",
  });
  if (response.status === 401) { location.assign("/login"); throw new Error("Please sign in again"); }
  const data = await response.json();
  if (!response.ok) throw new Error(data.error || "Request failed");
  return data;
}
async function loadAuth() {
  authState = await authRequest("status");
  if (authState.enabled && !authState.admin) { location.assign("/login"); return; }
  document.querySelector("#auth-summary").textContent = authState.enabled
    ? "Protection is enabled. Web access requires the master password; automated clients use tokens with API and/or MCP access."
    : "Protection is disabled. Anyone who can reach Tarsier can view media and control the camera. Set the first password on this computer, or use the CLI.";
  document.querySelector("#current-password-row").hidden = !authState.enabled;
  passwordForm.elements.current_password.required = authState.enabled;
  document.querySelector("#password-save").disabled = false;
  document.querySelector("#password-save").textContent = authState.enabled ? "Change master password" : "Set master password";
  document.querySelector("#password-disable").hidden = !authState.enabled;
  document.querySelector("#auth-logout").hidden = !authState.enabled;
  document.querySelector("#token-create").disabled = !authState.admin;
  const list = document.querySelector("#token-list");
  list.replaceChildren();
  if (authState.admin) {
    const {tokens} = await authRequest("tokens");
    for (const token of tokens) {
      const item = document.createElement("li");
      const label = document.createElement("div");
      label.className = "token-details";
      const name = document.createElement("strong");
      name.textContent = token.name;
      const created = document.createElement("time");
      const date = new Date(token.created_at_ms);
      created.dateTime = date.toISOString();
      created.textContent = `Created ${date.toLocaleDateString(undefined, {year: "numeric", month: "short", day: "numeric"})} · ${date.toLocaleTimeString(undefined, {hour: "2-digit", minute: "2-digit"})}`;
      label.append(name, created);
      const revoke = document.createElement("button");
      revoke.type = "button"; revoke.textContent = "Revoke";
      revoke.addEventListener("click", async () => {
        if (!confirm(`Revoke access for ${token.name}?`)) return;
        revoke.disabled = true;
        try { await authRequest(`tokens/${encodeURIComponent(token.id)}/revoke`, {}); await loadAuth(); }
        catch (error) { tokenStatus.textContent = error.message; revoke.disabled = false; }
      });
      const scope = document.createElement("div");
      scope.className = "token-scopes";
      for (const destination of token.destinations) {
        const badge = document.createElement("span");
        badge.textContent = destination.toUpperCase();
        badge.dataset.destination = destination;
        scope.append(badge);
      }
      item.append(label, scope, revoke); list.append(item);
    }
  }
}
passwordForm.addEventListener("submit", async event => {
  event.preventDefault();
  if (passwordForm.elements.password.value !== passwordForm.elements.confirmation.value) {
    authStatus.textContent = "Passwords do not match"; return;
  }
  const button = document.querySelector("#password-save"); button.disabled = true;
  try {
    await authRequest("password", {password: passwordForm.elements.password.value, current_password: passwordForm.elements.current_password.value});
    passwordForm.reset(); await loadAuth();
    authStatus.textContent = "Master password saved. Other web sessions have been signed out. Existing client tokens are preserved.";
  } catch (error) { authStatus.textContent = error.message; }
  finally { button.disabled = false; }
});
document.querySelector("#password-disable").addEventListener("click", async () => {
  if (!passwordForm.elements.current_password.value) { authStatus.textContent = "Enter your current master password first"; return; }
  if (!confirm("Disable authentication and revoke ALL tokens? Anyone who can reach Tarsier will have access.")) return;
  try {
    await authRequest("password", {password: "", current_password: passwordForm.elements.current_password.value});
    passwordForm.reset(); dismissToken(); await loadAuth();
    authStatus.textContent = "Protection disabled. All tokens revoked.";
  } catch (error) { authStatus.textContent = error.message; }
});
document.querySelector("#auth-logout").addEventListener("click", async () => {
  try { await authRequest("logout", {}); location.assign("/login"); }
  catch (error) { authStatus.textContent = error.message; }
});
function dismissToken() {
  document.querySelector("#token-value").value = "";
  document.querySelector("#new-token").hidden = true;
}
tokenForm.addEventListener("submit", async event => {
  event.preventDefault();
  const button = document.querySelector("#token-create"); button.disabled = true;
  try {
    const destinations = ["api", "mcp"].filter(name => tokenForm.elements[name].getAttribute("aria-pressed") === "true");
    if (!destinations.length) throw new Error("Select at least one destination.");
    const data = await authRequest("tokens", {name: tokenForm.elements.name.value, destinations});
    document.querySelector("#token-value").value = data.token;
    document.querySelector("#new-token").hidden = false;
    tokenForm.reset(); tokenStatus.textContent = "Token created."; await loadAuth();
  } catch (error) { tokenStatus.textContent = error.message; }
  finally { button.disabled = !authState?.admin; }
});
document.querySelector("#token-dismiss").addEventListener("click", dismissToken);
document.querySelector("#token-copy").addEventListener("click", async () => {
  const input = document.querySelector("#token-value");
  try { await navigator.clipboard.writeText(input.value); tokenStatus.textContent = "Token copied."; }
  catch { input.focus(); input.select(); tokenStatus.textContent = "Press Ctrl+C to copy the selected token."; }
});
loadAuth().catch(error => { authStatus.textContent = error.message; });


const devicesForm = document.querySelector('#devices-form');
const cameraSelect = document.querySelector('#device-camera');
const microphoneList = document.querySelector('#device-microphone-list');
const devicesStatus = document.querySelector('#devices-status');
const devicesSave = document.querySelector('#devices-save');
const devicesRefresh = document.querySelector('#devices-refresh');
let devicesState;
let devicesPending = false;
function deviceOption(select, value, label) {
  const option = document.createElement('option');
  option.value = value; option.textContent = label; select.append(option);
}
function setDevicesPending(pending) {
  devicesPending = pending;
  cameraSelect.disabled = devicesSave.disabled = pending || !devicesState?.can_apply;
  document.querySelector('#device-microphones').disabled = pending || !devicesState?.can_apply;
  devicesRefresh.disabled = pending;
}
async function loadDevices() {
  const response = await fetch('/api/v1/settings/devices', {cache: 'no-store'});
  if (!response.ok) throw new Error((await response.json()).error || 'Could not load devices');
  devicesState = await response.json();
  cameraSelect.replaceChildren();
  deviceOption(cameraSelect, '', 'Synthetic video');
  for (const camera of devicesState.cameras) deviceOption(cameraSelect, camera.id, `${camera.name} · ${camera.id.split('/').pop()}`);
  if (devicesState.camera && !devicesState.cameras.some(camera => camera.id === devicesState.camera)) {
    deviceOption(cameraSelect, devicesState.camera, `Disconnected · ${devicesState.camera}`);
  }
  cameraSelect.value = devicesState.camera;
  microphoneList.replaceChildren();
  const microphones = [...devicesState.microphones];
  for (const id of new Set([...Object.keys(devicesState.input_reservations), ...devicesState.capture_sources])) {
    if (!microphones.some(mic => mic.id === id)) microphones.push({id, name: `Disconnected · ${id}`});
  }
  for (const mic of microphones) {
    const row = document.createElement('div'); row.className = 'device-reservation-row';
    const name = document.createElement('span'); name.textContent = mic.name;
    const button = document.createElement('button'); button.type = 'button'; button.className = 'secondary compact'; button.dataset.source = mic.id;
    button.setAttribute('aria-label', `Automatically reserve ${mic.name}`);
    const render = locked => {
      button.setAttribute('aria-pressed', String(locked));
      button.textContent = locked ? 'Lock' : 'Share';
    };
    render(devicesState.input_reservations[mic.id] ?? devicesState.reserve_new_inputs);
    button.onclick = () => render(button.getAttribute('aria-pressed') !== 'true');
    row.append(name, button); microphoneList.append(row);
  }
  setDevicesPending(false);
  devicesStatus.textContent = devicesState.can_apply ? '' : 'Device controls or persistent settings are unavailable.';
}
devicesRefresh.addEventListener('click', () => {
  setDevicesPending(true);
  loadDevices().catch(error => { setDevicesPending(false); devicesStatus.textContent = error.message; });
});
devicesForm.addEventListener('submit', async event => {
  event.preventDefault();
  if (devicesPending || devicesSave.disabled) return;
  const input_reservations = Object.fromEntries([...microphoneList.querySelectorAll('button[data-source]')].map(button => [button.dataset.source, button.getAttribute('aria-pressed') === 'true']));
  const body = {camera: cameraSelect.value, input_reservations};
  setDevicesPending(true); devicesStatus.textContent = 'Applying devices…';
  try {
    const response = await fetch('/api/v1/settings/devices', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify(body)});
    if (!response.ok) throw new Error((await response.json()).error || 'Could not apply devices');
    await loadDevices(); devicesStatus.textContent = 'Device preferences saved and applied.';
  } catch (error) { setDevicesPending(false); devicesStatus.textContent = error.message; }
});
loadDevices().catch(error => { devicesStatus.textContent = error.message; });


const muteMediaForm = document.querySelector('#mute-media-form');
const muteMediaFile = document.querySelector('#mute-media-file');
const muteMediaSave = document.querySelector('#mute-media-save');
const muteMediaDefault = document.querySelector('#mute-media-default');
const muteMediaRemove = document.querySelector('#mute-media-remove');
const muteMediaLibrary = document.querySelector('#mute-media-library');
const muteMediaUse = document.querySelector('#mute-media-use');
const muteMediaDelete = document.querySelector('#mute-media-delete');
const muteMediaStatus = document.querySelector('#mute-media-status');
let muteMediaState = null;
let muteMediaPending = false;
let muteMediaRevision = null;
function renderMuteMedia() {
  muteMediaFile.disabled = muteMediaPending || !muteMediaState?.can_apply;
  muteMediaSave.disabled = muteMediaFile.disabled || !muteMediaFile.files.length;
  const selection = muteMediaState?.media.selection;
  const previousChoice = muteMediaLibrary.value;
  const library = muteMediaState?.library || [];
  muteMediaLibrary.replaceChildren(...library.map(item => {
    const option = document.createElement('option');
    option.value = item.filename;
    option.textContent = `${item.name} · ${item.kind === 'video' ? 'Video' : 'Image'}${selection?.filename === item.filename ? ' · Active' : ''}`;
    return option;
  }));
  if (!library.length) muteMediaLibrary.add(new Option('No saved media yet', ''));
  else if (library.some(item => item.filename === previousChoice)) muteMediaLibrary.value = previousChoice;
  else if (library.some(item => item.filename === selection?.filename)) muteMediaLibrary.value = selection.filename;
  muteMediaLibrary.disabled = muteMediaFile.disabled || !library.length;
  muteMediaUse.disabled = muteMediaLibrary.disabled || muteMediaLibrary.value === selection?.filename;
  muteMediaDelete.disabled = muteMediaLibrary.disabled;
  muteMediaDefault.disabled = muteMediaFile.disabled;
  muteMediaRemove.disabled = muteMediaFile.disabled || !selection;
  document.querySelector('#mute-media-name').textContent = selection
    ? `${selection.name} · ${selection.kind === 'video' ? 'Video loops without audio' : 'Image'}` : 'Black output';
  if (muteMediaRevision !== (selection?.filename ?? null)) {
    muteMediaRevision = selection?.filename ?? null;
    const image = document.querySelector('#mute-media-image');
    const video = document.querySelector('#mute-media-video');
    video.pause();
    image.removeAttribute('src');
    video.removeAttribute('src');
    image.hidden = !selection || selection.kind !== 'image';
    video.hidden = !selection || selection.kind !== 'video';
    document.querySelector('#mute-media-preview').hidden = !selection;
    if (selection) {
      (selection.kind === 'image' ? image : video).src = `/api/v1/settings/video-mute/media?v=${encodeURIComponent(selection.filename)}`;
    }
    video.load();
  }
}
async function loadMuteMedia() {
  const response = await fetch('/api/v1/settings/video-mute', {cache: 'no-store'});
  if (!response.ok) throw new Error('Could not load video mute replacement');
  muteMediaState = await response.json();
  renderMuteMedia();
  if (muteMediaState.media.error) muteMediaStatus.textContent = `Using black output: ${muteMediaState.media.error}`;
}
muteMediaFile.addEventListener('change', renderMuteMedia);
muteMediaForm.addEventListener('submit', async event => {
  event.preventDefault();
  const file = muteMediaFile.files[0];
  if (muteMediaPending || !file) return;
  if (file.size > 100 * 1024 * 1024) { muteMediaStatus.textContent = 'Choose a file up to 100 MB.'; return; }
  muteMediaPending = true;
  renderMuteMedia();
  muteMediaStatus.textContent = 'Uploading and checking media…';
  try {
    const response = await fetch(`/api/v1/settings/video-mute?name=${encodeURIComponent(file.name)}`, {
      method: 'POST', headers: {'Content-Type': 'application/octet-stream'}, body: file,
    });
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || 'Could not upload replacement');
    muteMediaState = result;
    muteMediaFile.value = '';
    muteMediaFile.dispatchEvent(new Event('change'));
    muteMediaStatus.textContent = 'Saved. This media will be shown whenever video output is muted.';
  } catch (error) { muteMediaStatus.textContent = error.message; }
  finally { muteMediaPending = false; renderMuteMedia(); }
});
async function selectBuiltinMuteMedia(useDefault) {
  if (muteMediaPending) return;
  muteMediaPending = true;
  renderMuteMedia();
  try {
    const response = await fetch(`/api/v1/settings/video-mute${useDefault ? '/default' : ''}`, {method: useDefault ? 'POST' : 'DELETE'});
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || 'Could not change replacement');
    muteMediaState = result;
    muteMediaStatus.textContent = useDefault ? 'Saved. Muted video output will show the Tarsier image.' : 'Saved. Muted video output will be black.';
  } catch (error) { muteMediaStatus.textContent = error.message; }
  finally { muteMediaPending = false; renderMuteMedia(); }
}
muteMediaRemove.addEventListener('click', () => selectBuiltinMuteMedia(false));
muteMediaDefault.addEventListener('click', () => selectBuiltinMuteMedia(true));
loadMuteMedia().catch(error => { muteMediaStatus.textContent = error.message; });

muteMediaLibrary.addEventListener('change', renderMuteMedia);
async function changeSavedMuteMedia(remove) {
  if (muteMediaPending || !muteMediaLibrary.value) return;
  const filename = muteMediaLibrary.value;
  muteMediaPending = true;
  renderMuteMedia();
  muteMediaStatus.textContent = remove ? 'Deleting media…' : 'Loading media…';
  try {
    const response = await fetch(`/api/v1/settings/video-mute/library?filename=${encodeURIComponent(filename)}`, {method: remove ? 'DELETE' : 'POST'});
    if (!response.ok) throw new Error('Could not change saved media. Reload settings and try again.');
    muteMediaState = await response.json();
    muteMediaStatus.textContent = remove ? 'Media deleted from the library.' : 'Saved media selected for muted video output.';
  } catch (error) { muteMediaStatus.textContent = error.message; }
  finally { muteMediaPending = false; renderMuteMedia(); }
}
muteMediaUse.addEventListener('click', () => changeSavedMuteMedia(false));
muteMediaDelete.addEventListener('click', () => changeSavedMuteMedia(true));


const avatarLibraryStatus = document.querySelector('#avatar-library-status');
const avatarRefresh = document.querySelector('#avatar-library-refresh');
const portraitImportForm = document.querySelector('#import-liveportrait-form');
const modelImportForm = document.querySelector('#import-portrait3d-form');
let avatarImportPending = false;
let avatarCanImport = false;

function updateAvatarImportControls() {
  for (const form of [portraitImportForm, modelImportForm]) {
    for (const input of form.querySelectorAll('input, button')) {
      input.disabled = avatarImportPending || !avatarCanImport;
    }
  }
  avatarRefresh.disabled = avatarImportPending;
  for (const button of document.querySelectorAll('#portrait3d-library button, #liveportrait-library button')) {
    button.disabled = avatarImportPending || !avatarCanImport;
  }
}

function avatarLibraryCard(item, kind) {
  const card = document.createElement('article');
  card.className = 'avatar-library-card';
  const image = document.createElement('img');
  const imageUrl = kind === 'portrait3d'
    ? `/api/v1/video/portrait3d/models/${encodeURIComponent(item.id)}/preview`
    : `/api/v1/video/liveportrait/source/${encodeURIComponent(item.id)}`;
  image.src = imageUrl;
  image.alt = '';
  image.loading = 'lazy';
  const placeholder = document.createElement(kind === 'portrait3d' ? 'button' : 'div');
  placeholder.className = 'avatar-preview-placeholder';
  placeholder.textContent = kind === 'portrait3d' ? 'Generate preview' : 'Preview unavailable';
  placeholder.hidden = true;
  image.addEventListener('error', () => { image.hidden = true; placeholder.hidden = false; });
  image.addEventListener('load', () => { image.hidden = false; placeholder.hidden = true; });
  const name = document.createElement('strong');
  name.textContent = item.name;
  card.append(image, placeholder, name);
  const remove = avatarDeleteButton(item, kind, loadAvatarLibrary);
  if (remove) card.append(remove);
  if (item.selected) {
    card.classList.add('is-selected');
    card.setAttribute('aria-label', `${item.name}, selected`);
  }
  if (kind === 'portrait3d') {
    const generate = placeholder;
    generate.type = 'button';
    generate.classList.add('secondary');
    generate.setAttribute('aria-label', `Generate preview for ${item.name}`);
    generate.addEventListener('click', async () => {
      if (avatarImportPending) return;
      avatarImportPending = true;
      updateAvatarImportControls();
      avatarLibraryStatus.textContent = 'Generating preview…';
      try {
        const response = await fetch(imageUrl, {method: 'POST'});
        const result = await response.json().catch(() => ({}));
        if (!response.ok) throw new Error(result.error || 'Could not generate preview');
        avatarLibraryStatus.textContent = result.preview ? 'Preview generated.' : 'Preview unavailable. You can try again later.';
        if (result.preview) {
          image.loading = 'eager';
          image.src = `${imageUrl}?v=${Date.now()}`;
        }
      } catch (error) { avatarLibraryStatus.textContent = error.message; }
      finally { avatarImportPending = false; updateAvatarImportControls(); }
    });
  }
  return card;
}

async function loadAvatarLibrary(signal) {
  const response = await fetch('/api/v1/settings/avatars', {cache: 'no-store', signal});
  if (!response.ok) throw new Error('Could not load avatar library. Refresh to try again.');
  const result = await response.json();
  if (signal?.aborted) return;
  avatarCanImport = result.can_import;
  for (const kind of ['liveportrait', 'portrait3d']) {
    const library = document.querySelector(`#${kind}-library`);
    library.replaceChildren(...result[kind].map(item => avatarLibraryCard(item, kind)));
    if (!result[kind].length) library.textContent = 'No avatars available yet.';
  }
  updateAvatarImportControls();
}
avatarRefresh.addEventListener('click', () => {
  avatarLibraryStatus.textContent = 'Loading library…';
  loadAvatarLibrary().then(() => { avatarLibraryStatus.textContent = ''; })
    .catch(error => { avatarLibraryStatus.textContent = error.message; });
});

async function importAvatar(kind, form, files) {
  if (avatarImportPending || !avatarCanImport) return;
  const status = document.querySelector(`#import-${kind}-status`);
  avatarImportPending = true;
  updateAvatarImportControls();
  status.textContent = kind === 'portrait3d' ? 'Uploading, checking model and generating preview…' : 'Importing portrait…';
  try {
    let body;
    let url = `/api/v1/settings/avatars/${kind}`;
    if (kind === 'liveportrait') {
      body = files[0];
      if (files.length !== 1 || !body || !body.size || body.size > 10 * 1024 * 1024 || !/\.(png|jpe?g)$/i.test(body.name)) throw new Error('Choose a PNG or JPEG up to 10 MB.');
      url += `?name=${encodeURIComponent(body.name)}`;
    } else {
      const manifestFile = files.find(file => file.name === 'manifest.json' && file.webkitRelativePath.split('/').length === 2);
      if (!manifestFile || manifestFile.size > 1024 * 1024) throw new Error('Choose an exported Tarsier model folder with a manifest.json file.');
      const manifest = JSON.parse(await manifestFile.text());
      if (manifest.schema_version !== 1 || !Array.isArray(manifest.draws) || !manifest.draws.length) throw new Error('Unsupported model export.');
      const required = new Set(['manifest.json', 'mesh.npz', ...manifest.draws.map(draw => draw.texture).filter(Boolean)]);
      const prefix = manifestFile.webkitRelativePath.slice(0, -manifestFile.name.length);
      body = new FormData();
      let bytes = 0;
      for (const name of required) {
        if (typeof name !== 'string' || name.includes('/') || name.includes('\\') || name.startsWith('.')) throw new Error('Model textures must be in the export folder.');
        const file = files.find(file => file.webkitRelativePath === prefix + name);
        if (!file) throw new Error(`The export is missing ${name}.`);
        bytes += file.size;
        body.append('files', file, name);
      }
      if (required.size > 128 || bytes > 256 * 1024 * 1024) throw new Error('Choose a model up to 256 MB and 128 files.');
    }
    const response = await fetch(url, {method: 'POST', body});
    const result = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(result.error || `Import failed (${response.status}).`);
    status.textContent = result.warning || 'Imported. Choose it from its folder button on the preview page.';
    form.reset();
    try { await loadAvatarLibrary(); }
    catch { status.textContent += ' Refresh the library to see the new avatar.'; }
  } catch (error) { status.textContent = error.message; }
  finally { avatarImportPending = false; updateAvatarImportControls(); }
}
for (const [kind, form] of [['liveportrait', portraitImportForm], ['portrait3d', modelImportForm]]) {
  form.addEventListener('submit', event => {
    event.preventDefault();
    importAvatar(kind, form, [...form.querySelector('input').files]);
  });
}

const portraitDropZone = portraitImportForm.closest('section');
let portraitDragDepth = 0;
const isFileDrag = event => [...(event.dataTransfer?.types || [])].includes('Files');
portraitDropZone.addEventListener('dragenter', event => {
  if (!isFileDrag(event)) return;
  event.preventDefault();
  portraitDragDepth++;
  if (!avatarImportPending && avatarCanImport) portraitDropZone.classList.add('is-drag-over');
});
portraitDropZone.addEventListener('dragover', event => {
  if (!isFileDrag(event)) return;
  event.preventDefault();
  event.dataTransfer.dropEffect = !avatarImportPending && avatarCanImport ? 'copy' : 'none';
});
portraitDropZone.addEventListener('dragleave', () => {
  portraitDragDepth = Math.max(0, portraitDragDepth - 1);
  if (!portraitDragDepth) portraitDropZone.classList.remove('is-drag-over');
});
function importPortraitFiles(files) {
  if (avatarImportPending) return;
  if (!avatarCanImport) {
    document.querySelector('#import-liveportrait-status').textContent = 'Portrait import is not available yet.';
    return;
  }
  if (files.length !== 1) {
    document.querySelector('#import-liveportrait-status').textContent = 'Choose one PNG or JPEG image at a time.';
    return;
  }
  importAvatar('liveportrait', portraitImportForm, files);
}
portraitDropZone.addEventListener('drop', event => {
  event.preventDefault();
  portraitDragDepth = 0;
  portraitDropZone.classList.remove('is-drag-over');
  importPortraitFiles([...(event.dataTransfer?.files || [])]);
});
document.addEventListener('paste', event => {
  if (document.querySelector('#settings-avatars').hidden) return;
  if (event.target instanceof Element && event.target.closest('input, textarea, [contenteditable]:not([contenteditable="false"])')) return;
  const files = [...(event.clipboardData?.files || [])];
  if (!files.some(file => file.type.startsWith('image/'))) return;
  event.preventDefault();
  importPortraitFiles(files);
});
subscribeVisibleRefresh(document.querySelector('#settings-avatars'), 'event:avatar.library.changed', loadAvatarLibrary,
  error => { avatarLibraryStatus.textContent = error.message; });


const voiceLibraryList = document.querySelector('#voice-library-list');
const voiceLibraryShow = document.querySelector('#voice-library-show');
const voiceLibraryForm = document.querySelector('#voice-library-import');
const voiceLibraryFile = document.querySelector('#voice-library-file');
const voiceLibraryStatus = document.querySelector('#voice-library-status');
let voiceLibraryBusy = false;
let voiceLibrary = null;
let voiceLibraryFocus = null;
function renderVoiceLibrary() {
  const showControls = voiceLibrary?.show_controls ?? true;
  voiceLibraryShow.textContent = showControls ? 'On' : 'Off';
  voiceLibraryShow.setAttribute('aria-pressed', String(showControls));
  voiceLibraryShow.disabled = voiceLibraryBusy || !voiceLibrary;
  voiceLibraryForm.querySelector('button[type="submit"]').disabled = voiceLibraryBusy || !voiceLibrary?.available || !voiceLibraryFile.files.length;
  document.querySelector('#voice-library-choose').disabled = voiceLibraryBusy || !voiceLibrary?.available;
  document.querySelector('#voice-library-filename').textContent = voiceLibraryFile.files[0]?.name || 'No file selected';
  voiceLibraryFile.disabled = voiceLibraryBusy || !voiceLibrary?.available;
  document.querySelector('#voice-library-refresh').disabled = voiceLibraryBusy;
  voiceLibraryList.replaceChildren();
  for (const model of voiceLibrary?.models || []) {
    const row = document.createElement('div'); row.className = 'voice-library-row';
    const name = document.createElement('strong'); name.textContent = model.name;
    const toggle = document.createElement('button'); toggle.type = 'button'; toggle.className = 'secondary compact voice-library-toggle';
    toggle.textContent = model.enabled ? 'On' : 'Off'; toggle.setAttribute('aria-pressed', String(model.enabled));
    toggle.disabled = voiceLibraryBusy; toggle.setAttribute('aria-label', `Enable ${model.name}`);
    toggle.onclick = () => {
      voiceLibraryFocus = toggle.getAttribute('aria-label');
      changeVoiceLibrary('/api/v1/settings/voice', {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify({model:model.id, enabled:!model.enabled})});
    };
    const remove = document.createElement('button'); remove.type = 'button'; remove.className = 'secondary compact'; remove.textContent = 'Delete';
    remove.disabled = voiceLibraryBusy; remove.setAttribute('aria-label', `Delete ${model.name}`);
    remove.onclick = () => changeVoiceLibrary(`/api/v1/audio/voice/models/${encodeURIComponent(model.id)}`, {method:'DELETE'});
    row.append(name, toggle, remove); voiceLibraryList.append(row);
  }
  if (voiceLibrary && !voiceLibrary.models.length) voiceLibraryList.textContent = 'No voices imported.';
  if (!voiceLibraryBusy && voiceLibraryFocus) {
    [...voiceLibraryList.querySelectorAll('button')].find(button => button.getAttribute('aria-label') === voiceLibraryFocus)?.focus();
    voiceLibraryFocus = null;
  }
}
async function changeVoiceLibrary(url = '/api/v1/settings/voice', options = {}) {
  if (voiceLibraryBusy) return;
  voiceLibraryBusy = true; renderVoiceLibrary(); voiceLibraryStatus.textContent = options.method ? 'Applying…' : 'Loading…';
  try {
    const response = await fetch(url, options);
    const body = await response.json();
    if (!response.ok) throw new Error(body.error || 'Could not update voice library');
    voiceLibrary = body;
    voiceLibraryStatus.textContent = !body.available ? 'Model storage is not configured.' : !body.worker_available ? 'Voice worker is not configured. You can still manage models.' : '';
  } catch (error) { voiceLibraryStatus.textContent = error.message; }
  finally { voiceLibraryBusy = false; renderVoiceLibrary(); }
}
voiceLibraryShow.onclick = () => changeVoiceLibrary('/api/v1/settings/voice', {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify({show_controls:voiceLibraryShow.getAttribute('aria-pressed') !== 'true'})});
voiceLibraryForm.onsubmit = async event => {
  event.preventDefault();
  const file = voiceLibraryFile.files[0];
  if (!file || !file.name.endsWith('.pth') || !file.size || file.size > 128 * 1024 * 1024) {
    voiceLibraryStatus.textContent = 'Choose a nonempty RVC .pth file up to 128 MiB.'; return;
  }
  await changeVoiceLibrary(`/api/v1/audio/voice/models?name=${encodeURIComponent(file.name)}`, {method:'POST', headers:{'Content-Type':'application/octet-stream'}, body:file});
  voiceLibraryFile.value = '';
  renderVoiceLibrary();
};
document.querySelector('#voice-library-choose').onclick = () => voiceLibraryFile.click();
voiceLibraryFile.onchange = () => renderVoiceLibrary();
document.querySelector('#voice-library-refresh').onclick = () => changeVoiceLibrary();
changeVoiceLibrary();


// Keep native file selection, with consistent buttons and a readable selection label.
for (const [id, caption] of [
  ['mute-media-file', 'Choose media'],
  ['import-liveportrait-file', 'Choose image'],
  ['import-portrait3d-files', 'Choose folder'],
]) {
  const input = document.getElementById(id);
  const picker = document.createElement('div'); picker.className = 'settings-file-picker';
  const choose = document.createElement('button'); choose.type = 'button'; choose.className = 'secondary'; choose.textContent = caption;
  const selection = document.createElement('span'); selection.className = 'hint'; selection.setAttribute('aria-live', 'polite');
  const refresh = () => {
    choose.disabled = input.disabled;
    const files = [...input.files];
    selection.textContent = !files.length ? 'No file selected' : input.webkitdirectory
      ? `${files[0].webkitRelativePath.split('/')[0]} · ${files.length} files` : files[0].name;
  };
  choose.onclick = () => input.click();
  input.hidden = true;
  // Import handlers validate the selected files and report errors inline.
  input.required = false;
  picker.append(choose, selection); input.after(picker);
  input.addEventListener('change', refresh);
  input.form?.addEventListener('reset', () => queueMicrotask(refresh));
  new MutationObserver(refresh).observe(input, {attributes:true, attributeFilter:['disabled']});
  refresh();
}
