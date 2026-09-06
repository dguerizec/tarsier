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


def test_renderer_exports_synchronized_straight_alpha_and_clears_on_tracking_loss(portrait):
    pytest.importorskip("moderngl")
    with Portrait3DAvatarEngine(portrait, 160, 90) as engine:
        image = engine.render(AvatarMotion())
        assert image.shape == (90, 160, 4)
        alpha = image[:, :, 3]
        assert alpha.min() == 0 and alpha.max() == 255
        edges = (alpha > 0) & (alpha < 255)
        assert edges.any()
        assert np.all(image[:, :, 2][edges] >= 250)
        assert np.all(image[:, :, :2][alpha > 0] == 0)
        assert not engine.render(None).any()
        assert engine.render(AvatarMotion()).any()
