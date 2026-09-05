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
