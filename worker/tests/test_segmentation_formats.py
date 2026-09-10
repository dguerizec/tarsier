from types import SimpleNamespace

import cv2
import numpy as np
import pytest

from tarsier_perception.worker import PoseConstraintStore, encode_segmentation_mask


def source(values):
    return [SimpleNamespace(numpy_view=lambda: values)]


def test_gpu_rgba_uses_red_confidence_without_rescaling_bytes():
    red = np.array([[0, 32, 128], [255, 200, 64]], dtype=np.uint8)
    rgba = np.zeros((2, 3, 4), dtype=np.uint8)
    rgba[..., 0] = red
    rgba[..., 3] = 255  # Alpha is not the confidence channel.
    mask = encode_segmentation_mask(source(rgba), 3, 2)
    np.testing.assert_array_equal(mask, red)
    assert mask.flags.c_contiguous
    assert len(mask.tobytes()) == 6
    enlarged = encode_segmentation_mask(source(rgba), 6, 4)
    np.testing.assert_array_equal(enlarged, cv2.resize(red, (6, 4)))
    store = PoseConstraintStore()
    store.update(mask)
    assert store.constrain(np.full((2, 3), 180, dtype=np.uint8)).shape == (2, 3)


@pytest.mark.parametrize('shape', [(1, 3, 1), (3, 1, 1), (1, 1, 1)])
def test_single_pixel_dimensions_are_preserved(shape):
    values = np.full(shape, .5, dtype=np.float32)
    result = encode_segmentation_mask(source(values), shape[1], shape[0])
    assert result.shape == shape[:2]
    assert np.all(result == 127)


def test_unknown_channel_layout_is_rejected():
    with pytest.raises(ValueError, match='Unsupported segmentation mask shape'):
        encode_segmentation_mask(source(np.zeros((2, 3, 2), dtype=np.float32)), 3, 2)
