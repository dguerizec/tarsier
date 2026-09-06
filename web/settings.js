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
    ? "Protection is enabled. Web access requires the master password; automated clients use API tokens."
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
      const label = document.createElement("span");
      label.textContent = `${token.name} · ${new Date(token.created_at_ms).toLocaleString()} `;
      const revoke = document.createElement("button");
      revoke.type = "button"; revoke.textContent = "Revoke";
      revoke.addEventListener("click", async () => {
        if (!confirm(`Revoke access for ${token.name}?`)) return;
        revoke.disabled = true;
        try { await authRequest(`tokens/${encodeURIComponent(token.id)}/revoke`, {}); await loadAuth(); }
        catch (error) { tokenStatus.textContent = error.message; revoke.disabled = false; }
      });
      item.append(label, revoke); list.append(item);
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
    authStatus.textContent = "Master password saved. Other web sessions have been signed out. Existing API tokens are preserved.";
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
    const data = await authRequest("tokens", {name: tokenForm.elements.name.value});
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
