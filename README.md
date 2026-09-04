# Tarsier

Tarsier is a local-first control and perception daemon for motorized cameras.
It owns camera capture and vendor control traffic, publishes a stable V4L2
virtual camera, exposes a local API and MCP gateway, and turns MediaPipe
observations into debounced semantic events.

The name comes from the tarsier: a small primate with very large eyes and a
highly mobile head. The project inherits the creature's attentive and slightly
mischievous personality without tying its core to one camera vendor.

> Status: working Linux prototype. The first vertical slice has been exercised
> on an OBSBOT Tiny 2 with live 720p30 video, bounded gimbal control, a generic
> V4L2 consumer, physically validated open-palm activation, supervised local
> perception, HTTP/MCP controls, and the embedded web UI. Camera attitude is
> provenance-labelled and defaults to the last commanded target because
> extended live polling proved unsafe. See
> [Validation](#validation) and
> [Known limitations](#known-limitations) before relying on it unattended.

## What works

- one Rust daemon owns `/dev/video0` and serializes OBSBOT extension-unit I/O;
- a GStreamer tee feeds an MJPEG preview and `/dev/video42` at 720p30;
- a video supervisor closes stale streams and rebuilds the pipeline against the
  stable device path after runtime errors or end-of-stream;
- generic V4L2 clients can consume `/dev/video42` while perception uses the
  daemon's internal preview branch;
- camera attitude is explicitly labelled `last-commanded`, `measured`,
  `simulated`, or `unavailable` rather than presenting an estimate as fact;
- bounded absolute gimbal moves, recentering, tracking, named presets, and
  separate Tiny 2 built-in gesture controls are available over HTTP;
- a supervised Python 3.12 worker performs local MediaPipe face and canned
  gesture recognition;
- face presence and open-palm observations pass through dwell, release, and
  cooldown stabilization before becoming semantic events;
- a responsive local web UI shows the preview, telemetry, perception state,
  presets, scenarios, and recent events, with an optional 21-point hand
  skeleton overlay; its MJPEG preview reconnects after either a pipeline or
  daemon restart;
- snapshots are available as JPEG over HTTP and as image content over MCP.

OBS, Stream Deck, scripts, and similar tools are possible API clients. OBS is
not a primary product target and is not required by Tarsier.

## Architecture

```mermaid
flowchart LR
    Camera[Motorized UVC camera] -->|MJPEG| Pipeline[Managed GStreamer pipeline]
    Camera <-->|serialized UVC/XU| Adapter[Camera adapter]

    Pipeline -->|YUY2 720p30| Loopback[V4L2 loopback]
    Pipeline --> Preview[Internal MJPEG preview]
    Preview --> UI[Local web UI]
    Preview --> Worker[MediaPipe worker]

    Adapter <--> Core[Rust runtime and event bus]
    Worker -->|versioned observations| API[HTTP API]
    API --> Core
    Core --> Scenarios[Semantic stabilizers and scenarios]
    API <--> UI
    API <--> MCP[MCP stdio gateway]
    API <--> Clients[Local clients]
```

The process-local bus uses Tokio primitives. External modules communicate
through versioned HTTP schemas; they do not gain direct access to hardware or
the bus. The Python worker receives only downscaled JPEG frames and posts
compact observations back to the loopback-only API.

## Requirements

The current prototype targets Linux and expects:

- a Rust toolchain with edition 2024 support (tested with Rust 1.93);
- Python 3.12 and `uv`;
- GStreamer runtime, base/good plugins, and development headers;
- `v4l2loopback`, `v4l-utils`, and a free virtual device;
- an OBSBOT Tiny 2 reachable through the configured video-device path for the
  real adapter.

On Ubuntu, the native packages can be installed with:

```sh
sudo apt install \
  build-essential pkg-config \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-tools gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good \
  v4l-utils v4l2loopback-dkms v4l2loopback-utils
```

Load a loopback device matching the example configuration:

```sh
sudo modprobe v4l2loopback video_nr=42 card_label=Tarsier exclusive_caps=1
```

The physical camera and `/dev/video42` must be free before the daemon starts.
The example uses the stable `/dev/v4l/by-id/...-video-index0` camera symlink so
a manual restart still finds the device after USB re-enumeration.

## Quick start

Install the locked Python environment and the pinned MediaPipe model assets:

```sh
uv sync --project worker --locked
uv run --project worker tarsier-perception models --download
```

Validate the configuration, then start Tarsier:

```sh
cargo run -- config --config config/tarsier.example.toml
cargo run -- serve --config config/tarsier.example.toml
```

Open <http://127.0.0.1:8742/> for the embedded preview and controls. In another
terminal, inspect the daemon or consume its public virtual camera. The **Hand
skeleton** button overlays MediaPipe landmarks in the UI without modifying the
public V4L2 feed. Embedded UI assets use `Cache-Control: no-store`, so one page
reload always installs the current client behavior after an upgrade.

```sh
cargo run -- status

gst-launch-1.0 v4l2src device=/dev/video42 \
  ! video/x-raw,format=YUY2,width=1280,height=720,framerate=30/1 \
  ! videoconvert ! autovideosink
```

Any V4L2-compatible player may replace the GStreamer command. Tarsier keeps
the loopback output available because its own perception worker reads the
internal MJPEG preview instead.

For a hardware-free smoke run, copy the example configuration and set:

```toml
[video]
source = "test"
loopback_enabled = false

[camera]
adapter = "mock"
```

## Configuration

[`config/tarsier.example.toml`](config/tarsier.example.toml) is the reference
configuration. It defines:

- the loopback-only server address;
- physical and virtual video devices, frame size, rate, preview quality, and
  pipeline recovery delay;
- camera adapter, extension-unit selector, polling cadence, and movement
  limits;
- worker supervision, perception rate, confidence, dwell, release, and
  cooldown thresholds;
- bounded named camera presets;
- event-to-action scenario declarations.

Configuration is validated at startup and is not hot-reloaded. The default
camera limit is +/-130 degrees yaw and +/-90 degrees pitch; every HTTP and MCP
move is validated again by the daemon. The `mock` and `disabled` camera
adapters support development without claiming real control hardware.

`camera.poll_interval_ms = 0` disables proprietary live-attitude polling and is
the safe default. A non-zero value enables the experimental `GIM_GET_STATE`
query and labels successful samples `measured`, but an extended test reset the
tested camera while streaming. Do not enable it for normal preview use.

The first scenario action is deliberately small: an activation publishes the
configured action name as a structured `scenario.activated` event. It does not
run arbitrary shell commands or make outbound requests.

## HTTP and WebSocket API

The default server binds only to `127.0.0.1:8742`.

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/health` | Health, version, and uptime |
| `GET` | `/api/v1/state` | Complete runtime state |
| `GET` | `/api/v1/config` | Effective configuration |
| `GET` | `/api/v1/camera/state` | Camera availability and attitude |
| `POST` | `/api/v1/camera/move` | Bounded absolute yaw/pitch/roll target |
| `POST` | `/api/v1/camera/tracking` | Enable or disable built-in tracking |
| `POST` | `/api/v1/camera/built-in-gestures/{feature}` | Enable or disable `target-selection`, `zoom`, or `dynamic-zoom` gestures |
| `POST` | `/api/v1/camera/actions/recenter` | Recenter the gimbal |
| `GET` | `/api/v1/camera/presets` | List configured presets |
| `POST` | `/api/v1/camera/presets/{id}/recall` | Recall a named preset |
| `GET` | `/api/v1/camera/snapshot` | Latest preview frame as JPEG |
| `GET` | `/api/v1/preview.mjpeg` | Multipart MJPEG preview |
| `GET` | `/api/v1/scenarios` | List configured scenarios |
| `POST` | `/api/v1/scenarios/{id}/trigger` | Manually activate a scenario |
| `GET` | `/api/v1/events/recent` | Recent semantic and control events |
| `WS` | `/api/v1/events` | Live event stream |

`POST /api/v1/perception/observations` is the local worker ingestion endpoint.
It is not intended as an operator control.

Examples:

```sh
curl -fsS http://127.0.0.1:8742/api/v1/state

curl -fsS -X POST http://127.0.0.1:8742/api/v1/camera/move \
  -H 'content-type: application/json' \
  -d '{"yaw":-20,"pitch":5,"roll":0}'

curl -fsS -X POST \
  http://127.0.0.1:8742/api/v1/camera/built-in-gestures/zoom \
  -H 'content-type: application/json' \
  -d '{"enabled":false}'

curl -fsS http://127.0.0.1:8742/api/v1/camera/snapshot \
  --output snapshot.jpg
```

## MCP gateway

The `tarsier-mcp` binary is a stateless stdio gateway over the daemon's public
API. Start the daemon first, then configure an MCP client to execute:

```sh
cargo run --bin tarsier-mcp -- \
  --daemon-url http://127.0.0.1:8742
```

For a release installation, use the built executable instead:

```text
/absolute/path/to/tarsier-mcp --daemon-url http://127.0.0.1:8742
```

The gateway exposes eleven typed tools:

- read-only: `get_state`, `get_config`, `list_scenarios`, `recent_events`,
  `list_camera_presets`, and `take_snapshot`;
- mutating: `move_camera`, `set_tracking`, `recenter_camera`,
  `recall_camera_preset`, and `trigger_scenario`.

MCP transports commands, state, events, and snapshots, not continuous video.
Movement limits and event recording remain enforced by the daemon regardless
of the MCP client.

## Development and tests

Run the complete automated check set with:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
uv lock --project worker --check
uv run --project worker ruff check worker
uv run --project worker pytest -q worker
node --check web/app.js
```

The worker can exercise face and gesture stabilization deterministically
without camera hardware:

```sh
uv run --project worker tarsier-perception mock --open-palm
```

Protocol construction, CRCs, packet validation, state stabilization, safety
limits, routing, configuration, and MCP negotiation have focused tests. See
[`docs/obsbot-tiny2-protocol.md`](docs/obsbot-tiny2-protocol.md) for the
clean-room interoperability notes behind the first camera adapter.

## Validation

The first vertical slice was validated on 2026-09-05 with an OBSBOT Tiny 2
(USB ID `3564:fef8`, firmware `6.0.10.4`) on Ubuntu 26.04:

- the physical pipeline sustained approximately 30 FPS at 1280x720 with no
  pipeline restart;
- target-selection, zoom, and dynamic-zoom gesture-disable commands were
  accepted while streaming without a pipeline restart or USB re-enumeration;
- a short attitude-polling run returned changing live values during capture;
- a generic GStreamer V4L2 reader consumed 90 frames from `/dev/video42` and
  exited successfully while preview, perception, and polling continued;
- the camera kept the same USB bus address throughout the initial loopback,
  move, tracking, snapshot, UI, and MCP checks;
- a bounded move and tracking-disable request were accepted, reflected in
  telemetry/state, and recorded on the event bus;
- the supervised MediaPipe worker detected a real face with roughly 10 ms
  processing latency on the tested machine;
- the worker published all 21 normalized landmarks for a real detected hand,
  which the UI can draw as a toggleable canvas overlay without re-encoding the
  preview or modifying `/dev/video42`;
- a physically held open palm emitted `gesture.open_palm.held` at 67.6%
  confidence after the calibrated 60% trigger threshold and activated the
  configured `open-palm-demo` scenario;
- the HTTP snapshot returned a valid 640x360 JPEG;
- a real MCP stdio client handshake listed all eleven tools and returned both
  live structured camera state and a JPEG snapshot;
- the embedded UI was rendered against the live daemon at desktop size and
  showed the real preview, telemetry, perception health, and events;
- one browser tab remained open across a complete daemon stop/start cycle,
  changed to `Reconnecting`, removed its stale image, then received a new
  640x360 MJPEG stream without a page reload;
- the video supervisor rebuilt a live synthetic GStreamer pipeline after a
  controlled EOS and recorded both failure and restart events;
- after the repair restart, the real 720p30 pipeline remained healthy for more
  than six minutes on the camera's 480 Mbit/s fallback link, passing 11,000
  frames without another USB event or required restart;
- all 29 daemon tests, 2 MCP tests, 6 Python tests, JavaScript syntax checks,
  formatting, lint, configuration, protocol, and API checks passed.

An extended run changed the camera result: after approximately six minutes of
2 Hz proprietary attitude queries during streaming, the device disconnected
and re-enumerated on USB. A later disconnect with polling disabled moved the
same camera from its SuperSpeed bus to the companion 480 Mbit/s bus, indicating
that vendor polling is not the only possible source of link loss. The current
video supervisor now rebuilds failed pipelines, while continuous vendor-query
polling remains disabled because its extended test is independently unsafe.

## Known limitations

- only the OBSBOT Tiny 2 and its tested Linux UVC/XU path have a real adapter;
- device discovery is configuration-driven; automatic recovery uses the stable
  configured path and has synthetic EOS coverage, but a physical unplug/reset
  recovery cycle and long soak have not yet been revalidated;
- the V4L2 loopback device must be created before startup;
- absolute movement is safely bounded but has not been calibrated for precise
  agreement between requested and settled angles;
- live vendor attitude polling can reset the tested camera during streaming;
  the safe default reports the last commanded target with explicit provenance;
- live tracking state is only known after Tarsier issues a tracking command;
- open-palm thresholds were calibrated for one operator and environment;
  broader lighting, distance, skin-tone, orientation, and operator coverage is
  still required;
- pipeline telemetry reports effective FPS, frame count, last frame, errors,
  and restart count, but not queue pressure or dropped-frame attribution;
- configuration changes require a restart and runtime state is not persisted;
- the API has no authentication because it binds to loopback only; remote
  exposure is unsupported;
- there is no system service, release packaging, multi-camera support, zoom
  control, or production soak test yet.

## Product principles and next work

- **Local first:** frames, telemetry, configuration, and perception remain on
  the machine unless an explicit future integration sends data elsewhere.
- **One device owner:** the daemon owns physical capture and proprietary
  control traffic; consumers use the virtual camera or public API.
- **Vendor-independent core:** camera-specific protocols remain behind
  adapters.
- **Stable semantic events:** scenarios consume debounced events, not raw
  landmarks or individual frames.
- **Agent safety:** camera actions are bounded, observable, serialized, and
  explicitly exposed.
- **Headless core:** the daemon and API are the product; the web UI is a local
  control surface.

The next focused increments are a safe live-attitude source (or an explicit
last-commanded product contract), USB recovery soak testing, and broader
gesture robustness testing. Background replacement, avatars, full-body pose,
speech, robotics, ROS, cloud video processing, and a large gesture vocabulary
remain outside the first version.
