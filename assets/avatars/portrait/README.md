# Personal 3D identity

Select **Personal 3D** in Identity. The local MediaPipe tracker drives head
rotation, blinks, jaw opening, smiles, and eyebrows. Pose uses camera-relative
orientation and is smoothed; expressions are not smoothed. The first
version has fixed framing, approximate mouth deformation, and no independent
eye gaze. Losing face tracking holds the last portrait and silhouette until tracking
resumes, without recalibrating the center. Before the first detection, the
portrait is transparent. Extreme profiles can still exceed MediaPipe tracking
coverage; the renderer does not invent motion while tracking is unavailable.

The renderer sends straight BGRA pixels to `/api/v1/avatar/frame` with
`X-Tarsier-Avatar-Pixel-Format: bgra`. Image and silhouette share one frame ID
and timestamp and are published atomically. The daemon composites the existing
Green screen or Pixel Party background using alpha coverage, without a second
camera segmentation pass. Off and Blur show a neutral synthetic backdrop.
Stale generated output retains the existing privacy fallback; camera pixels
are never substituted for the portrait.

## Updating the model

The editable Blender scene is the source of truth. Keep its `Head` vertex group,
eight named shape keys, world coordinates, and head pivot `(0, .2, -.22)` stable.
The renderer consumes triangle streams rather than vertex indices shared with
an earlier version, so topology and texture changes do not require application
code changes. Large anatomy changes can require adjusting deformation weights.

For the current photographic scene, bake the corrected projection materials to
one atlas, then export the baked scene. Use new output directories for each
revision; the tools refuse to overwrite an existing directory.

```sh
blender --background /path/to/corrected.blend --python tools/bake_portrait.py -- \
  --output /path/to/baked-revision
blender --background /path/to/baked-revision/video-model-runtime-baked.blend \
  --python tools/export_portrait.py -- \
  --output /path/to/runtime-revision --revision my-revision
```

These tools target this portrait's objects: `Measured video head`,
`Beard edge fibers`, `Mouth cavity`, and `Upper tooth*`. The bake expects emission
photo materials and preserves existing projection coordinates before creating
`RuntimeUV`. Export operates on base mesh data and shape keys, not unapplied
modifiers. Check neutral and animated views before activating a revision.

Set `[avatar].portrait_model` to the exported directory and restart the daemon.
The default is `assets/avatars/portrait/current`, ignored by Git because it
contains personal assets. Keep the previous revision for rollback. To reload
assets at the same path, switch away from Personal 3D and back after replacing
the complete directory while it is inactive.

Schema 1 consists of `manifest.json`, `mesh.npz`, and local PNG textures.
Each triangle vertex has 30 float32 values: world position (3), UV (2), Head
weight (1), and eight world-space shape-key deltas (24). The manifest records
revision, ordered morph names, material colors, texture paths, and draw arrays.
The loader rejects unsupported schemas, malformed triangles, nonfinite values,
and textures outside the asset directory. Personal assets stay local.
