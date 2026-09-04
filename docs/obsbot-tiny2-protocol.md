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
| Tracking | 6 | n/a | n/a | `16 02 02` enabled, `16 02 00` disabled, then zeros |

The attitude response begins with signed 16-bit roll, pitch, and yaw values in
hundredths of a degree. The public Tarsier state normalizes their order to yaw,
pitch, and roll.

Absolute movement targets are additionally checked against configured yaw and
pitch limits and a fixed +/-45 degree roll limit before a frame is queued.
Tracking uses a separate raw selector-6 payload rather than the framed
selector-2 mailbox.

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
- Sleep, zoom, image controls, tracking modes, and firmware update operations
  are deliberately unimplemented.
- Requested absolute angles and final physical attitude can differ; calibration
  and settling semantics need further study.
- Continuous selector-2 attitude polling is known to be unsafe during streaming
  on the tested firmware and is disabled by default.
- Reconnect and recovery after unplug, firmware failure, or USB bus reset are
  implemented but have not yet been physically revalidated or soak-tested.

The executable protocol logic and packet fixtures live in
`src/camera/protocol.rs`; Linux UVC transport lives in
`src/camera/linux_uvc.rs`.
