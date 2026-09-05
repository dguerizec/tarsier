from __future__ import annotations

import json
import math
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import numpy as np


@dataclass(frozen=True)
class AvatarMotion:
    pitch: float = 0.0
    yaw: float = 0.0
    roll: float = 0.0
    blink_left: float = 0.0
    blink_right: float = 0.0
    jaw_open: float = 0.0
    smile: float = 0.0
    brow_raise: float = 0.0
    pucker: float = 0.0

    @classmethod
    def neutral(cls) -> AvatarMotion:
        return cls()


def _score(categories: dict[str, float], name: str) -> float:
    return float(np.clip(categories.get(name, 0.0), 0.0, 1.0))


def motion_from_mediapipe(result: Any) -> AvatarMotion | None:
    if not result.face_landmarks:
        return None
    categories = {
        category.category_name: float(category.score or 0.0)
        for category in result.face_blendshapes[0]
    }
    pitch = yaw = roll = 0.0
    if result.facial_transformation_matrixes:
        import cv2

        matrix = np.asarray(result.facial_transformation_matrixes[0], dtype=np.float32)
        pitch_degrees, yaw_degrees, roll_degrees = cv2.RQDecomp3x3(matrix[:3, :3])[0]
        pitch = float(np.clip(math.radians(pitch_degrees), -0.45, 0.45))
        yaw = float(np.clip(math.radians(yaw_degrees), -0.65, 0.65))
        roll = float(np.clip(math.radians(roll_degrees), -0.35, 0.35))
    return AvatarMotion(
        pitch=pitch,
        yaw=yaw,
        roll=roll,
        blink_left=_score(categories, "eyeBlinkLeft"),
        blink_right=_score(categories, "eyeBlinkRight"),
        jaw_open=_score(categories, "jawOpen"),
        smile=max(_score(categories, "mouthSmileLeft"), _score(categories, "mouthSmileRight")),
        brow_raise=max(
            _score(categories, "browInnerUp"),
            _score(categories, "browOuterUpLeft"),
            _score(categories, "browOuterUpRight"),
        ),
        pucker=_score(categories, "mouthPucker"),
    )


def _hex_color(value: str) -> tuple[float, float, float]:
    if len(value) != 7 or not value.startswith("#"):
        raise ValueError(f"invalid RGB color: {value}")
    return tuple(int(value[index : index + 2], 16) / 255.0 for index in (1, 3, 5))


def _load_profile(path: Path) -> dict[str, tuple[float, float, float]]:
    source = json.loads(path.read_text())
    required = {
        "skin",
        "skin_shadow",
        "hair",
        "hair_grey",
        "beard",
        "beard_grey",
        "eyes",
        "glasses",
        "shirt",
        "shirt_accent",
        "wall",
        "wall_accent",
        "window",
        "outline",
    }
    missing = sorted(required - source.keys())
    if missing:
        raise ValueError(f"avatar profile is missing colors: {', '.join(missing)}")
    return {name: _hex_color(source[name]) for name in required}


def _normalize(vector: np.ndarray) -> np.ndarray:
    length = np.linalg.norm(vector)
    return vector if length == 0 else vector / length


def _perspective(fov_y: float, aspect: float, near: float, far: float) -> np.ndarray:
    scale = 1.0 / math.tan(fov_y / 2.0)
    return np.array(
        [
            [scale / aspect, 0.0, 0.0, 0.0],
            [0.0, scale, 0.0, 0.0],
            [0.0, 0.0, (far + near) / (near - far), (2.0 * far * near) / (near - far)],
            [0.0, 0.0, -1.0, 0.0],
        ],
        dtype=np.float32,
    )


def _look_at(eye: tuple[float, float, float], target: tuple[float, float, float]) -> np.ndarray:
    eye_vector = np.asarray(eye, dtype=np.float32)
    forward = _normalize(np.asarray(target, dtype=np.float32) - eye_vector)
    side = _normalize(np.cross(forward, np.array([0.0, 1.0, 0.0], dtype=np.float32)))
    up = np.cross(side, forward)
    view = np.eye(4, dtype=np.float32)
    view[0, :3] = side
    view[1, :3] = up
    view[2, :3] = -forward
    view[:3, 3] = -view[:3, :3] @ eye_vector
    return view


def _translate(x: float, y: float, z: float) -> np.ndarray:
    matrix = np.eye(4, dtype=np.float32)
    matrix[:3, 3] = (x, y, z)
    return matrix


def _scale(x: float, y: float, z: float) -> np.ndarray:
    return np.diag([x, y, z, 1.0]).astype(np.float32)


def _rotate_x(angle: float) -> np.ndarray:
    cosine, sine = math.cos(angle), math.sin(angle)
    return np.array(
        [[1, 0, 0, 0], [0, cosine, -sine, 0], [0, sine, cosine, 0], [0, 0, 0, 1]],
        dtype=np.float32,
    )


def _rotate_y(angle: float) -> np.ndarray:
    cosine, sine = math.cos(angle), math.sin(angle)
    return np.array(
        [[cosine, 0, sine, 0], [0, 1, 0, 0], [-sine, 0, cosine, 0], [0, 0, 0, 1]],
        dtype=np.float32,
    )


def _rotate_z(angle: float) -> np.ndarray:
    cosine, sine = math.cos(angle), math.sin(angle)
    return np.array(
        [[cosine, -sine, 0, 0], [sine, cosine, 0, 0], [0, 0, 1, 0], [0, 0, 0, 1]],
        dtype=np.float32,
    )


def _transform(
    position: tuple[float, float, float],
    scale: tuple[float, float, float],
    rotation: tuple[float, float, float] = (0.0, 0.0, 0.0),
) -> np.ndarray:
    return (
        _translate(*position)
        @ _rotate_z(rotation[2])
        @ _rotate_y(rotation[1])
        @ _rotate_x(rotation[0])
        @ _scale(*scale)
    )


def _sphere(rows: int = 20, columns: int = 28) -> tuple[np.ndarray, np.ndarray]:
    vertices: list[float] = []
    indices: list[int] = []
    for row in range(rows + 1):
        latitude = math.pi * row / rows
        y = math.cos(latitude)
        radius = math.sin(latitude)
        for column in range(columns + 1):
            longitude = 2.0 * math.pi * column / columns
            x = radius * math.cos(longitude)
            z = radius * math.sin(longitude)
            vertices.extend((x, y, z, x, y, z))
    for row in range(rows):
        for column in range(columns):
            first = row * (columns + 1) + column
            second = first + columns + 1
            indices.extend((first, second, first + 1, second, second + 1, first + 1))
    return np.asarray(vertices, dtype=np.float32), np.asarray(indices, dtype=np.int32)


def _cube() -> tuple[np.ndarray, np.ndarray]:
    faces = (
        ((0, 0, 1), ((-1, -1, 1), (1, -1, 1), (1, 1, 1), (-1, 1, 1))),
        ((0, 0, -1), ((1, -1, -1), (-1, -1, -1), (-1, 1, -1), (1, 1, -1))),
        ((1, 0, 0), ((1, -1, 1), (1, -1, -1), (1, 1, -1), (1, 1, 1))),
        ((-1, 0, 0), ((-1, -1, -1), (-1, -1, 1), (-1, 1, 1), (-1, 1, -1))),
        ((0, 1, 0), ((-1, 1, 1), (1, 1, 1), (1, 1, -1), (-1, 1, -1))),
        ((0, -1, 0), ((-1, -1, -1), (1, -1, -1), (1, -1, 1), (-1, -1, 1))),
    )
    vertices: list[float] = []
    indices: list[int] = []
    for normal, corners in faces:
        offset = len(vertices) // 6
        for corner in corners:
            vertices.extend((*corner, *normal))
        indices.extend((offset, offset + 1, offset + 2, offset, offset + 2, offset + 3))
    return np.asarray(vertices, dtype=np.float32), np.asarray(indices, dtype=np.int32)


def _cone(segments: int = 28) -> tuple[np.ndarray, np.ndarray]:
    vertices: list[float] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0]
    indices: list[int] = []
    slope = 0.65
    for index in range(segments + 1):
        angle = 2.0 * math.pi * index / segments
        x, y = math.cos(angle), math.sin(angle)
        normal = _normalize(np.array([x, y, slope], dtype=np.float32))
        vertices.extend((x, y, -1.0, *normal))
    for index in range(segments):
        indices.extend((0, index + 1, index + 2))
    base_center = len(vertices) // 6
    vertices.extend((0.0, 0.0, -1.0, 0.0, 0.0, -1.0))
    for index in range(segments + 1):
        angle = 2.0 * math.pi * index / segments
        vertices.extend((math.cos(angle), math.sin(angle), -1.0, 0.0, 0.0, -1.0))
    for index in range(segments):
        indices.extend((base_center, base_center + index + 2, base_center + index + 1))
    return np.asarray(vertices, dtype=np.float32), np.asarray(indices, dtype=np.int32)


class _Mesh:
    def __init__(self, context: Any, program: Any, geometry: tuple[np.ndarray, np.ndarray]) -> None:
        vertices, indices = geometry
        self._vertices = context.buffer(vertices.tobytes())
        self._indices = context.buffer(indices.tobytes())
        self._array = context.vertex_array(
            program,
            [(self._vertices, "3f 3f", "in_position", "in_normal")],
            self._indices,
        )

    def render(self) -> None:
        self._array.render()

    def release(self) -> None:
        self._array.release()
        self._indices.release()
        self._vertices.release()


class Stylized3DAvatarEngine:
    def __init__(self, profile: Path, width: int, height: int) -> None:
        import moderngl

        self._moderngl = moderngl
        self._width = width
        self._height = height
        self._colors = _load_profile(profile)
        self._context = moderngl.create_standalone_context(backend="egl")
        self._context.enable(moderngl.DEPTH_TEST | moderngl.CULL_FACE)
        self._program = self._context.program(
            vertex_shader="""
                #version 330
                uniform mat4 model;
                uniform mat4 view_projection;
                uniform mat3 normal_matrix;
                in vec3 in_position;
                in vec3 in_normal;
                out vec3 world_normal;
                out vec3 world_position;
                void main() {
                    vec4 position = model * vec4(in_position, 1.0);
                    world_position = position.xyz;
                    world_normal = normalize(normal_matrix * in_normal);
                    gl_Position = view_projection * position;
                }
            """,
            fragment_shader="""
                #version 330
                uniform vec3 base_color;
                uniform vec3 outline_color;
                uniform vec3 camera_position;
                in vec3 world_normal;
                in vec3 world_position;
                out vec4 fragment_color;
                void main() {
                    vec3 normal = normalize(world_normal);
                    vec3 light = normalize(vec3(-0.45, 0.85, 0.75));
                    float diffuse = max(dot(normal, light), 0.0);
                    float band = diffuse > 0.72 ? 1.0 : (diffuse > 0.35 ? 0.78 : 0.58);
                    vec3 view_direction = normalize(camera_position - world_position);
                    float edge = smoothstep(0.13, 0.34, abs(dot(normal, view_direction)));
                    vec3 color = mix(outline_color, base_color * band, edge);
                    fragment_color = vec4(color, 1.0);
                }
            """,
        )
        self._meshes = {
            "sphere": _Mesh(self._context, self._program, _sphere()),
            "cube": _Mesh(self._context, self._program, _cube()),
            "cone": _Mesh(self._context, self._program, _cone()),
        }
        self._framebuffer = self._context.simple_framebuffer((width, height), components=4)
        self._projection = _perspective(math.radians(31.0), width / height, 0.1, 100.0)
        self._camera_position = (0.0, 0.0, 7.2)
        self._view = _look_at(self._camera_position, (0.0, -0.35, 0.0))
        self._view_projection = self._projection @ self._view
        self._smoothed = AvatarMotion.neutral()

    def _draw(
        self,
        mesh: str,
        model: np.ndarray,
        color: tuple[float, float, float],
    ) -> None:
        normal_matrix = np.linalg.inv(model[:3, :3]).T.astype(np.float32)
        self._program["model"].write(model.T.astype(np.float32).tobytes())
        self._program["view_projection"].write(self._view_projection.T.astype(np.float32).tobytes())
        self._program["normal_matrix"].write(normal_matrix.T.tobytes())
        self._program["base_color"].value = color
        self._program["outline_color"].value = self._colors["outline"]
        self._program["camera_position"].value = self._camera_position
        self._meshes[mesh].render()

    def _smooth(self, target: AvatarMotion | None) -> AvatarMotion:
        target = target or AvatarMotion.neutral()
        current = self._smoothed
        factor = 0.34
        values = {
            name: getattr(current, name) + (getattr(target, name) - getattr(current, name)) * factor
            for name in AvatarMotion.__dataclass_fields__
        }
        self._smoothed = AvatarMotion(**values)
        return self._smoothed

    def _draw_room(self) -> None:
        colors = self._colors
        self._draw("cube", _transform((0, 0, -3.0), (5.8, 3.3, 0.12)), colors["wall"])
        self._draw(
            "cube", _transform((-4.25, 0.15, -2.65), (1.1, 3.0, 0.10)), colors["wall_accent"]
        )
        self._draw("cube", _transform((3.55, 0.55, -2.65), (1.38, 1.6, 0.08)), colors["window"])
        frame_color = colors["outline"]
        for x in (2.18, 4.92):
            self._draw("cube", _transform((x, 0.55, -2.48), (0.055, 1.65, 0.055)), frame_color)
        for y in (-1.03, 2.13):
            self._draw("cube", _transform((3.55, y, -2.48), (1.43, 0.055, 0.055)), frame_color)
        self._draw("cube", _transform((3.55, 0.55, -2.45), (0.04, 1.58, 0.04)), frame_color)
        self._draw("cube", _transform((3.55, 0.55, -2.44), (1.36, 0.04, 0.04)), frame_color)
        self._draw("cube", _transform((-3.35, -1.1, -2.35), (0.75, 0.08, 0.30)), colors["hair"])
        self._draw("cube", _transform((-3.35, -0.2, -2.35), (0.75, 0.08, 0.30)), colors["hair"])
        for x, y, scale in ((-3.7, -0.62, 0.30), (-3.25, -0.62, 0.38), (-2.92, -0.62, 0.25)):
            self._draw(
                "sphere", _transform((x, y, -2.15), (scale, 0.54, scale)), colors["wall_accent"]
            )
        self._draw("cube", _transform((0, -2.65, -0.3), (5.8, 0.2, 3.0)), colors["skin_shadow"])

    def _draw_avatar(self, motion: AvatarMotion) -> None:
        colors = self._colors
        self._draw("sphere", _transform((0, -1.75, -0.15), (1.65, 1.12, 0.75)), colors["shirt"])
        self._draw(
            "sphere", _transform((0, -0.82, -0.06), (0.37, 0.55, 0.34)), colors["skin_shadow"]
        )
        self._draw(
            "cube",
            _transform((-0.28, -1.12, 0.54), (0.34, 0.07, 0.06), (0, 0, -0.3)),
            colors["shirt_accent"],
        )
        self._draw(
            "cube",
            _transform((0.28, -1.12, 0.54), (0.34, 0.07, 0.06), (0, 0, 0.3)),
            colors["shirt_accent"],
        )

        head = (
            _translate(0.0, 0.35, 0.0)
            @ _rotate_z(-motion.roll * 0.85)
            @ _rotate_y(-motion.yaw * 0.9)
            @ _rotate_x(motion.pitch * 0.75)
        )

        def local(
            mesh: str,
            position: tuple[float, float, float],
            scale: tuple[float, float, float],
            color: tuple[float, float, float],
            rotation: tuple[float, float, float] = (0.0, 0.0, 0.0),
        ) -> None:
            self._draw(mesh, head @ _transform(position, scale, rotation), color)

        local("sphere", (0, 0.15, 0), (0.92, 1.15, 0.80), colors["skin"])
        local("sphere", (-0.92, 0.10, -0.03), (0.18, 0.31, 0.14), colors["skin_shadow"])
        local("sphere", (0.92, 0.10, -0.03), (0.18, 0.31, 0.14), colors["skin_shadow"])

        # Short, slightly receding hair and two grey temple accents.
        local("sphere", (0, 0.92, -0.08), (0.86, 0.43, 0.73), colors["hair"])
        local("sphere", (-0.68, 0.58, 0.03), (0.18, 0.32, 0.58), colors["hair_grey"])
        local("sphere", (0.68, 0.58, 0.03), (0.18, 0.32, 0.58), colors["hair_grey"])

        # Beard volumes preserve a solid silhouette during large head rotations.
        local("sphere", (0, -0.50, 0.28), (0.72, 0.67, 0.58), colors["beard"])
        local(
            "cone",
            (0, -0.73 - motion.jaw_open * 0.08, 0.34),
            (0.48, 0.38, 0.58),
            colors["beard"],
            rotation=(math.pi / 2, 0, 0),
        )
        local(
            "cone",
            (0.0, -0.73, 0.67),
            (0.07, 0.07, 0.50),
            colors["beard_grey"],
            rotation=(math.pi / 2, 0, 0),
        )
        local("sphere", (-0.25, -0.25, 0.84), (0.27, 0.13, 0.07), colors["beard"])
        local("sphere", (0.25, -0.25, 0.84), (0.27, 0.13, 0.07), colors["beard"])

        # Nose, eyes, pupils, brows, and glasses all inherit the head transform.
        local("cone", (0, 0.05, 0.88), (0.16, 0.15, 0.32), colors["skin_shadow"])
        eye_height_left = max(0.025, 0.17 * (1.0 - motion.blink_left * 0.92))
        eye_height_right = max(0.025, 0.17 * (1.0 - motion.blink_right * 0.92))
        for x, eye_height in ((-0.36, eye_height_left), (0.36, eye_height_right)):
            local("sphere", (x, 0.29, 0.73), (0.23, eye_height, 0.13), (0.96, 0.94, 0.88))
            local(
                "sphere",
                (x, 0.29, 0.86),
                (0.085, max(0.025, eye_height * 0.58), 0.035),
                colors["eyes"],
            )
            local(
                "sphere",
                (x, 0.29, 0.90),
                (0.035, max(0.018, eye_height * 0.34), 0.018),
                colors["outline"],
            )
        brow_y = 0.57 + motion.brow_raise * 0.13
        local(
            "cube",
            (-0.36, brow_y, 0.79),
            (0.28, 0.045, 0.045),
            colors["hair"],
            rotation=(0, 0, -0.10),
        )
        local(
            "cube",
            (0.36, brow_y, 0.79),
            (0.28, 0.045, 0.045),
            colors["hair"],
            rotation=(0, 0, 0.10),
        )

        for x in (-0.36, 0.36):
            local("cube", (x, 0.49, 0.91), (0.29, 0.025, 0.025), colors["glasses"])
            local("cube", (x, 0.08, 0.91), (0.29, 0.025, 0.025), colors["glasses"])
            local("cube", (x - 0.29, 0.285, 0.91), (0.025, 0.18, 0.025), colors["glasses"])
            local("cube", (x + 0.29, 0.285, 0.91), (0.025, 0.18, 0.025), colors["glasses"])
        local("cube", (0, 0.285, 0.91), (0.075, 0.025, 0.025), colors["glasses"])
        local(
            "cube",
            (-0.71, 0.28, 0.83),
            (0.18, 0.025, 0.025),
            colors["glasses"],
            rotation=(0, -0.18, 0),
        )
        local(
            "cube",
            (0.71, 0.28, 0.83),
            (0.18, 0.025, 0.025),
            colors["glasses"],
            rotation=(0, 0.18, 0),
        )

        mouth_width = 0.34 * (1.0 - motion.pucker * 0.45)
        mouth_height = 0.035 + motion.jaw_open * 0.18
        mouth_y = -0.43 - motion.jaw_open * 0.04
        local("sphere", (0, mouth_y, 0.91), (mouth_width, mouth_height, 0.035), colors["outline"])
        corner_y = mouth_y + motion.smile * 0.08
        for x, angle in ((-mouth_width, motion.smile * 0.35), (mouth_width, -motion.smile * 0.35)):
            local(
                "cube",
                (x, corner_y, 0.90),
                (0.075, 0.018, 0.018),
                colors["outline"],
                rotation=(0, 0, angle),
            )

    def render(self, target: AvatarMotion | None) -> np.ndarray:
        motion = self._smooth(target)
        self._framebuffer.use()
        self._context.viewport = (0, 0, self._width, self._height)
        self._context.clear(0.72, 0.79, 0.78, 1.0, depth=1.0)
        self._draw_room()
        self._draw_avatar(motion)
        rgba = np.frombuffer(
            self._framebuffer.read(components=4, alignment=1), dtype=np.uint8
        ).reshape(self._height, self._width, 4)
        rgba = np.flipud(rgba)
        return np.ascontiguousarray(rgba[..., [2, 1, 0, 3]])

    def close(self) -> None:
        for mesh in self._meshes.values():
            mesh.release()
        self._program.release()
        self._framebuffer.release()
        self._context.release()

    def __enter__(self) -> Stylized3DAvatarEngine:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()
