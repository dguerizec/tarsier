# Comparing MediaPipe delegates

MediaPipe acceleration is selected independently for five task instances in the
Tarsier configuration file. All entries default to `cpu`; omitted entries keep
that default. Restart the daemon after changing them.

```toml
[perception.delegates]
face = "cpu"
hands = "cpu"
pose = "cpu"
segmentation = "cpu"
avatar_face = "cpu"
```

Change individual entries to `gpu` to test a strategy. GPU strategies should be compared with the same input and pipeline settings;
see the native diagnostic observations below.

`face`, `hands` and `pose` control the observation worker. `hands` includes gesture
recognition. `segmentation` controls the separate selfie mask task. `avatar_face`
controls the separate face tracker/cropper used by Personal 3D and LivePortrait.
It does not select the avatar renderer or the PyTorch inference backend.

The worker CLI exposes `--face-delegate`, `--hands-delegate`, `--pose-delegate`,
`--segmentation-delegate` and `--avatar-face-delegate`, each accepting `cpu` or
`gpu`. The supervisor passes every value explicitly. Tarsier never retries a
failed GPU initialization using CPU. Optional avatar initialization errors retain
the existing avatar retry behavior. Revert the affected entry to `cpu` if the
requested GPU context cannot initialize.

The [MediaPipe BaseOptions documentation](https://ai.google.dev/edge/api/mediapipe/python/mp/tasks/BaseOptions)
describes the GPU delegate and platform support. GPU delegation is not a promise
that every operation in a graph runs on GPU: some subgraphs still use CPU. It is
also not the CUDA selection used by the depth and LivePortrait models.

Telemetry records these settings under
`pipeline_context.perception.requested_delegates`. They describe the daemon's
requested configuration, not a per-operation hardware trace. For manually started
workers, align the worker CLI with the daemon configuration before benchmarking.
The worker logs its requested delegates at startup. Existing JSONL recording
includes this context automatically.

## Comparison protocol

Keep a single checkout and a single running service because branches share camera,
virtual video, audio and API resources. Changing branches alone does not replace a
running compiled daemon. Rebuild and restart the same service when changing Rust
code; restart the worker via the supervised daemon when changing Python code or
configuration.

Record the commit, delegate settings, model/asset hashes, resolution, cadence,
preview clients and pipeline modes. Use at least 15 seconds of stabilization and
45 seconds of samples per condition. Repeat camera-only at the beginning and end.
Keep CPU-only as the control, then compare observation GPU with segmentation and
avatar face on CPU. Test avatar face separately rather than changing both groups
at once. Initial isolated tests showed slower GPU selfie segmentation, so keep it
on CPU until a full-pipeline measurement demonstrates a benefit.

Compare total and worker CPU, RSS, output FPS, and actual stage calls/second.
Output FPS can include repeated avatar frames. Stage wall time is not CPU time.
NVML missing process activity is not zero, and device utilization includes other
applications. Check worker restarts, detection output and native MediaPipe warnings
alongside timings; performance alone is not correctness acceptance.

## Current NVIDIA validation

With MediaPipe 1.0.1 on the tested RTX 3070:

- Isolated face/pose GPU probes on the bundled portrait and a short live stream
  complete without the native tensor synchronization warning.
- A hands GPU probe emits the warning even with no detected hands.
- Starting with ten black frames before the portrait reproduces the warning for
  face and pose GPU independently. The live worker also emits it with face/pose
  GPU and hands CPU. Absence of a detected subject is therefore an important test
  condition, not just successful detection on a fixed image.

These probes identify a reproducible condition; they do not establish the exact
native call path or demonstrate corrupted output. MediaPipe logs the error only
once at the relevant source location, so a quiet remainder of a run cannot be
interpreted as proof that it stopped occurring. See the upstream
[Tensor write implementation](https://github.com/google-ai-edge/mediapipe/blob/master/mediapipe/framework/formats/tensor.cc).

The first live pilot was interrupted for diagnosis. The user confirmed being
out of frame and requested continuing the comparison without treating these
native warnings as blockers. The next strategy uses face, hands and pose GPU,
with segmentation and avatar face CPU. The warnings remain diagnostic evidence;
absence from the frame is not by itself proof of native synchronization safety.
Real visible-hand and gesture validation remains pending.
