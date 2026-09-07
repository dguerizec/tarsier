from __future__ import annotations

import math
from dataclasses import dataclass
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
        pitch, yaw, roll = map(math.radians, (pitch_degrees, yaw_degrees, roll_degrees))
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
    )
