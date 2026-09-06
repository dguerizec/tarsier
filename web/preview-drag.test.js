import test from "node:test";
import assert from "node:assert/strict";
import { dragDirection, sourcePanTiltDirection, installPreviewDrag } from "./preview-drag.js";

function harness() {
  const listeners = new Map(), classes = new Set(), timers = new Map();
  const directions = [];
  let captured = null, timerId = 0, stops = 0, enabled = true;
  const element = {
    classList: { add: value => classes.add(value), remove: value => classes.delete(value) },
    addEventListener(name, fn) { listeners.set(name, fn); },
    setPointerCapture(id) { captured = id; },
    hasPointerCapture(id) { return captured === id; },
    releasePointerCapture() { captured = null; },
  };
  const drag = installPreviewDrag(element, {
    canControl: () => enabled,
    onDirection: direction => directions.push(direction),
    onStop: () => stops++,
    schedule(fn) { timers.set(++timerId, fn); return timerId; },
    unschedule(id) { timers.delete(id); },
  });
  return {
    drag, classes, directions,
    get stops() { return stops; },
    get captured() { return captured; },
    set enabled(value) { enabled = value; },
    emit(name, values = {}) {
      listeners.get(name)({ pointerId: 1, isPrimary: true, button: 0, buttons: 1, clientX: 100, clientY: 100, preventDefault() {}, ...values });
    },
    idle() { for (const fn of timers.values()) fn(); timers.clear(); },
  };
}

test("drag directions include diagonals and ignore click jitter", () => {
  assert.equal(dragDirection(1, 1), null);
  for (const [x, y, direction] of [[10, 0, "right"], [-10, 0, "left"], [0, -10, "up"], [0, 10, "down"], [10, -10, "up-right"], [-10, 10, "down-left"]]) {
    assert.equal(dragDirection(x, y), direction);
  }
});

test("screen directions respect mirror and rotation", () => {
  assert.equal(sourcePanTiltDirection("right", 0, true), "left");
  assert.equal(sourcePanTiltDirection("up-right", 0, true), "up-left");
  assert.equal(sourcePanTiltDirection("right", 90, false), "up");
  assert.equal(sourcePanTiltDirection("up-right", 90, true), "down-left");
  for (const direction of ["right", "down-right", "down", "down-left", "left", "up-left", "up", "up-right"]) {
    assert.equal(sourcePanTiltDirection(sourcePanTiltDirection(direction, 90, false), 270, false), direction);
    assert.equal(sourcePanTiltDirection(sourcePanTiltDirection(direction, 0, true), 0, true), direction);
  }
});

test("click captures the pointer without moving; movement stops on idle and resumes", () => {
  const h = harness();
  h.emit("pointerdown");
  assert.equal(h.captured, 1);
  assert(h.classes.has("pan-tilt-dragging"));
  assert.deepEqual(h.directions, []);
  h.emit("pointermove", { clientX: 101 });
  assert.deepEqual(h.directions, []);
  h.emit("pointermove", { clientX: 110, clientY: 90 });
  assert.deepEqual(h.directions, ["down-left"]);
  h.idle();
  assert.equal(h.stops, 1);
  assert.equal(h.captured, 1);
  h.emit("pointermove", { clientX: 100, clientY: 90 });
  assert.deepEqual(h.directions, ["down-left", "right"]);
});

test("dragging pulls the image with the pointer in every direction", () => {
  for (const [dx, dy, expected] of [[10, 0, "left"], [-10, 0, "right"], [0, 10, "up"], [0, -10, "down"], [10, 10, "up-left"], [-10, -10, "down-right"]]) {
    const h = harness();
    h.emit("pointerdown");
    h.emit("pointermove", { clientX: 100 + dx, clientY: 100 + dy });
    assert.deepEqual(h.directions, [expected]);
  }
});

for (const reason of ["pointerup", "pointercancel", "lostpointercapture"]) {
  test(`${reason} stops and clears the drag`, () => {
    const h = harness(); h.emit("pointerdown"); h.emit("pointermove", { clientX: 110 }); h.emit(reason); h.idle();
    assert.equal(h.stops, 1);
    assert.equal(h.captured, null);
    assert(!h.classes.has("pan-tilt-dragging"));
  });
}

test("unavailable controls, other buttons and secondary pointers do not start", () => {
  const h = harness();
  h.enabled = false; h.emit("pointerdown"); h.enabled = true;
  h.emit("pointerdown", { button: 2 }); h.emit("pointerdown", { isPrimary: false });
  assert.equal(h.captured, null);
  h.emit("pointerdown"); h.emit("pointermove", { pointerId: 2, clientX: 200 });
  assert.deepEqual(h.directions, []);
  h.enabled = false; h.emit("pointermove", { clientX: 200 });
  assert.equal(h.stops, 1);
});

test("external cancellation and missing pressed button clear all pending movement", () => {
  const h = harness(); h.emit("pointerdown"); h.emit("pointermove", { clientX: 110 }); h.drag.cancel(false); h.idle();
  assert.equal(h.stops, 0);
  assert.equal(h.captured, null);
  h.emit("pointerdown"); h.emit("pointermove", { buttons: 0 });
  assert.equal(h.stops, 1);
});
