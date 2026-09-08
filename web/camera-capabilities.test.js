import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

test("camera sections follow capabilities across source changes and outages", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const render = source.slice(source.indexOf("function renderCameraCapabilities("),
    source.indexOf("const daemonMonitor ="));
  const sections = ["power", "pan_tilt", "zoom", "tracking,hdr", "image_settings"]
    .map(key => ({ dataset: { cameraCapability: key }, hidden: false }));
  const heading = {};
  const preview = {};
  const context = vm.createContext({
    document: { querySelectorAll: () => sections },
    $: () => heading, preview,
  });
  vm.runInContext(render, context);
  const show = camera => context.renderCameraCapabilities(camera);
  show({ name: "Gimbal", capabilities: { power: true, pan_tilt: true, zoom: true, hdr: true, image_settings: true } });
  assert.ok(sections.every(section => !section.hidden));
  show({ name: "Laptop Camera", available: true, capabilities: { image_settings: true } });
  assert.deepEqual(sections.map(section => section.hidden), [true, true, true, true, false]);
  assert.equal(heading.textContent, "Laptop Camera");
  assert.equal(preview.title, "Camera preview");
  // Temporary unavailability does not erase a known capability.
  show({ name: "Laptop Camera", available: false, capabilities: { image_settings: true } });
  assert.equal(sections[4].hidden, false);
  show({ capabilities: {} });
  assert.ok(sections.every(section => section.hidden));
});
