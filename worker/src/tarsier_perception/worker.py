from __future__ import annotations

import json
import logging
import time
import urllib.error
import urllib.request
from collections.abc import Iterator
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

import cv2
import mediapipe as mp
import numpy as np

LOGGER = logging.getLogger(__name__)


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
        mask = encode_segmentation_mask(pose_result.segmentation_masks, width, height)
        return face_landmarks, hand_landmarks, pose_landmarks, gesture, confidence, mask

    def close(self) -> None:
        self._gesture.close()
        self._face.close()
        self._pose.close()

    def __enter__(self) -> MediaPipeDetector:
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
    daemon_url: str,
    model_dir: Path,
    minimum_confidence: float,
) -> None:
    publisher = ObservationPublisher(daemon_url)
    frame_interval = 1.0 / fps
    next_frame_at = time.monotonic()
    started_at = time.monotonic()
    with MediaPipeDetector(model_dir, minimum_confidence) as detector:
        for frame_id, frame in enumerate(capture_frames(source, width, height), start=1):
            now = time.monotonic()
            if now < next_frame_at:
                continue
            next_frame_at = now + frame_interval
            timestamp_ms = max(0, int((now - started_at) * 1000))
            captured_at_ms = time.time_ns() // 1_000_000
            inference_started = time.perf_counter()
            (
                face_landmarks,
                hand_landmarks,
                pose_landmarks,
                gesture,
                confidence,
                mask,
            ) = detector.detect(frame, timestamp_ms)
            latency_ms = (time.perf_counter() - inference_started) * 1000.0
            observation = Observation(
                frame_id=frame_id,
                captured_at_ms=captured_at_ms,
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
                publisher.publish_mask(frame_id, captured_at_ms, mask)
            except RuntimeError as error:
                LOGGER.warning("%s", error)
            try:
                publisher.publish(observation)
            except RuntimeError as error:
                LOGGER.warning("%s", error)


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
