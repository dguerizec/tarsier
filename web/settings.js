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
    const destinations = ["api", "mcp"].filter(name => tokenForm.elements[name].checked);
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
    const label = document.createElement('label');
    const row = document.createElement('p');
    const input = document.createElement('input');
    input.type = 'checkbox'; input.value = mic.id; input.dataset.label = mic.name;
    input.checked = devicesState.input_reservations[mic.id] ?? devicesState.reserve_new_inputs;
    label.append(input, document.createTextNode(` ${mic.name}`)); row.append(label); microphoneList.append(row);
  }
  setDevicesPending(false);
  devicesStatus.textContent = devicesState.can_apply ? '' : 'Device changes require the supervised Tarsier service.';
}
devicesRefresh.addEventListener('click', () => {
  setDevicesPending(true);
  loadDevices().catch(error => { setDevicesPending(false); devicesStatus.textContent = error.message; });
});
devicesForm.addEventListener('submit', async event => {
  event.preventDefault();
  if (devicesPending || devicesSave.disabled) return;
  const input_reservations = Object.fromEntries([...microphoneList.querySelectorAll('input')].map(input => [input.value, input.checked]));
  const body = {camera: cameraSelect.value, input_reservations};
  const previousStart = devicesState.started_at_ms;
  setDevicesPending(true); devicesStatus.textContent = 'Saving and restarting…';
  try {
    const response = await fetch('/api/v1/settings/devices', {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify(body)});
    if (!response.ok) throw new Error((await response.json()).error || 'Could not save devices');
    const deadline = Date.now() + 30000;
    while (Date.now() < deadline) {
      await new Promise(resolve => setTimeout(resolve, 1000));
      try {
        const health = await fetch('/api/v1/health', {cache: 'no-store', signal: AbortSignal.timeout(2000)});
        if (health.ok && (await health.json()).started_at_ms !== previousStart) {
          await loadDevices(); devicesStatus.textContent = 'Devices saved. Tarsier has restarted.'; return;
        }
      } catch { /* Wait for the supervised daemon. */ }
    }
    throw new Error('Settings saved, but the service has not reconnected. Check the service and refresh this page.');
  } catch (error) { setDevicesPending(false); devicesStatus.textContent = error.message; }
});
loadDevices().catch(error => { devicesStatus.textContent = error.message; });
