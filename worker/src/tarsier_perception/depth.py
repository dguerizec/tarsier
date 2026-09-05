from __future__ import annotations

import logging
import math
import queue
import threading
import time
import urllib.error
import urllib.request
from contextlib import suppress
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import cv2
import numpy as np

from .avatar import VideoIdentityClient

LOGGER = logging.getLogger(__name__)
DEPTH_MODEL_DIRECTORY = "depth-anything-v2-small"
DEPTH_REPRESENTATION = "relative-inverse-depth-f32le"


@dataclass(frozen=True)
class DepthEstimate:
    frame_id: int
    captured_at_ms: int
    values: np.ndarray
    far: float
    near: float


class TemporalDepthScale:
    def __init__(self, smoothing: float = 0.08) -> None:
        if not 0.0 < smoothing <= 1.0:
            raise ValueError("depth scale smoothing must be between zero and one")
        self._smoothing = smoothing
        self._far: float | None = None
        self._near: float | None = None

    def update(self, values: np.ndarray) -> tuple[float, float]:
        far, near = (float(value) for value in np.percentile(values, (2.0, 98.0)))
        if not np.isfinite(far) or not np.isfinite(near) or near - far < 1e-6:
            raise RuntimeError("depth estimate does not contain a usable finite range")
        if self._far is None or self._near is None:
            self._far, self._near = far, near
        else:
            keep = 1.0 - self._smoothing
            self._far = self._far * keep + far * self._smoothing
            self._near = self._near * keep + near * self._smoothing
        return self._far, self._near


def depth_input_width(frame_width: int, frame_height: int, input_height: int) -> int:
    if frame_width <= 0 or frame_height <= 0 or input_height <= 0:
        raise ValueError("depth dimensions must be greater than zero")
    return max(14, math.ceil(input_height * frame_width / frame_height / 14) * 14)


class DepthEstimator:
    def __init__(self, model_dir: Path, input_height: int) -> None:
        import torch
        from transformers import AutoModelForDepthEstimation

        self._torch = torch
        self._input_height = input_height
        self._device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
        model_path = model_dir / DEPTH_MODEL_DIRECTORY
        self._model = AutoModelForDepthEstimation.from_pretrained(
            model_path,
            local_files_only=True,
        ).to(self._device)
        self._model.eval()
        self._mean = np.array([0.485, 0.456, 0.406], dtype=np.float32)
        self._standard_deviation = np.array([0.229, 0.224, 0.225], dtype=np.float32)
        self._scale = TemporalDepthScale()
        self._latest: DepthEstimate | None = None
        LOGGER.info("depth model ready on %s", self._device)

    @property
    def latest(self) -> DepthEstimate | None:
        return self._latest

    def estimate(self, frame: DepthInputFrame) -> DepthEstimate:
        height, width = frame.frame_bgr.shape[:2]
        input_width = depth_input_width(width, height, self._input_height)
        rgb = cv2.cvtColor(frame.frame_bgr, cv2.COLOR_BGR2RGB)
        resized = cv2.resize(
            rgb,
            (input_width, self._input_height),
            interpolation=cv2.INTER_CUBIC,
        ).astype(np.float32)
        normalized = (resized / 255.0 - self._mean) / self._standard_deviation
        pixels = np.ascontiguousarray(np.transpose(normalized, (2, 0, 1)))
        tensor = self._torch.from_numpy(pixels).unsqueeze(0).to(self._device)
        with self._torch.inference_mode():
            predicted = self._model(tensor).predicted_depth
            predicted = self._torch.nn.functional.interpolate(
                predicted.unsqueeze(1),
                size=(height, width),
                mode="bicubic",
                align_corners=False,
            ).squeeze()
        values = np.ascontiguousarray(predicted.float().cpu().numpy(), dtype=np.float32)
        if not np.isfinite(values).all():
            raise RuntimeError("depth model produced non-finite values")
        far, near = self._scale.update(values)
        estimate = DepthEstimate(frame.frame_id, frame.captured_at_ms, values, far, near)
        self._latest = estimate
        return estimate

    def close(self) -> None:
        model = self._model
        self._model = None
        del model
        if self._device.type == "cuda":
            self._torch.cuda.empty_cache()

    def __enter__(self) -> DepthEstimator:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


class DepthPublisher:
    def __init__(self, daemon_url: str, timeout_seconds: float = 2.0) -> None:
        self._url = f"{daemon_url.rstrip('/')}/api/v1/depth/frame"
        self._timeout_seconds = timeout_seconds

    def publish(self, estimate: DepthEstimate) -> None:
        if estimate.values.ndim != 2 or estimate.values.dtype != np.float32:
            raise ValueError("depth values must be a two-dimensional float32 array")
        height, width = estimate.values.shape
        little_endian = np.ascontiguousarray(estimate.values, dtype="<f4")
        request = urllib.request.Request(
            self._url,
            data=little_endian.tobytes(),
            headers={
                "Content-Type": "application/octet-stream",
                "X-Tarsier-Frame-Id": str(estimate.frame_id),
                "X-Tarsier-Captured-At-Ms": str(estimate.captured_at_ms),
                "X-Tarsier-Depth-Width": str(width),
                "X-Tarsier-Depth-Height": str(height),
                "X-Tarsier-Depth-Far": repr(estimate.far),
                "X-Tarsier-Depth-Near": repr(estimate.near),
                "X-Tarsier-Depth-Representation": DEPTH_REPRESENTATION,
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=self._timeout_seconds) as response:  # noqa: S310
                if response.status != 204:
                    raise RuntimeError(f"daemon returned HTTP {response.status}")
        except urllib.error.URLError as error:
            raise RuntimeError(f"failed to publish depth frame: {error.reason}") from error


@dataclass(frozen=True)
class DepthInputFrame:
    frame_id: int
    captured_at_ms: int
    frame_bgr: np.ndarray


class DepthProcessor:
    def __init__(self, daemon_url: str, model_dir: Path, input_height: int) -> None:
        self._daemon_url = daemon_url
        self._model_dir = model_dir
        self._input_height = input_height
        self._frames: queue.Queue[DepthInputFrame | None] = queue.Queue(maxsize=1)
        self._lock = threading.Lock()
        self._published_count = 0
        self._error: BaseException | None = None
        self._thread = threading.Thread(target=self._run, name="tarsier-depth", daemon=True)

    @property
    def published_count(self) -> int:
        with self._lock:
            return self._published_count

    def submit(self, frame: DepthInputFrame) -> None:
        self.raise_if_failed()
        while True:
            try:
                self._frames.put_nowait(frame)
                return
            except queue.Full:
                with suppress(queue.Empty):
                    self._frames.get_nowait()

    def raise_if_failed(self) -> None:
        with self._lock:
            error = self._error
        if error is not None:
            raise RuntimeError("depth processor failed") from error

    def close(self) -> None:
        while True:
            try:
                self._frames.put_nowait(None)
                break
            except queue.Full:
                with suppress(queue.Empty):
                    self._frames.get_nowait()
        self._thread.join()
        self.raise_if_failed()

    def _run(self) -> None:
        try:
            identity = VideoIdentityClient(self._daemon_url)
            publisher = DepthPublisher(self._daemon_url)
            estimator: DepthEstimator | None = None
            retry_at = 0.0
            while (frame := self._frames.get()) is not None:
                if identity.selected_identity() != "depth-map":
                    if estimator is not None:
                        estimator.close()
                        estimator = None
                        LOGGER.info("depth model released")
                    continue
                if estimator is None:
                    if time.monotonic() < retry_at:
                        continue
                    try:
                        estimator = DepthEstimator(self._model_dir, self._input_height)
                    except (ImportError, OSError, RuntimeError, ValueError):
                        retry_at = time.monotonic() + 5.0
                        LOGGER.exception("failed to initialize depth model")
                        continue
                try:
                    publisher.publish(estimator.estimate(frame))
                    with self._lock:
                        self._published_count += 1
                except RuntimeError as error:
                    LOGGER.warning("%s", error)
                    identity.invalidate()
            if estimator is not None:
                estimator.close()
        except BaseException as error:
            LOGGER.exception("depth processor stopped")
            with self._lock:
                self._error = error

    def __enter__(self) -> DepthProcessor:
        self._thread.start()
        return self

    def __exit__(self, *_: Any) -> None:
        self.close()
