import { installPreviewDrag, sourcePanTiltDirection } from "/assets/preview-drag.js";
import { syncAudioCapture } from "/assets/audio.js";
import { createElement, FolderOpen, FlipHorizontal2, Bone, Power, ChevronDown, ScanFace, Hand, ZoomIn } from "/assets/lucide.js";

const $ = (selector) => document.querySelector(selector);
const connection = $("#connection");
const daemonRestartDialog = $("#daemon-restart-dialog");
const daemonRestartForm = $("#daemon-restart-form");
const daemonRestartCancel = $("#daemon-restart-cancel");
const daemonRestartConfirm = $("#daemon-restart-confirm");
const daemonRestartError = $("#daemon-restart-error");
const events = $("#events");
let photoPending = false;
let recordingPending = false;
let recordingState = null;
let recordingStatusKey = null;
const recordVideo = $("#record-video");
const recordingStatus = $("#recording-status");
let resolutionPending = false;
const cameraOnly4k = () => (state?.pipeline?.width || 0) >= 3840;
const takePhoto = $("#take-photo");
const photoStatus = $("#photo-status");
const preview = $("#preview");
const overlay = $("#landmark-overlay");
const overlayContext = overlay.getContext("2d");
const skeletonToggle = $("#skeleton-toggle");
const outputModeInputs = [...document.querySelectorAll("[data-output-mode]")];
const backgroundToggle = $("#background-toggle");
const backgroundEffectInputs = [...document.querySelectorAll("[data-background-effect]")];
const cameraPowerToggle = $("#camera-power-toggle");
const faceTrackingToggle = $("#face-tracking-toggle");
const handsTrackingToggle = $("#hands-tracking-toggle");
const zoomSlider = $("#zoom-slider");
const zoomReset = $("#zoom-reset");
const autoZoomToggle = $("#auto-zoom-toggle");
const imageSettingsGroups = $("#image-settings-groups");
const panTiltButtons = [...document.querySelectorAll("[data-pan-tilt]")];
let state = null;
let skeletonEnabled = (
  localStorage.getItem("tarsier.skeletons")
  ?? localStorage.getItem("tarsier.handSkeleton")
) === "true";
let previewMirrorEnabled = localStorage.getItem("tarsier.previewMirror") === "true";
let socketConnected = false;
let pipelineWasRunning = null;
let previewRetry = null;
let daemonStartedAt = null;
let reloadRequested = false;
let daemonRestartAvailable = false;
let daemonRestartPending = false;
let cameraControlError = null;
let cameraPowerPending = false;
let cameraPowerError = null;
let cameraPowerDraft = null;
let zoomDraft = null;
let zoomPending = false;
let queuedZoom = null;
let zoomSendTimer = null;
let autoZoomPending = false;
let autoZoomDraft = null;
let hdrPending = false;
let trackingPending = false;
let faceTrackingPending = false;
let handsTrackingPending = false;
let imageSettingError = null;
let outputModePending = false;
let outputModeError = null;
let outputModeDraft = null;
let backgroundPending = false;
let backgroundError = null;
let backgroundDraft = null;
let panTiltPending = false;
let panTiltSyncQueued = false;
let panTiltKeepaliveTimer = null;
let heldDirections = [];
let previewDrag = null;
const pendingGestureFeatures = new Set();
const pendingImageSettings = new Set();
const imageSettingDrafts = new Map();
const imageSettingTimers = new Map();
const zoomUpdateIntervalMs = 100;
const imageSettingUpdateIntervalMs = 100;
const panTiltKeepaliveIntervalMs = 100;

const builtInGestureControls = [
  { feature: "target-selection", key: "target_selection", state: "#gesture-target-selection-state" },
  { feature: "zoom", key: "zoom", state: "#gesture-zoom-state" },
  { feature: "dynamic-zoom", key: "dynamic_zoom", state: "#gesture-dynamic-zoom-state" },
];

const imageSettingGroups = [
  {
    label: "Image",
    controls: [
      { control: "brightness", label: "Brightness", kind: "integer", help: "Digital image brightness." },
      { control: "contrast", label: "Contrast", kind: "integer", help: "Difference between dark and light tones." },
      { control: "saturation", label: "Saturation", kind: "integer", help: "Color intensity." },
      { control: "hue", label: "Hue", kind: "integer", help: "Overall color shift." },
      { control: "sharpness", label: "Sharpness", kind: "integer", help: "Digital edge enhancement." },
      { control: "power-line-frequency", label: "Anti-flicker", kind: "menu", help: "Match the local mains frequency." },
    ],
  },
  {
    label: "Exposure",
    controls: [
      { control: "auto-exposure", label: "Mode", kind: "menu", help: "Automatic or manual sensor exposure." },
      { control: "face-priority-auto-exposure", label: "Face priority", kind: "boolean", help: "Meter automatic exposure for a detected face." },
      { control: "exposure-dynamic-framerate", label: "Dynamic frame rate", kind: "boolean", help: "Allow automatic exposure to reduce frame rate." },
      { control: "exposure-time-absolute", label: "Shutter", kind: "integer", help: "Manual exposure time in 100 µs units.", format: "exposure" },
      { control: "gain", label: "Gain", kind: "integer", help: "Manual sensor gain." },
      { control: "backlight-compensation", label: "Backlight", kind: "integer", help: "Compensate for a bright background." },
    ],
  },
  {
    label: "White balance",
    controls: [
      { control: "white-balance-automatic", label: "Automatic", kind: "boolean", help: "Let the camera adapt color balance." },
      { control: "white-balance-temperature", label: "Color temperature", kind: "integer", help: "Manual white point in Kelvin.", format: "kelvin" },
      { control: "red-balance", label: "Red balance", kind: "integer", help: "Manual red-channel balance." },
      { control: "blue-balance", label: "Blue balance", kind: "integer", help: "Manual blue-channel balance." },
    ],
  },
  {
    label: "Focus",
    controls: [
      { control: "focus-automatic-continuous", label: "Continuous autofocus", kind: "boolean", help: "Keep focus under automatic camera control." },
      { control: "focus-absolute", label: "Manual focus", kind: "integer", help: "Fixed focus position." },
    ],
  },
];

const imageSettingDefinitions = imageSettingGroups.flatMap((group) => group.controls);

const handConnections = [
  [0, 1], [1, 2], [2, 3], [3, 4],
  [0, 5], [5, 6], [6, 7], [7, 8],
  [5, 9], [9, 10], [10, 11], [11, 12],
  [9, 13], [13, 14], [14, 15], [15, 16],
  [13, 17], [0, 17], [17, 18], [18, 19], [19, 20],
];

const poseConnections = [
  [11, 12],
  [11, 13], [13, 15], [15, 17], [15, 19], [15, 21],
  [12, 14], [14, 16], [16, 18], [16, 20], [16, 22],
  [11, 23], [12, 24], [23, 24],
  [23, 25], [25, 27], [27, 29], [29, 31],
  [24, 26], [26, 28], [28, 30], [30, 32],
];

const faceContourConnections = [
  61, 146, 146, 91, 91, 181, 181, 84, 84, 17, 17, 314, 314, 405, 405, 321,
  321, 375, 375, 291, 61, 185, 185, 40, 40, 39, 39, 37, 37, 0, 0, 267,
  267, 269, 269, 270, 270, 409, 409, 291, 78, 95, 95, 88, 88, 178, 178, 87,
  87, 14, 14, 317, 317, 402, 402, 318, 318, 324, 324, 308, 78, 191, 191, 80,
  80, 81, 81, 82, 82, 13, 13, 312, 312, 311, 311, 310, 310, 415, 415, 308,
  263, 249, 249, 390, 390, 373, 373, 374, 374, 380, 380, 381, 381, 382, 382, 362,
  263, 466, 466, 388, 388, 387, 387, 386, 386, 385, 385, 384, 384, 398, 398, 362,
  276, 283, 283, 282, 282, 295, 295, 285, 300, 293, 293, 334, 334, 296, 296, 336,
  33, 7, 7, 163, 163, 144, 144, 145, 145, 153, 153, 154, 154, 155, 155, 133,
  33, 246, 246, 161, 161, 160, 160, 159, 159, 158, 158, 157, 157, 173, 173, 133,
  46, 53, 53, 52, 52, 65, 65, 55, 70, 63, 63, 105, 105, 66, 66, 107,
  10, 338, 338, 297, 297, 332, 332, 284, 284, 251, 251, 389, 389, 356, 356, 454,
  454, 323, 323, 361, 361, 288, 288, 397, 397, 365, 365, 379, 379, 378, 378, 400,
  400, 377, 377, 152, 152, 148, 148, 176, 176, 149, 149, 150, 150, 136, 136, 172,
  172, 58, 58, 132, 132, 93, 93, 234, 234, 127, 127, 162, 162, 21, 21, 54,
  54, 103, 103, 67, 67, 109, 109, 10,
];

const angle = (value) => value == null ? "—" : `${value.toFixed(1)}°`;
const angleVector = (roll, pitch, yaw, suffix = "°") => [roll, pitch, yaw].some((value) => value == null)
  ? "—"
  : `R ${roll.toFixed(1)}${suffix} · P ${pitch.toFixed(1)}${suffix} · Y ${yaw.toFixed(1)}${suffix}`;
const age = (timestamp) => timestamp == null ? "No sample" : `${Math.max(0, (Date.now() - timestamp) / 1000).toFixed(1)}s ago`;
const attitudeLabel = (source) => ({
  "last-commanded": "Last command",
  measured: "Measured",
  simulated: "Simulated",
}[source] || "No attitude sample");
const magnification = (value) => `×${Number(value).toFixed(1)}`;
const cameraIsPowered = (camera) => camera.powered_on !== false;
const cameraControlsAvailable = (camera) => camera.available && cameraIsPowered(camera) && !cameraPowerPending;

function buildImageSettingsUi() {
  imageSettingsGroups.innerHTML = imageSettingGroups.map((group) => `
    <details class="image-settings-group" data-image-settings-group="${group.label.toLowerCase().replaceAll(" ", "-")}" open>
      <summary><h4>${group.label}</h4></summary>
      ${group.controls.map((definition) => `
        <div class="image-setting-row kind-${definition.kind}" data-image-setting-row="${definition.control}">
          <div class="image-setting-copy">
            ${definition.kind === "boolean"
              ? `<span id="image-setting-${definition.control}-label">${definition.label}</span>`
              : `<label id="image-setting-${definition.control}-label" for="image-setting-${definition.control}">${definition.label}</label>`}
            <small data-image-setting-help="${definition.control}">${definition.help}</small>
          </div>
          <div class="image-setting-editor">
            <output data-image-setting-value="${definition.control}">Unknown</output>
            ${definition.kind === "integer" ? `
              <input id="image-setting-${definition.control}" type="range" data-image-setting-input="${definition.control}" disabled>
            ` : definition.kind === "menu" ? `
              <select id="image-setting-${definition.control}" data-image-setting-input="${definition.control}" disabled></select>
            ` : `
              <div class="segmented-control" role="group" aria-labelledby="image-setting-${definition.control}-label">
                <button class="secondary compact" type="button" data-image-setting-button="${definition.control}" data-image-setting-value-option="1" disabled>On</button>
                <button class="secondary compact" type="button" data-image-setting-button="${definition.control}" data-image-setting-value-option="0" disabled>Off</button>
              </div>
            `}
          </div>
        </div>
      `).join("")}
    </details>
  `).join("");
  imageSettingsGroups.querySelectorAll("details[data-image-settings-group]").forEach((card) => {
    card.querySelector("summary").prepend(createElement(ChevronDown, { width: 18, height: 18, "aria-hidden": "true", focusable: "false" }));
    const storageKey = `tarsier.imageSettings.${card.dataset.imageSettingsGroup}.open`;
    try {
      card.open = localStorage.getItem(storageKey) !== "false";
    } catch {
      // Keep the default when browser storage is unavailable.
    }
    card.addEventListener("toggle", () => {
      try {
        localStorage.setItem(storageKey, String(card.open));
      } catch {
        // Folding remains available when browser storage cannot be written.
      }
    });
  });
}

function imageSettingState(camera, control) {
  return camera.image_settings?.controls?.find((setting) => setting.control === control) || null;
}

function imageSettingMode(camera, control) {
  const value = (dependency) => imageSettingState(camera, dependency)?.value ?? null;
  if (["exposure-time-absolute", "gain"].includes(control)) {
    return value("auto-exposure") === 1
      ? { active: true, reason: null }
      : { active: false, reason: "Select Manual exposure first." };
  }
  if (["face-priority-auto-exposure", "exposure-dynamic-framerate"].includes(control)) {
    const exposureMode = value("auto-exposure");
    return exposureMode != null && exposureMode !== 1
      ? { active: true, reason: null }
      : { active: false, reason: "Available only with automatic exposure." };
  }
  if (["white-balance-temperature", "red-balance", "blue-balance"].includes(control)) {
    return value("white-balance-automatic") === 0
      ? { active: true, reason: null }
      : { active: false, reason: "Turn Automatic white balance off first." };
  }
  if (control === "focus-absolute") {
    return value("focus-automatic-continuous") === 0
      ? { active: true, reason: null }
      : { active: false, reason: "Turn Continuous autofocus off first." };
  }
  return { active: true, reason: null };
}

function formatImageSettingValue(definition, controlState, value) {
  if (value == null) return "Unknown";
  if (definition.kind === "boolean") return value === 1 ? "On" : "Off";
  if (definition.kind === "menu") {
    return controlState?.options?.find((option) => option.value === value)?.label || String(value);
  }
  if (definition.format === "kelvin") return `${value} K`;
  if (definition.format === "exposure") return `${value} · ${(value / 10).toFixed(1)} ms`;
  return String(value);
}

function renderImageSettings(camera) {
  const settings = camera.image_settings || { controls: [] };
  const samples = settings.controls
    .map((control) => control.sample_at_ms)
    .filter(Number.isFinite);
  const newestSample = samples.length ? Math.max(...samples) : null;
  const unavailable = settings.controls.filter((control) => !control.available).length;
  $("#image-settings-readback").textContent = pendingImageSettings.size > 0
    ? `Applying ${pendingImageSettings.size} change${pendingImageSettings.size === 1 ? "" : "s"}…`
    : newestSample == null ? "Awaiting camera readback"
    : unavailable > 0 ? `Readback ${age(newestSample)} · ${unavailable} unavailable`
    : `Readback ${age(newestSample)}`;

  const error = imageSettingError || settings.error;
  $("#image-settings-error").hidden = !error;
  $("#image-settings-error").textContent = error || "";

  for (const definition of imageSettingDefinitions) {
    const controlState = imageSettingState(camera, definition.control);
    const row = document.querySelector(`[data-image-setting-row="${definition.control}"]`);
    const output = document.querySelector(`[data-image-setting-value="${definition.control}"]`);
    const help = document.querySelector(`[data-image-setting-help="${definition.control}"]`);
    const pending = pendingImageSettings.has(definition.control);
    const draft = imageSettingDrafts.get(definition.control);
    const value = draft ?? controlState?.value ?? null;
    const mode = imageSettingMode(camera, definition.control);
    const writable = cameraControlsAvailable(camera)
      && controlState?.available === true
      && controlState.active === true
      && controlState.read_only !== true
      && mode.active;
    const unavailableReason = controlState?.error
      ? `Unavailable: ${controlState.error}`
      : controlState && !controlState.active ? "Inactive in the current camera mode."
      : mode.reason;

    row.classList.toggle("inactive", !writable);
    row.title = controlState?.sample_at_ms == null
      ? "Awaiting camera readback"
      : `Camera readback ${age(controlState.sample_at_ms)}`;
    output.textContent = pending
      ? `Applying ${formatImageSettingValue(definition, controlState, value)}`
      : formatImageSettingValue(definition, controlState, value);
    help.textContent = unavailableReason
      ? `${definition.help} ${unavailableReason}`
      : definition.help;

    if (definition.kind === "integer") {
      const input = document.querySelector(`[data-image-setting-input="${definition.control}"]`);
      if (controlState?.minimum != null) input.min = String(controlState.minimum);
      if (controlState?.maximum != null) input.max = String(controlState.maximum);
      if (controlState?.step != null) input.step = String(controlState.step);
      if (draft == null && controlState?.value != null) input.value = String(controlState.value);
      input.disabled = !writable;
    } else if (definition.kind === "menu") {
      const input = document.querySelector(`[data-image-setting-input="${definition.control}"]`);
      const options = controlState?.options || [];
      const optionKey = JSON.stringify(options);
      if (input.dataset.options !== optionKey) {
        input.replaceChildren(...options.map((option) => {
          const element = document.createElement("option");
          element.value = String(option.value);
          element.textContent = option.label;
          return element;
        }));
        input.dataset.options = optionKey;
      }
      if (value != null) input.value = String(value);
      input.disabled = !writable || options.length === 0 || pending;
    } else {
      document.querySelectorAll(`[data-image-setting-button="${definition.control}"]`).forEach((button) => {
        const buttonValue = Number(button.dataset.imageSettingValueOption);
        button.setAttribute("aria-pressed", String(value != null && value === buttonValue));
        button.disabled = !writable || pending;
      });
    }
  }
}

function syncDaemonRestartControl() {
  connection.disabled = !socketConnected || !daemonRestartAvailable || daemonRestartPending;
  connection.title = daemonRestartAvailable
    ? "Restart the supervised Tarsier daemon"
    : "Daemon restart is unavailable without service supervision";
}

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
    daemonRestartAvailable = health.restart_available === true;
    syncDaemonRestartControl();
    observeDaemon(health.started_at_ms);
  } catch {
    // A stopped daemon is expected during upgrades; the WebSocket owns the status display.
  }
}

function drawSkeletons(
  faceLandmarks = state?.perception.face_landmarks || [],
  handLandmarks = state?.perception.hand_landmarks || [],
  poseLandmarks = state?.perception.pose_landmarks || [],
) {
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
  if (!skeletonEnabled || state?.video_effects?.output_mode !== "camera") return;

  const sourceWidth = preview.naturalWidth || 16;
  const sourceHeight = preview.naturalHeight || 9;
  const scale = Math.min(bounds.width / sourceWidth, bounds.height / sourceHeight);
  const renderedWidth = sourceWidth * scale;
  const renderedHeight = sourceHeight * scale;
  const offsetX = (bounds.width - renderedWidth) / 2;
  const offsetY = (bounds.height - renderedHeight) / 2;
  const project = (landmarks) => landmarks.map((point) => {
    const transformed = transformLandmark(point, sourceWidth, sourceHeight);
    return {
      x: offsetX + transformed.x * renderedWidth,
      y: offsetY + transformed.y * renderedHeight,
      visibility: point.visibility,
    };
  });

  overlayContext.lineCap = "round";
  overlayContext.lineJoin = "round";
  overlayContext.shadowColor = "rgba(9, 13, 11, 0.9)";
  overlayContext.shadowBlur = 4;

  if (poseLandmarks.length === 33) {
    const points = project(poseLandmarks);
    const visible = (point) => point.visibility == null || point.visibility >= 0.5;
    overlayContext.lineWidth = 3.5;
    overlayContext.strokeStyle = "rgba(216, 132, 255, 0.92)";
    overlayContext.beginPath();
    for (const [fromIndex, toIndex] of poseConnections) {
      const from = points[fromIndex];
      const to = points[toIndex];
      if (!visible(from) || !visible(to)) continue;
      overlayContext.moveTo(from.x, from.y);
      overlayContext.lineTo(to.x, to.y);
    }
    overlayContext.stroke();

    overlayContext.shadowBlur = 0;
    overlayContext.fillStyle = "#f5d9ff";
    for (const point of points.slice(11)) {
      if (!visible(point)) continue;
      overlayContext.beginPath();
      overlayContext.arc(point.x, point.y, 3.5, 0, Math.PI * 2);
      overlayContext.fill();
    }
  }

  if (faceLandmarks.length === 478) {
    const points = project(faceLandmarks);
    overlayContext.lineWidth = 1.4;
    overlayContext.strokeStyle = "rgba(112, 218, 255, 0.85)";
    overlayContext.beginPath();
    for (let index = 0; index < faceContourConnections.length; index += 2) {
      const from = points[faceContourConnections[index]];
      const to = points[faceContourConnections[index + 1]];
      overlayContext.moveTo(from.x, from.y);
      overlayContext.lineTo(to.x, to.y);
    }
    overlayContext.stroke();

    overlayContext.shadowBlur = 0;
    overlayContext.fillStyle = "rgba(185, 235, 255, 0.7)";
    for (const point of points) {
      overlayContext.beginPath();
      overlayContext.arc(point.x, point.y, 0.9, 0, Math.PI * 2);
      overlayContext.fill();
    }
  }

  const handColors = ["rgba(183, 239, 122, 0.9)", "rgba(240, 196, 105, 0.9)"];
  const handCount = handLandmarks.length / 21;
  if (Number.isInteger(handCount) && handCount >= 1 && handCount <= 2) {
    for (let handIndex = 0; handIndex < handCount; handIndex += 1) {
      const points = project(handLandmarks.slice(handIndex * 21, (handIndex + 1) * 21));
      overlayContext.shadowBlur = 4;
      overlayContext.lineWidth = 2.5;
      overlayContext.strokeStyle = handColors[handIndex];
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
  }
}

function setSkeletonEnabled(enabled) {
  skeletonEnabled = enabled;
  localStorage.setItem("tarsier.skeletons", String(enabled));
  skeletonToggle.setAttribute("aria-pressed", String(enabled));
  skeletonToggle.title = enabled ? "Hide skeletons" : "Show skeletons";
  drawSkeletons();
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
  drawSkeletons([], []);
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
  $("#gesture-readback").textContent = gestures.sample_at_ms == null
    ? "Awaiting camera readback"
    : `Measured ${age(gestures.sample_at_ms)}`;
  for (const control of builtInGestureControls) {
    const value = gestures[control.key] ?? null;
    const pending = pendingGestureFeatures.has(control.feature);
    $(control.state).textContent = pending ? "Applying…" : value == null ? "Unknown" : value ? "On" : "Off";
    document.querySelectorAll(`[data-gesture-feature="${control.feature}"]`).forEach((button) => {
      const buttonValue = button.dataset.gestureEnabled === "true";
      button.setAttribute("aria-pressed", String(value != null && value === buttonValue));
      button.disabled = !cameraControlsAvailable(camera) || pending;
    });
  }
}

function renderZoom(camera) {
  const faceTracking = camera.face_tracking || {};
  const autoZoom = faceTracking.auto_zoom || {};
  const current = (autoZoom.enabled ? autoZoom.zoom_magnification : null)
    ?? camera.zoom_magnification ?? null;
  if (zoomDraft == null && current != null) zoomSlider.value = String(current);
  const displayed = zoomDraft ?? current;
  $("#zoom-value").textContent = zoomPending
    ? `Applying ${magnification(displayed)}`
    : displayed == null ? "Unknown" : magnification(displayed);
  zoomSlider.disabled = !cameraControlsAvailable(camera);
  zoomReset.disabled = !cameraControlsAvailable(camera) || zoomPending;
  const autoZoomEnabled = autoZoomDraft ?? autoZoom.enabled === true;
  autoZoomToggle.setAttribute("aria-pressed", String(autoZoomEnabled));
  autoZoomToggle.title = !faceTracking.enabled ? "Auto zoom needs face tracking" : autoZoomPending ? "Switching auto zoom…" : autoZoomEnabled ? "Stop auto zoom" : "Enable auto zoom";
  autoZoomToggle.setAttribute("aria-label", autoZoomToggle.title);
  autoZoomToggle.disabled = !cameraControlsAvailable(camera) || !faceTracking.enabled
    || autoZoomPending || faceTrackingPending || handsTrackingPending;
  $("#auto-zoom-readback").textContent = autoZoomPending
    ? "Switching auto zoom…"
    : autoZoom.error ? `Auto zoom failed: ${autoZoom.error}.`
    : !faceTracking.enabled ? "Auto zoom needs face tracking."
    : !autoZoom.enabled ? "Auto zoom is off."
    : !autoZoom.calibrated ? "Auto zoom is waiting for a fresh face."
    : autoZoom.face_size == null ? "Auto zoom is waiting for the face to return."
    : autoZoom.at_limit ? "Auto zoom reached the lens limit."
    : "Auto zoom is holding the calibrated face size.";
  $("#zoom-readback").textContent = camera.zoom_sample_at_ms == null
    ? "Awaiting camera readback."
    : `Camera readback ${age(camera.zoom_sample_at_ms)}.`;
}

function renderHdr(camera) {
  $("#hdr-feature-state").textContent = hdrPending
    ? "Applying…"
    : camera.hdr == null ? "Unknown" : camera.hdr ? "On" : "Off";
  document.querySelectorAll('[data-camera-feature="hdr"]').forEach((button) => {
    const buttonValue = button.dataset.cameraEnabled === "true";
    button.setAttribute("aria-pressed", String(camera.hdr != null && camera.hdr === buttonValue));
    button.disabled = !cameraControlsAvailable(camera) || hdrPending;
    button.title = camera.hdr_sample_at_ms == null
      ? "Awaiting camera readback"
      : `Camera readback ${age(camera.hdr_sample_at_ms)}`;
  });
}

function renderTracking(camera) {
  $("#tracking-feature-state").textContent = trackingPending
    ? "Applying…"
    : camera.tracking == null ? "Unknown" : camera.tracking ? "On" : "Off";
  document.querySelectorAll('[data-camera-feature="tracking"]').forEach((button) => {
    const buttonValue = button.dataset.cameraEnabled === "true";
    button.setAttribute("aria-pressed", String(camera.tracking != null && camera.tracking === buttonValue));
    button.disabled = !cameraControlsAvailable(camera) || trackingPending
      || faceTrackingPending || handsTrackingPending;
    button.title = camera.tracking_sample_at_ms == null
      ? "Awaiting camera readback"
      : `Camera readback ${age(camera.tracking_sample_at_ms)}`;
  });
}

function renderFaceTracking(camera) {
  const tracking = camera.face_tracking || {};
  const targetName = tracking.target_source === "shoulders" ? "shoulders" : "face";
  faceTrackingToggle.disabled = !cameraControlsAvailable(camera) || faceTrackingPending
    || handsTrackingPending || trackingPending;
  faceTrackingToggle.setAttribute("aria-pressed", String(tracking.enabled === true));
  faceTrackingToggle.setAttribute("aria-label", faceTrackingPending
    ? "Switching…"
    : tracking.enabled ? "Stop face tracking" : "Face tracking");
  faceTrackingToggle.title = tracking.error
    || (tracking.enabled
      ? tracking.target_visible ? `Tracking from detected ${targetName}` : "Waiting for a face or shoulders"
      : "Track the detected face, with shoulders as a fallback");
}

function renderHandsTracking(camera) {
  const tracking = camera.hands_tracking || {};
  handsTrackingToggle.disabled = !cameraControlsAvailable(camera) || handsTrackingPending
    || faceTrackingPending || trackingPending;
  handsTrackingToggle.setAttribute("aria-pressed", String(tracking.enabled === true));
  handsTrackingToggle.setAttribute("aria-label", handsTrackingPending
    ? "Switching…"
    : tracking.enabled ? "Stop hands tracking" : "Hands tracking");
  handsTrackingToggle.title = tracking.error
    || (tracking.enabled
      ? tracking.rapid_motion ? "Rapid hand movement detected; camera motion is frozen"
      : tracking.hands_visible === 2 ? "Framing both detected hands"
      : tracking.hands_visible === 1 ? "Following one hand with pan and tilt; zoom is frozen"
      : "Waiting for a hand"
      : "Keep up to two detected hands in frame");
}

function activeDirection() {
  return heldDirections.at(-1) ?? null;
}

function canDragPreview() {
  return socketConnected && state?.pipeline.running && !videoTransformPending
    && !faceTrackingPending && !handsTrackingPending && !trackingPending
    && cameraControlsAvailable(state?.camera || {});
}

function renderPanTilt(camera) {
  preview.classList.toggle("pan-tilt-ready", !!canDragPreview());
  if (!canDragPreview()) previewDrag?.cancel();
  const direction = activeDirection();
  const trackingLocked = faceTrackingPending || handsTrackingPending || trackingPending;
  for (const button of panTiltButtons) {
    button.disabled = !cameraControlsAvailable(camera) || trackingLocked;
    button.setAttribute("aria-pressed", String(heldDirections.includes(button.dataset.panTilt)));
  }
  const faceTracking = camera.face_tracking || {};
  const trackingTarget = faceTracking.target_source === "shoulders" ? "shoulders" : "face";
  const handsTracking = camera.hands_tracking || {};
  $("#pan-tilt-status").textContent = trackingPending || faceTrackingPending || handsTrackingPending
    ? "Switching tracking…"
    : camera.tracking === true ? "Camera tracking controls the gimbal"
    : handsTracking.enabled && handsTracking.rapid_motion ? "Hands tracking · rapid movement · holding"
    : handsTracking.enabled && handsTracking.hands_visible === 0 ? "Hands tracking · waiting for hands · zoom frozen"
    : handsTracking.enabled && handsTracking.hands_visible === 1 && handsTracking.active
      ? "Hands tracking · one hand · following · zoom frozen"
    : handsTracking.enabled && handsTracking.hands_visible === 1
      ? "Hands tracking · one hand · centered · zoom frozen"
    : handsTracking.enabled && handsTracking.active ? "Hands tracking · two hands · framing"
    : handsTracking.enabled ? "Hands tracking · two hands · framed"
    : faceTracking.enabled && !faceTracking.target_visible ? "Face tracking · searching for face or shoulders"
    : faceTracking.enabled && faceTracking.active ? `Face tracking · ${trackingTarget} · centering`
    : faceTracking.enabled ? `Face tracking · ${trackingTarget} · centered`
    : direction ? `Moving ${direction}`
    : panTiltPending ? "Stopping…" : "Drag the preview, hold a button, or use arrow keys";
}

function renderCameraPower(camera) {
  const poweredOn = cameraPowerDraft ?? camera.powered_on === true;
  cameraPowerToggle.setAttribute("aria-pressed", String(poweredOn));
  cameraPowerToggle.classList.toggle("status-on", poweredOn);
  cameraPowerToggle.classList.toggle("status-off", !poweredOn);
  cameraPowerToggle.disabled = !camera.available || cameraPowerPending;
  const powerStatus = cameraPowerError
    ? "Power · failed"
    : cameraPowerPending ? (poweredOn ? "Power · waking…" : "Power · sleeping…")
    : !camera.available ? "Power · unavailable"
    : camera.powered_on == null ? "Power · unknown"
    : poweredOn ? "Power · on" : "Power · off";
  const powerAction = poweredOn ? "Put the physical camera to sleep" : "Wake the physical camera";
  cameraPowerToggle.title = `${powerStatus} · ${powerAction}`;
  cameraPowerToggle.setAttribute("aria-label", cameraPowerToggle.title);
  cameraPowerToggle.setAttribute("aria-busy", String(cameraPowerPending));
}

function backgroundState(videoEffects) {
  const effect = ["green-screen", "blur", "pixel-party"].includes(videoEffects.background_effect)
    ? videoEffects.background_effect
    : "green-screen";
  const enabled = videoEffects.background_enabled
    ?? (videoEffects.green_screen_enabled === true);
  return { enabled, effect };
}

function renderOutputMode(videoEffects) {
  const configuredIdentity = videoEffects.output_mode === "comic-avatar"
    ? (videoEffects.avatar_engine || "stylized-3d")
    : videoEffects.output_mode === "depth-map" ? "depth-map"
    : "camera";
  const identity = outputModeDraft || configuredIdentity;
  const publishedFresh = videoEffects.avatar_published_at_ms != null
    && Date.now() - videoEffects.avatar_published_at_ms <= 500;
  const capturedFresh = videoEffects.avatar_captured_at_ms != null
    && Date.now() - videoEffects.avatar_captured_at_ms <= 500;
  const avatarFresh = videoEffects.avatar_available && publishedFresh && capturedFresh;
  const depthPublishedFresh = videoEffects.depth_published_at_ms != null
    && Date.now() - videoEffects.depth_published_at_ms <= 500;
  const depthCapturedFresh = videoEffects.depth_captured_at_ms != null
    && Date.now() - videoEffects.depth_captured_at_ms <= 500;
  const depthFresh = videoEffects.depth_available && depthPublishedFresh && depthCapturedFresh;
  for (const input of outputModeInputs) {
    input.disabled = outputModePending || (cameraOnly4k() && input.value !== "camera");
    input.checked = input.value === identity;
  }
  skeletonToggle.disabled = identity !== "camera";
  const status = outputModeError
    ? "Change failed"
    : outputModePending ? "Switching…"
    : identity === "camera" ? "Real camera"
    : identity === "depth-map" ? (depthFresh ? "Relative depth active" : "Privacy fallback · waiting for depth")
    : avatarFresh ? (identity === "portrait3d" ? "Personal 3D active" : identity === "stylized-3d" ? "Stylized 3D active" : "LivePortrait active")
    : `Privacy fallback · waiting for ${identity !== "liveportrait" ? "3D renderer" : "LivePortrait"}`;
  $("#output-status").textContent = status;
  $("#output-error").hidden = !outputModeError;
  $("#output-error").textContent = outputModeError || "";
}

function renderBackground(videoEffects) {
  const current = backgroundDraft || backgroundState(videoEffects);
  const publishedFresh = videoEffects.mask_published_at_ms != null
    && Date.now() - videoEffects.mask_published_at_ms <= 200;
  const capturedFresh = videoEffects.mask_captured_at_ms != null
    && Date.now() - videoEffects.mask_captured_at_ms <= 200;
  const portraitActive = videoEffects.output_mode === "comic-avatar"
    && videoEffects.avatar_engine === "portrait3d";
  const maskFresh = portraitActive
    ? videoEffects.avatar_available
      && Date.now() - (videoEffects.avatar_captured_at_ms || 0) <= 500
      && Date.now() - (videoEffects.avatar_published_at_ms || 0) <= 500
    : publishedFresh && capturedFresh;
  const alternateOutputActive = videoEffects.output_mode !== "camera" && !portraitActive;
  backgroundToggle.disabled = backgroundPending || alternateOutputActive || cameraOnly4k();
  backgroundToggle.checked = current.enabled;
  for (const input of backgroundEffectInputs) {
    input.disabled = backgroundPending || alternateOutputActive || cameraOnly4k();
    input.checked = input.value === current.effect;
  }

  const effectLabel = {
    "green-screen": "Green screen",
    blur: "Blur",
    "pixel-party": "Pixel Party",
  }[current.effect] || "Green screen";
  const status = backgroundError
    ? "Change failed"
    : (alternateOutputActive ? "Available for Camera and Personal 3D"
      : backgroundPending ? `Applying ${effectLabel.toLowerCase()}…`
      : !current.enabled ? "Off"
      : !maskFresh ? "Privacy fallback · waiting for a fresh mask"
      : `${effectLabel} active`);
  $("#background-status").textContent = status;
  $("#background-error").hidden = !backgroundError;
  $("#background-error").textContent = backgroundError || "";
  backgroundToggle.title = current.enabled ? "Disable the background effect" : "Enable the selected background effect";
}

function render(next) {
  observeDaemon(next.started_at_ms);
  state = next;
  syncAudioCapture(next.audio_capture_sources || [], next.audio_virtual, next.audio_reservations, next.audio_released_sources, next.audio_busy_sources, next.audio_output_applications);
  if (next.last_photo) renderSavedPhoto(next.last_photo);
  renderVideoTransform();
  const camera = next.camera;
  const pipeline = next.pipeline;
  const perception = next.perception;
  $("#yaw").textContent = angle(camera.yaw_degrees);
  $("#pitch").textContent = angle(camera.pitch_degrees);
  $("#roll").textContent = angle(camera.roll_degrees);
  $("#camera-euler").textContent = angleVector(
    camera.euler_roll_degrees,
    camera.euler_pitch_degrees,
    camera.euler_yaw_degrees,
  );
  $("#camera-velocity").textContent = angleVector(
    camera.roll_velocity_degrees_per_second,
    camera.pitch_velocity_degrees_per_second,
    camera.yaw_velocity_degrees_per_second,
    "°/s",
  );
  $("#tracking").textContent = trackingPending
    ? "Applying…"
    : camera.tracking == null ? "—" : camera.tracking ? "On" : "Off";
  $("#camera-age").textContent = camera.sample_at_ms == null
    ? attitudeLabel(camera.attitude_source)
    : `${attitudeLabel(camera.attitude_source)} · ${age(camera.sample_at_ms)}`;
  renderZoom(camera);
  renderImageSettings(camera);
  renderHdr(camera);
  renderTracking(camera);
  renderFaceTracking(camera);
  renderHandsTracking(camera);
  renderPanTilt(camera);
  renderBuiltInGestures(camera);
  renderCameraPower(camera);
  renderOutputMode(next.video_effects);
  renderBackground(next.video_effects);
  $("#pipeline-summary").textContent = pipeline.running
    ? ` · ${pipeline.fps.toFixed(1)} fps · ${pipeline.frame_count} frames`
    : camera.powered_on === false ? "Camera off"
    : pipeline.error || "Pipeline stopped";
  $("#video-resolution").textContent = pipeline.width ? `${pipeline.width}×${pipeline.height}` : "Resolution";
  for (const button of document.querySelectorAll("[data-resolution]")) {
    button.disabled = resolutionPending || recordingState?.active || !socketConnected || !daemonRestartAvailable;
    button.setAttribute("aria-pressed", String(button.dataset.resolution === `${pipeline.width}x${pipeline.height}`));
  }
  syncPreview(pipeline);
  takePhoto.disabled = photoPending || !socketConnected || !pipeline.running;
  renderRecording();
  $("#worker").textContent = perception.worker_connected ? `Frame ${perception.frame_id}` : "Worker offline";
  $("#gesture").textContent = perception.gesture || "No gesture";
  $("#confidence").textContent = gestureDetail(perception);
  $("#gesture-icon").classList.toggle("active", perception.gesture === "open_palm");
  $("#perception-error").hidden = !perception.error;
  $("#perception-error").textContent = perception.error || "";
  const cameraError = [
    cameraControlError,
    cameraPowerError,
    camera.power_error,
    camera.error,
    camera.telemetry_error,
    camera.tracking_error,
    camera.face_tracking?.error,
    camera.face_tracking?.auto_zoom?.error,
    camera.hands_tracking?.error,
    camera.hands_tracking?.zoom_error,
    camera.zoom_error,
    camera.hdr_error,
    camera.built_in_gestures?.error,
  ].filter(Boolean).join(" · ");
  $("#camera-error").hidden = !cameraError;
  $("#camera-error").textContent = cameraError || "";
  document.querySelectorAll("[data-action], [data-preset]").forEach((button) => {
    button.disabled = !cameraControlsAvailable(camera) || faceTrackingPending
      || handsTrackingPending || trackingPending;
  });
  drawSkeletons(
    socketConnected && pipeline.running ? perception.face_landmarks : [],
    socketConnected && pipeline.running ? perception.hand_landmarks : [],
    socketConnected && pipeline.running ? perception.pose_landmarks : [],
  );
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
    syncDaemonRestartControl();
    refreshPreview();
    pipelineWasRunning = true;
  });
  socket.addEventListener("message", ({ data }) => {
    const message = JSON.parse(data);
    if (message.type === "state") render(message.data);
    if (message.type === "recording") {
      recordingState = message.data;
      renderRecording();
    }
    if (message.type === "event") appendEvent(message.data);
  });
  socket.addEventListener("close", () => {
    clearHeldDirections();
    socketConnected = false;
    recordingState = null;
    renderRecording();
    pipelineWasRunning = true;
    stopPreview();
    pipelineWasRunning = false;
    connection.textContent = "Reconnecting";
    connection.className = "status status-off";
    syncDaemonRestartControl();
    setTimeout(connect, 1000);
  });
}

connection.addEventListener("click", () => {
  if (connection.disabled || daemonRestartPending) return;
  daemonRestartError.hidden = true;
  daemonRestartError.textContent = "";
  daemonRestartDialog.showModal();
});

daemonRestartCancel.addEventListener("click", () => daemonRestartDialog.close());

daemonRestartForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (daemonRestartPending) return;
  daemonRestartPending = true;
  daemonRestartConfirm.disabled = true;
  daemonRestartCancel.disabled = true;
  daemonRestartError.hidden = true;
  daemonRestartConfirm.textContent = "Restarting…";
  syncDaemonRestartControl();
  try {
    const response = await fetch("/api/v1/daemon/restart", { method: "POST" });
    if (!response.ok) {
      const payload = await response.json().catch(() => ({}));
      throw new Error(payload.error || `Daemon restart failed (${response.status})`);
    }
    daemonRestartDialog.close();
    connection.textContent = "Restarting";
    connection.className = "status status-off";
  } catch (error) {
    daemonRestartPending = false;
    daemonRestartConfirm.disabled = false;
    daemonRestartCancel.disabled = false;
    daemonRestartConfirm.textContent = "Restart daemon";
    daemonRestartError.textContent = error instanceof Error ? error.message : String(error);
    daemonRestartError.hidden = false;
    syncDaemonRestartControl();
  }
});

$("#demo-trigger").addEventListener("click", async () => {
  await fetch("/api/v1/scenarios/open-palm-demo/trigger", { method: "POST" });
});

skeletonToggle.addEventListener("click", () => setSkeletonEnabled(!skeletonEnabled));
async function setBackground(enabled, effect) {
  if (backgroundPending || !state) return;
  backgroundPending = true;
  backgroundError = null;
  backgroundDraft = { enabled, effect };
  render(state);
  try {
    const response = await fetch("/api/v1/video/background", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ enabled, effect }),
    });
    if (!response.ok) {
      const payload = await response.json().catch(() => ({}));
      throw new Error(payload.error || `Video effect failed (${response.status})`);
    }
    Object.assign(state.video_effects, {
      background_enabled: enabled,
      background_effect: effect,
      green_screen_enabled: enabled && effect === "green-screen",
    });
  } catch (error) {
    backgroundError = error instanceof Error ? error.message : String(error);
  } finally {
    backgroundPending = false;
    backgroundDraft = null;
    if (state) render(state);
  }
}

async function setOutputMode(identity) {
  if (outputModePending || !state) return;
  outputModePending = true;
  outputModeError = null;
  outputModeDraft = identity;
  render(state);
  try {
    const response = await fetch("/api/v1/video/identity", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ identity }),
    });
    if (!response.ok) {
      const payload = await response.json().catch(() => ({}));
      throw new Error(payload.error || `Output mode failed (${response.status})`);
    }
    state.video_effects.output_mode = identity === "camera"
      ? "camera"
      : identity === "depth-map" ? "depth-map"
      : "comic-avatar";
    if (identity === "depth-map") {
      state.video_effects.depth_available = false;
      state.video_effects.depth_frame_id = null;
      state.video_effects.depth_captured_at_ms = null;
      state.video_effects.depth_published_at_ms = null;
    } else if (identity !== "camera") {
      state.video_effects.avatar_engine = identity;
      state.video_effects.avatar_available = false;
      state.video_effects.avatar_frame_id = null;
      state.video_effects.avatar_captured_at_ms = null;
      state.video_effects.avatar_published_at_ms = null;
    }
  } catch (error) {
    outputModeError = error instanceof Error ? error.message : String(error);
  } finally {
    outputModePending = false;
    outputModeDraft = null;
    if (state) render(state);
  }
}

for (const input of outputModeInputs) {
  input.addEventListener("change", () => {
    if (input.checked) void setOutputMode(input.value);
  });
}

backgroundToggle.addEventListener("change", () => {
  if (!state) return;
  const { effect } = backgroundState(state.video_effects);
  void setBackground(backgroundToggle.checked, effect);
});

async function setCameraPower(enabled) {
  if (cameraPowerPending || !state?.camera.available) return;
  cameraPowerPending = true;
  cameraPowerError = null;
  cameraPowerDraft = enabled;
  previewDrag?.cancel(false);
  heldDirections = [];
  updatePanTiltKeepalive();
  render(state);
  try {
    const response = await fetch("/api/v1/camera/power", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ enabled }),
    });
    if (!response.ok) {
      const payload = await response.json().catch(() => ({}));
      throw new Error(payload.error || `Camera power change failed (${response.status})`);
    }
    state.camera.powered_on = enabled;
    state.camera.power_error = null;
    state.pipeline.enabled = enabled;
    state.pipeline.running = enabled;
  } catch (error) {
    cameraPowerError = error instanceof Error ? error.message : String(error);
  } finally {
    cameraPowerPending = false;
    cameraPowerDraft = null;
    if (state) render(state);
  }
}

cameraPowerToggle.addEventListener("click", () => {
  if (state) void setCameraPower(state.camera.powered_on !== true);
});

for (const input of backgroundEffectInputs) {
  input.addEventListener("change", () => {
    if (!input.checked || !state) return;
    const { enabled } = backgroundState(state.video_effects);
    void setBackground(enabled, input.value);
  });
}
preview.addEventListener("load", () => {
  preview.classList.remove("disconnected");
  drawSkeletons();
});
preview.addEventListener("error", retryPreview);
new ResizeObserver(() => drawSkeletons()).observe($(".preview-stage"));

document.querySelectorAll("[data-action]").forEach((button) => {
  button.addEventListener("click", async () => {
    clearHeldDirections();
    await fetch(`/api/v1/camera/actions/${button.dataset.action}`, { method: "POST" });
  });
});

function clearHeldDirections() {
  previewDrag?.cancel(false);
  const previous = activeDirection();
  heldDirections = [];
  updatePanTiltKeepalive();
  if (state) render(state);
  if (previous != null) void syncPanTiltMotion();
}

function holdDirection(direction) {
  if (videoTransformPending || faceTrackingPending || handsTrackingPending || trackingPending
    || !cameraControlsAvailable(state?.camera || {})) return;
  const previous = activeDirection();
  heldDirections = heldDirections.filter((held) => held !== direction);
  heldDirections.push(direction);
  updatePanTiltKeepalive();
  if (state) render(state);
  if (previous !== direction) void syncPanTiltMotion();
}

function releaseDirection(direction) {
  const previous = activeDirection();
  heldDirections = heldDirections.filter((held) => held !== direction);
  updatePanTiltKeepalive();
  if (state) render(state);
  if (previous !== activeDirection()) void syncPanTiltMotion();
}

function updatePanTiltKeepalive() {
  if (activeDirection() != null && panTiltKeepaliveTimer == null) {
    panTiltKeepaliveTimer = setInterval(() => void syncPanTiltMotion(), panTiltKeepaliveIntervalMs);
  } else if (activeDirection() == null && panTiltKeepaliveTimer != null) {
    clearInterval(panTiltKeepaliveTimer);
    panTiltKeepaliveTimer = null;
  }
}

async function syncPanTiltMotion() {
  if (!state || !cameraControlsAvailable(state.camera)) return;
  if (panTiltPending) {
    panTiltSyncQueued = true;
    return;
  }

  const direction = activeDirection();
  panTiltPending = true;
  panTiltSyncQueued = false;
  cameraControlError = null;
  if (state) render(state);
  let failed = false;
  try {
    const response = await fetch(`/api/v1/camera/nudge/${sourceDirection(direction) ?? "stop"}`, {
      method: "POST",
      keepalive: direction == null,
    });
    if (!response.ok) {
      const payload = await response.json().catch(() => ({}));
      throw new Error(payload.error || `Camera command failed (${response.status})`);
    }
  } catch (error) {
    failed = true;
    cameraControlError = error instanceof Error ? error.message : String(error);
    previewDrag?.cancel(false);
    heldDirections = [];
    updatePanTiltKeepalive();
  } finally {
    panTiltPending = false;
    const shouldResync = !failed && (panTiltSyncQueued || activeDirection() !== direction);
    panTiltSyncQueued = false;
    if (state) render(state);
    if (shouldResync) void syncPanTiltMotion();
  }
}

previewDrag = installPreviewDrag(preview, {
  canControl: canDragPreview,
  onDirection(direction) {
    if (activeDirection() === direction) return;
    heldDirections = [];
    holdDirection(direction);
  },
  onStop() { releaseDirection(activeDirection()); },
});

for (const button of panTiltButtons) {
  const direction = button.dataset.panTilt;
  button.addEventListener("pointerdown", (event) => {
    if (button.disabled) return;
    event.preventDefault();
    previewDrag.cancel();
    button.setPointerCapture(event.pointerId);
    holdDirection(direction);
  });
  for (const eventName of ["pointerup", "pointercancel", "lostpointercapture"]) {
    button.addEventListener(eventName, () => releaseDirection(direction));
  }
  button.addEventListener("click", (event) => {
    event.preventDefault();
    if (event.detail !== 0 || button.disabled) return;
    holdDirection(direction);
    releaseDirection(direction);
  });
}

const arrowDirections = {
  ArrowLeft: "left",
  ArrowRight: "right",
  ArrowUp: "up",
  ArrowDown: "down",
};

function blocksArrowControl(target) {
  return target instanceof HTMLInputElement
    || target instanceof HTMLTextAreaElement
    || target instanceof HTMLSelectElement
    || target?.isContentEditable;
}

document.addEventListener("keydown", (event) => {
  const direction = arrowDirections[event.key];
  if (!direction || event.altKey || event.ctrlKey || event.metaKey || blocksArrowControl(event.target)
    || faceTrackingPending || handsTrackingPending || trackingPending
    || !cameraControlsAvailable(state?.camera || {})) return;
  event.preventDefault();
  previewDrag?.cancel();
  if (!heldDirections.includes(direction)) holdDirection(direction);
});

document.addEventListener("keyup", (event) => {
  const direction = arrowDirections[event.key];
  if (!direction) return;
  event.preventDefault();
  releaseDirection(direction);
});

window.addEventListener("blur", clearHeldDirections);
window.addEventListener("pagehide", clearHeldDirections);
document.addEventListener("visibilitychange", () => {
  if (document.hidden) clearHeldDirections();
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

document.querySelectorAll("[data-camera-feature]").forEach((button) => {
  button.addEventListener("click", async () => {
    const feature = button.dataset.cameraFeature;
    const enabled = button.dataset.cameraEnabled === "true";
    if ((feature === "hdr" && hdrPending) || (feature === "tracking" && trackingPending)) return;
    if (feature === "hdr") hdrPending = true;
    else {
      trackingPending = true;
      clearHeldDirections();
    }
    cameraControlError = null;
    if (state) render(state);
    try {
      const response = await fetch(`/api/v1/camera/${feature}`, {
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
      if (feature === "hdr") hdrPending = false;
      else trackingPending = false;
      if (state) render(state);
    }
  });
});

function scheduleImageSetting(control) {
  if (imageSettingTimers.has(control) || pendingImageSettings.has(control)) return;
  const timer = setTimeout(() => {
    imageSettingTimers.delete(control);
    void sendImageSetting(control);
  }, imageSettingUpdateIntervalMs);
  imageSettingTimers.set(control, timer);
}

async function sendImageSetting(control) {
  if (pendingImageSettings.has(control) || !imageSettingDrafts.has(control)) return;
  const value = imageSettingDrafts.get(control);
  pendingImageSettings.add(control);
  imageSettingError = null;
  if (state) render(state);
  try {
    const response = await fetch(`/api/v1/camera/image-settings/${control}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ value }),
    });
    const payload = await response.json().catch(() => ({}));
    if (!response.ok) {
      throw new Error(payload.error || `Image setting failed (${response.status})`);
    }
    const readback = imageSettingState(state?.camera || {}, control);
    if (readback && Number.isInteger(payload.value)) {
      readback.value = payload.value;
      readback.sample_at_ms = Date.now();
      readback.error = null;
    }
  } catch (error) {
    const definition = imageSettingDefinitions.find((item) => item.control === control);
    const message = error instanceof Error ? error.message : String(error);
    imageSettingError = `${definition?.label || control}: ${message}`;
  } finally {
    pendingImageSettings.delete(control);
    if (imageSettingDrafts.get(control) === value) imageSettingDrafts.delete(control);
    else scheduleImageSetting(control);
    if (state) render(state);
  }
}

function queueImageSetting(control, value, realtime = false) {
  if (!Number.isInteger(value)) return;
  imageSettingDrafts.set(control, value);
  imageSettingError = null;
  if (state) render(state);
  if (realtime) scheduleImageSetting(control);
  else void sendImageSetting(control);
}

imageSettingsGroups.addEventListener("input", (event) => {
  const input = event.target.closest('input[type="range"][data-image-setting-input]');
  if (!input) return;
  queueImageSetting(input.dataset.imageSettingInput, Number(input.value), true);
});

imageSettingsGroups.addEventListener("change", (event) => {
  const input = event.target.closest("select[data-image-setting-input]");
  if (!input) return;
  queueImageSetting(input.dataset.imageSettingInput, Number(input.value));
});

imageSettingsGroups.addEventListener("click", (event) => {
  const button = event.target.closest("button[data-image-setting-button]");
  if (!button || button.disabled) return;
  queueImageSetting(
    button.dataset.imageSettingButton,
    Number(button.dataset.imageSettingValueOption),
  );
});

faceTrackingToggle.addEventListener("click", async () => {
  if (faceTrackingPending || handsTrackingPending || trackingPending || !state
    || !cameraControlsAvailable(state.camera)) return;
  const enabled = state.camera.face_tracking?.enabled !== true;
  faceTrackingPending = true;
  clearHeldDirections();
  cameraControlError = null;
  render(state);
  try {
    const response = await fetch("/api/v1/camera/face-tracking", {
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
    faceTrackingPending = false;
    if (state) render(state);
  }
});

handsTrackingToggle.addEventListener("click", async () => {
  if (handsTrackingPending || faceTrackingPending || trackingPending || !state
    || !cameraControlsAvailable(state.camera)) return;
  const enabled = state.camera.hands_tracking?.enabled !== true;
  handsTrackingPending = true;
  clearHeldDirections();
  cameraControlError = null;
  render(state);
  try {
    const response = await fetch("/api/v1/camera/hands-tracking", {
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
    handsTrackingPending = false;
    if (state) render(state);
  }
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

autoZoomToggle.addEventListener("click", async () => {
  if (autoZoomPending || !state || !cameraControlsAvailable(state.camera)
    || !state.camera.face_tracking?.enabled) return;
  const enabled = state.camera.face_tracking.auto_zoom?.enabled !== true;
  autoZoomPending = true;
  autoZoomDraft = enabled;
  cameraControlError = null;
  render(state);
  try {
    const response = await fetch("/api/v1/camera/auto-zoom", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ enabled }),
    });
    if (!response.ok) {
      const payload = await response.json().catch(() => ({}));
      throw new Error(payload.error || `Auto zoom change failed (${response.status})`);
    }
    state.camera.face_tracking.auto_zoom = { enabled };
  } catch (error) {
    cameraControlError = error instanceof Error ? error.message : String(error);
  } finally {
    autoZoomPending = false;
    autoZoomDraft = null;
    if (state) render(state);
  }
});

let videoTransformPending = false;

function videoTransform() {
  return state?.video_effects?.transform || { rotation: 0, mirror: false };
}

function transformLandmark(point, width, height) {
  const { rotation, mirror } = videoTransform();
  let { x, y } = point;
  if (rotation === 90) [x, y] = [1 - y, x];
  if (rotation === 180) [x, y] = [1 - x, 1 - y];
  if (rotation === 270) [x, y] = [y, 1 - x];
  if (rotation === 90 || rotation === 270) {
    const scale = Math.min(width / height, height / width);
    const rw = Math.round(height * scale);
    const rh = Math.round(width * scale);
    x = (Math.floor((width - rw) / 2) + x * rw) / width;
    y = (Math.floor((height - rh) / 2) + y * rh) / height;
  }
  return { x: mirror ? 1 - x : x, y };
}

function setPreviewMirror(enabled) {
  clearHeldDirections();
  previewMirrorEnabled = enabled;
  localStorage.setItem("tarsier.previewMirror", String(enabled));
  $("#preview-mirror").setAttribute("aria-pressed", String(enabled));
  $(".preview-stage").classList.toggle("mirrored", enabled);
}

function sourceDirection(direction) {
  const { rotation, mirror } = videoTransform();
  return sourcePanTiltDirection(direction, rotation, mirror !== previewMirrorEnabled);
}

function renderVideoTransform() {
  const transform = videoTransform();
  for (const button of document.querySelectorAll("[data-video-rotation]")) {
    button.setAttribute("aria-pressed", String(Number(button.dataset.videoRotation) === transform.rotation));
    button.disabled = !socketConnected || videoTransformPending || cameraOnly4k();
  }
  $("#video-mirror").checked = transform.mirror;
  $("#video-mirror").disabled = !socketConnected || videoTransformPending || cameraOnly4k();
}

async function setVideoTransform(transform) {
  if (videoTransformPending) return;
  videoTransformPending = true;
  clearHeldDirections();
  renderVideoTransform();
  const errorElement = $("#video-transform-error");
  errorElement.hidden = true;
  try {
    const response = await fetch("/api/v1/video/transform", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(transform),
    });
    const payload = await response.json();
    if (!response.ok) throw new Error(payload.error || "Could not update video orientation");
    if (state) state.video_effects.transform = payload;
  } catch (error) {
    errorElement.textContent = error.message;
    errorElement.hidden = false;
  } finally {
    videoTransformPending = false;
    if (state) render(state);
  }
}

for (const button of document.querySelectorAll("[data-video-rotation]")) {
  button.addEventListener("click", () => void setVideoTransform({ ...videoTransform(), rotation: Number(button.dataset.videoRotation) }));
}
$("#video-mirror").addEventListener("change", (event) => void setVideoTransform({ ...videoTransform(), mirror: event.target.checked }));

for (const [button, icon] of [[faceTrackingToggle, ScanFace], [handsTrackingToggle, Hand], [autoZoomToggle, ZoomIn]]) {
  button.append(createElement(icon, { width: 18, height: 18, "aria-hidden": "true", focusable: "false" }));
}

cameraPowerToggle.append(createElement(Power, { width: 18, height: 18, "aria-hidden": "true", focusable: "false" }));

for (const [button, icon] of [[$("#preview-mirror"), FlipHorizontal2], [skeletonToggle, Bone]]) {
  button.append(createElement(icon, { width: 15, height: 15, "aria-hidden": "true", focusable: "false" }));
}
$("#preview-mirror").addEventListener("click", () => setPreviewMirror(!previewMirrorEnabled));
setPreviewMirror(previewMirrorEnabled);

buildImageSettingsUi();
setInterval(() => state && render(state), 500);
setInterval(checkDaemonInstance, 2000);
setSkeletonEnabled(skeletonEnabled);
loadRecentEvents().catch(console.error);
loadPresets().catch(console.error);
connect();
checkDaemonInstance();

let photoStatusKey = null;
function renderSavedPhoto(photo) {
  if (!photo?.url || photoStatusKey === photo.url) return;
  const link = document.createElement("a");
  link.href = photo.url;
  link.textContent = photo.path;
  link.target = "_blank";
  link.rel = "noopener";
  photoStatus.classList.remove("error");
  photoStatus.replaceChildren("Saved: ", link, " ", ...localMediaOpenControls(photo.url, "photo"));
  photoStatusKey = photo.url;
}

takePhoto.addEventListener("click", async () => {
  if (photoPending) return;
  photoPending = true;
  takePhoto.disabled = true;
  photoStatus.classList.remove("error");
  photoStatus.textContent = "Saving photo…";
  try {
    const response = await fetch("/api/v1/camera/photos", { method: "POST" });
    const payload = await response.json();
    if (!response.ok) throw new Error(payload.error || "Could not save photo");
    renderSavedPhoto(payload);
  } catch (error) {
    photoStatus.classList.add("error");
    photoStatus.textContent = error instanceof Error ? error.message : String(error);
  } finally {
    photoPending = false;
    takePhoto.disabled = !socketConnected || !state?.pipeline.running;
  }
});

document.addEventListener("keydown", (event) => {
  if (event.code !== "Space" || event.altKey || event.metaKey
    || event.shiftKey || event.isComposing || event.defaultPrevented
    || blocksArrowControl(event.target)
    || (!event.ctrlKey && event.target?.closest?.("button, a, summary, [role='button'], [role='slider']"))
    || document.querySelector("dialog[open]")) return;
  event.preventDefault();
  const button = event.ctrlKey ? recordVideo : takePhoto;
  if (!event.repeat && !button.disabled) button.click();
});

const resolutionPicker = $("#resolution-picker");
function positionResolutionPicker() {
  if (!resolutionPicker.matches(":popover-open")) return;
  const link = $("#video-resolution").getBoundingClientRect();
  const popup = resolutionPicker.getBoundingClientRect();
  const margin = 8;
  const gap = 4;
  const viewportWidth = document.documentElement.clientWidth;
  const viewportHeight = document.documentElement.clientHeight;
  const left = Math.max(margin, Math.min(link.left, viewportWidth - popup.width - margin));
  const below = link.bottom + gap;
  const top = below + popup.height <= viewportHeight - margin
    ? below
    : Math.max(margin, link.top - gap - popup.height);
  resolutionPicker.style.left = `${left}px`;
  resolutionPicker.style.top = `${top}px`;
}
$("#video-resolution").addEventListener("click", (event) => {
  event.preventDefault();
  resolutionPicker.togglePopover();
  positionResolutionPicker();
});
resolutionPicker.addEventListener("toggle", () => {
  $("#video-resolution").setAttribute("aria-expanded", String(resolutionPicker.matches(":popover-open")));
  positionResolutionPicker();
});
window.addEventListener("resize", positionResolutionPicker);
document.addEventListener("scroll", positionResolutionPicker, true);
new ResizeObserver(positionResolutionPicker).observe(resolutionPicker);
for (const button of document.querySelectorAll("[data-resolution]")) {
  button.addEventListener("click", async () => {
    if (resolutionPending) return;
    const [width, height] = button.dataset.resolution.split("x").map(Number);
    if (state?.pipeline.width === width && state?.pipeline.height === height) {
      resolutionPicker.hidePopover();
      return;
    }
    resolutionPending = true;
    for (const option of document.querySelectorAll("[data-resolution]")) option.disabled = true;
    $("#resolution-status").textContent = "Switching resolution… Video will reconnect automatically.";
    try {
      const response = await fetch("/api/v1/video/resolution", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ width, height }),
      });
      if (!response.ok) throw new Error((await response.json()).error || "Could not change resolution");
    } catch (error) {
      resolutionPending = false;
      $("#resolution-status").textContent = error.message;
    }
  });
}

function renderRecording() {
  const active = recordingState?.active === true;
  recordVideo.disabled = recordingPending || !socketConnected || !recordingState || (!active && !state?.pipeline.running);
  recordVideo.classList.toggle("recording", active);
  recordVideo.setAttribute("aria-pressed", String(active));
  const seconds = Math.max(0, Math.floor((Date.now() - (recordingState?.started_at_ms || Date.now())) / 1000));
  const elapsed = `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
  recordVideo.textContent = recordingPending ? (active ? "Saving video…" : "Starting…") : active ? `Stop recording · ${elapsed}` : "Record video";
  if (active) {
    recordingStatus.hidden = false;
    recordingStatus.textContent = "Recording video without audio · Includes current video effects.";
    recordingStatusKey = null;
  } else if (recordingState?.error) {
    recordingStatus.hidden = false;
    recordingStatus.textContent = recordingState.error;
    recordingStatusKey = null;
  } else if (recordingState?.url && recordingStatusKey !== recordingState.url) {
    const link = document.createElement("a");
    link.href = recordingState.url;
    link.textContent = recordingState.path;
    link.target = "_blank";
    link.rel = "noopener";
    recordingStatus.replaceChildren("Saved video: ", link, " ", ...localMediaOpenControls(recordingState.url, "video"));
    recordingStatus.hidden = false;
    recordingStatusKey = recordingState.url;
  }
}

recordVideo.addEventListener("click", async () => {
  if (recordingPending || !recordingState) return;
  recordingPending = true;
  renderRecording();
  try {
    const response = await fetch(`/api/v1/video/recording${recordingState.active ? "/stop" : ""}`, {
      method: "POST", headers: { "Content-Type": "application/json" }, body: "{}",
    });
    const payload = await response.json();
    if (!response.ok) throw new Error(payload.error || "Could not change recording state");
    recordingState = payload;
  } catch (error) {
    recordingState = { ...recordingState, error: error.message };
  } finally {
    recordingPending = false;
    renderRecording();
  }
});


function localMediaOpenControls(url, mediaName) {
  const fileLink = document.createElement("button");
  fileLink.type = "button";
  fileLink.className = "photo-file-link";
  fileLink.append(createElement(FolderOpen, { width: 18, height: 18, "aria-hidden": "true", focusable: "false" }));
  fileLink.title = `Show ${mediaName} in folder`;
  fileLink.setAttribute("aria-label", `Show ${mediaName} in folder`);
  const openStatus = document.createElement("span");
  fileLink.addEventListener("click", async () => {
    fileLink.disabled = true;
    openStatus.textContent = " Opening…";
    try {
      const response = await fetch(`${url}/open`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "{}",
      });
      if (!response.ok) throw new Error((await response.json()).error || `Could not show ${mediaName} in folder`);
      openStatus.textContent = "";
    } catch (error) {
      openStatus.textContent = ` ${error.message}`;
    } finally {
      fileLink.disabled = false;
    }
  });
  return [fileLink, openStatus];
}
