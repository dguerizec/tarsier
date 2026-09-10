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
