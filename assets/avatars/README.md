# Bundled LivePortrait source

`liveportrait-default.png` is a newly generated fictional adult portrait, created
on 2026-09-07 with OpenAI's built-in image generation tool. No reference images,
personal portraits, or third-party photographs were supplied. The generated PNG
is included unmodified. It is a project demo asset, not a depiction or endorsement
of the user or an intentionally represented real person.

Permission is granted to use, copy, modify, and redistribute this bundled image,
including commercially, without attribution. This permission concerns this image
only; the neural runtime and model weights have their own licenses.

## Configuration and framing

The daemon default and `config/tarsier.example.toml` use this bundled image.
Relative paths are resolved against the daemon's build-time project directory.
Set `[avatar].source_image` to another path to use a custom portrait. Existing
explicit values, including `assets/avatars/liveportrait-source.png`, are preserved;
there is no automatic migration or replacement of personal images. The folder
icon on the LivePortrait card can save a UI override outside the repository,
in the user settings. The running worker prepares the selected source while the
previous avatar stays visible, then switches on its first accepted frame. The
choice also takes precedence on subsequent starts; it does not overwrite images. Other PNGs
in this directory and `portrait/current/` remain ignored by Git.

The local LivePortrait implementation reads the image as BGR with OpenCV, takes
a center-square crop, converts to RGB, and downsamples to 256 × 256. It does not
perform source-face alignment. Keep one large, centered, frontal face with eyes,
mouth, jaw, and hair visible. The bundled square portrait uses a neutral expression
and plain background for this reason. Runtime dependencies, model downloads, and
a CUDA-capable GPU are still required; bundling the source does not install them.

## Generation prompt

> Create a square 1024x1024 photorealistic studio head-and-shoulders portrait of a wholly fictional adult person, around 35 years old, for a bundled default LivePortrait animation source. Invent the identity from scratch; no reference images, no real person or celebrity likeness. Straight-on frontal face, level head, looking directly at camera, relaxed neutral expression, lips gently closed, both eyes fully open, eyebrows and entire jaw clearly visible. Short tidy dark brown hair away from eyes, no glasses, no beard, no accessories. Warm medium skin tone, simple muted blue crewneck shirt. Entire head with comfortable margin above the hair, head and face large and centered, shoulders visible at bottom, symmetrical composition suitable for center-square crop and reduction to 256x256. Soft even studio lighting, plain pale warm-gray background, realistic natural facial anatomy and skin texture, crisp clear eyes and mouth. One person only. No hands, no text, no logos, no watermark.

The tool returned a 1254 × 1254 PNG despite the requested 1024 × 1024 size;
LivePortrait accepts it through the same square crop and 256 × 256 preprocessing.
