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
| Read attitude (unsafe while streaming) | 2 | `0x03` | `0x0043` | empty query |
| Recenter | 2 | `0x03` | `0x00c3` | six zero bytes |
| Move absolute | 2 | `0x04` | `0x6444` | float32 roll, pitch, yaw |
| Target-selection gesture | 2 | `0x04` | `0x30c4` | one byte: `1` enabled, `0` disabled |
| Zoom gesture | 2 | `0x04` | `0x3144` | one byte: `1` enabled, `0` disabled |
| Dynamic-zoom gesture | 2 | `0x04` | `0x3344` | one byte: `1` enabled, `0` disabled |
| Tracking | 6 | n/a | n/a | `16 02 02` enabled, `16 02 00` disabled, then zeros |

The attitude response begins with signed 16-bit roll, pitch, and yaw values in
hundredths of a degree. The public Tarsier state normalizes their order to yaw,
pitch, and roll.

Absolute movement targets are additionally checked against configured yaw and
pitch limits and a fixed +/-45 degree roll limit before a frame is queued.
Tracking uses a separate raw selector-6 payload rather than the framed
selector-2 mailbox.

The three built-in gesture controls are independent of Tarsier's MediaPipe
gesture recognition. They use the Tiny 2 commands documented by the vendor
SDK's model-specific compatibility API rather than the newer unified gesture
parameter command, which that SDK categorizes for Tail 2 and later products.
Tarsier records a gesture setting only after the UVC write succeeds. It does
not issue an additional proprietary readback while video is streaming, so each
setting starts as unknown after a daemon restart.

## Standard UVC pan/tilt nudges

The direction pad uses the standard `V4L2_CID_PAN_SPEED` and
`V4L2_CID_TILT_SPEED` controls instead of a proprietary query or an assumed
absolute starting angle. Tarsier queries both supported ranges, selects 25% of
the available speed in the requested direction, applies both axes in one
`VIDIOC_S_EXT_CTRLS` call, waits 120 ms, and writes zero to both controls. The
start and stop writes share the camera-owner thread with every other control.

On the tested Tiny 2, pan advertises -160 through 160 and tilt -120 through
120. The resulting pulses use pan -40 for left, pan 40 for right, tilt 30 for
up, and tilt -30 for down. This speed-control pan convention is the reverse of
the device's absolute-pan convention. Hardware validation confirmed movement
on both axes and verified that `pan_speed` and `tilt_speed` return to zero after
every API request.

The UI repeats these independently bounded pulses while a direction button or
keyboard arrow remains held. Closing or disconnecting the browser cannot make
one pulse exceed 120 ms because stopping is owned by the daemon. A speed pulse
does not reveal its final angle, so Tarsier clears the previous attitude sample
instead of presenting stale absolute coordinates as current telemetry.

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
is used for startup readback. The tested Tiny 2 reported a range of 0 through
100 with step 1: x2.5 therefore maps to raw 50, and x1 maps to raw 0. Both
positions were read back successfully while 720p30 capture continued without a
pipeline restart or USB re-enumeration.

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

That result did not survive a longer run. After approximately six minutes at
the same 2 Hz query rate, the kernel reported a failed selector-2 `SET_CUR`, the
device disconnected, and it re-enumerated at a new USB address. The physical
pipeline stopped in that build. This matches earlier evidence that framed
`GIM_GET_STATE` queries can destabilize the tested firmware when a physical UVC
stream is active.

Tarsier now supervises GStreamer and retries the stable configured device path
after an error or EOS. The Linux XU transport also reopens its control path for
definite stale-device errors before retrying. This recovery logic has a live
synthetic EOS test; a deliberate physical unplug/reset cycle has not yet been
rerun against the current build.

Tarsier therefore sets `camera.poll_interval_ms = 0` by default. A non-zero
value retains the query for deliberate experiments, emits a startup warning,
and marks successful state as `measured`. Normal operation reports an accepted
movement target as `last-commanded`; it does not claim that value is live
telemetry.

## Boundaries and uncertainty

- These values are validated only for the device and firmware above.
- Tarsier does not link, load, bundle, or redistribute a proprietary SDK.
- The adapter does not yet discover compatible firmware capabilities.
- Sleep, image controls, tracking modes, and firmware update operations are
  deliberately unimplemented.
- Built-in gesture state is the last setting accepted by the control transport,
  not device readback, and returns to unknown when the daemon restarts.
- Requested absolute angles and final physical attitude can differ; calibration
  and settling semantics need further study.
- Continuous selector-2 attitude polling is known to be unsafe during streaming
  on the tested firmware and is disabled by default.
- Reconnect and recovery after unplug, firmware failure, or USB bus reset are
  implemented but have not yet been physically revalidated or soak-tested.

The executable protocol logic and packet fixtures live in
`src/camera/protocol.rs`; Linux UVC transport lives in
`src/camera/linux_uvc.rs`.
