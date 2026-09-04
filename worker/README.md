# Tarsier perception worker

This project-local Python 3.12 worker reads frames from the Tarsier V4L2
loopback device, runs the official MediaPipe face detector and canned gesture
recognizer, and publishes versioned observations to the Rust daemon.

The model files are cached outside the repository and checked against pinned
SHA-256 digests. Set up the worker with:

```sh
uv sync --project worker --locked
uv run --project worker tarsier-perception models --download
```

Run real inference against the default daemon and loopback device with:

```sh
uv run --project worker tarsier-perception serve
```

The deterministic publisher exercises the daemon's face-presence and open-palm
stabilizers without a camera:

```sh
uv run --project worker tarsier-perception mock --open-palm
```

No frame or landmark leaves the machine. Only a compact observation containing
face presence, the best gesture candidate, confidence, and processing latency
is sent over the loopback HTTP API.
