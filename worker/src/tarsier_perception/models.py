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
        "face_landmarker.task",
        "https://storage.googleapis.com/mediapipe-models/face_landmarker/"
        "face_landmarker/float16/1/face_landmarker.task",
        "64184e229b263107bc2b804c6625db1341ff2bb731874b0bcc2fe6544e0bc9ff",
    ),
    ModelAsset(
        "pose_landmarker_lite.task",
        "https://storage.googleapis.com/mediapipe-models/pose_landmarker/"
        "pose_landmarker_lite/float16/1/pose_landmarker_lite.task",
        "59929e1d1ee95287735ddd833b19cf4ac46d29bc7afddbbf6753c459690d574a",
    ),
    ModelAsset(
        "selfie_segmenter.tflite",
        "https://storage.googleapis.com/mediapipe-models/image_segmenter/"
        "selfie_segmenter/float16/1/selfie_segmenter.tflite",
        "191ac9529ae506ee0beefa6b2c945a172dab9d07d1e802a290a4e4038226658b",
    ),
)

AVATAR_MODEL_ASSETS = (
    ModelAsset(
        "liveportrait/base_models/appearance_feature_extractor.pth",
        "https://huggingface.co/KlingTeam/LivePortrait/resolve/main/"
        "liveportrait/base_models/appearance_feature_extractor.pth",
        "5279bb8654293dbdf327030b397f107237dd9212fb11dd75b83dfb635211ceb5",
    ),
    ModelAsset(
        "liveportrait/base_models/motion_extractor.pth",
        "https://huggingface.co/KlingTeam/LivePortrait/resolve/main/"
        "liveportrait/base_models/motion_extractor.pth",
        "251e6a94ad667a1d0c69526d292677165110ef7f0cf0f6d199f0e414e8aa0ca5",
    ),
    ModelAsset(
        "liveportrait/base_models/spade_generator.pth",
        "https://huggingface.co/KlingTeam/LivePortrait/resolve/main/"
        "liveportrait/base_models/spade_generator.pth",
        "4780afc7909a9f84e24c01d73b31a555ef651521a1fe3b2429bd04534d992aee",
    ),
    ModelAsset(
        "liveportrait/base_models/warping_module.pth",
        "https://huggingface.co/KlingTeam/LivePortrait/resolve/main/"
        "liveportrait/base_models/warping_module.pth",
        "2f61a6f265fe344f14132364859a78bdbbc2068577170693da57fb96d636e282",
    ),
    ModelAsset(
        "liveportrait/retargeting_models/stitching_retargeting_module.pth",
        "https://huggingface.co/KlingTeam/LivePortrait/resolve/main/"
        "liveportrait/retargeting_models/stitching_retargeting_module.pth",
        "3652d5a3f95099141a56986aaddec92fadf0a73c87a20fac9a2c07c32b28b611",
    ),
)


def default_model_dir() -> Path:
    return Path.home() / ".cache" / "tarsier" / "models"


def download_models(
    model_dir: Path, *, force: bool = False, include_avatar: bool = False
) -> list[Path]:
    model_dir.mkdir(parents=True, exist_ok=True)
    paths: list[Path] = []
    for asset in _model_assets(include_avatar):
        destination = model_dir / asset.filename
        if destination.is_file() and _sha256(destination) == asset.sha256 and not force:
            paths.append(destination)
            continue

        destination.parent.mkdir(parents=True, exist_ok=True)
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


def describe_models(
    model_dir: Path, *, include_avatar: bool = False
) -> list[dict[str, str | int | bool]]:
    descriptions = []
    for asset in _model_assets(include_avatar):
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


def _model_assets(include_avatar: bool) -> tuple[ModelAsset, ...]:
    return MODEL_ASSETS + (AVATAR_MODEL_ASSETS if include_avatar else ())


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()
