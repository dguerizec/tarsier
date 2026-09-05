from __future__ import annotations

import json
import logging
import queue
import threading
import time
import urllib.error
import urllib.request
from collections.abc import Callable
from contextlib import ExitStack, suppress
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

    def publish(
        self,
        engine: str,
        frame_id: int,
        captured_at_ms: int,
        frame_bgrx: np.ndarray,
    ) -> None:
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
                "X-Tarsier-Avatar-Engine": engine,
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


class AvatarIdentityClient:
    def __init__(
        self,
        daemon_url: str,
        refresh_seconds: float = 0.25,
        timeout_seconds: float = 1.0,
    ) -> None:
        self._url = f"{daemon_url.rstrip('/')}/api/v1/video/identity"
        self._refresh_seconds = refresh_seconds
        self._timeout_seconds = timeout_seconds
        self._identity = "camera"
        self._next_refresh = 0.0

    def selected_engine(self) -> str | None:
        now = time.monotonic()
        if now >= self._next_refresh:
            self._refresh(now)
        return None if self._identity == "camera" else self._identity

    def _refresh(self, now: float) -> None:
        self._next_refresh = now + self._refresh_seconds
        request = urllib.request.Request(self._url, method="GET")
        try:
            with urllib.request.urlopen(request, timeout=self._timeout_seconds) as response:  # noqa: S310
                payload = json.load(response)
            identity = payload.get("identity")
            if identity not in {"camera", "stylized-3d", "liveportrait"}:
                raise ValueError(f"invalid video identity: {identity!r}")
            self._identity = identity
        except (OSError, ValueError, urllib.error.URLError) as error:
            self._identity = "camera"
            self._next_refresh = now + max(1.0, self._refresh_seconds)
            LOGGER.warning("failed to read selected video identity: %s", error)


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
            if self._engine not in {"stylized-3d", "liveportrait"}:
                raise RuntimeError(f"unsupported avatar engine: {self._engine}")
            self._run_switchable()
        except BaseException as error:
            LOGGER.exception("avatar processor stopped")
            with self._lock:
                self._error = error

    def _run_switchable(self) -> None:
        publisher = AvatarPublisher(self._daemon_url)
        identity = AvatarIdentityClient(self._daemon_url)
        active_engine: str | None = None
        resources = ExitStack()
        render: Callable[[AvatarInputFrame], np.ndarray | None] | None = None
        failed_engine: str | None = None
        retry_at = 0.0
        try:
            while (frame := self._frames.get()) is not None:
                selected_engine = identity.selected_engine()
                if selected_engine != active_engine:
                    resources.close()
                    resources = ExitStack()
                    active_engine = None
                    render = None
                if selected_engine is None:
                    failed_engine = None
                    continue
                if render is None:
                    if failed_engine == selected_engine and time.monotonic() < retry_at:
                        continue
                    try:
                        render = self._open_engine(resources, selected_engine)
                        active_engine = selected_engine
                        failed_engine = None
                        LOGGER.info("avatar engine ready: %s", selected_engine)
                    except (ImportError, OSError, RuntimeError, ValueError):
                        resources.close()
                        resources = ExitStack()
                        failed_engine = selected_engine
                        retry_at = time.monotonic() + 5.0
                        LOGGER.exception("failed to initialize avatar engine: %s", selected_engine)
                        continue
                output = render(frame)
                if output is not None:
                    self._publish(publisher, selected_engine, frame, output)
        finally:
            resources.close()

    def _open_engine(
        self,
        resources: ExitStack,
        engine_name: str,
    ) -> Callable[[AvatarInputFrame], np.ndarray | None]:
        if engine_name == "stylized-3d":
            if self._profile is None:
                raise RuntimeError("stylized 3D avatar requires a profile")
            from .stylized3d import Stylized3DAvatarEngine

            tracker = resources.enter_context(MediaPipeAvatarTracker(self._model_dir))
            engine = resources.enter_context(
                Stylized3DAvatarEngine(self._profile, self._width, self._height)
            )

            def render_stylized(frame: AvatarInputFrame) -> np.ndarray:
                motion = tracker.track(frame.frame_bgr, frame.timestamp_ms)
                return engine.render(motion)

            return render_stylized

        if engine_name != "liveportrait":
            raise RuntimeError(f"unsupported avatar engine: {engine_name}")
        if self._source_image is None:
            raise RuntimeError("LivePortrait avatar requires a source image")
        from .liveportrait.engine import ComicAvatarEngine

        source_bgr = cv2.imread(str(self._source_image), cv2.IMREAD_COLOR)
        if source_bgr is None:
            raise RuntimeError(f"cannot read avatar source image: {self._source_image}")
        engine = resources.enter_context(
            ComicAvatarEngine(
                self._source_image,
                self._model_dir,
                compile_models=self._compile_models,
            )
        )
        cropper = resources.enter_context(MediaPipeFaceCropper(self._model_dir))

        def render_liveportrait(frame: AvatarInputFrame) -> np.ndarray | None:
            driving = cropper.crop(frame.frame_bgr, frame.timestamp_ms)
            if driving is None:
                return None
            animated = engine.render(driving)
            return compose_avatar_frame(
                source_bgr,
                animated,
                self._width,
                self._height,
            )

        return render_liveportrait

    def _publish(
        self,
        publisher: AvatarPublisher,
        engine: str,
        frame: AvatarInputFrame,
        output: np.ndarray,
    ) -> None:
        try:
            publisher.publish(engine, frame.frame_id, frame.captured_at_ms, output)
            with self._lock:
                self._published_count += 1
        except RuntimeError as error:
            LOGGER.warning("%s", error)

    def __enter__(self) -> AvatarProcessor:
        self._thread.start()
        return self

    def __exit__(self, *_: object) -> None:
        self.close()
