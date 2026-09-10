# Local perception video fixture

The reusable gesture and movement recording is:

`local-test-media/gestures-and-motion.mp4`

This is a private local fixture, intentionally excluded from Git by
`local-test-media/.gitignore`. A fresh session in this checkout can reuse it;
a fresh clone will not contain the video. Keep the original recording intact.

- Original: `20260910-170229-0.mp4`
- Duration: 41.574333 seconds
- Video: H.264, 1280x720, 30 fps
- Audio: AAC (not consumed by the perception comparison)
- Size: 33,010,763 bytes
- SHA-256: `932d45f5a70b207e87f547b77f0712c024b2dbaedd7ac878e8be166e9d9ce2fe`

From the repository root, run:

```sh
uv run --project worker python tools/compare_perception_delegates.py \
  --output local-test-media/cpu-gpu-comparison.json
```

Choose a new output filename for each run; existing results are never overwritten.
The tool processes every third frame at 640x360, preserving video timestamps,
first with CPU observation tasks and then with GPU observation tasks. Selfie
segmentation stays CPU in both runs. It measures detected landmarks and gestures,
pose mask overlap, empty-mask regressions and per-call wall/process CPU times.
The pose constraint is applied to the selfie mask just as in the worker.

This is an offline functional comparison on identical frames, not a full-pipeline
performance benchmark. It does not publish observations, access camera devices,
change live settings, invoke tracking controls or process audio. CPU results are
a reference, not annotated ground truth. Existing backgrounds/effects baked into
the recording cannot be removed by replaying it.

For live pipeline telemetry and CPU/GPU controls, see
[MediaPipe delegates](mediapipe-delegates.md).

## Recorded validation after the GPU mask fix

`local-test-media/cpu-gpu-mask-fix-comparison.json` contains the first comparison
on this clip after scalarizing GPU RGBA masks (MediaPipe 1.0.1, RTX 3070).

- 415 identical sampled input frames per backend.
- Pose detected on all 415 frames in both runs; zero detected poses with an empty
  mask. Median CPU/GPU pose-mask intersection-over-union: 97.4%.
- Hands detected on 255 CPU frames and 256 GPU frames.
- Framewise gesture labels agree on 414 of 415 frames (including no gesture).
- Mean offline call wall time: CPU 36.9 ms, GPU 21.1 ms. Mean process CPU time:
  CPU 49.5 ms, GPU 31.0 ms. These include observation tasks and CPU selfie masking;
  concurrent live workloads and warmup are not excluded. They are not a claim
  about end-to-end daemon resource savings.

The mask fix was also inspected in the live preview with Camera + Pixel Party
and GPU pose active: the subject was visible in front of the background.

## Full-pipeline replay comparison (2026-09-10)

The local run is stored in `local-test-media/full-pipeline-cpu-gpu-20260910/`:
`report.md`, `comparison.json`, per-strategy `summary.json` and raw JSONL files,
plus the measurement and analysis scripts. These files remain private and ignored
by Git. The run used commit `7a46944`, the debug daemon, RTX 3070, 1280x720 at
30 fps and the fixed gesture clip through the new file input pipeline.

Both strategies used CPU selfie segmentation and CPU avatar face tracking.
The GPU strategy moved only observation face, hands and pose to GPU. Each of six
conditions had at least 15 seconds of stabilization and 84 seconds of sampling
at two-second intervals: 504 valid samples total, with no worker restarts during
measurement. Plain video was repeated at the end of each strategy.

| Mode | Total CPU, CPU delegates → GPU delegates | Output fps, CPU → GPU |
|---|---:|---:|
| Plain video | 160.7% → 142.6% | 30.0 → 30.0 |
| Pixel Party | 277.9% → 271.3% | 23.9 → 27.9 |
| Depth map | 299.6% → 296.0% | 30.1 → 30.0 |
| Personal 3D | 349.3% → 319.3% | 29.9 → 30.0 |
| LivePortrait | 324.7% → 340.3% | 30.0 → 30.0 |
| Plain video repeat | 162.2% → 143.4% | 30.0 → 30.0 |

CPU 100% means one logical core; totals exclude the browser. Plain-video worker
CPU fell from 73.2% to 55.0%. Personal 3D's CPU reduction came with fewer avatar
render calls (22.5/s → 20.5/s); LivePortrait also rendered slightly less often
(7.9/s → 7.6/s). Output FPS includes repeated frames, so the avatar results do
not establish an unconditional GPU benefit. GPU contention is one possible cause
of the regressions, not a conclusion isolated by this experiment.

This is a sequential comparison on one desktop, with uncontrolled external load
and preview clients. Windows cover approximately two clip loops, without exact
frame alignment. NVML process activity remains sparse in some modes; missing
samples are not zero. RSS includes models retained from earlier scenarios.
See the local report for stage timings, memory, GPU sample counts and limits.
The original delegates and effects were restored; the video remains selected.

The current process boundaries, effect dependencies and candidate demand rules are
documented in [Effect pipelines](effect-pipelines.md).

## Demand-driven selfie segmentation (2026-09-10)

Private evidence is in `local-test-media/segmentation-demand-20260910/`:
`report.md`, before/after manifests, JSONL and summaries, transition checks and
snapshots, plus the measurement scripts and implementation patch. The fixture
and its original recording remain unchanged. This comparison uses the same
1280x720/30 fps replay and debug daemon, face/hands/pose/**avatar face** on GPU,
and selfie segmentation on CPU (the actual initial settings for this run).

Each measured condition has 15 seconds of stabilization and 48 seconds sampled
at two-second intervals, covering a clip EOF crossing. The matched before/after
conditions each contain 24 valid samples, with no worker restart within their
windows. A pilot overlapping compilation is excluded. A manual source change
interrupted the final before-repeat before its first valid sample; that window
is excluded. Replay was restored before the supervised restart and after run.

| Mode | Total CPU, before → after | Worker CPU, before → after | Segmentation calls/s, before → after | Output fps, before → after |
|---|---:|---:|---:|---:|
| Plain video | 146.2% → 100.5% | 57.0% → 27.6% | 29.88 → 0 | 30.0 → 30.0 |
| Camera + Pixel Party | 272.5% → 273.1% | 165.5% → 165.2% | 29.58 → 29.52 | 26.3 → 26.5 |

The after plain-video repeat measured 101.5% total CPU, 28.1% worker CPU,
30 fps and zero segmentation calls. Face, hands and pose stayed near 10 calls/s.
CPU 100% means one logical core. These sequential desktop measurements exclude
the browser but do not control external load or align exact clip frames; the
CPU difference is evidence from this run, not a universal performance guarantee.

Eight runtime transitions passed: camera off, green, off again, blur, depth-map,
Personal 3D, LivePortrait and camera Pixel Party resume. Each check required
three distinct telemetry windows with expected segmentation activity, live
observations and the selected depth/avatar renderer. All used one worker PID.
Static snapshots were inspected for green, blur, depth and both avatars; this
is not exhaustive transition-video or browser QA. The initial Camera + Pixel
Party, remembered LivePortrait engine, delegates, mute and device settings were
restored, retaining the replay input.

## Shared-frame input transport (2026-09-10)

Implementation and protocol: [Shared-frame transport](shared-frame-transport.md).
Private evidence is in `local-test-media/shared-frame-transport-20260910/`,
including `report.md`, JSONL with pipeline context, summaries and run scripts.
The replay, delegates and segmentation-demand optimization remain fixed. Each
window has 15 seconds stabilization and 48 seconds sampling (24 valid samples).
Three MJPEG windows and three final BGR shared-memory windows cover plain video,
Pixel Party and plain video again. A final MJPEG plain-video control uses the
same final binary; the first full MJPEG series predates the shared-only change
from BGRx to direct BGR. The discarded BGRx candidate is archived separately.

| Mode | Total CPU, MJPEG → shared | Worker CPU, MJPEG → shared | Output fps, MJPEG → shared | Input wall ms/frame, decode → copy |
|---|---:|---:|---:|---:|
| Plain video | 99.27% → 97.46% | 26.06% → 24.04% | 30.00 → 30.00 | 0.54 → 0.08 |
| Pixel Party | 269.04% → 263.92% | 164.71% → 160.60% | 23.92 → 23.59 | 0.64 → 0.09 |
| Plain repeat | 101.29% → 98.25% | 28.08% → 25.75% | 29.97 → 30.00 | 0.54 → 0.07 |

The final MJPEG control measured 98.38% total CPU, 25.44% worker and 30 fps.
Thus the plain-video CPU difference is small relative to run drift; these
sequential desktop measurements do not establish a substantial global CPU gain.
Pixel Party throughput is essentially unchanged. The input-stage wall-time
reduction is clear, and shared mode reports zero JPEG decode calls. CPU 100% is
one logical core; stage wall time is not CPU attribution. Raw model inputs also
avoid an extra lossy JPEG generation, so they are not byte-identical to MJPEG.

No browser or diagnostic MJPEG client was added during the measured windows.
The raw buffer is 691,264 bytes at 640x360, plus a 691,200-byte private worker
image; this is bounded shared transport with a copy, not end-to-end zero-copy.

Validation passed: 307 Rust tests (five ignored), 77 Python tests, Ruff and
35 JavaScript tests. Live checks covered plain camera, Pixel Party, depth-map,
Personal 3D, LivePortrait and Pixel Party resume, followed by supervised worker
termination/reconnection to the same daemon buffer. Shared-copy counters remained
active and JPEG decode counters stayed zero. Snapshots for the four effect modes
were inspected; this is static output validation, not exhaustive browser/video
QA. The authenticated diagnostic MJPEG endpoint also returned a JPEG on demand.
Initial effects were restored and replay remains selected.
