import json

import numpy as np
import pytest

from tarsier_perception.portrait3d import MORPHS, Portrait3DAvatarEngine, load_asset
from tarsier_perception.stylized3d import AvatarMotion


@pytest.fixture
def portrait(tmp_path):
    data = np.zeros((3, 30), np.float32)
    data[:, :3] = [[-0.5, 0, -0.4], [0.5, 0, -0.4], [0, 0, 0.6]]
    data[:, 5] = 1
    np.savez(tmp_path / "mesh.npz", draw0=data)
    (tmp_path / "manifest.json").write_text(
        json.dumps(
            {
                "schema_version": 1,
                "morphs": list(MORPHS),
                "draws": [{"array": "draw0", "texture": None, "color": [1, 0, 0]}],
            }
        )
    )
    return tmp_path


def test_asset_rejects_nonfinite_morphs_and_external_textures(portrait):
    manifest, arrays = load_asset(portrait)
    arrays["draw0"][0, 8] = np.nan
    np.savez(portrait / "mesh.npz", **arrays)
    with pytest.raises(ValueError, match="triangle stream"):
        load_asset(portrait)
    arrays["draw0"][0, 8] = 0
    np.savez(portrait / "mesh.npz", **arrays)
    manifest["draws"][0]["texture"] = "../outside.png"
    (portrait / "manifest.json").write_text(json.dumps(manifest))
    with pytest.raises(ValueError, match="local asset"):
        load_asset(portrait)


def test_renderer_exports_synchronized_straight_alpha_and_holds_on_tracking_loss(portrait):
    pytest.importorskip("moderngl")
    with Portrait3DAvatarEngine(portrait, 160, 90) as engine:
        assert not engine.render(None).any()
        image = engine.render(AvatarMotion())
        assert image.shape == (90, 160, 4)
        alpha = image[:, :, 3]
        assert alpha.min() == 0 and alpha.max() == 255
        edges = (alpha > 0) & (alpha < 255)
        assert edges.any()
        assert np.all(image[:, :, 2][edges] >= 250)
        assert np.all(image[:, :, :2][alpha > 0] == 0)
        assert np.array_equal(engine.render(None), image)
        assert engine.render(AvatarMotion()).any()


def test_turn_loss_reacquisition_returns_to_camera_center(portrait, monkeypatch):
    pytest.importorskip("moderngl")
    clock = [0.0]
    monkeypatch.setattr("tarsier_perception.portrait3d.time.monotonic", lambda: clock[0])
    with Portrait3DAvatarEngine(portrait, 160, 90) as engine:
        # Starting while turned must not establish a new neutral orientation.
        turned = engine.render(AvatarMotion(yaw=-1.1))
        rotation = np.frombuffer(engine.program["rotation"].read(), np.float32).reshape(3, 3).T
        assert rotation[1, 0] < -0.8
        clock[0] += 1
        assert np.array_equal(engine.render(None), turned)
        previous_yaw = -1.1
        for target in [-0.9, -0.6, -0.3, 0.0] + [0.0] * 12:
            clock[0] += 0.1
            engine.render(AvatarMotion(yaw=target))
            rotation = np.frombuffer(engine.program["rotation"].read(), np.float32).reshape(3, 3).T
            yaw = np.arctan2(rotation[1, 0], rotation[0, 0])
            assert previous_yaw <= yaw <= 0.000001
            previous_yaw = yaw
        assert abs(yaw) < 0.00001


def test_personal_pose_preserves_profile_angles_and_rotation_order(portrait):
    from types import SimpleNamespace

    import cv2

    from tarsier_perception.stylized3d import motion_from_mediapipe

    # MediaPipe camera axes: X right, Y up, Z toward the viewer.
    source_rotation = cv2.Rodrigues(np.array([0.35, -1.1, 0.2]))[0]
    result = SimpleNamespace(
        face_landmarks=[[object()]],
        face_blendshapes=[[]],
        facial_transformation_matrixes=[source_rotation],
    )
    motion = motion_from_mediapipe(result, limit_pose=False)
    assert abs(motion.yaw) > 0.65
    assert abs(motion_from_mediapipe(result).yaw) == 0.65
    pytest.importorskip("moderngl")
    with Portrait3DAvatarEngine(portrait, 160, 90) as engine:
        engine.render(motion)
        actual = np.frombuffer(engine.program["rotation"].read(), np.float32).reshape(3, 3).T
        # Portrait axes: X right, Y away from viewer, Z up.
        basis = np.array([[1, 0, 0], [0, 0, -1], [0, 1, 0]])
        np.testing.assert_allclose(actual, basis @ source_rotation @ basis.T, atol=1e-6)
