from pathlib import Path
from types import SimpleNamespace

import numpy as np
import pytest

from tarsier_perception import worker


@pytest.mark.parametrize("rotation", [0, 90, 180, 270])
def test_rotation_preserves_rectangular_pixels_and_source_landmarks(rotation):
    source = np.arange(24, dtype=np.uint8).reshape(4, 6)
    upright = worker.rotate_for_inference(source, rotation)
    assert upright.flags.c_contiguous
    assert upright.shape == ((6, 4) if rotation in (90, 270) else (4, 6))
    np.testing.assert_array_equal(worker.rotate_for_inference(upright, (-rotation) % 360), source)
    # A source point at (0.2, 0.3), represented in each upright frame.
    x, y = {0: (0.2, 0.3), 90: (0.7, 0.2), 180: (0.8, 0.7), 270: (0.3, 0.8)}[rotation]
    z = 0.6 * (6 / 4 if rotation in (90, 270) else 1)
    point = worker.source_landmarks([worker.Landmark(x, y, z, 0.9)], rotation, 6, 4)[0]
    assert (point.x, point.y, point.z, point.visibility) == pytest.approx((0.2, 0.3, 0.6, 0.9))


def test_observation_processor_detects_upright_and_publishes_source_coordinates(monkeypatch):
    source = np.arange(18, dtype=np.uint8).reshape(2, 3, 3)
    upright_mask = np.array([[0, 0, 255], [0, 255, 255]], dtype=np.uint8)
    published, masks = [], []

    class Detector:
        def __init__(self, *_):
            pass

        def __enter__(self):
            return self

        def __exit__(self, *_):
            pass

        def detect(self, frame, timestamp):
            np.testing.assert_array_equal(frame, source[::-1, ::-1])
            assert timestamp == 123
            points = [worker.Landmark(0.2, 0.3, 0.1, 0.9)]
            return points, points, points, "open_palm", 0.95, upright_mask

    monkeypatch.setattr(worker, "MediaPipeDetector", Detector)
    monkeypatch.setattr(
        worker, "ObservationPublisher", lambda _: SimpleNamespace(publish=published.append)
    )
    processor = worker.ObservationProcessor(
        "http://unused", Path("unused"), 0.5, SimpleNamespace(update=masks.append)
    )
    processor._frames.put(worker.DetectionFrame(42, 456, 123, source, 180))
    # Run synchronously and end after the single test frame.
    original_get = processor._frames.get
    calls = iter([True, False])
    monkeypatch.setattr(processor._frames, "get", lambda: original_get() if next(calls) else None)
    processor._run()
    processor.raise_if_failed()
    assert processor.published_count == 1
    observation = published[0]
    assert (observation.frame_id, observation.captured_at_ms) == (42, 456)
    assert (observation.image_width, observation.image_height) == (3, 2)
    assert observation.face_detected
    for points in [
        observation.face_landmarks,
        observation.hand_landmarks,
        observation.pose_landmarks,
    ]:
        assert (points[0].x, points[0].y) == pytest.approx((0.8, 0.7))
    np.testing.assert_array_equal(masks[0], upright_mask[::-1, ::-1])


def test_segmenter_restores_mask_before_source_pose_constraint():
    source = np.zeros((4, 6, 3), dtype=np.uint8)
    probabilities = np.zeros((6, 4), dtype=np.float32)
    probabilities[1, 2] = 1.0
    seen = []

    def segment(image, timestamp):
        assert image.numpy_view().shape == (6, 4, 3)
        assert timestamp == 123
        return SimpleNamespace(confidence_masks=[SimpleNamespace(numpy_view=lambda: probabilities)])

    def constrain(mask):
        seen.append(mask)
        return mask

    segmenter = worker.MediaPipeSegmenter.__new__(worker.MediaPipeSegmenter)
    segmenter._segmenter = SimpleNamespace(segment_for_video=segment)
    segmenter._pose_constraints = SimpleNamespace(constrain=constrain)
    result = segmenter.segment(source, 123, 90)
    assert seen[0].shape == (4, 6)
    assert result[1, 1] == 255
    assert np.count_nonzero(result) == 1
