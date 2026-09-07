# OBSBOT Tiny 2 interoperability notes

This document records the minimum protocol facts used by Tarsier's clean-room
camera adapter. It is an interoperability record, not vendor SDK source or a
claim that the undocumented protocol is stable across firmware revisions.

## Tested device

- product: OBSBOT Tiny 2
- USB ID: `3564:fef8`
- tested firmware: `6.0.10.4`
- tested operating system: Ubuntu 26.04
- validation date: 2026-09-05

The adapter opens the configured V4L2 device and sends UVC extension-unit
queries through unit `2`. All extension-unit operations are serialized by one
dedicated thread. A bounded command queue prevents concurrent callers from
interleaving packets, and a configurable minimum interval (20 ms by default)
is enforced between I/O operations.

## Vendor mailbox frame

The vendor mailbox uses selector `2` and fixed 60-byte buffers. Multi-byte
integers are little-endian.

| Offset | Size | Meaning |
| --- | ---: | --- |
| `0` | 1 | Magic `0xaa` |
| `1` | 1 | Flags (`0x01` query, `0x25` command) |
| `2` | 2 | Sequence number |
| `4` | 2 | Header length, currently `12` |
| `6` | 2 | Header CRC |
| `8` | 1 | Sender, `0x0a` |
| `9` | 1 | Receiver |
| `10` | 2 | Command |
| `12` | 2 | Payload length |
| `14` | 2 | Payload CRC |
| `16` | up to 44 | Payload |

Both CRC fields use CRC-16/USB: initial value `0xffff`, reflected polynomial
`0xa001`, and final XOR `0xffff`. The corresponding CRC field is zero while its
segment is calculated.

Tarsier validates the magic, header CRC, payload length, payload CRC, response
sequence, and response command before accepting a reply. A missing or invalid
reply becomes an unavailable/error state; it is never interpreted as a zero
angle and never triggers an implicit device reset.

## Implemented commands

| Operation | Selector | Receiver | Command | Payload |
| --- | ---: | ---: | ---: | --- |
| Wake | 2 | `0x02` | `0xa0c2` | four zero bytes |
| Sleep | 2 | `0x02` | `0xa0c2` | `01 00 00 00` |
| Read AI gimbal state | 2 | `0x04` | `0x6604` | empty query |
| Read AI quick status | 2 | `0x04` | `0x0104` | empty query |
| Recenter | 2 | `0x03` | `0x00c3` | six zero bytes |
| Move absolute | 2 | `0x04` | `0x6444` | float32 roll, pitch, yaw |
| Target-selection gesture | 2 | `0x04` | `0x30c4` | one byte: `1` enabled, `0` disabled |
| Zoom gesture | 2 | `0x04` | `0x3144` | one byte: `1` enabled, `0` disabled |
| Dynamic-zoom gesture | 2 | `0x04` | `0x3344` | one byte: `1` enabled, `0` disabled |
| Tracking | 6 | n/a | n/a | `16 02 02` enabled, `16 02 00` disabled, then zeros |
| HDR/WDR | 6 | n/a | n/a | `01 01 01` enabled, `01 01 00` disabled, then zeros |

`AI_GET_GIM_STATE` matches the Tiny 2 UVC path used by libdev's
`aiGetGimbalStateR()`. Its response starts with nine signed little-endian
16-bit values in this order: Euler roll/pitch/yaw, motor roll/pitch/yaw, then
roll/pitch/yaw angular velocity. Every value uses a 0.1 scale. The primary
`yaw_degrees`, `pitch_degrees`, and `roll_degrees` API fields expose the motor
coordinates; Euler coordinates and angular velocities are also retained.

`AI_GET_QUICK_STATUS` matches libdev's `aiGetAiStatusR()` path for a Tiny 2.
The target-selection, zoom, and dynamic-zoom gesture enables are bytes 3, 4,
and 5 of its response payload. Tarsier treats non-zero values as enabled.

Absolute movement targets are additionally checked against configured yaw and
pitch limits and a fixed +/-45 degree roll limit before a frame is queued.
Tracking uses a separate raw selector-6 payload rather than the framed
selector-2 mailbox.

The three built-in gesture controls are independent of Tarsier's MediaPipe
gesture recognition. They use the Tiny 2 commands documented by the vendor
SDK's model-specific compatibility API rather than the newer unified gesture
parameter command, which that SDK categorizes for Tail 2 and later products.
Tarsier records a gesture setting immediately after the UVC write succeeds,
then replaces that accepted value with measured camera state on the next quick
status read. Quick status runs at one fifth of the pose rate, with a minimum
five-second interval, to limit selector-2 traffic.

## Power sequencing

The public power control coordinates capture and extension-unit traffic rather
than sending an isolated vendor command. To switch off, Tarsier first stops
face-tracking movement, releases the active GStreamer pipeline and its physical
capture descriptor, and only then sends the sleep frame. Telemetry polling is
suspended after that write succeeds, so it cannot accidentally wake the camera.
The perception supervisor intentionally stops its worker while frames are
paused. The daemon, API, and embedded UI remain active. To switch on, Tarsier
sends the wake frame, waits 100 ms for the device, rebuilds the managed video
pipeline, and starts a fresh perception worker. A failed sleep write restarts
capture as a rollback.

Other camera controls are rejected while the adapter records the device as
powered off. Power state in the public API therefore represents the last
successful daemon-owned transition, not an inferred sensor or USB state.

## Selector-6 camera status

The fixed 60-byte selector-6 `GET_CUR` status block exposes the current AI mode
at offset `0x18` and its sub-mode at `0x1c`. The tuple `(0, 0)` means tracking
is disabled. The known Tiny 2 tracking modes
`(1, 0)`, `(3, 0)`, `(4, 0)`, `(5, 0)`, and `(2, 0..4)` mean it is enabled.
Other tuples, including the observed transition value `(6, 0)`, are left
unknown so a mode change cannot be misreported as a settled tracking state. An
unknown sample preserves the last confirmed indicator. The same snapshot
exposes the firmware's zoom position at offset `0x04` on a 0-to-100 scale.
Tarsier maps it to x1 through x4 and uses it as the live zoom source because
AI-driven reframing does not reliably update the standard V4L2 zoom control.
HDR is reported at offset `0x06`; zero means disabled and a non-zero value means
enabled.

HDR writes use libdev's raw selector-6 `[tag, length, value]` layout rather than
the framed selector-2 mailbox. The SDK warns that switching HDR is expensive
and recommends at least three seconds between transitions, which the camera
owner enforces. On the tested camera, an enabled-to-disabled-to-enabled round
trip was confirmed by readback without USB re-enumeration or a pipeline restart.
The effective frame rate dipped during reconfiguration and returned to 30 FPS.

## LED brightness experiment

On 2026-09-07, a bounded hardware experiment confirmed independent LED
brightness control while the Tiny 2 continued producing video. This control is
not yet exposed by Tarsier's API or UI.

- Write: unit `2`, selector `6`, `SET_CUR`, 60-byte buffer beginning with
  `[0x1a, 0x01, level]`, followed by zeros.
- Read: unit `2`, selector `6`, `GET_CUR`, 60-byte status buffer; byte `0x21`
  contains the brightness level.
- Levels: `0` off, `1` low, `2` medium, `3` high.

The write layout matches the Tiny-family UVC branch of libdev's
`Device::sysMgSetLedBrightnessR(unsigned char)`. The packed `CameraStatus.tiny`
definition in the [SDK header](https://github.com/aaronsb/obsbot-camera-control/blob/main/sdk/v1.0.2/include/dev/dev.hpp)
documents `led_brightness_level` as off or one of three brightness levels.

The sequence `3 -> 0 -> 1 -> 2 -> 3` returned the requested value at every
readback. Only byte `0x21` changed in the sampled selector-6 status blocks.
Snapshots of the camera reflected in a mirror visually confirmed extinction
at zero and increasing green illumination at levels one through three.
Exposure was automatic, so these images are not calibrated photometry.

The existing daemon was briefly suspended during each standalone USB
transaction and resumed in a `finally` block, preventing concurrent camera
I/O. Capture descriptors remained open. After every transition, the pipeline
reported running at approximately 30 FPS, with increasing frame counts,
zero restarts, and no pipeline error. This establishes live video with the LED
off, not frame-by-frame continuity during the brief process suspensions.
The original level (`3`) and camera orientation were restored afterward.

Persistence across power cycles was not tested. A future integration should
execute these operations inside the existing camera-owner thread rather than
use the experimental suspension method.

### Native blinking and color investigation

A follow-up experiment on the same date confirmed the SDK's Tiny 2
`Device::cameraSetLedCtrlU(bool)` special pattern:

- Start: unit `2`, selector `6`, `SET_CUR`, 60 bytes beginning
  `[0x18, 0x01, 0x01]`, followed by zeros.
- Stop: the same transaction beginning `[0x18, 0x01, 0x00]`.

With tracking disabled and brightness level three, the start command produced
autonomous green blinking. Eight seconds of snapshots sampled at 10 Hz showed
approximately one cycle per second, with roughly 0.7 seconds lit and 0.3 seconds
dark. These timings are approximate because of snapshot sampling and preview
latency. Only one start command was sent; host-side brightness toggling was not
needed. The stop command restored steady green illumination. A second start/stop
sequence reproduced the effect. No selector-6 status bytes changed immediately
after either command, so a successful write is not a measured blink-state
readback. Video remained active at about 30 FPS with no pipeline restart.

No independent RGB/color-selection control was found in the inspected SDK
header and libdev symbols. This is a search limitation, not proof that the
firmware has no such command. The [official manual](https://resource-cdn.obsbothk.com/download/obsbot-tiny-2/manual/OBSBOT%20Tiny%202%20User%20Manual_EN.pdf)
assigns colors to operating states: green for no selected target, blue for human
tracking, yellow for a lost target, purple for hand tracking, and red for faults.
A brief human-tracking request while looking at the mirror did not produce a
confirmed tracking lock; sampled state returned false and the LED stayed green.
This experiment therefore confirmed no additional colors and no independent
color override. It did not test configurable blink frequency or duty cycle.

The special pattern was disabled, brightness restored to three, tracking left
disabled as initially observed, and the original measured camera orientation
restored. Final daemon health was checked after restoration.

## Available image and perception surfaces

The tested Tiny 2 advertises standard V4L2 controls for automatic/manual
exposure, exposure time, gain, dynamic frame rate, backlight compensation,
continuous/manual focus, white-balance auto mode, white-balance color
temperature, red/blue balance, anti-flicker, brightness, contrast, saturation,
hue, and sharpness. Tarsier reads and writes these controls with `VIDIOC_G_CTRL`
and `VIDIOC_S_CTRL` inside the same camera-owner thread as proprietary XU
traffic. Each successful write is immediately read back; a lower-rate poll
keeps external changes visible in runtime state.

Manual exposure time and gain require `V4L2_CID_EXPOSURE_AUTO = 1`. Manual
temperature and red/blue balance require automatic white balance to be off,
and manual focus requires continuous autofocus to be off. The daemon enforces
these dependencies independently of the web UI.

Face-priority auto exposure is a separate raw selector-6 control: write
`[0x03, 0x01, 0x00]` for global metering or `[0x03, 0x01, 0x01]` for face
metering, zero-padded to 60 bytes. The state is read from selector-6 offset
`0x07`. This is available only with automatic exposure and is distinct from
face-priority autofocus.

The local MediaPipe worker currently reduces its face-detector result to a
boolean. Its result already contains a bounding box, so Tarsier can expose and
overlay a local face box in a later increment. The Tiny 2 selector-6 status
reports the selected AI mode, not a live subject or face bounding box.

## Standard UVC pan/tilt movement

The direction pad uses the standard `V4L2_CID_PAN_SPEED` and
`V4L2_CID_TILT_SPEED` controls instead of a proprietary query or an assumed
absolute starting angle. Tarsier queries both supported ranges, selects 25% of
the available speed in the requested direction, and applies both axes in one
`VIDIOC_S_EXT_CTRLS` call. A repeated request for the same direction renews a
350 ms movement lease without rewriting the UVC controls. A direction change
writes the new speed pair, while `stop` writes zero to both controls. All
hardware writes share the camera-owner thread with every other control.

On the tested Tiny 2, pan advertises -160 through 160 and tilt -120 through
120. Movement uses pan -40 for left, pan 40 for right, tilt 30 for up, and tilt
-30 for down. This speed-control pan convention is the reverse of the device's
absolute-pan convention. Hardware validation held pan 40 continuously across
16 reads during lease renewal and returned both controls to zero after an
explicit stop.

The UI renews the lease every 100 ms while a direction button or keyboard arrow
remains held. It sends an explicit stop on release, page blur, visibility loss,
or page exit. The camera-owner worker independently expires an unrenewed lease
and stops both axes, so a lost browser or request stream cannot leave the motor
running. A hardware test confirmed that one unrenewed command was active after
100 ms and stopped by 450 ms. Relative movement does not reveal its final angle,
so Tarsier clears the previous attitude sample instead of presenting stale
absolute coordinates as current telemetry.

## Standard UVC zoom

Lens zoom does not use the proprietary selector-2 mailbox. Tarsier discovers
the standard `V4L2_CID_ZOOM_ABSOLUTE` range with `VIDIOC_QUERYCTRL`, reads its
current value with `VIDIOC_G_CTRL`, and writes targets with `VIDIOC_S_CTRL`.
These operations run through the same camera-owner thread as extension-unit
traffic, so callers cannot interleave zoom and vendor commands.

The public API expresses the Tiny 2's physical magnification as x1 through x4.
For a discovered raw range from `minimum` to `maximum`, Tarsier uses:

```text
position = (magnification - 1) / 3
raw = minimum + position * (maximum - minimum)
```

The raw result is clamped and snapped to the reported step. The inverse mapping
is used as an initial fallback before the first selector-6 status sample. The
tested Tiny 2 reported a range of 0 through 100 with step 1: x2.5 therefore maps
to raw 50, and x1 maps to raw 0. Both positions were read back successfully
while 720p30 capture continued without a pipeline restart or USB
re-enumeration.

## Concurrency evidence and safety decision

Earlier investigation established that extension-unit GET and SET operations
must share one serialized cadence. Tarsier implements that ownership rule and
keeps video capture, preview, perception, and virtual-camera output in one
managed process.

An initial short test sustained 1280x720 MJPEG capture at approximately 30 FPS
while polling attitude twice per second. A separate generic client read 90
YUY2 frames from `/dev/video42` while preview, MediaPipe inference, and attitude
polling continued. Small absolute-move and tracking-disable commands also
completed during streaming without an immediate reset.

That result used the gimbal receiver's `GIM_GET_STATE` command (`0x0043`) and
did not survive a longer run. After approximately six minutes at the same 2 Hz
query rate, the kernel reported a failed selector-2 `SET_CUR`, the device
disconnected, and it re-enumerated at a new USB address. The physical pipeline
stopped in that build.

The replacement path follows libdev more closely: it uses
`AI_GET_GIM_STATE` (`0x6604`), schedules telemetry only when the owner command
queue is empty, performs at most one due telemetry operation before checking
the command queue again, and starts at 1 Hz in the reference configuration.
Pose, selector-6 camera status, and gesture status have independent exponential
backoffs capped at 60 seconds. A telemetry failure preserves camera
availability and the last timestamped sample instead of repeatedly querying or
disabling the control surface.

Tarsier now supervises GStreamer and retries the stable configured device path
after an error or EOS. The Linux XU transport also reopens its control path for
definite stale-device errors before retrying. This recovery logic has a live
synthetic EOS test; a deliberate physical unplug/reset cycle has not yet been
rerun against the current build.

The library default remains `camera.poll_interval_ms = 0`, while the reference
Tiny 2 configuration enables the new path at 1000 ms for controlled validation.
A zero value disables all periodic camera readback. Successful pose samples are
labelled `measured`; an accepted absolute target remains `last-commanded` until
the next measured sample arrives.

## Boundaries and uncertainty

- These values are validated only for the device and firmware above.
- The libdev-derived selector-6 byte at offset `0x02` reported sleep after one
  successful live wake/capture cycle, so Tarsier does not use it as independent
  power confirmation. Public power state records the successful command path.
- Tarsier does not link, load, bundle, or redistribute a proprietary SDK.
- The adapter does not yet discover compatible firmware capabilities.
- Image controls other than HDR and zoom, tracking-mode selection, and firmware
  update operations are deliberately unimplemented.
- Built-in gesture state is measured through `AI_GET_QUICK_STATUS` when polling
  is enabled; immediately after a write it temporarily represents the accepted
  command until readback arrives.
- The Tiny 2 status layout exposed by libdev contains no numeric device
  temperature. The SDK's numeric CPU and lens temperature fields belong to its
  network-camera status path. Its normal/high-temperature notification callback
  is documented only for Tail Air, not Tiny 2, and provides no value in degrees.
  V4L2 and Linux `hwmon` expose no device-temperature control for the tested
  camera either. The V4L2 white-balance temperature is a color setting, not a
  hardware temperature.
- Requested absolute angles and final physical attitude can differ; calibration
  and settling semantics need further study.
- The previous `0x0043` selector-2 polling path reset the camera during an
  extended test. The libdev-derived `0x6604` replacement passed a five-minute
  live soak but requires a longer unattended endurance run.
- Reconnect and recovery after unplug, firmware failure, or USB bus reset are
  implemented but have not yet been physically revalidated or soak-tested.

The executable protocol logic and packet fixtures live in
`src/camera/protocol.rs`; Linux UVC transport lives in
`src/camera/linux_uvc.rs`.
