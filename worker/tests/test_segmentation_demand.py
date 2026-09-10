"""Demand gating must skip inference without starving other worker consumers."""
from contextlib import nullcontext
from pathlib import Path
from types import SimpleNamespace

import numpy as np
import pytest

from tarsier_perception import worker
from tarsier_perception.avatar import VideoIdentityClient
from tarsier_perception.delegates import Delegates


@pytest.mark.parametrize("identity,background,required", [
    ("camera", False, False), ("camera", True, True),
    ("depth-map", True, False), ("portrait3d", True, False),
    ("liveportrait", True, False),
])
def test_segmentation_demand(monkeypatch, identity, background, required):
    client = VideoIdentityClient("http://localhost")
    monkeypatch.setattr(client, "selected_identity", lambda: identity)
    client._background_enabled = background
    assert client.segmentation_required() is required


@pytest.mark.parametrize("delegate", ["cpu", "gpu"])
def test_worker_lazily_resumes_and_releases_segmentation_without_starving_consumers(
    monkeypatch, delegate,
):
    demand = [False, False, True, True, False, False, True]
    active = iter(demand)
    opened, closed, segmented, masks = [], [], [], []
    observations, depths, avatars = [], [], []
    clock = iter(range(100, 200))
    monkeypatch.setattr(worker.time, "monotonic", lambda: next(clock))
    monkeypatch.setattr(worker, "VideoIdentityClient", lambda _: SimpleNamespace(
        segmentation_required=lambda: next(active)))
    monkeypatch.setattr(worker.stages, "publishing", lambda _: nullcontext())

    class Segmenter:
        def __init__(self, model_dir, constraints, delegate):
            opened.append(delegate)

        def __enter__(self):
            return self

        def __exit__(self, *_):
            closed.append(True)

        def segment(self, frame, timestamp, rotation):
            segmented.append((int(frame[0, 0, 0]), rotation))
            return np.full((2, 2), 123, dtype=np.uint8)

    def processor(items):
        return nullcontext(SimpleNamespace(
            raise_if_failed=lambda: None, submit=items.append,
            published_count=0, refined_mask_count=0, mask_refinement_active=False))

    monkeypatch.setattr(worker, "MediaPipeSegmenter", Segmenter)
    monkeypatch.setattr(worker, "ObservationProcessor", lambda *a: processor(observations))
    monkeypatch.setattr(worker, "DepthProcessor", lambda *a: processor(depths))
    monkeypatch.setattr(worker, "AvatarProcessor", lambda *a, **kw: processor(avatars))
    monkeypatch.setattr(worker, "ObservationPublisher", lambda _: SimpleNamespace(
        publish_mask=lambda *args: masks.append(args)))
    monkeypatch.setattr(worker, "capture_frames", lambda *a: iter([
        worker.SourceFrame(i + 100, i + 1000, np.full((2, 2, 3), i, np.uint8), 90)
        for i in range(len(demand))
    ]))
    worker.run_worker(source="unused", width=2, height=2, fps=10, mask_fps=30,
                      daemon_url="http://localhost", model_dir=Path("unused"),
                      minimum_confidence=0.5, delegates=Delegates(segmentation=delegate),
                      avatar_engine="portrait3d", depth_enabled=True)
    assert opened == [delegate, delegate]
    assert len(closed) == 2
    assert segmented == [(2, 90), (3, 90), (6, 90)]
    assert [(m[0], m[1]) for m in masks] == [(102, 1002), (103, 1003), (106, 1006)]
    assert len(observations) == len(depths) == len(avatars) == len(demand)
    assert [f.person_mask is not None for f in depths] == demand
