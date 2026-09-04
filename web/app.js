const $ = (selector) => document.querySelector(selector);
const connection = $("#connection");
const events = $("#events");
const preview = $("#preview");
const overlay = $("#hand-overlay");
const overlayContext = overlay.getContext("2d");
const skeletonToggle = $("#skeleton-toggle");
const zoomSlider = $("#zoom-slider");
const zoomReset = $("#zoom-reset");
let state = null;
let skeletonEnabled = localStorage.getItem("tarsier.handSkeleton") === "true";
let socketConnected = false;
let pipelineWasRunning = null;
let previewRetry = null;
let daemonStartedAt = null;
let reloadRequested = false;
let cameraControlError = null;
let zoomDraft = null;
let zoomPending = false;
let queuedZoom = null;
let zoomSendTimer = null;
const pendingGestureFeatures = new Set();
const zoomUpdateIntervalMs = 100;

const builtInGestureControls = [
  { feature: "target-selection", key: "target_selection", state: "#gesture-target-selection-state" },
  { feature: "zoom", key: "zoom", state: "#gesture-zoom-state" },
  { feature: "dynamic-zoom", key: "dynamic_zoom", state: "#gesture-dynamic-zoom-state" },
];

const handConnections = [
  [0, 1], [1, 2], [2, 3], [3, 4],
  [0, 5], [5, 6], [6, 7], [7, 8],
  [5, 9], [9, 10], [10, 11], [11, 12],
  [9, 13], [13, 14], [14, 15], [15, 16],
  [13, 17], [0, 17], [17, 18], [18, 19], [19, 20],
];

const angle = (value) => value == null ? "—" : `${value.toFixed(1)}°`;
const age = (timestamp) => timestamp == null ? "No sample" : `${Math.max(0, (Date.now() - timestamp) / 1000).toFixed(1)}s ago`;
const attitudeLabel = (source) => ({
  "last-commanded": "Last command",
  measured: "Measured",
  simulated: "Simulated",
}[source] || "No attitude sample");
const magnification = (value) => `×${Number(value).toFixed(1)}`;

function observeDaemon(startedAt) {
  if (!Number.isFinite(startedAt)) return;
  if (daemonStartedAt == null) {
    daemonStartedAt = startedAt;
    return;
  }
  if (startedAt !== daemonStartedAt && !reloadRequested) {
    reloadRequested = true;
    location.reload();
  }
}

async function checkDaemonInstance() {
  try {
    const response = await fetch("/api/v1/health", { cache: "no-store" });
    if (!response.ok) return;
    const health = await response.json();
    observeDaemon(health.started_at_ms);
  } catch {
    // A stopped daemon is expected during upgrades; the WebSocket owns the status display.
  }
}

function drawHandSkeleton(landmarks = state?.perception.hand_landmarks || []) {
  const bounds = overlay.getBoundingClientRect();
  if (bounds.width === 0 || bounds.height === 0) return;

  const pixelRatio = window.devicePixelRatio || 1;
  const width = Math.round(bounds.width * pixelRatio);
  const height = Math.round(bounds.height * pixelRatio);
  if (overlay.width !== width || overlay.height !== height) {
    overlay.width = width;
    overlay.height = height;
  }
  overlayContext.setTransform(pixelRatio, 0, 0, pixelRatio, 0, 0);
  overlayContext.clearRect(0, 0, bounds.width, bounds.height);
  if (!skeletonEnabled || landmarks.length !== 21) return;

  const sourceWidth = preview.naturalWidth || 16;
  const sourceHeight = preview.naturalHeight || 9;
  const scale = Math.min(bounds.width / sourceWidth, bounds.height / sourceHeight);
  const renderedWidth = sourceWidth * scale;
  const renderedHeight = sourceHeight * scale;
  const offsetX = (bounds.width - renderedWidth) / 2;
  const offsetY = (bounds.height - renderedHeight) / 2;
  const points = landmarks.map((point) => ({
    x: offsetX + point.x * renderedWidth,
    y: offsetY + point.y * renderedHeight,
  }));

  overlayContext.lineCap = "round";
  overlayContext.lineJoin = "round";
  overlayContext.lineWidth = 2.5;
  overlayContext.strokeStyle = "rgba(183, 239, 122, 0.9)";
  overlayContext.shadowColor = "rgba(9, 13, 11, 0.9)";
  overlayContext.shadowBlur = 4;
  overlayContext.beginPath();
  for (const [from, to] of handConnections) {
    overlayContext.moveTo(points[from].x, points[from].y);
    overlayContext.lineTo(points[to].x, points[to].y);
  }
  overlayContext.stroke();

  overlayContext.shadowBlur = 0;
  overlayContext.fillStyle = "#f2f5ef";
  for (const point of points) {
    overlayContext.beginPath();
    overlayContext.arc(point.x, point.y, 3, 0, Math.PI * 2);
    overlayContext.fill();
  }
}

function setSkeletonEnabled(enabled) {
  skeletonEnabled = enabled;
  localStorage.setItem("tarsier.handSkeleton", String(enabled));
  skeletonToggle.setAttribute("aria-pressed", String(enabled));
  skeletonToggle.textContent = enabled ? "Hide skeleton" : "Hand skeleton";
  drawHandSkeleton();
}

function refreshPreview() {
  if (previewRetry != null) {
    clearTimeout(previewRetry);
    previewRetry = null;
  }
  preview.classList.remove("disconnected");
  preview.src = `/api/v1/preview.mjpeg?generation=${Date.now()}`;
}

function stopPreview() {
  if (previewRetry != null) {
    clearTimeout(previewRetry);
    previewRetry = null;
  }
  preview.classList.add("disconnected");
  preview.removeAttribute("src");
  drawHandSkeleton([]);
}

function syncPreview(pipeline) {
  const running = socketConnected && pipeline.running;
  if (running && pipelineWasRunning !== true) refreshPreview();
  if (!running && pipelineWasRunning !== false) stopPreview();
  pipelineWasRunning = running;
}

function retryPreview() {
  preview.classList.add("disconnected");
  if (!socketConnected || !state?.pipeline.running || previewRetry != null) return;
  previewRetry = setTimeout(refreshPreview, 1000);
}

function gestureDetail(perception) {
  if (perception.gesture) return `${Math.round(perception.confidence * 100)}% confidence`;
  if (perception.hand_detected) return "Hand detected · not classified";
  if (perception.peak_gesture) {
    return `Peak ${perception.peak_gesture} · ${Math.round(perception.peak_gesture_confidence * 100)}%`;
  }
  return perception.last_hand_at_ms == null
    ? "No hand detected"
    : `Hand last seen ${age(perception.last_hand_at_ms)}`;
}

function renderBuiltInGestures(camera) {
  const gestures = camera.built_in_gestures || {};
  for (const control of builtInGestureControls) {
    const value = gestures[control.key] ?? null;
    const pending = pendingGestureFeatures.has(control.feature);
    $(control.state).textContent = pending ? "Applying…" : value == null ? "Unknown" : value ? "On" : "Off";
    document.querySelectorAll(`[data-gesture-feature="${control.feature}"]`).forEach((button) => {
      const buttonValue = button.dataset.gestureEnabled === "true";
      button.setAttribute("aria-pressed", String(value != null && value === buttonValue));
      button.disabled = !camera.available || pending;
    });
  }
}

function renderZoom(camera) {
  const current = camera.zoom_magnification ?? null;
  if (zoomDraft == null && current != null) zoomSlider.value = String(current);
  const displayed = zoomDraft ?? current;
  $("#zoom-value").textContent = zoomPending
    ? `Applying ${magnification(displayed)}`
    : displayed == null ? "Unknown" : magnification(displayed);
  zoomSlider.disabled = !camera.available;
  zoomReset.disabled = !camera.available || zoomPending;
}

function render(next) {
  observeDaemon(next.started_at_ms);
  state = next;
  const camera = next.camera;
  const pipeline = next.pipeline;
  const perception = next.perception;
  $("#yaw").textContent = angle(camera.yaw_degrees);
  $("#pitch").textContent = angle(camera.pitch_degrees);
  $("#roll").textContent = angle(camera.roll_degrees);
  $("#tracking").textContent = camera.tracking == null ? "—" : camera.tracking ? "On" : "Off";
  $("#camera-age").textContent = camera.sample_at_ms == null
    ? attitudeLabel(camera.attitude_source)
    : `${attitudeLabel(camera.attitude_source)} · ${age(camera.sample_at_ms)}`;
  renderZoom(camera);
  renderBuiltInGestures(camera);
  $("#pipeline-summary").textContent = pipeline.running
    ? `${pipeline.width}×${pipeline.height} · ${pipeline.fps.toFixed(1)} fps · ${pipeline.frame_count} frames`
    : pipeline.error || "Pipeline stopped";
  syncPreview(pipeline);
  $("#worker").textContent = perception.worker_connected ? `Frame ${perception.frame_id}` : "Worker offline";
  $("#gesture").textContent = perception.gesture || "No gesture";
  $("#confidence").textContent = gestureDetail(perception);
  $("#gesture-icon").classList.toggle("active", perception.gesture === "open_palm");
  $("#perception-error").hidden = !perception.error;
  $("#perception-error").textContent = perception.error || "";
  const cameraError = cameraControlError || camera.error;
  $("#camera-error").hidden = !cameraError;
  $("#camera-error").textContent = cameraError || "";
  document.querySelectorAll("[data-action], [data-preset]").forEach((button) => { button.disabled = !camera.available; });
  drawHandSkeleton(socketConnected && pipeline.running ? perception.hand_landmarks : []);
}

function appendEvent(event) {
  events.querySelector(".empty")?.remove();
  const item = document.createElement("li");
  const when = new Date(event.emitted_at_ms).toLocaleTimeString();
  item.innerHTML = `<time>${when}</time><code></code><span></span>`;
  item.querySelector("code").textContent = event.kind;
  item.querySelector("span").textContent = event.source;
  events.prepend(item);
  while (events.children.length > 40) events.lastElementChild.remove();
}

async function loadRecentEvents() {
  const response = await fetch("/api/v1/events/recent");
  for (const event of await response.json()) appendEvent(event);
}

async function loadPresets() {
  const response = await fetch("/api/v1/camera/presets");
  const presets = await response.json();
  const container = $("#presets");
  for (const preset of presets) {
    const button = document.createElement("button");
    button.className = "secondary";
    button.dataset.preset = preset.id;
    button.textContent = preset.id;
    button.addEventListener("click", async () => {
      await fetch(`/api/v1/camera/presets/${encodeURIComponent(preset.id)}/recall`, { method: "POST" });
    });
    container.append(button);
  }
  if (state) render(state);
}

function connect() {
  const protocol = location.protocol === "https:" ? "wss" : "ws";
  const socket = new WebSocket(`${protocol}://${location.host}/api/v1/events`);
  socket.addEventListener("open", () => {
    socketConnected = true;
    pipelineWasRunning = false;
    connection.textContent = "Live";
    connection.className = "status status-on";
    refreshPreview();
    pipelineWasRunning = true;
  });
  socket.addEventListener("message", ({ data }) => {
    const message = JSON.parse(data);
    if (message.type === "state") render(message.data);
    if (message.type === "event") appendEvent(message.data);
  });
  socket.addEventListener("close", () => {
    socketConnected = false;
    pipelineWasRunning = true;
    stopPreview();
    pipelineWasRunning = false;
    connection.textContent = "Reconnecting";
    connection.className = "status status-off";
    setTimeout(connect, 1000);
  });
}

$("#demo-trigger").addEventListener("click", async () => {
  await fetch("/api/v1/scenarios/open-palm-demo/trigger", { method: "POST" });
});

skeletonToggle.addEventListener("click", () => setSkeletonEnabled(!skeletonEnabled));
preview.addEventListener("load", () => {
  preview.classList.remove("disconnected");
  drawHandSkeleton();
});
preview.addEventListener("error", retryPreview);
new ResizeObserver(() => drawHandSkeleton()).observe($(".preview-stage"));

document.querySelectorAll("[data-action]").forEach((button) => {
  button.addEventListener("click", async () => {
    await fetch(`/api/v1/camera/actions/${button.dataset.action}`, { method: "POST" });
  });
});

document.querySelectorAll("[data-gesture-feature]").forEach((button) => {
  button.addEventListener("click", async () => {
    const feature = button.dataset.gestureFeature;
    const enabled = button.dataset.gestureEnabled === "true";
    pendingGestureFeatures.add(feature);
    cameraControlError = null;
    if (state) render(state);
    try {
      const response = await fetch(`/api/v1/camera/built-in-gestures/${feature}`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ enabled }),
      });
      if (!response.ok) {
        const payload = await response.json().catch(() => ({}));
        throw new Error(payload.error || `Camera command failed (${response.status})`);
      }
    } catch (error) {
      cameraControlError = error instanceof Error ? error.message : String(error);
    } finally {
      pendingGestureFeatures.delete(feature);
      if (state) render(state);
    }
  });
});

function scheduleZoom() {
  if (zoomPending || zoomSendTimer != null) return;
  zoomSendTimer = setTimeout(() => {
    zoomSendTimer = null;
    sendQueuedZoom();
  }, zoomUpdateIntervalMs);
}

async function sendQueuedZoom() {
  if (zoomPending || queuedZoom == null) return;
  const value = queuedZoom;
  queuedZoom = null;
  zoomPending = true;
  cameraControlError = null;
  if (state) render(state);
  try {
    const response = await fetch("/api/v1/camera/zoom", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ magnification: value }),
    });
    if (!response.ok) {
      const payload = await response.json().catch(() => ({}));
      throw new Error(payload.error || `Camera command failed (${response.status})`);
    }
  } catch (error) {
    cameraControlError = error instanceof Error ? error.message : String(error);
  } finally {
    zoomPending = false;
    if (queuedZoom == null) zoomDraft = null;
    else scheduleZoom();
    if (state) render(state);
  }
}

zoomSlider.addEventListener("input", () => {
  zoomDraft = Number(zoomSlider.value);
  queuedZoom = zoomDraft;
  if (state) render(state);
  scheduleZoom();
});
zoomReset.addEventListener("click", () => {
  zoomSlider.value = "1";
  zoomDraft = 1;
  queuedZoom = 1;
  if (state) render(state);
  scheduleZoom();
});

setInterval(() => state && render(state), 500);
setInterval(checkDaemonInstance, 2000);
setSkeletonEnabled(skeletonEnabled);
loadRecentEvents().catch(console.error);
loadPresets().catch(console.error);
connect();
checkDaemonInstance();
