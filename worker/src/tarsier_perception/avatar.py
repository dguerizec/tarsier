from __future__ import annotations

import json
import logging
import math
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

from .auth import authorize
from .delegates import base_options
from .telemetry import stages, timed

LOGGER = logging.getLogger(__name__)


@dataclass(frozen=True)
class FaceCropGeometry:
    center_x: float
    center_y: float
    size: float


def face_crop_geometry(
    frame_bgr: np.ndarray,
    landmarks: list[Any],
    scale: float = 3.0,
) -> FaceCropGeometry:
    if not landmarks:
        raise ValueError("face landmarks are required")
    height, width = frame_bgr.shape[:2]
    xs = np.array([point.x * width for point in landmarks], dtype=np.float32)
    ys = np.array([point.y * height for point in landmarks], dtype=np.float32)
    return FaceCropGeometry(
        center_x=float((xs.min() + xs.max()) / 2),
        center_y=float((ys.min() + ys.max()) / 2),
        size=max(2.0, max(float(np.ptp(xs)), float(np.ptp(ys))) * scale),
    )


class TemporalFaceCrop:
    def __init__(self, half_life_ms: float = 350.0, reset_after_ms: int = 1000) -> None:
        if half_life_ms <= 0:
            raise ValueError("face crop half-life must be greater than zero")
        self._half_life_ms = half_life_ms
        self._reset_after_ms = reset_after_ms
        self._geometry: FaceCropGeometry | None = None
        self._timestamp_ms: int | None = None

    def update(self, target: FaceCropGeometry, timestamp_ms: int) -> FaceCropGeometry:
        if (
            self._geometry is None
            or self._timestamp_ms is None
            or timestamp_ms <= self._timestamp_ms
            or timestamp_ms - self._timestamp_ms >= self._reset_after_ms
        ):
            self._geometry = target
        else:
            elapsed_ms = timestamp_ms - self._timestamp_ms
            factor = 1.0 - math.exp(-math.log(2.0) * elapsed_ms / self._half_life_ms)
            current = self._geometry
            self._geometry = FaceCropGeometry(
                center_x=current.center_x + (target.center_x - current.center_x) * factor,
                center_y=current.center_y + (target.center_y - current.center_y) * factor,
                size=current.size + (target.size - current.size) * factor,
            )
        self._timestamp_ms = timestamp_ms
        return self._geometry


def crop_face_square(
    frame_bgr: np.ndarray,
    landmarks: list[Any],
    scale: float = 3.0,
    *,
    geometry: FaceCropGeometry | None = None,
) -> np.ndarray:
    if not landmarks:
        raise ValueError("face landmarks are required")
    geometry = geometry or face_crop_geometry(frame_bgr, landmarks, scale)
    height, width = frame_bgr.shape[:2]
    size = max(2, int(round(geometry.size)))
    left = int(round(geometry.center_x - size / 2))
    top = int(round(geometry.center_y - size / 2))
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
    animated_bgr: np.ndarray,
    width: int,
    height: int,
) -> np.ndarray:
    source_height, source_width = animated_bgr.shape[:2]
    scale = min(width / source_width, height / source_height)
    fitted_width = max(1, min(width, round(source_width * scale)))
    fitted_height = max(1, min(height, round(source_height * scale)))
    animated = cv2.resize(
        animated_bgr,
        (fitted_width, fitted_height),
        interpolation=cv2.INTER_CUBIC,
    )
    base = np.zeros((height, width, 3), dtype=np.uint8)
    left = (width - fitted_width) // 2
    top = (height - fitted_height) // 2
    base[top : top + fitted_height, left : left + fitted_width] = animated
    return cv2.cvtColor(base, cv2.COLOR_BGR2BGRA)


class MediaPipeFaceCropper:
    def __init__(self, model_dir: Path, delegate: str = "cpu") -> None:
        vision = mp.tasks.vision
        self._landmarker = vision.FaceLandmarker.create_from_options(
            vision.FaceLandmarkerOptions(
                base_options=base_options(model_dir / "face_landmarker.task", delegate),
                running_mode=vision.RunningMode.VIDEO,
                num_faces=1,
                min_face_detection_confidence=0.5,
                min_face_presence_confidence=0.5,
                min_tracking_confidence=0.5,
            )
        )
        self._temporal_crop = TemporalFaceCrop()

    @timed("avatar_tracking")
    def crop(self, frame_bgr: np.ndarray, timestamp_ms: int) -> np.ndarray | None:
        frame_rgb = cv2.cvtColor(frame_bgr, cv2.COLOR_BGR2RGB)
        image = mp.Image(image_format=mp.ImageFormat.SRGB, data=frame_rgb)
        result = self._landmarker.detect_for_video(image, timestamp_ms)
        if not result.face_landmarks:
            return None
        landmarks = result.face_landmarks[0]
        geometry = self._temporal_crop.update(
            face_crop_geometry(frame_bgr, landmarks),
            timestamp_ms,
        )
        return crop_face_square(frame_bgr, landmarks, geometry=geometry)

    def close(self) -> None:
        self._landmarker.close()

    def __enter__(self) -> MediaPipeFaceCropper:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


class MediaPipeAvatarTracker:
    def __init__(self, model_dir: Path, delegate: str = "cpu") -> None:
        vision = mp.tasks.vision
        self._landmarker = vision.FaceLandmarker.create_from_options(
            vision.FaceLandmarkerOptions(
                base_options=base_options(model_dir / "face_landmarker.task", delegate),
                running_mode=vision.RunningMode.VIDEO,
                num_faces=1,
                min_face_detection_confidence=0.5,
                min_face_presence_confidence=0.5,
                min_tracking_confidence=0.5,
                output_face_blendshapes=True,
                output_facial_transformation_matrixes=True,
            )
        )

    @timed("avatar_tracking")
    def track(self, frame_bgr: np.ndarray, timestamp_ms: int):  # noqa: ANN201
        from .avatar_motion import motion_from_mediapipe

        frame_rgb = cv2.cvtColor(frame_bgr, cv2.COLOR_BGR2RGB)
        image = mp.Image(image_format=mp.ImageFormat.SRGB, data=frame_rgb)
        return motion_from_mediapipe(
            self._landmarker.detect_for_video(image, timestamp_ms)
        )

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

    @timed("avatar_publish")
    def publish(
        self,
        engine: str,
        frame_id: int,
        captured_at_ms: int,
        frame_bgrx: np.ndarray,
        source_revision: int = 0,
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
                "X-Tarsier-Avatar-Source-Revision": str(source_revision),
                "X-Tarsier-Avatar-Pixel-Format": "bgra" if engine == "portrait3d" else "bgrx",
                "X-Tarsier-Avatar-Width": str(width),
                "X-Tarsier-Avatar-Height": str(height),
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(  # noqa: S310
                authorize(request), timeout=self._timeout_seconds
            ) as response:
                if response.status != 204:
                    raise RuntimeError(f"daemon returned HTTP {response.status}")
        except urllib.error.URLError as error:
            raise RuntimeError(f"failed to publish avatar frame: {error.reason}") from error


class VideoIdentityClient:
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
        self._background_enabled = False
        self._next_refresh = 0.0
        self.portrait_source: tuple[int, Path] | None = None
        self.portrait_active_revision = 0
        self.portrait_model: Path | None = None

    def selected_identity(self) -> str:
        now = time.monotonic()
        if now >= self._next_refresh:
            self._refresh(now)
        return self._identity

    def selected_avatar_engine(self) -> str | None:
        identity = self.selected_identity()
        return identity if identity in {"portrait3d", "liveportrait"} else None

    def report_portrait_error(self, revision: int, error: str) -> None:
        request = urllib.request.Request(
            self._url.replace("/video/identity", "/video/liveportrait/status"),
            data=json.dumps({"revision": revision, "error": error[:500]}).encode(),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(authorize(request), timeout=self._timeout_seconds):
                pass
        except (OSError, urllib.error.URLError):
            LOGGER.warning("failed to report portrait preparation error")

    def segmentation_required(self) -> bool:
        """Only camera backgrounds consume the selfie mask, including depth refinement."""
        return self.selected_identity() == "camera" and self._background_enabled

    def depth_usage(self) -> str | None:
        identity = self.selected_identity()
        if identity == "depth-map":
            return "visualization"
        if identity == "camera" and self._background_enabled:
            return "mask-refinement"
        return None

    def invalidate(self) -> None:
        self._next_refresh = 0.0

    def _refresh(self, now: float) -> None:
        self._next_refresh = now + self._refresh_seconds
        request = urllib.request.Request(self._url, method="GET")
        try:
            with urllib.request.urlopen(  # noqa: S310
                authorize(request), timeout=self._timeout_seconds
            ) as response:
                payload = json.load(response)
            identity = payload.get("identity")
            if identity not in {"camera", "portrait3d", "liveportrait", "depth-map"}:
                raise ValueError(f"invalid video identity: {identity!r}")
            model = payload.get("portrait3d_model")
            if isinstance(model, str) and model:
                self.portrait_model = Path(model)
            portrait = payload.get("liveportrait")
            if isinstance(portrait, dict):
                revision, source = portrait.get("revision"), portrait.get("source")
                if isinstance(revision, int) and revision >= 0 and isinstance(source, str):
                    self.portrait_source = (revision, Path(source))
                    active_revision = portrait.get("active_revision")
                    if isinstance(active_revision, int) and active_revision >= 0:
                        self.portrait_active_revision = active_revision
            self._identity = identity
            self._background_enabled = payload.get("background_enabled") is True
        except (OSError, ValueError, urllib.error.URLError) as error:
            # Keep rendering the last accepted identity during a transient poll failure.
            # The daemon still rejects frames for identities it no longer wants.
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
        width: int,
        height: int,
        *,
        compile_models: bool,
        portrait_model: Path | None = None,
        face_delegate: str = "cpu",
    ) -> None:
        self._daemon_url = daemon_url
        self._model_dir = model_dir
        self._engine = engine
        self._source_image = source_image
        self._face_delegate = face_delegate
        self._portrait_model = portrait_model
        self._width = width
        self._height = height
        self._compile_models = compile_models
        self._portrait_request: tuple[int, Path] | None = None
        self._portrait_switcher = None
        self._portrait_identity: VideoIdentityClient | None = None
        self._rendered_source_revision = 0
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
            if self._engine not in {"portrait3d", "liveportrait"}:
                raise RuntimeError(f"unsupported avatar engine: {self._engine}")
            self._run_switchable()
        except BaseException as error:
            LOGGER.exception("avatar processor stopped")
            with self._lock:
                self._error = error

    def _run_switchable(self) -> None:
        publisher = AvatarPublisher(self._daemon_url)
        identity = VideoIdentityClient(self._daemon_url)
        self._portrait_identity = identity
        active_engine: str | None = None
        resources = ExitStack()
        render: Callable[[AvatarInputFrame], np.ndarray | None] | None = None
        failed_engine: str | None = None
        retry_at = 0.0
        try:
            while (frame := self._frames.get()) is not None:
                selected_engine = identity.selected_avatar_engine()
                self._portrait_request = identity.portrait_source
                model_changed = (
                    selected_engine == "portrait3d"
                    and identity.portrait_model is not None
                    and identity.portrait_model != self._portrait_model
                )
                if model_changed:
                    self._portrait_model = identity.portrait_model
                    failed_engine = None
                if selected_engine != active_engine or model_changed:
                    resources.close()
                    self._portrait_switcher = None
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
                if selected_engine == "liveportrait" and self._portrait_switcher is not None:
                    self._portrait_switcher.accept(identity.portrait_active_revision)
                    if self._portrait_request is not None:
                        self._portrait_switcher.request(*self._portrait_request)
                output = render(frame)
                if output is not None:
                    accepted = self._publish(publisher, selected_engine, frame, output)
                    if selected_engine == "liveportrait":
                        if accepted:
                            self._portrait_switcher.accept(self._rendered_source_revision)
                        else:
                            identity.invalidate()
        finally:
            resources.close()

    def _open_engine(
        self,
        resources: ExitStack,
        engine_name: str,
    ) -> Callable[[AvatarInputFrame], np.ndarray | None]:
        if engine_name == "portrait3d":
            if self._portrait_model is None:
                raise RuntimeError("personal 3D avatar requires a model directory")
            from .portrait3d import Portrait3DAvatarEngine

            tracker = resources.enter_context(
                MediaPipeAvatarTracker(self._model_dir, self._face_delegate)
            )
            engine = resources.enter_context(
                Portrait3DAvatarEngine(self._portrait_model, self._width, self._height)
            )

            def render_portrait(frame: AvatarInputFrame) -> np.ndarray:
                motion = tracker.track(frame.frame_bgr, frame.timestamp_ms)
                with stages.measure("avatar_render"):
                    return engine.render(motion)

            return render_portrait

        if engine_name != "liveportrait":
            raise RuntimeError(f"unsupported avatar engine: {engine_name}")
        revision, source_image = self._portrait_request or (0, self._source_image)
        if source_image is None:
            raise RuntimeError("LivePortrait avatar requires a source image")
        from .liveportrait.engine import ComicAvatarEngine
        from .liveportrait.switching import PortraitSwitcher

        engine = resources.enter_context(
            ComicAvatarEngine(source_image, self._model_dir, compile_models=self._compile_models)
        )
        cropper = resources.enter_context(
            MediaPipeFaceCropper(self._model_dir, self._face_delegate)
        )
        assert self._portrait_identity is not None
        switcher = resources.enter_context(PortraitSwitcher(
            revision, engine.source, engine.prepare_source, engine.render,
            self._portrait_identity.report_portrait_error,
        ))
        self._portrait_switcher = switcher

        def render_liveportrait(frame: AvatarInputFrame) -> np.ndarray | None:
            switcher.prepare_pending()
            driving = cropper.crop(frame.frame_bgr, frame.timestamp_ms)
            if driving is None:
                return None
            with stages.measure("avatar_render"):
                animated, portrait = switcher.render(driving)
                self._rendered_source_revision = portrait.revision
                return compose_avatar_frame(
                    animated, self._width, self._height,
                )

        return render_liveportrait

    def _publish(
        self,
        publisher: AvatarPublisher,
        engine: str,
        frame: AvatarInputFrame,
        output: np.ndarray,
    ) -> bool:
        try:
            publisher.publish(
                engine, frame.frame_id, frame.captured_at_ms, output,
                self._rendered_source_revision if engine == "liveportrait" else 0,
            )
            with self._lock:
                self._published_count += 1
            return True
        except RuntimeError as error:
            LOGGER.warning("%s", error)
            return False

    def __enter__(self) -> AvatarProcessor:
        self._thread.start()
        return self

    def __exit__(self, *_: object) -> None:
        self.close()
