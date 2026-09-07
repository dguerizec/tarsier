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

### Brightness zero and blinking interaction

A further mirror experiment on 2026-09-07 tested both command orders. At
brightness three, enabling the special pattern produced blinking. Setting
brightness to zero then extinguished the LED; restoring brightness three
resumed blinking without another special-pattern command. Conversely, setting
brightness zero with the special pattern disabled, then enabling the pattern,
left the LED dark after transition latency. Raising brightness to three again
revealed blinking without resending its enable command.

Each phase sampled 40 snapshots over approximately four seconds. The
zero-after-blink phase contained no lit samples. The blink-after-zero phase
contained one initially lit sample followed by 39 dark samples; the first
sample may have been a preview frame retained from before the transition.
Brightness readback was zero in both dark phases. Thus brightness zero masks
the pattern while retaining its enabled state, as inferred from its later
resumption; there is still no direct blink-state readback.

The first attempt encountered HTTP 503 from the snapshot endpoint and one
automatic video-pipeline restart. Restoration ran before retrying. The cause
was not established. The complete second attempt added no pipeline restarts.
The original brightness and orientation were restored, the special pattern
disabled, and final health checked as OK.

For API/UI design, brightness and blinking can remain independent desired
settings: zero brightness means no visible light without discarding the
blinking preference. Brightness can be reported as measured; blinking should
be reported as the last successfully sent request, not a measured device
state. A daemon restart or external controller can invalidate that assumption
unless the desired pattern is explicitly reapplied.

### Host-driven fast blinking experiment

A mirror trial on 2026-09-07 disabled the native pattern and alternated
brightness zero and three at a target of three complete cycles per second
for six seconds. All 36 writes returned the requested brightness on readback;
the median interval between completed writes was approximately 166 ms.
Captured mirror images confirmed repeated lit and dark phases consistent
with this cadence. This uses host-timed brightness commands, not a native
blink-speed parameter.

The trial obtained 44 snapshots and encountered 41 HTTP 503 responses. The
video-pipeline restart counter increased from one to three. The experimental
helper briefly suspends the daemon for each USB exchange, including a 60 ms
settling delay, to avoid concurrent camera access. This confounds the trial:
it demonstrates visible fast blinking, but does not establish uninterrupted
video operation or show whether the device itself causes these interruptions.
A production feasibility test would need commands serialized within the
existing camera-owner thread without suspending the daemon.

Brightness three and the original camera orientation were restored, with the
native pattern disabled. Final health was OK and video returned to about
30 frames per second.

A follow-up stopped the daemon entirely and captured the physical camera
directly with FFmpeg at 1280x720 MJPEG, 30 fps, while a separate control file
descriptor alternated brightness at 3 Hz without process suspension. In the
measured repeat, all 48 writes over eight seconds matched immediate readback.
The 12-second capture delivered 361 frames; the largest host-observed interval
between decoded frames was 42.34 ms, with no interval above 100 ms. The longest
write-plus-readback took 14.90 ms. FFmpeg reported an initial duplicate timestamp
warning in the first trial, which otherwise also maintained approximately
30 fps. Frame arrival timing is a host-side continuity measurement, not proof
of unique sensor exposures or a test of Tarsier's future scheduling integration.

These results support fast brightness toggling alongside uninterrupted camera
capture, and suggest that the earlier interruptions came from the experimental
access method rather than an unavoidable hardware limitation. The transient
systemd unit disappeared when stopped and was recreated to restore the daemon.
Brightness three and native blinking disabled were restored. After the measured
repeat, daemon health was OK, the original pose remained unchanged, and its
video pipeline ran at approximately 30 fps with zero restarts.

### Deeper local SDK inspection: indicator-state commands

A subsequent static inspection used the exact local file
`/data/audit/obsbot-camera-control/sdk/v1.0.2/lib/libdev.so`, without loading it
into a running process or sending device commands. The library is an unstripped
x86-64 ELF with DWARF debug information:

- Build ID: `5a0debca07356e8f3146024b19ca1701e0e21dad`.
- SHA-256: `d9fc9cd7f6743a3eefd50dbae104ccaff9c3d45ee37816722cb0bd683854e7a4`.

Symbol, string, DWARF type, and disassembly inspection found an additional
candidate mechanism that the initial LED-name search missed:

| Export | ELF address | Internal command enum | V3 command ID |
| --- | --- | --- | --- |
| `Device::sysMgSetIndicatorStateR(uint8_t state_id)` | `0x6e3b0` | `0x10` | `0x01c0` |
| `Device::sysMgClearIndicatorStateR(uint8_t state_id)` | `0x6e480` | `0x11` | `0x01c1` |

The internal command set is `CMD_SET_SYS_MG` (`0x0d`). Both wrappers copy the
single `state_id` byte into their message payload and call `sendMsgAsync`.
Their bodies contain no color table, state-ID validation, or product-type
branch. The V3 IDs above come from the static initializer for
`kCmdIdSysMgV3`; they are not selector-6 tags or complete Tiny 2 wire packets.

These functions are declared in the **local** adjacent `include/dev/dev.hpp`
at lines 3500 and 3508, categorized for **Tail Air and Tail 2**. The header
retrieved earlier from GitHub differs and does not declare them. The local
header does not explain the state IDs; the clear function's brief description
also appears to be a copied buzzer comment. No indicator-state enum or
state-to-color mapping was found in the inspected debug information and
strings. A wrapper accepting one byte does not establish that every byte is
valid or that Tiny 2 firmware implements the command.

Other exported candidates include LED enable/brightness, tally enable and
brightness, and battery-light enable. The inspected protobuf LED and tally
configuration types contain operation options and scalar sliders, not RGB
fields. `cameraSetBgColorU` is documented as a Meet/Meet 4K virtual-background
control and is unrelated to the physical indicator.

The set/clear pair is therefore a concrete candidate for preset indicator
states, potentially including colors, rather than evidence of arbitrary RGB
selection. Tiny 2 support, valid IDs, actual visual effects, and restoration
semantics remain unverified. No camera settings were changed for this static
inspection.

### Search for known indicator-state values

A follow-up source search found no documented numeric `state_id` suitable for
a bounded color test. The local SDK and application contain declarations but
no example calls to the set/clear pair; no direct call site was found in the
inspected library disassembly either.

The public SDK copy under
[`malko/obsbot-js-sdk/libdev_v2.1.0_7`](https://github.com/malko/obsbot-js-sdk/tree/main/libdev_v2.1.0_7)
has the same unexplained declarations, as does `libdev_v2.1.0_8` in
`batatrax/obsbot-linux`; both retain the Tail Air/Tail 2 categorization.
The `libdev_v2.1.0_7` `OBSBOT_Sample/main.cpp` uses
`cameraSetLedCtrlU(true)` and `cameraSetLedCtrlU(false)` around zone-tracking
configuration, corroborating the already tested special-pattern switch rather
than providing an indicator-state ID. Searches of public source files in
`batatrax/obsbot-linux`, `Domatix/obsbot-control`, `ananthb/libobsbot`, and
`OpenFoxes/Tiny4Linux` found no matching indicator-state or LED-color example.
These negative searches do not establish firmware limitations.

The [official SDK page](https://www.obsbot.jp/jp/sdk) distributes SDK packages
through a signed-in request followed by email delivery, rather than a public
download link on that page. No request was submitted. No arbitrary state IDs
were sent to the camera. The remaining prerequisite is a documented state-ID
mapping applicable to Tiny 2, or a known application call/USB trace with its
corresponding clear operation. Merely finding both exported functions is not
sufficient to establish safe restoration for an unknown ID.

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
