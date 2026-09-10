from pathlib import Path
from unittest.mock import Mock

import mediapipe as mp
import pytest

from tarsier_perception.avatar import MediaPipeAvatarTracker, MediaPipeFaceCropper
from tarsier_perception.cli import build_parser
from tarsier_perception.delegates import Delegates, base_options
from tarsier_perception.worker import MediaPipeDetector, MediaPipeSegmenter, PoseConstraintStore


def test_delegate_defaults_and_validation():
    assert set(vars(Delegates()).values()) == {"cpu"}
    with pytest.raises(ValueError, match="Unsupported hands delegate"):
        Delegates(hands="cuda")
    with pytest.raises(ValueError, match="Unsupported MediaPipe delegate"):
        base_options(Path("model"), "cuda")
    args = build_parser().parse_args(["serve", "--hands-delegate", "gpu"])
    assert args.hands_delegate == "gpu"
    assert args.face_delegate == args.pose_delegate == args.segmentation_delegate == "cpu"
    with pytest.raises(SystemExit):
        build_parser().parse_args(["serve", "--pose-delegate", "automatic"])


def test_models_receive_independent_delegates(monkeypatch):
    options = {}

    def capture(name):
        def create(value):
            options[name] = value.base_options
            return Mock()
        return create

    vision = mp.tasks.vision
    for name in ("FaceLandmarker", "GestureRecognizer", "PoseLandmarker", "ImageSegmenter"):
        monkeypatch.setattr(getattr(vision, name), "create_from_options", capture(name))
    with MediaPipeDetector(Path("models"), .5, Delegates(face="gpu", hands="cpu", pose="gpu")):
        pass
    with MediaPipeSegmenter(Path("models"), PoseConstraintStore()):
        pass
    enum = mp.tasks.BaseOptions.Delegate
    assert options['FaceLandmarker'].delegate == enum.GPU
    assert options['GestureRecognizer'].delegate == enum.CPU
    assert options['PoseLandmarker'].delegate == enum.GPU
    assert options['ImageSegmenter'].delegate == enum.CPU
    assert options['GestureRecognizer'].model_asset_path == 'models/gesture_recognizer.task'
    for tracker in (MediaPipeAvatarTracker, MediaPipeFaceCropper):
        with tracker(Path("models"), "gpu"):
            assert options['FaceLandmarker'].delegate == enum.GPU


def test_gpu_initialization_failure_is_not_silently_retried_on_cpu(monkeypatch):
    factory = Mock(side_effect=RuntimeError("GPU context unavailable"))
    monkeypatch.setattr(mp.tasks.vision.ImageSegmenter, "create_from_options", factory)
    with pytest.raises(RuntimeError, match="GPU context unavailable"):
        MediaPipeSegmenter(Path("models"), PoseConstraintStore(), "gpu")
    assert factory.call_count == 1
    assert factory.call_args.args[0].base_options.delegate == mp.tasks.BaseOptions.Delegate.GPU
