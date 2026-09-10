"""Explicit MediaPipe task delegates, with no application-level CPU fallback."""
from dataclasses import dataclass
from pathlib import Path

import mediapipe as mp


@dataclass(frozen=True)
class Delegates:
    face: str = "cpu"
    hands: str = "cpu"
    pose: str = "cpu"
    segmentation: str = "cpu"
    avatar_face: str = "cpu"

    def __post_init__(self):
        for name, value in vars(self).items():
            if value not in {"cpu", "gpu"}:
                raise ValueError(f"Unsupported {name} delegate: {value}")


DEFAULT_DELEGATES = Delegates()


def base_options(model: Path, delegate: str):
    if delegate not in {"cpu", "gpu"}:
        raise ValueError(f"Unsupported MediaPipe delegate: {delegate}")
    return mp.tasks.BaseOptions(
        model_asset_path=str(model),
        delegate=mp.tasks.BaseOptions.Delegate[delegate.upper()],
    )
