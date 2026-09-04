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
class Observation:
    frame_id: int
    captured_at_ms: int
    face_detected: bool
    hand_detected: bool
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
                num_hands=1,
                min_hand_detection_confidence=minimum_confidence,
                min_hand_presence_confidence=minimum_confidence,
                min_tracking_confidence=minimum_confidence,
                canned_gesture_classifier_options=mp.tasks.components.processors.ClassifierOptions(
                    category_denylist=["None"]
                ),
            )
        )
        self._face = vision.FaceDetector.create_from_options(
            vision.FaceDetectorOptions(
                base_options=base_options(
                    model_asset_path=str(model_dir / "blaze_face_short_range.tflite")
                ),
                running_mode=vision.RunningMode.VIDEO,
                min_detection_confidence=minimum_confidence,
            )
        )

    def detect(
        self, frame_bgr: np.ndarray, timestamp_ms: int
    ) -> tuple[bool, bool, str | None, float]:
        frame_rgb = cv2.cvtColor(frame_bgr, cv2.COLOR_BGR2RGB)
        image = mp.Image(image_format=mp.ImageFormat.SRGB, data=frame_rgb)
        face_result = self._face.detect_for_video(image, timestamp_ms)
        gesture_result = self._gesture.recognize_for_video(image, timestamp_ms)
        gesture, confidence = select_gesture(gesture_result.gestures)
        return (
            bool(face_result.detections),
            bool(gesture_result.hand_landmarks),
            gesture,
            confidence,
        )

    def close(self) -> None:
        self._gesture.close()
        self._face.close()

    def __enter__(self) -> MediaPipeDetector:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


class ObservationPublisher:
    def __init__(self, daemon_url: str, timeout_seconds: float = 2.0) -> None:
        self._url = f"{daemon_url.rstrip('/')}/api/v1/perception/observations"
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
            face, hand, gesture, confidence = detector.detect(frame, timestamp_ms)
            latency_ms = (time.perf_counter() - inference_started) * 1000.0
            observation = Observation(
                frame_id=frame_id,
                captured_at_ms=captured_at_ms,
                face_detected=face,
                hand_detected=hand,
                gesture=gesture,
                confidence=confidence,
                latency_ms=latency_ms,
            )
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
                hand_detected=open_palm,
                gesture="open_palm" if open_palm else None,
                confidence=0.96 if open_palm else 0.0,
                latency_ms=(time.perf_counter() - started) * 1000.0,
            )
        )
        time.sleep(interval)
