from __future__ import annotations

import json
import logging
import queue
import threading
import time
import urllib.error
import urllib.request
from collections.abc import Iterator
from contextlib import suppress
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

import cv2
import mediapipe as mp
import numpy as np

LOGGER = logging.getLogger(__name__)
POSE_CONSTRAINT_RADIUS = 32


@dataclass(frozen=True)
class Landmark:
    x: float
    y: float
    z: float
    visibility: float | None = None


@dataclass(frozen=True)
class Observation:
    frame_id: int
    captured_at_ms: int
    face_detected: bool
    face_landmarks: list[Landmark]
    hand_detected: bool
    hand_landmarks: list[Landmark]
    pose_detected: bool
    pose_landmarks: list[Landmark]
    gesture: str | None
    confidence: float
    latency_ms: float


def normalize_gesture(name: str | None) -> str | None:
    if not name or name == "None":
        return None
    return name.lower()


def select_gesture(gestures: list[list[Any]]) -> tuple[str | None, float]:
    candidates = [
        (gesture, float(category.score or 0.0))
        for categories in gestures
        for category in categories
        if (gesture := normalize_gesture(category.category_name)) is not None
    ]
    if not candidates:
        return None, 0.0
    return max(candidates, key=lambda candidate: candidate[1])


def select_landmarks(groups: list[list[Any]], limit: int) -> list[Landmark]:
    return [
        Landmark(
            x=float(point.x),
            y=float(point.y),
            z=float(point.z),
            visibility=(
                float(point.visibility) if getattr(point, "visibility", None) is not None else None
            ),
        )
        for group in groups[:limit]
        for point in group
    ]


def encode_segmentation_mask(masks: list[Any], width: int, height: int) -> np.ndarray:
    if not masks:
        return np.zeros((height, width), dtype=np.uint8)
    probabilities = np.asarray(masks[0].numpy_view(), dtype=np.float32).squeeze()
    if probabilities.shape != (height, width):
        probabilities = cv2.resize(probabilities, (width, height), interpolation=cv2.INTER_LINEAR)
    return np.clip(probabilities * 255.0, 0.0, 255.0).astype(np.uint8)


def rate_is_due(now: float, deadline: float, interval: float) -> bool:
    tolerance = min(interval * 0.1, 0.005)
    return now + tolerance >= deadline


def advance_deadline(deadline: float, now: float, interval: float) -> float:
    steps = max(1, int((now - deadline) / interval) + 1)
    return deadline + steps * interval


def expand_pose_constraint(mask: np.ndarray) -> np.ndarray:
    diameter = POSE_CONSTRAINT_RADIUS * 2 + 1
    kernel = cv2.getStructuringElement(cv2.MORPH_RECT, (diameter, diameter))
    return cv2.dilate(mask, kernel)


class PoseConstraintStore:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._mask: np.ndarray | None = None

    def update(self, pose_mask: np.ndarray) -> None:
        constraint = expand_pose_constraint(pose_mask)
        with self._lock:
            self._mask = constraint

    def constrain(self, person_mask: np.ndarray) -> np.ndarray:
        with self._lock:
            constraint = self._mask
        if constraint is None or constraint.shape != person_mask.shape:
            return np.zeros_like(person_mask)
        return cv2.min(person_mask, constraint)


class MediaPipeDetector:
    def __init__(self, model_dir: Path, minimum_confidence: float) -> None:
        vision = mp.tasks.vision
        base_options = mp.tasks.BaseOptions
        self._gesture = vision.GestureRecognizer.create_from_options(
            vision.GestureRecognizerOptions(
                base_options=base_options(
                    model_asset_path=str(model_dir / "gesture_recognizer.task")
                ),
                running_mode=vision.RunningMode.VIDEO,
                num_hands=2,
                min_hand_detection_confidence=minimum_confidence,
                min_hand_presence_confidence=minimum_confidence,
                min_tracking_confidence=minimum_confidence,
                canned_gesture_classifier_options=mp.tasks.components.processors.ClassifierOptions(
                    category_denylist=["None"]
                ),
            )
        )
        self._face = vision.FaceLandmarker.create_from_options(
            vision.FaceLandmarkerOptions(
                base_options=base_options(model_asset_path=str(model_dir / "face_landmarker.task")),
                running_mode=vision.RunningMode.VIDEO,
                num_faces=1,
                min_face_detection_confidence=minimum_confidence,
                min_face_presence_confidence=minimum_confidence,
                min_tracking_confidence=minimum_confidence,
            )
        )
        self._pose = vision.PoseLandmarker.create_from_options(
            vision.PoseLandmarkerOptions(
                base_options=base_options(
                    model_asset_path=str(model_dir / "pose_landmarker_lite.task")
                ),
                running_mode=vision.RunningMode.VIDEO,
                num_poses=1,
                min_pose_detection_confidence=minimum_confidence,
                min_pose_presence_confidence=minimum_confidence,
                min_tracking_confidence=minimum_confidence,
                output_segmentation_masks=True,
            )
        )

    def detect(
        self, frame_bgr: np.ndarray, timestamp_ms: int
    ) -> tuple[
        list[Landmark],
        list[Landmark],
        list[Landmark],
        str | None,
        float,
        np.ndarray,
    ]:
        frame_rgb = cv2.cvtColor(frame_bgr, cv2.COLOR_BGR2RGB)
        image = mp.Image(image_format=mp.ImageFormat.SRGB, data=frame_rgb)
        face_result = self._face.detect_for_video(image, timestamp_ms)
        gesture_result = self._gesture.recognize_for_video(image, timestamp_ms)
        pose_result = self._pose.detect_for_video(image, timestamp_ms)
        gesture, confidence = select_gesture(gesture_result.gestures)
        face_landmarks = select_landmarks(face_result.face_landmarks, 1)
        hand_landmarks = select_landmarks(gesture_result.hand_landmarks, 2)
        pose_landmarks = select_landmarks(pose_result.pose_landmarks, 1)
        height, width = frame_bgr.shape[:2]
        pose_mask = encode_segmentation_mask(pose_result.segmentation_masks, width, height)
        return (
            face_landmarks,
            hand_landmarks,
            pose_landmarks,
            gesture,
            confidence,
            pose_mask,
        )

    def close(self) -> None:
        self._gesture.close()
        self._face.close()
        self._pose.close()

    def __enter__(self) -> MediaPipeDetector:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


class MediaPipeSegmenter:
    def __init__(self, model_dir: Path, pose_constraints: PoseConstraintStore) -> None:
        vision = mp.tasks.vision
        self._segmenter = vision.ImageSegmenter.create_from_options(
            vision.ImageSegmenterOptions(
                base_options=mp.tasks.BaseOptions(
                    model_asset_path=str(model_dir / "selfie_segmenter.tflite")
                ),
                running_mode=vision.RunningMode.VIDEO,
                output_confidence_masks=True,
                output_category_mask=False,
            )
        )
        self._pose_constraints = pose_constraints

    def segment(self, frame_bgr: np.ndarray, timestamp_ms: int) -> np.ndarray:
        frame_rgb = cv2.cvtColor(frame_bgr, cv2.COLOR_BGR2RGB)
        image = mp.Image(image_format=mp.ImageFormat.SRGB, data=frame_rgb)
        result = self._segmenter.segment_for_video(image, timestamp_ms)
        height, width = frame_bgr.shape[:2]
        mask = encode_segmentation_mask(result.confidence_masks, width, height)
        return self._pose_constraints.constrain(mask)

    def close(self) -> None:
        self._segmenter.close()

    def __enter__(self) -> MediaPipeSegmenter:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


class ObservationPublisher:
    def __init__(self, daemon_url: str, timeout_seconds: float = 2.0) -> None:
        self._url = f"{daemon_url.rstrip('/')}/api/v1/perception/observations"
        self._mask_url = f"{daemon_url.rstrip('/')}/api/v1/perception/mask"
        self._timeout_seconds = timeout_seconds

    def publish(self, observation: Observation) -> None:
        body = json.dumps(asdict(observation), separators=(",", ":")).encode()
        request = urllib.request.Request(
            self._url,
            data=body,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=self._timeout_seconds) as response:  # noqa: S310
                if response.status != 204:
                    raise RuntimeError(f"daemon returned HTTP {response.status}")
        except urllib.error.URLError as error:
            raise RuntimeError(f"failed to publish observation: {error.reason}") from error

    def publish_mask(self, frame_id: int, captured_at_ms: int, mask: np.ndarray) -> None:
        if mask.ndim != 2 or mask.dtype != np.uint8:
            raise ValueError("segmentation mask must be a two-dimensional uint8 array")
        height, width = mask.shape
        request = urllib.request.Request(
            self._mask_url,
            data=np.ascontiguousarray(mask).tobytes(),
            headers={
                "Content-Type": "application/octet-stream",
                "X-Tarsier-Frame-Id": str(frame_id),
                "X-Tarsier-Captured-At-Ms": str(captured_at_ms),
                "X-Tarsier-Mask-Width": str(width),
                "X-Tarsier-Mask-Height": str(height),
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=self._timeout_seconds) as response:  # noqa: S310
                if response.status != 204:
                    raise RuntimeError(f"daemon returned HTTP {response.status}")
        except urllib.error.URLError as error:
            raise RuntimeError(f"failed to publish video mask: {error.reason}") from error


@dataclass(frozen=True)
class DetectionFrame:
    frame_id: int
    captured_at_ms: int
    timestamp_ms: int
    frame_bgr: np.ndarray


class ObservationProcessor:
    def __init__(
        self,
        daemon_url: str,
        model_dir: Path,
        minimum_confidence: float,
        pose_constraints: PoseConstraintStore,
    ) -> None:
        self._daemon_url = daemon_url
        self._model_dir = model_dir
        self._minimum_confidence = minimum_confidence
        self._pose_constraints = pose_constraints
        self._frames: queue.Queue[DetectionFrame | None] = queue.Queue(maxsize=1)
        self._lock = threading.Lock()
        self._published_count = 0
        self._error: BaseException | None = None
        self._thread = threading.Thread(
            target=self._run,
            name="tarsier-observations",
            daemon=True,
        )

    @property
    def published_count(self) -> int:
        with self._lock:
            return self._published_count

    def submit(self, frame: DetectionFrame) -> None:
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
            raise RuntimeError("observation processor failed") from error

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
            publisher = ObservationPublisher(self._daemon_url)
            with MediaPipeDetector(self._model_dir, self._minimum_confidence) as detector:
                while (frame := self._frames.get()) is not None:
                    inference_started = time.perf_counter()
                    (
                        face_landmarks,
                        hand_landmarks,
                        pose_landmarks,
                        gesture,
                        confidence,
                        pose_mask,
                    ) = detector.detect(frame.frame_bgr, frame.timestamp_ms)
                    latency_ms = (time.perf_counter() - inference_started) * 1000.0
                    self._pose_constraints.update(pose_mask)
                    observation = Observation(
                        frame_id=frame.frame_id,
                        captured_at_ms=frame.captured_at_ms,
                        face_detected=bool(face_landmarks),
                        face_landmarks=face_landmarks,
                        hand_detected=bool(hand_landmarks),
                        hand_landmarks=hand_landmarks,
                        pose_detected=bool(pose_landmarks),
                        pose_landmarks=pose_landmarks,
                        gesture=gesture,
                        confidence=confidence,
                        latency_ms=latency_ms,
                    )
                    try:
                        publisher.publish(observation)
                        with self._lock:
                            self._published_count += 1
                    except RuntimeError as error:
                        LOGGER.warning("%s", error)
        except BaseException as error:
            LOGGER.exception("observation processor stopped")
            with self._lock:
                self._error = error

    def __enter__(self) -> ObservationProcessor:
        self._thread.start()
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


def capture_frames(source: str, width: int, height: int) -> Iterator[np.ndarray]:
    capture_source: int | str = int(source) if source.isdigit() else source
    if source.startswith(("http://", "https://")):
        capture = cv2.VideoCapture(capture_source)
    else:
        capture = cv2.VideoCapture(capture_source, cv2.CAP_V4L2)
    capture.set(cv2.CAP_PROP_FRAME_WIDTH, width)
    capture.set(cv2.CAP_PROP_FRAME_HEIGHT, height)
    capture.set(cv2.CAP_PROP_BUFFERSIZE, 1)
    if not capture.isOpened():
        capture.release()
        raise RuntimeError(f"failed to open video source {source}")
    try:
        while True:
            ok, frame = capture.read()
            if not ok:
                raise RuntimeError(f"failed to read a frame from {source}")
            yield frame
    finally:
        capture.release()


def run_worker(
    *,
    source: str,
    width: int,
    height: int,
    fps: float,
    mask_fps: float,
    daemon_url: str,
    model_dir: Path,
    minimum_confidence: float,
) -> None:
    publisher = ObservationPublisher(daemon_url)
    pose_constraints = PoseConstraintStore()
    observation_interval = 1.0 / fps
    mask_interval = 1.0 / mask_fps
    next_observation_at = time.monotonic()
    next_mask_at = next_observation_at
    started_at = time.monotonic()
    metrics_started_at = started_at
    masks_published = 0
    previous_observations_published = 0
    with (
        MediaPipeSegmenter(model_dir, pose_constraints) as segmenter,
        ObservationProcessor(
            daemon_url,
            model_dir,
            minimum_confidence,
            pose_constraints,
        ) as observation_processor,
    ):
        for frame_id, frame in enumerate(capture_frames(source, width, height), start=1):
            observation_processor.raise_if_failed()
            now = time.monotonic()
            mask_due = rate_is_due(now, next_mask_at, mask_interval)
            observation_due = rate_is_due(now, next_observation_at, observation_interval)
            if not mask_due and not observation_due:
                continue
            timestamp_ms = max(0, int((now - started_at) * 1000))
            captured_at_ms = time.time_ns() // 1_000_000
            if observation_due:
                next_observation_at = advance_deadline(
                    next_observation_at, now, observation_interval
                )
                observation_processor.submit(
                    DetectionFrame(frame_id, captured_at_ms, timestamp_ms, frame)
                )
            if mask_due:
                next_mask_at = advance_deadline(next_mask_at, now, mask_interval)
                mask = segmenter.segment(frame, timestamp_ms)
                try:
                    publisher.publish_mask(frame_id, captured_at_ms, mask)
                    masks_published += 1
                except RuntimeError as error:
                    LOGGER.warning("%s", error)
            metrics_elapsed = now - metrics_started_at
            if metrics_elapsed >= 10.0:
                observations_published = observation_processor.published_count
                LOGGER.info(
                    "worker cadence: masks %.1f FPS, observations %.1f FPS",
                    masks_published / metrics_elapsed,
                    (observations_published - previous_observations_published) / metrics_elapsed,
                )
                metrics_started_at = now
                masks_published = 0
                previous_observations_published = observations_published


def run_mock(daemon_url: str, fps: float, open_palm: bool) -> None:
    publisher = ObservationPublisher(daemon_url)
    interval = 1.0 / fps
    for frame_id in range(1, 31):
        started = time.perf_counter()
        publisher.publish(
            Observation(
                frame_id=frame_id,
                captured_at_ms=time.time_ns() // 1_000_000,
                face_detected=True,
                face_landmarks=[],
                hand_detected=open_palm,
                hand_landmarks=[],
                pose_detected=False,
                pose_landmarks=[],
                gesture="open_palm" if open_palm else None,
                confidence=0.96 if open_palm else 0.0,
                latency_ms=(time.perf_counter() - started) * 1000.0,
            )
        )
        time.sleep(interval)
