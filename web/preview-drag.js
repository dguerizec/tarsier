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

export function installPreviewDrag(element, { canControl, onDirection, onStop, schedule = setTimeout, unschedule = clearTimeout }) {
  let pointerId = null;
  let x = 0;
  let y = 0;
  let idleTimer = null;
  const cancel = (notify = true) => {
    if (pointerId == null) return;
    const id = pointerId;
    pointerId = null;
    unschedule(idleTimer);
    idleTimer = null;
    element.classList.remove("pan-tilt-dragging");
    if (element.hasPointerCapture(id)) element.releasePointerCapture(id);
    if (notify) onStop();
  };
  element.addEventListener("dragstart", event => event.preventDefault());
  element.addEventListener("pointerdown", event => {
    if (event.button !== 0 || !event.isPrimary || !canControl() || pointerId != null) return;
    event.preventDefault();
    pointerId = event.pointerId;
    x = event.clientX;
    y = event.clientY;
    element.setPointerCapture(pointerId);
    element.classList.add("pan-tilt-dragging");
  });
  element.addEventListener("pointermove", event => {
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
    element.addEventListener(name, event => { if (event.pointerId === pointerId) cancel(); });
  }
  return { cancel };
}
