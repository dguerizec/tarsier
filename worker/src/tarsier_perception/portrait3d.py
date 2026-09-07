"""Photographic portrait rendering with a synchronized, straight-alpha silhouette."""

from __future__ import annotations

import json
import math
import time
from pathlib import Path

import cv2
import numpy as np

from .avatar_motion import AvatarMotion

MORPHS = (
    "eyeBlinkLeft",
    "eyeBlinkRight",
    "jawOpen",
    "mouthSmileLeft",
    "mouthSmileRight",
    "browInnerUp",
    "browOuterUpLeft",
    "browOuterUpRight",
)


def load_asset(directory: Path) -> tuple[dict, dict[str, np.ndarray]]:
    manifest = json.loads((directory / "manifest.json").read_text())
    if manifest.get("schema_version") != 1 or tuple(manifest.get("morphs", [])) != MORPHS:
        raise ValueError("unsupported portrait asset schema or facial expressions")
    if not manifest.get("draws"):
        raise ValueError("portrait asset has no draw surfaces")
    with np.load(directory / "mesh.npz", allow_pickle=False) as source:
        arrays = {draw["array"]: source[draw["array"]] for draw in manifest["draws"]}
    for draw in manifest["draws"]:
        data = arrays[draw["array"]]
        if (
            data.dtype != np.float32
            or data.ndim != 2
            or data.shape[1] != 30
            or not len(data)
            or len(data) % 3
            or not np.isfinite(data).all()
        ):
            raise ValueError("invalid portrait triangle stream")
        if np.any((data[:, 5] < 0) | (data[:, 5] > 1)):
            raise ValueError("invalid portrait skin weights")
        texture = draw.get("texture")
        if texture and (Path(texture).name != texture or not (directory / texture).is_file()):
            raise ValueError("portrait texture must be a local asset file")
        if len(draw.get("color", [])) != 3 or not np.isfinite(draw["color"]).all():
            raise ValueError("invalid portrait material color")
    return manifest, arrays


class Portrait3DAvatarEngine:
    def __init__(self, directory: Path, width: int, height: int) -> None:
        import moderngl

        self.width, self.height = width, height
        self.manifest, arrays = load_asset(directory)
        self._pose = None
        self._last_frame = None
        self._last = time.monotonic()
        self.ctx = moderngl.create_standalone_context(backend="egl")
        self.ctx.enable(moderngl.DEPTH_TEST)
        self._resources = []
        try:
            self.frame = self.ctx.simple_framebuffer((width * 2, height * 2), components=4)
            self._resources.append(self.frame)
            declarations = "\n".join(f"in vec3 d{i}; uniform float w{i};" for i in range(8))
            offset = "+".join(f"d{i}*w{i}" for i in range(8))
            vertex = (
                """#version 330
in vec3 position; in vec2 uv; in float head;
uniform mat3 rotation; uniform float aspect;
out vec2 texcoord;
"""
                + declarations
                + "\nvoid main(){vec3 p=position+"
                + offset
                + """;
vec3 pivot=vec3(0,.2,-.22);p=mix(p,rotation*(p-pivot)+pivot,head);
gl_Position=vec4(p.x/(.88*aspect),(p.z-.10)/.88,p.y/5.,1);texcoord=uv;}
"""
            )
            fragment = """#version 330
in vec2 texcoord; uniform sampler2D photo; uniform bool textured;
uniform vec3 color; out vec4 frag;
void main(){vec3 c=textured?texture(photo,texcoord).rgb:
mix(12.92*color,1.055*pow(color,vec3(1./2.4))-.055,step(vec3(.0031308),color));
frag=vec4(c,1);}
"""
            self.program = self.ctx.program(vertex_shader=vertex, fragment_shader=fragment)
            self._resources.append(self.program)
            self.program["aspect"].value = width / height
            self.draws = []
            textures = {}
            for draw in self.manifest["draws"]:
                buffer = self.ctx.buffer(arrays[draw["array"]].tobytes())
                self._resources.append(buffer)
                vao = self.ctx.vertex_array(
                    self.program,
                    [
                        (
                            buffer,
                            "3f 2f 1f " + "3f " * 8,
                            "position",
                            "uv",
                            "head",
                            *[f"d{i}" for i in range(8)],
                        )
                    ],
                )
                self._resources.append(vao)
                name = draw.get("texture")
                if name and name not in textures:
                    image = cv2.imread(str(directory / name), cv2.IMREAD_COLOR)
                    if image is None:
                        raise ValueError(f"cannot decode portrait texture: {name}")
                    image = cv2.cvtColor(image, cv2.COLOR_BGR2RGB)
                    texture = self.ctx.texture(
                        (image.shape[1], image.shape[0]), 3, np.flipud(image).tobytes()
                    )
                    texture.filter = (moderngl.LINEAR, moderngl.LINEAR)
                    self._resources.append(texture)
                    textures[name] = texture
                self.draws.append((vao, textures.get(name), tuple(draw["color"])))
        except BaseException:
            self.close()
            raise

    def render(self, motion: AvatarMotion | None) -> np.ndarray:
        now = time.monotonic()
        if motion is None:
            self._last = now
            # Keep the last rendered identity and its alpha while the driver is occluded.
            if self._last_frame is not None:
                return self._last_frame.copy()
            return np.zeros((self.height, self.width, 4), np.uint8)
        self.frame.use()
        self.frame.clear(0, 0, 0, 0, depth=1)
        pose = np.array([motion.pitch, motion.yaw, -motion.roll])
        if self._pose is None:
            self._pose = pose.copy()
        # Use camera-relative angles, never a new neutral pose on reacquisition.
        # Stabilize pose only; rapid blinks and speech remain unfiltered.
        factor = 1 - math.exp(-min(now - self._last, 0.2) / 0.065)
        self._last = now
        self._pose += (pose - self._pose) * factor
        pitch, yaw, roll = self._pose
        cx, sx = math.cos(pitch), math.sin(pitch)
        cz, sz = math.cos(yaw), math.sin(yaw)
        cy, sy = math.cos(roll), math.sin(roll)
        rotation = (
            np.array([[cy, 0, sy], [0, 1, 0], [-sy, 0, cy]])
            @ np.array([[cz, -sz, 0], [sz, cz, 0], [0, 0, 1]])
            @ np.array([[1, 0, 0], [0, cx, -sx], [0, sx, cx]])
        )
        self.program["rotation"].write(rotation.astype("f4").T.tobytes())
        weights = [
            motion.blink_left,
            motion.blink_right,
            motion.jaw_open,
            motion.smile,
            motion.smile,
            motion.brow_raise,
            motion.brow_raise,
            motion.brow_raise,
        ]
        for i, weight in enumerate(weights):
            self.program[f"w{i}"].value = float(np.clip(weight, 0, 1))
        for vao, texture, color in self.draws:
            self.program["textured"].value = texture is not None
            self.program["color"].value = color
            if texture:
                texture.use(0)
            vao.render()
        rgba = np.frombuffer(self.frame.read(components=4, alignment=1), np.uint8).reshape(
            self.height * 2, self.width * 2, 4
        )[::-1]
        rgba = cv2.resize(rgba, (self.width, self.height), interpolation=cv2.INTER_AREA)
        # Downsampling produces premultiplied edge colors; the daemon expects straight BGRA.
        alpha = rgba[:, :, 3:4].astype(np.float32)
        rgba[:, :, :3] = np.clip(
            rgba[:, :, :3].astype(np.float32) * 255 / np.maximum(alpha, 1), 0, 255
        ).astype(np.uint8)
        self._last_frame = cv2.cvtColor(rgba, cv2.COLOR_RGBA2BGRA)
        return self._last_frame.copy()

    def close(self) -> None:
        for resource in reversed(self._resources):
            resource.release()
        self._resources.clear()
        self.ctx.release()

    def __enter__(self) -> Portrait3DAvatarEngine:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()
