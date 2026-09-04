from __future__ import annotations

import hashlib
import urllib.request
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class ModelAsset:
    filename: str
    url: str
    sha256: str


MODEL_ASSETS = (
    ModelAsset(
        "gesture_recognizer.task",
        "https://storage.googleapis.com/mediapipe-models/gesture_recognizer/"
        "gesture_recognizer/float16/1/gesture_recognizer.task",
        "97952348cf6a6a4915c2ea1496b4b37ebabc50cbbf80571435643c455f2b0482",
    ),
    ModelAsset(
        "blaze_face_short_range.tflite",
        "https://storage.googleapis.com/mediapipe-models/face_detector/"
        "blaze_face_short_range/float16/1/blaze_face_short_range.tflite",
        "b4578f35940bf5a1a655214a1cce5cab13eba73c1297cd78e1a04c2380b0152f",
    ),
)


def default_model_dir() -> Path:
    return Path.home() / ".cache" / "tarsier" / "models"


def download_models(model_dir: Path, *, force: bool = False) -> list[Path]:
    model_dir.mkdir(parents=True, exist_ok=True)
    paths: list[Path] = []
    for asset in MODEL_ASSETS:
        destination = model_dir / asset.filename
        if destination.is_file() and _sha256(destination) == asset.sha256 and not force:
            paths.append(destination)
            continue

        temporary = destination.with_suffix(destination.suffix + ".part")
        with (
            urllib.request.urlopen(asset.url, timeout=60) as response,  # noqa: S310
            temporary.open("wb") as output,
        ):
            while chunk := response.read(1024 * 1024):
                output.write(chunk)
        actual_sha256 = _sha256(temporary)
        if actual_sha256 != asset.sha256:
            temporary.unlink(missing_ok=True)
            raise RuntimeError(
                f"model checksum mismatch for {asset.filename}: expected {asset.sha256}, "
                f"received {actual_sha256}"
            )
        temporary.replace(destination)
        paths.append(destination)
    return paths


def describe_models(model_dir: Path) -> list[dict[str, str | int | bool]]:
    descriptions = []
    for asset in MODEL_ASSETS:
        path = model_dir / asset.filename
        exists = path.is_file()
        descriptions.append(
            {
                "name": asset.filename,
                "path": str(path),
                "exists": exists,
                "bytes": path.stat().st_size if exists else 0,
                "sha256": _sha256(path) if exists else "",
                "verified": exists and _sha256(path) == asset.sha256,
            }
        )
    return descriptions


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()
