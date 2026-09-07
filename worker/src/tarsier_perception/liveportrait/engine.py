from __future__ import annotations

import gc
from collections import OrderedDict
from contextlib import nullcontext
from dataclasses import dataclass
from pathlib import Path
from threading import Lock
from typing import Any

import cv2
import numpy as np
import torch
import torch.nn.functional as functional
import yaml

from .modules.appearance_feature_extractor import AppearanceFeatureExtractor
from .modules.motion_extractor import MotionExtractor
from .modules.spade_generator import SPADEDecoder
from .modules.stitching_retargeting_network import StitchingRetargetingNetwork
from .modules.warping_network import WarpingNetwork


def center_square(image: np.ndarray) -> np.ndarray:
    height, width = image.shape[:2]
    size = min(height, width)
    left = (width - size) // 2
    top = (height - size) // 2
    return image[top : top + size, left : left + size]


def headpose_pred_to_degree(prediction: torch.Tensor) -> torch.Tensor:
    if prediction.ndim > 1 and prediction.shape[1] == 66:
        indices = torch.arange(66, dtype=torch.float32, device=prediction.device)
        prediction = functional.softmax(prediction, dim=1)
        return torch.sum(prediction * indices, dim=1) * 3 - 97.5
    return prediction


def rotation_matrix(
    pitch_degrees: torch.Tensor,
    yaw_degrees: torch.Tensor,
    roll_degrees: torch.Tensor,
) -> torch.Tensor:
    pitch = torch.deg2rad(pitch_degrees).reshape(-1, 1)
    yaw = torch.deg2rad(yaw_degrees).reshape(-1, 1)
    roll = torch.deg2rad(roll_degrees).reshape(-1, 1)
    ones = torch.ones_like(pitch)
    zeros = torch.zeros_like(pitch)
    rotation_x = torch.cat(
        [
            ones,
            zeros,
            zeros,
            zeros,
            torch.cos(pitch),
            -torch.sin(pitch),
            zeros,
            torch.sin(pitch),
            torch.cos(pitch),
        ],
        dim=1,
    ).reshape(-1, 3, 3)
    rotation_y = torch.cat(
        [
            torch.cos(yaw),
            zeros,
            torch.sin(yaw),
            zeros,
            ones,
            zeros,
            -torch.sin(yaw),
            zeros,
            torch.cos(yaw),
        ],
        dim=1,
    ).reshape(-1, 3, 3)
    rotation_z = torch.cat(
        [
            torch.cos(roll),
            -torch.sin(roll),
            zeros,
            torch.sin(roll),
            torch.cos(roll),
            zeros,
            zeros,
            zeros,
            ones,
        ],
        dim=1,
    ).reshape(-1, 3, 3)
    return (rotation_z @ rotation_y @ rotation_x).permute(0, 2, 1)


def _clean_state_dict(state_dict: dict[str, Any]) -> OrderedDict[str, Any]:
    return OrderedDict((key.removeprefix("module."), value) for key, value in state_dict.items())


class MotionStabilizer:
    """Low-pass head pose without delaying short-lived mouth expressions."""

    _KEYS = ("pitch", "yaw", "roll")

    def __init__(self, factor: float = 0.35) -> None:
        if not 0.0 < factor <= 1.0:
            raise ValueError("motion smoothing factor must be between zero and one")
        self._factor = factor
        self._state: dict[str, torch.Tensor] | None = None

    def update(self, motion: dict[str, torch.Tensor]) -> dict[str, torch.Tensor]:
        stabilized = motion.copy()
        if self._state is None:
            self._state = {key: motion[key].clone() for key in self._KEYS}
        else:
            self._state = {
                key: current + (motion[key] - current) * self._factor
                for key, current in self._state.items()
            }
        stabilized.update(self._state)
        return stabilized


def limit_relative_pose(
    motion: dict[str, torch.Tensor],
    initial: dict[str, torch.Tensor],
    *,
    strength: float = 0.35,
    pitch_limit: float = 8.0,
    yaw_limit: float = 12.0,
    roll_limit: float = 8.0,
) -> dict[str, torch.Tensor]:
    """Keep a single-source portrait inside the angles it can reproduce faithfully."""
    limits = {"pitch": pitch_limit, "yaw": yaw_limit, "roll": roll_limit}
    return {
        key: initial[key] + ((motion[key] - initial[key]) * strength).clamp(-limit, limit)
        for key, limit in limits.items()
    }


def transfer_motion(
    source: dict[str, torch.Tensor],
    source_rotation: torch.Tensor,
    driving: dict[str, torch.Tensor],
    initial: dict[str, torch.Tensor],
    initial_rotation: torch.Tensor,
) -> torch.Tensor:
    pose = limit_relative_pose(driving, initial)
    driving_rotation = rotation_matrix(pose["pitch"], pose["yaw"], pose["roll"])
    rotation = (driving_rotation @ initial_rotation.permute(0, 2, 1)) @ source_rotation
    expression = source["exp"] + (driving["exp"] - initial["exp"])
    # The live crop already follows the driving face. Reapplying its noisy
    # scale and translation makes the generated head bounce inside the
    # otherwise fixed portrait, so keep those global components anchored
    # to the source and transfer only bounded pose and expression.
    return source["scale"] * (source["kp"] @ rotation + expression) + source["t"]


@dataclass(frozen=True)
class PreparedPortrait:
    image_bgr: np.ndarray
    info: dict[str, torch.Tensor]
    rotation: torch.Tensor
    keypoints: torch.Tensor
    features: torch.Tensor


class ComicAvatarEngine:
    """LivePortrait inference with shared weights and replaceable source features."""

    def __init__(self, source_image: Path, model_dir: Path, *, compile_models: bool) -> None:
        if not torch.cuda.is_available():
            raise RuntimeError("LivePortrait avatar output requires a CUDA-capable GPU")
        self._device = torch.device("cuda")
        module_dir = Path(__file__).parent
        with (module_dir / "models.yaml").open(encoding="utf-8") as source:
            config = yaml.safe_load(source)["model_params"]
        weights = model_dir / "liveportrait"

        self._appearance = self._load_model(
            AppearanceFeatureExtractor(**config["appearance_feature_extractor_params"]),
            weights / "base_models/appearance_feature_extractor.pth",
        )
        self._motion = self._load_model(
            MotionExtractor(**config["motion_extractor_params"]),
            weights / "base_models/motion_extractor.pth",
        )
        self._warping = self._load_model(
            WarpingNetwork(**config["warping_module_params"]),
            weights / "base_models/warping_module.pth",
        )
        self._generator = self._load_model(
            SPADEDecoder(**config["spade_generator_params"]),
            weights / "base_models/spade_generator.pth",
        )
        stitching_checkpoint = torch.load(
            weights / "retargeting_models/stitching_retargeting_module.pth",
            map_location="cpu",
        )
        self._stitching = StitchingRetargetingNetwork(
            **config["stitching_retargeting_module_params"]["stitching"]
        )
        self._stitching.load_state_dict(
            _clean_state_dict(stitching_checkpoint["retarget_shoulder"])
        )
        self._stitching = self._stitching.to(self._device).eval()

        self._compiled = compile_models
        if compile_models:
            torch._dynamo.config.suppress_errors = True
            self._warping = torch.compile(self._warping, mode="max-autotune")
            self._generator = torch.compile(self._generator, mode="max-autotune")

        self._inference_lock = Lock()
        self.source = self.prepare_source(source_image)
        self._driving_initial_info: dict[str, torch.Tensor] | None = None
        self._driving_initial_rotation: torch.Tensor | None = None
        self._motion_stabilizer = MotionStabilizer()

    def prepare_source(self, source_image: Path) -> PreparedPortrait:
        """Prepare only source features; the read-only neural weights stay loaded."""
        source_bgr = cv2.imread(str(source_image), cv2.IMREAD_COLOR)
        if source_bgr is None:
            raise RuntimeError(f"cannot read avatar source image: {source_image}")
        source_rgb = cv2.cvtColor(center_square(source_bgr), cv2.COLOR_BGR2RGB)
        # CUDA graph capture cannot overlap another thread's neural calls.
        # Decode outside the lock; serialize only the short shared-model work.
        with self._inference_lock:
            source_tensor = self._prepare(source_rgb)
            info = self._keypoint_info(source_tensor)
            rotation = rotation_matrix(info["pitch"], info["yaw"], info["roll"])
            keypoints = self._transform_keypoints(info)
            with torch.inference_mode(), self._autocast():
                features = self._appearance(source_tensor).float()
            if self._device.type == "cuda":
                torch.cuda.current_stream(self._device).synchronize()
            return PreparedPortrait(source_bgr, info, rotation, keypoints, features)

    def _load_model(self, model: torch.nn.Module, path: Path) -> torch.nn.Module:
        model.load_state_dict(torch.load(path, map_location="cpu"))
        return model.to(self._device).eval()

    def _autocast(self):  # noqa: ANN202
        if self._device.type != "cuda":
            return nullcontext()
        return torch.autocast(device_type="cuda", dtype=torch.float16)

    def _prepare(self, image_rgb: np.ndarray) -> torch.Tensor:
        image = cv2.resize(image_rgb, (256, 256), interpolation=cv2.INTER_AREA)
        tensor = torch.from_numpy(image.astype(np.float32) / 255.0)
        return tensor.permute(2, 0, 1).unsqueeze(0).to(self._device)

    def _keypoint_info(self, image: torch.Tensor) -> dict[str, torch.Tensor]:
        with torch.inference_mode(), self._autocast():
            info = self._motion(image)
        batch_size = info["kp"].shape[0]
        return {
            **info,
            "pitch": headpose_pred_to_degree(info["pitch"])[:, None].float(),
            "yaw": headpose_pred_to_degree(info["yaw"])[:, None].float(),
            "roll": headpose_pred_to_degree(info["roll"])[:, None].float(),
            "kp": info["kp"].reshape(batch_size, -1, 3).float(),
            "exp": info["exp"].reshape(batch_size, -1, 3).float(),
            "scale": info["scale"].float(),
            "t": info["t"].float(),
        }

    @staticmethod
    def _transform_keypoints(info: dict[str, torch.Tensor]) -> torch.Tensor:
        transformed = info["kp"] @ rotation_matrix(info["pitch"], info["yaw"], info["roll"])
        transformed = info["scale"][..., None] * (transformed + info["exp"])
        transformed[..., :2] += info["t"][:, None, :2]
        return transformed

    def render(
        self, driving_bgr: np.ndarray, source: PreparedPortrait | None = None
    ) -> np.ndarray:
        with self._inference_lock:
            return self._render_frame(driving_bgr, self.source if source is None else source)

    def _render_frame(self, driving_bgr: np.ndarray, source: PreparedPortrait) -> np.ndarray:
        driving_rgb = cv2.cvtColor(driving_bgr, cv2.COLOR_BGR2RGB)
        driving_info = self._motion_stabilizer.update(
            self._keypoint_info(self._prepare(driving_rgb))
        )
        if self._driving_initial_info is None:
            self._driving_initial_info = {key: value.clone() for key, value in driving_info.items()}
            self._driving_initial_rotation = rotation_matrix(
                driving_info["pitch"], driving_info["yaw"], driving_info["roll"]
            )
        assert self._driving_initial_rotation is not None

        initial = self._driving_initial_info
        driven = transfer_motion(
            source.info,
            source.rotation,
            driving_info,
            initial,
            self._driving_initial_rotation,
        )

        features = torch.cat([source.keypoints.reshape(1, -1), driven.reshape(1, -1)], dim=1)
        with torch.inference_mode():
            delta = self._stitching(features)
        keypoint_count = source.keypoints.shape[1]
        driven = driven.clone()
        driven += delta[..., : 3 * keypoint_count].reshape(1, keypoint_count, 3)
        driven[..., :2] += delta[..., 3 * keypoint_count :].reshape(1, 1, 2)

        if self._compiled:
            torch.compiler.cudagraph_mark_step_begin()
        with torch.inference_mode(), self._autocast():
            warped = self._warping(
                source.features,
                kp_source=source.keypoints,
                kp_driving=driven,
            )
            output = self._generator(feature=warped["out"])
        rendered_rgb = output.float().clamp(0, 1).mul(255).byte()
        rendered_rgb = rendered_rgb[0].permute(1, 2, 0).cpu().numpy()
        return cv2.cvtColor(rendered_rgb, cv2.COLOR_RGB2BGR)

    def close(self) -> None:
        self._appearance = None
        self._motion = None
        self._warping = None
        self._generator = None
        self._stitching = None
        self.source = None
        self._driving_initial_info = None
        self._driving_initial_rotation = None
        self._motion_stabilizer = None
        gc.collect()
        torch.cuda.empty_cache()

    def __enter__(self) -> ComicAvatarEngine:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()
