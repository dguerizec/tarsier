"""Independent detector lifetime, invocation and daemon-demand failure behavior."""
import io
import json
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

import numpy as np
import pytest

from tarsier_perception import demand, worker


def test_independent_models_skip_inference_and_release_after_grace(monkeypatch):
    now = [10.0]
    monkeypatch.setattr(worker, "time", SimpleNamespace(monotonic=lambda: now[0]))
    factories = {}
    for name in ("face", "hands", "pose"):
        task = Mock()
        task.detect_for_video.return_value = SimpleNamespace(
            face_landmarks=[], pose_landmarks=[], segmentation_masks=[]
        )
        task.recognize_for_video.return_value = SimpleNamespace(hand_landmarks=[], gestures=[])
        factories[name] = Mock(return_value=task)
        monkeypatch.setattr(worker.MediaPipeDetector, "_create_" + name, factories[name])
    with worker.MediaPipeDetector(Path("unused"), .5) as detector:
        assert not any(f.called for f in factories.values())
        detector.set_demand({"face": False, "hands": True, "pose": False})
        detector.detect(np.zeros((8, 8, 3), dtype=np.uint8), 1)
        factories['hands'].return_value.recognize_for_video.assert_called_once()
        assert not factories['face'].called and not factories['pose'].called
        detector.set_demand(dict.fromkeys(demand.MODELS, False))
        result = detector.detect(np.zeros((8, 8, 3), dtype=np.uint8), 2)
        assert result[:5] == ([], [], [], None, 0.0)
        factories['hands'].return_value.close.assert_not_called()
        now[0] += 3
        detector.set_demand(dict.fromkeys(demand.MODELS, False))
        factories['hands'].return_value.close.assert_called_once()
        detector.set_demand({"face": True, "hands": False, "pose": True})
        assert factories['face'].call_count == factories['pose'].call_count == 1
        assert detector.active_models == {"face": True, "hands": False, "pose": True}


def test_demand_cache_retries_and_preserves_detection_on_failure(monkeypatch):
    now = [0.0]
    monkeypatch.setattr(demand, "time", SimpleNamespace(monotonic=lambda: now[0]))
    models = {"face": False, "hands": True, "pose": False}
    request = Mock(side_effect=[io.BytesIO(json.dumps(models).encode()), OSError('offline')])
    monkeypatch.setattr(demand.urllib.request, "urlopen", request)
    client = demand.DetectionDemandClient('http://unused')
    assert client.models() == models
    assert client.models() == models
    assert request.call_count == 1
    now[0] += 1
    assert all(client.models().values())


@pytest.mark.parametrize('value', [None, {}, {"face": 1, "hands": False, "pose": False}])
def test_invalid_demand_does_not_disable_gestures(monkeypatch, value):
    monkeypatch.setattr(
        demand.urllib.request, "urlopen",
        lambda *_args, **_kwargs: io.BytesIO(json.dumps(value).encode()),
    )
    assert all(demand.DetectionDemandClient('http://unused').models().values())
