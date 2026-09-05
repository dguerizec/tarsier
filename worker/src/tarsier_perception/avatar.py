from __future__ import annotations

import logging
import queue
import threading
import urllib.error
import urllib.request
from contextlib import suppress
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import cv2
import mediapipe as mp
import numpy as np

LOGGER = logging.getLogger(__name__)


def crop_face_square(frame_bgr: np.ndarray, landmarks: list[Any], scale: float = 3.0) -> np.ndarray:
    if not landmarks:
        raise ValueError("face landmarks are required")
    height, width = frame_bgr.shape[:2]
    xs = np.array([point.x * width for point in landmarks], dtype=np.float32)
    ys = np.array([point.y * height for point in landmarks], dtype=np.float32)
    center_x = float((xs.min() + xs.max()) / 2)
    center_y = float((ys.min() + ys.max()) / 2)
    size = max(2, int(np.ceil(max(float(np.ptp(xs)), float(np.ptp(ys))) * scale)))
    left = int(round(center_x - size / 2))
    top = int(round(center_y - size / 2))
    right = left + size
    bottom = top + size
    pad_left = max(0, -left)
    pad_top = max(0, -top)
    pad_right = max(0, right - width)
    pad_bottom = max(0, bottom - height)
    padded = cv2.copyMakeBorder(
        frame_bgr,
        pad_top,
        pad_bottom,
        pad_left,
        pad_right,
        cv2.BORDER_REPLICATE,
    )
    left += pad_left
    right += pad_left
    top += pad_top
    bottom += pad_top
    return padded[top:bottom, left:right]


def compose_avatar_frame(
    source_bgr: np.ndarray,
    animated_bgr: np.ndarray,
    width: int,
    height: int,
) -> np.ndarray:
    base = cv2.resize(source_bgr, (width, height), interpolation=cv2.INTER_AREA)
    square_size = min(width, height)
    animated = cv2.resize(
        animated_bgr,
        (square_size, square_size),
        interpolation=cv2.INTER_CUBIC,
    )
    left = (width - square_size) // 2
    top = (height - square_size) // 2
    feather = max(1, square_size // 32)
    alpha = np.ones((square_size, square_size), dtype=np.float32)
    ramp = np.linspace(0.0, 1.0, feather, dtype=np.float32)
    alpha[:, :feather] *= ramp
    alpha[:, -feather:] *= ramp[::-1]
    alpha[:feather, :] *= ramp[:, None]
    alpha[-feather:, :] *= ramp[::-1, None]
    alpha = alpha[..., None]
    region = base[top : top + square_size, left : left + square_size]
    region[:] = np.clip(animated * alpha + region * (1.0 - alpha), 0, 255).astype(np.uint8)
    return cv2.cvtColor(base, cv2.COLOR_BGR2BGRA)


class MediaPipeFaceCropper:
    def __init__(self, model_dir: Path) -> None:
        vision = mp.tasks.vision
        self._landmarker = vision.FaceLandmarker.create_from_options(
            vision.FaceLandmarkerOptions(
                base_options=mp.tasks.BaseOptions(
                    model_asset_path=str(model_dir / "face_landmarker.task")
                ),
                running_mode=vision.RunningMode.VIDEO,
                num_faces=1,
                min_face_detection_confidence=0.5,
                min_face_presence_confidence=0.5,
                min_tracking_confidence=0.5,
            )
        )

    def crop(self, frame_bgr: np.ndarray, timestamp_ms: int) -> np.ndarray | None:
        frame_rgb = cv2.cvtColor(frame_bgr, cv2.COLOR_BGR2RGB)
        image = mp.Image(image_format=mp.ImageFormat.SRGB, data=frame_rgb)
        result = self._landmarker.detect_for_video(image, timestamp_ms)
        if not result.face_landmarks:
            return None
        return crop_face_square(frame_bgr, result.face_landmarks[0])

    def close(self) -> None:
        self._landmarker.close()

    def __enter__(self) -> MediaPipeFaceCropper:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


class MediaPipeAvatarTracker:
    def __init__(self, model_dir: Path) -> None:
        vision = mp.tasks.vision
        self._landmarker = vision.FaceLandmarker.create_from_options(
            vision.FaceLandmarkerOptions(
                base_options=mp.tasks.BaseOptions(
                    model_asset_path=str(model_dir / "face_landmarker.task")
                ),
                running_mode=vision.RunningMode.VIDEO,
                num_faces=1,
                min_face_detection_confidence=0.5,
                min_face_presence_confidence=0.5,
                min_tracking_confidence=0.5,
                output_face_blendshapes=True,
                output_facial_transformation_matrixes=True,
            )
        )

    def track(self, frame_bgr: np.ndarray, timestamp_ms: int):  # noqa: ANN201
        from .stylized3d import motion_from_mediapipe

        frame_rgb = cv2.cvtColor(frame_bgr, cv2.COLOR_BGR2RGB)
        image = mp.Image(image_format=mp.ImageFormat.SRGB, data=frame_rgb)
        return motion_from_mediapipe(self._landmarker.detect_for_video(image, timestamp_ms))

    def close(self) -> None:
        self._landmarker.close()

    def __enter__(self) -> MediaPipeAvatarTracker:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


class AvatarPublisher:
    def __init__(self, daemon_url: str, timeout_seconds: float = 2.0) -> None:
        self._url = f"{daemon_url.rstrip('/')}/api/v1/avatar/frame"
        self._timeout_seconds = timeout_seconds

    def publish(self, frame_id: int, captured_at_ms: int, frame_bgrx: np.ndarray) -> None:
        if frame_bgrx.ndim != 3 or frame_bgrx.shape[2] != 4 or frame_bgrx.dtype != np.uint8:
            raise ValueError("avatar frame must be a BGRx uint8 image")
        height, width = frame_bgrx.shape[:2]
        request = urllib.request.Request(
            self._url,
            data=np.ascontiguousarray(frame_bgrx).tobytes(),
            headers={
                "Content-Type": "application/octet-stream",
                "X-Tarsier-Frame-Id": str(frame_id),
                "X-Tarsier-Captured-At-Ms": str(captured_at_ms),
                "X-Tarsier-Avatar-Width": str(width),
                "X-Tarsier-Avatar-Height": str(height),
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=self._timeout_seconds) as response:  # noqa: S310
                if response.status != 204:
                    raise RuntimeError(f"daemon returned HTTP {response.status}")
        except urllib.error.URLError as error:
            raise RuntimeError(f"failed to publish avatar frame: {error.reason}") from error


@dataclass(frozen=True)
class AvatarInputFrame:
    frame_id: int
    captured_at_ms: int
    timestamp_ms: int
    frame_bgr: np.ndarray


class AvatarProcessor:
    def __init__(
        self,
        daemon_url: str,
        model_dir: Path,
        engine: str,
        source_image: Path | None,
        profile: Path | None,
        width: int,
        height: int,
        *,
        compile_models: bool,
    ) -> None:
        self._daemon_url = daemon_url
        self._model_dir = model_dir
        self._engine = engine
        self._source_image = source_image
        self._profile = profile
        self._width = width
        self._height = height
        self._compile_models = compile_models
        self._frames: queue.Queue[AvatarInputFrame | None] = queue.Queue(maxsize=1)
        self._lock = threading.Lock()
        self._published_count = 0
        self._error: BaseException | None = None
        self._thread = threading.Thread(target=self._run, name="tarsier-avatar", daemon=True)

    @property
    def published_count(self) -> int:
        with self._lock:
            return self._published_count

    def submit(self, frame: AvatarInputFrame) -> None:
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
            raise RuntimeError("avatar processor failed") from error

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
            if self._engine == "stylized-3d":
                self._run_stylized_3d()
            elif self._engine == "liveportrait":
                self._run_liveportrait()
            else:
                raise RuntimeError(f"unsupported avatar engine: {self._engine}")
        except BaseException as error:
            LOGGER.exception("avatar processor stopped")
            with self._lock:
                self._error = error

    def _run_stylized_3d(self) -> None:
        from .stylized3d import Stylized3DAvatarEngine

        if self._profile is None:
            raise RuntimeError("stylized 3D avatar requires a profile")
        publisher = AvatarPublisher(self._daemon_url)
        with (
            MediaPipeAvatarTracker(self._model_dir) as tracker,
            Stylized3DAvatarEngine(self._profile, self._width, self._height) as engine,
        ):
            while (frame := self._frames.get()) is not None:
                motion = tracker.track(frame.frame_bgr, frame.timestamp_ms)
                output = engine.render(motion)
                self._publish(publisher, frame, output)

    def _run_liveportrait(self) -> None:
        if self._source_image is None:
            raise RuntimeError("LivePortrait avatar requires a source image")
        try:
            from .liveportrait.engine import ComicAvatarEngine

            source_bgr = cv2.imread(str(self._source_image), cv2.IMREAD_COLOR)
            if source_bgr is None:
                raise RuntimeError(f"cannot read avatar source image: {self._source_image}")
            engine = ComicAvatarEngine(
                self._source_image,
                self._model_dir,
                compile_models=self._compile_models,
            )
            publisher = AvatarPublisher(self._daemon_url)
            with MediaPipeFaceCropper(self._model_dir) as cropper:
                while (frame := self._frames.get()) is not None:
                    driving = cropper.crop(frame.frame_bgr, frame.timestamp_ms)
                    if driving is None:
                        continue
                    animated = engine.render(driving)
                    output = compose_avatar_frame(
                        source_bgr,
                        animated,
                        self._width,
                        self._height,
                    )
                    self._publish(publisher, frame, output)
        except ImportError as error:
            raise RuntimeError("LivePortrait dependencies are not installed") from error

    def _publish(
        self,
        publisher: AvatarPublisher,
        frame: AvatarInputFrame,
        output: np.ndarray,
    ) -> None:
        try:
            publisher.publish(frame.frame_id, frame.captured_at_ms, output)
            with self._lock:
                self._published_count += 1
        except RuntimeError as error:
            LOGGER.warning("%s", error)

    def __enter__(self) -> AvatarProcessor:
        self._thread.start()
        return self

    def __exit__(self, *_: object) -> None:
        self.close()
