export function dragDirection(dx, dy) {
  if (Math.hypot(dx, dy) < 3) return null;
  const horizontal = Math.abs(dx) >= Math.abs(dy) * 0.5 ? (dx > 0 ? "right" : "left") : "";
  const vertical = Math.abs(dy) >= Math.abs(dx) * 0.5 ? (dy > 0 ? "down" : "up") : "";
  return [vertical, horizontal].filter(Boolean).join("-");
}

export function sourcePanTiltDirection(direction, rotation, mirrored) {
  if (direction == null) return null;
  const compass = ["right", "down-right", "down", "down-left", "left", "up-left", "up", "up-right"];
  if (mirrored) direction = direction.includes("left") ? direction.replace("left", "right") : direction.replace("right", "left");
  return compass[(compass.indexOf(direction) - rotation / 45 + 8) % 8];
}

export function installPreviewDrag(element, { canControl, onDirection, onStop, getZoom, onZoom, schedule = setTimeout, unschedule = clearTimeout }) {
  let pointerId = null;
  let x = 0;
  let y = 0;
  let idleTimer = null;
  const touches = new Map();
  let pinching = false;
  let pinchDistance = null;
  const distance = () => {
    const [a, b] = [...touches.values()];
    return a && b ? Math.hypot(a.x - b.x, a.y - b.y) : null;
  };
  const zoom = value => {
    if (Number.isFinite(value)) onZoom(Math.max(1, Math.min(4, value)));
  };
  const cancelDrag = (notify = true, release = true) => {
    if (pointerId == null) return;
    const id = pointerId;
    pointerId = null;
    unschedule(idleTimer);
    idleTimer = null;
    element.classList.remove("pan-tilt-dragging");
    if (release && element.hasPointerCapture(id)) element.releasePointerCapture(id);
    if (notify) onStop();
  };
  const cancel = (notify = true) => {
    const ids = [...touches.keys()];
    touches.clear();
    pinching = false;
    pinchDistance = null;
    cancelDrag(notify);
    for (const id of ids) {
      if (element.hasPointerCapture(id)) element.releasePointerCapture(id);
    }
  };
  element.addEventListener("wheel", event => {
    if (!onZoom || !canControl() || !Number.isFinite(event.deltaY) || event.deltaY === 0) return;
    event.preventDefault();
    const unit = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? (element.clientHeight || 800) : 1;
    const delta = Math.max(-500, Math.min(500, event.deltaY * unit));
    zoom(getZoom() * Math.exp(-delta * 0.0015));
  }, { passive: false });
  element.addEventListener("dragstart", event => event.preventDefault());
  element.addEventListener("pointerdown", event => {
    if (event.pointerType === "touch" && onZoom && canControl()) {
      event.preventDefault();
      touches.set(event.pointerId, { x: event.clientX, y: event.clientY });
      element.setPointerCapture(event.pointerId);
      if (touches.size >= 2) {
        pinching = true;
        pinchDistance = distance();
        cancelDrag(true, false);
      }
      if (pinching) return;
    }
    if (event.button !== 0 || !event.isPrimary || !canControl() || pointerId != null) return;
    event.preventDefault();
    pointerId = event.pointerId;
    x = event.clientX;
    y = event.clientY;
    element.setPointerCapture(pointerId);
    element.classList.add("pan-tilt-dragging");
  });
  element.addEventListener("pointermove", event => {
    if (touches.has(event.pointerId)) {
      if (!canControl()) { cancel(); return; }
      touches.set(event.pointerId, { x: event.clientX, y: event.clientY });
      if (pinching) {
        event.preventDefault();
        const nextDistance = distance();
        if (nextDistance > 0 && pinchDistance > 0) zoom(getZoom() * nextDistance / pinchDistance);
        pinchDistance = nextDistance;
        return;
      }
    }
    if (event.pointerId !== pointerId) return;
    if (!canControl() || (event.buttons & 1) === 0) { cancel(); return; }
    // Move the camera opposite the gesture so the image follows the pointer.
    const direction = dragDirection(x - event.clientX, y - event.clientY);
    if (!direction) return;
    event.preventDefault();
    x = event.clientX;
    y = event.clientY;
    onDirection(direction);
    unschedule(idleTimer);
    idleTimer = schedule(() => { idleTimer = null; onStop(); }, 150);
  });
  for (const name of ["pointerup", "pointercancel", "lostpointercapture"]) {
    element.addEventListener(name, event => {
      if (touches.has(event.pointerId)) {
        if (name !== "pointerup") { cancel(); return; }
        touches.delete(event.pointerId);
        pinchDistance = distance();
        if (touches.size === 0) pinching = false;
        // Keep the remaining finger out of pan/tilt until all fingers lift.
        if (element.hasPointerCapture(event.pointerId)) element.releasePointerCapture(event.pointerId);
      }
      if (event.pointerId === pointerId) cancel();
    });
  }
  return { cancel };
}
