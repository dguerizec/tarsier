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
