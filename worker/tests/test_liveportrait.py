from __future__ import annotations

import pytest
import torch

from tarsier_perception.liveportrait.engine import (
    MotionStabilizer,
    limit_relative_pose,
    rotation_matrix,
    transfer_motion,
)


def motion(value: float) -> dict[str, torch.Tensor]:
    return {
        "pitch": torch.full((1, 1), value),
        "yaw": torch.full((1, 1), value),
        "roll": torch.full((1, 1), value),
        "exp": torch.full((1, 21, 3), value),
        "scale": torch.full((1, 1), value),
        "t": torch.full((1, 3), value),
    }


def test_motion_stabilizer_reduces_pose_jitter_without_delaying_expression() -> None:
    stabilizer = MotionStabilizer(factor=0.25)

    first = stabilizer.update(motion(0.0))
    jitter = stabilizer.update(motion(1.0))

    assert first["yaw"].item() == 0.0
    assert jitter["yaw"].item() == pytest.approx(0.25)
    assert jitter["exp"][0, 0, 0].item() == pytest.approx(1.0)


def test_motion_stabilizer_leaves_global_motion_for_explicit_anchoring() -> None:
    stabilizer = MotionStabilizer(factor=0.25)
    stabilizer.update(motion(0.0))

    stabilized = stabilizer.update(motion(1.0))

    assert stabilized["scale"].item() == 1.0
    assert stabilized["t"][0, 0].item() == 1.0


def test_relative_pose_is_attenuated_and_bounded_around_the_initial_view() -> None:
    initial = motion(10.0)
    driving = motion(110.0)

    pose = limit_relative_pose(driving, initial)

    assert pose["pitch"].item() == pytest.approx(18.0)
    assert pose["yaw"].item() == pytest.approx(22.0)
    assert pose["roll"].item() == pytest.approx(18.0)

    moderate = limit_relative_pose(motion(30.0), initial)

    assert moderate["yaw"].item() == pytest.approx(17.0)


def test_motion_transfer_anchors_scale_and_translation_to_the_source() -> None:
    source = motion(0.0)
    source["kp"] = torch.zeros((1, 21, 3))
    source["scale"] = torch.full((1, 1), 2.0)
    source["t"] = torch.full((1, 3), 0.5)
    initial = motion(0.0)
    driving = motion(0.0)
    driving["scale"] = torch.full((1, 1), 50.0)
    driving["t"] = torch.full((1, 3), 50.0)
    identity_rotation = rotation_matrix(
        torch.zeros((1, 1)), torch.zeros((1, 1)), torch.zeros((1, 1))
    )

    driven = transfer_motion(
        source,
        identity_rotation,
        driving,
        initial,
        identity_rotation,
    )

    assert torch.all(driven == 0.5)


def test_source_preparation_does_not_replace_active_source_or_driving_state(tmp_path):
    from threading import Lock

    import cv2
    import numpy as np

    from tarsier_perception.liveportrait.engine import ComicAvatarEngine

    engine = ComicAvatarEngine.__new__(ComicAvatarEngine)
    engine._device = torch.device("cpu")
    engine._inference_lock = Lock()
    engine.source = object()
    original = engine.source
    engine._driving_initial_info = object()
    driving = engine._driving_initial_info
    info = motion(0.0)
    info["kp"] = torch.zeros((1, 21, 3))
    info["scale"] = torch.ones((1, 1))
    engine._keypoint_info = lambda _: info
    engine._appearance = lambda tensor: tensor.mean()
    portrait = tmp_path / "new.png"
    cv2.imwrite(str(portrait), np.full((256, 256, 3), 128, dtype=np.uint8))
    prepared = engine.prepare_source(portrait)
    assert prepared.features.item() == pytest.approx(128 / 255)
    assert prepared.keypoints.shape == (1, 21, 3)
    assert engine.source is original
    assert engine._driving_initial_info is driving
