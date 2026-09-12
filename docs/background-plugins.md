# Animated background packages

The core owns subject segmentation, composition, preview and virtual-camera
output. A separate generic Python/OpenGL worker renders the selected content
package. Only one background worker runs, and only while capture is running and
a shader background is enabled for Camera or Personal 3D. Shader rendering does
not import or run the perception worker.

## Install and select content

Kelp forest is bundled in the daemon. Additional packages are directories under
`$XDG_DATA_HOME/tarsier/backgrounds/<id>/` (normally
`~/.local/share/tarsier/backgrounds/<id>/`). Copy a package there, reload the web
page, and select it under **Animated landscape**. Installation does not select
or execute a package. The reserved `kelp` identifier always uses bundled content.

A version 1 package consists of `manifest.json` and one GLSL 330 fragment shader:

```json
{
  "version": 1,
  "id": "my-landscape",
  "name": "My landscape",
  "shader": "landscape.frag",
  "width": 640,
  "height": 360,
  "fps": 24,
  "parameters": { "speed": 0.45, "relief": 1.0, "palette": 0 }
}
```

IDs contain lowercase ASCII letters, digits and hyphens (1–64 characters).
Render dimensions range from 16×16 to 1280×720, with 1–30 FPS. The core scales
the result to the output frame. Use the output's aspect ratio (normally 16:9).
The manifest is limited to 16 KiB, source to 128 KiB, and scalar parameters to 32.
Shader paths must resolve inside the package. Invalid packages are omitted from
the catalog and cannot be selected for rendering.

The worker draws a full-screen triangle. Supported uniforms are:

- `vec2 resolution`: render dimensions in pixels;
- `float time`: elapsed seconds multiplied by the `speed` parameter;
- named numeric uniforms from `parameters`: use JSON integers for GLSL `int`
  and JSON decimals for GLSL `float`.

Uniforms can be omitted when unused. Output opaque RGB through a fragment output
such as `out vec4 color`. Version 1 supports one pass and scalar parameters;
texture assets, multiple passes and parameter-editing UI are not implemented.
Edit a package's defaults and switch away/back to reload it.

`GET /api/v1/video/background/plugins` returns validated package metadata.
`POST /api/v1/video/background` accepts
`{"enabled":true,"effect":"shader","plugin":"my-landscape"}`.
Selection persists with the other video settings. Existing background requests
remain valid. State exposes `background_plugin`, `background_ready`,
`background_worker_pid`, `background_renderer` and `background_error`.

## Worker and IPC lifecycle

The daemon starts the embedded `tools/background_worker.py` source with
`worker/.venv/bin/python`; override the interpreter using
`TARSIER_BACKGROUND_PYTHON`. This environment needs `moderngl` (already included
in the perception environment) and a working OpenGL 3.3 EGL driver. The bundled
shader and script are embedded in the Rust binary and require a rebuild to change.

The parent sends validated package JSON plus shader source over stdin. The child
creates a size-sealed private Linux memfd and returns one bounded JSON handshake
containing its PID, descriptor number and GPU renderer. The parent opens that
child's `/proc/<pid>/fd/<fd>` read-only. Pixels do not travel through pipes.

Frames use the existing `TARSFRM1` 64-byte little-endian header: width (offset 8),
height (12), stride (16), rotation (20, always zero), publication sequence (24),
frame ID (32), timestamp in Unix milliseconds (40), payload length (48). Pixels
are top-down BGR with rows aligned to four bytes. The child publishes header and
pixels under an exclusive `flock`. The parent takes a nonblocking shared lock,
validates the header, and copies only new complete frames. Busy readers/writers
skip that iteration. This is latest-frame IPC, with no accumulating frame queue.

Each content/effect/identity change advances a core generation and invalidates
cached content. Frames from obsolete workers cannot enter the new generation.
The compositor never waits for the renderer: before the first background it uses
black; after a renderer failure it retains the last complete background while
compositing fresh subject frames. Existing missing-mask handling holds the last
safe composed output. No failure falls back to the raw camera.

The supervisor kills and reaps the worker on deselection, stopped capture or
shutdown. Exits and five seconds without a new frame trigger a bounded-delay
restart. Startup has a timeout. State exposes startup/render failures. The child
also checks for parent death between frames. A shader process is not a sandbox;
only install shader content you trust.

GPU readback and a CPU copy remain necessary in this version because the core
compositor uses CPU pixels. Shared texture transport and third-party executable
engines are future extensions; neither is required by the content package format.

## Validation

`cargo test --locked` covers package bounds, generation rejection, IPC lock
contention, API selection, and privacy composition. The opt-in EGL checks are:

```sh
cargo test --locked background::tests::gpu_ -- --ignored --nocapture
```

These use synthetic frames and do not open the physical camera. They verify GPU
animation, worker crash/restart, stopped-capture shutdown, and background disable.
