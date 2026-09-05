# Tarsier perception worker

This project-local Python 3.12 worker reads the daemon's internal MJPEG preview,
runs the official MediaPipe face and pose landmarkers plus the canned gesture
recognizer, and publishes versioned observations to the Rust daemon. The public
V4L2 loopback therefore remains available to external video clients.

The model files are cached outside the repository and checked against pinned
SHA-256 digests. Set up the worker with:

```sh
uv sync --project worker --locked
uv run --project worker tarsier-perception models --download
```

Run real inference against the default daemon with:

```sh
uv run --project worker tarsier-perception serve
```

Pass `--source /dev/video43` only when a dedicated perception device is
preferred. The worker also accepts `--device` as a compatibility alias.

The deterministic publisher exercises the daemon's face-presence and open-palm
stabilizers without a camera:

```sh
uv run --project worker tarsier-perception mock --open-palm
```

No frame or landmark leaves the machine. Only an observation containing face
presence, the 478 normalized face landmarks, the best gesture candidate,
confidence, processing latency, 33 normalized pose landmarks, and up to 42
normalized hand landmarks is sent over the loopback HTTP API. The web UI
renders the face, body, and each detected hand locally as toggleable skeleton
overlays.
