# Tarsier perception worker

This project-local Python 3.12 worker reads the daemon's internal MJPEG preview,
runs the official MediaPipe face and pose landmarkers, canned gesture
recognizer, and dedicated selfie segmenter. It publishes versioned observations
at the configured perception rate and an 8-bit person mask at the independently
configured mask rate. A widened pose silhouette constrains the fast mask so
nearby furniture is not mistaken for part of the subject during motion. The
worker reads a raw internal branch; the public V4L2 loopback remains available
for the daemon's final, optionally processed output.

With the optional `avatar` dependency group, the same worker runs a local
stylized 3D renderer. A dedicated MediaPipe face landmarker extracts facial
blendshapes and a head transformation. A head-and-bust OpenGL scene maps those
signals to pose, blinking, jaw, smile, and eyebrow controls before publishing a
complete BGRx frame. The geometry is procedural for this first vertical slice;
recognition colors live in `assets/avatars/stylized-3d.json`. Camera
pixels never enter the rendered frame, and the queue retains only the newest
input so latency cannot grow without bound.

With the optional `depth` dependency group, Depth Anything V2 Small estimates a
relative inverse-depth field from that same raw input. The model is loaded only
while **Depth map** is selected and releases its GPU allocation on a mode
switch. The original two-dimensional `float32` values are published without
colorization; the Rust daemon retains them for machine consumers and derives
the false-color video output independently. Larger values are nearer, but the
monocular estimate is not a metric distance map.

The MediaPipe model files are cached outside the repository and checked against
pinned SHA-256 digests. Set up the default 3D worker with:

```sh
uv sync --project worker --extra avatar --extra depth --locked
uv run --project worker --extra avatar --extra depth --locked \
  tarsier-perception models --download --avatar --depth
```

Run real inference against the default daemon with:

```sh
uv run --project worker tarsier-perception serve
```

Pass `--source /dev/video43` only when a dedicated perception device is
preferred. The worker also accepts `--device` as a compatibility alias.

Avatar output is enabled by the daemon's `[avatar]` configuration. The
supervisor installs both optional dependency groups and supplies the 3D profile,
LivePortrait source image, output dimensions, and cadence automatically. The
worker polls the daemon's selected identity and loads only the requested engine;
PyTorch is not imported while Camera or Stylized 3D is selected. The depth and
LivePortrait models load independently and only for their corresponding
identity. Models and generated data stay on the local machine. Every generated
frame identifies its source, and the daemon accepts it only when that identity
is still selected. It emits black when matching data is unavailable or older
than 500 ms.

The `liveportrait` dependency group provides the experimental neural portrait
fallback. It requires the five additional weights and a source illustration:

```sh
uv sync --project worker --extra liveportrait --locked
uv run --project worker --extra liveportrait --locked \
  tarsier-perception models --download --avatar
```

The fallback's vendored LivePortrait neural-network modules and five downloaded
core weights are MIT-licensed; provenance is recorded beside the integration. The
upstream InsightFace detection assets are not included. They may be evaluated
later as an explicit alternative in this personal, non-commercial research
project if MediaPipe cropping proves insufficient, but their upstream
non-commercial-research restriction must be revisited before any broader or
commercial use.

The deterministic publisher exercises the daemon's face-presence and open-palm
stabilizers without a camera:

```sh
uv run --project worker tarsier-perception mock --open-palm
```

No frame or landmark leaves the machine. An observation containing face
presence, the 478 normalized face landmarks, the best gesture candidate,
confidence, processing latency, 33 normalized pose landmarks, and up to 42
normalized hand landmarks is sent over the loopback HTTP API. A separate raw
grayscale mask is published to the daemon's internal video-mask channel for
background and future final-output effects. The web UI renders the face, body,
and each detected hand locally as toggleable skeleton overlays.

While Depth map is selected, `GET /api/v1/depth/frame` returns the latest raw
field as row-major little-endian `float32`. Response headers provide its width,
height, capture/publication timestamps, frame id, representation, and the
display-only far/near bounds. The pinned Hugging Face Depth Anything V2 Small
checkpoint and Transformers integration are Apache-2.0 licensed.
