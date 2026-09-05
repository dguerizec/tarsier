from __future__ import annotations

import io
from dataclasses import dataclass
from pathlib import Path

import pytest

from tarsier_perception.avatar import (
    AvatarPublisher,
    FaceCropGeometry,
    TemporalFaceCrop,
    VideoIdentityClient,
    compose_avatar_frame,
    crop_face_square,
)
from tarsier_perception.depth import (
    DEPTH_REPRESENTATION,
    DepthEstimate,
    DepthMaskRefiner,
    DepthPublisher,
    TemporalDepthScale,
    depth_input_width,
)
from tarsier_perception.models import describe_models
from tarsier_perception.stylized3d import (
    AvatarMotion,
    Stylized3DAvatarEngine,
    motion_from_mediapipe,
)
from tarsier_perception.worker import (
    Landmark,
    PoseConstraintStore,
    advance_deadline,
    encode_segmentation_mask,
    expand_pose_constraint,
    normalize_gesture,
    rate_is_due,
    read_mjpeg_parts,
    select_gesture,
    select_landmarks,
)


@dataclass
class Category:
    category_name: str
    score: float


@dataclass
class SourceLandmark:
    x: float
    y: float
    z: float


@dataclass
class SourcePoseLandmark(SourceLandmark):
    visibility: float


class SourceMask:
    def __init__(self, values: list[list[float]]) -> None:
        self._values = values

    def numpy_view(self):  # noqa: ANN201
        return self._values


@dataclass
class FaceCategory:
    category_name: str
    score: float


@dataclass
class FaceResult:
    face_landmarks: list[list[SourceLandmark]]
    face_blendshapes: list[list[FaceCategory]]
    facial_transformation_matrixes: list


class HttpResponse(io.BytesIO):
    status = 204

    def __enter__(self):  # noqa: ANN204
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


def test_normalizes_mediapipe_gesture_names() -> None:
    assert normalize_gesture("Open_Palm") == "open_palm"
    assert normalize_gesture("None") is None
    assert normalize_gesture(None) is None


def test_selects_best_available_gesture() -> None:
    gesture, confidence = select_gesture(
        [
            [Category("Closed_Fist", 0.42)],
            [Category("Open_Palm", 0.93)],
        ]
    )
    assert gesture == "open_palm"
    assert confidence == 0.93


def test_empty_results_are_an_absent_gesture() -> None:
    assert select_gesture([]) == (None, 0.0)
    assert select_gesture([[]]) == (None, 0.0)
    assert select_gesture([[Category("None", 0.99)]]) == (None, 0.0)


def test_neutral_category_does_not_hide_a_named_candidate() -> None:
    assert select_gesture([[Category("None", 0.91), Category("Open_Palm", 0.67)]]) == (
        "open_palm",
        0.67,
    )


def test_selects_landmarks_from_each_group_up_to_the_limit() -> None:
    groups = [
        [SourceLandmark(0.1, 0.2, -0.3), SourceLandmark(0.4, 0.5, -0.6)],
        [SourceLandmark(0.7, 0.8, -0.9)],
        [SourceLandmark(0.9, 0.8, -0.7)],
    ]
    assert select_landmarks(groups, 2) == [
        Landmark(0.1, 0.2, -0.3),
        Landmark(0.4, 0.5, -0.6),
        Landmark(0.7, 0.8, -0.9),
    ]
    assert select_landmarks([], 2) == []


def test_preserves_pose_landmark_visibility() -> None:
    assert select_landmarks([[SourcePoseLandmark(0.1, 0.2, -0.3, 0.85)]], 1) == [
        Landmark(0.1, 0.2, -0.3, 0.85)
    ]


def test_encodes_segmentation_probability_as_grayscale_mask() -> None:
    mask = encode_segmentation_mask([SourceMask([[0.0, 0.5], [1.0, 0.25]])], 2, 2)
    assert mask.dtype.name == "uint8"
    assert mask.tolist() == [[0, 127], [255, 63]]


def test_missing_segmentation_is_an_empty_mask() -> None:
    assert encode_segmentation_mask([], 3, 2).tolist() == [[0, 0, 0], [0, 0, 0]]


def test_rate_gate_keeps_its_cadence_across_small_frame_jitter() -> None:
    interval = 0.1
    deadline = 10.0
    assert rate_is_due(9.996, deadline, interval)
    assert not rate_is_due(9.990, deadline, interval)
    assert abs(advance_deadline(deadline, 10.001, interval) - 10.1) < 1e-9
    assert abs(advance_deadline(deadline, 10.350, interval) - 10.4) < 1e-9


def test_mjpeg_reader_preserves_source_frame_provenance() -> None:
    stream = io.BytesIO(
        b"--tarsier-frame\r\n"
        b"Content-Type: image/jpeg\r\n"
        b"Content-Length: 4\r\n"
        b"X-Tarsier-Frame-Id: 152\r\n"
        b"X-Tarsier-Captured-At-Ms: 1725000000033\r\n"
        b"\r\n"
        b"jpeg\r\n"
    )

    parts = list(read_mjpeg_parts(stream))

    assert parts == [
        (
            {
                "content-type": "image/jpeg",
                "content-length": "4",
                "x-tarsier-frame-id": "152",
                "x-tarsier-captured-at-ms": "1725000000033",
            },
            b"jpeg",
        )
    ]


def test_pose_constraint_expands_around_the_detected_person() -> None:
    import numpy as np

    pose_mask = np.zeros((100, 100), dtype=np.uint8)
    pose_mask[50, 50] = 255
    constraint = expand_pose_constraint(pose_mask)
    assert constraint[50, 50] == 255
    assert constraint[50, 50 + 32] == 255
    assert constraint[50, 50 + 33] == 0


def test_pose_constraint_fails_closed_until_a_pose_is_available() -> None:
    import numpy as np

    constraints = PoseConstraintStore()
    person_mask = np.full((100, 100), 255, dtype=np.uint8)
    assert np.count_nonzero(constraints.constrain(person_mask)) == 0

    pose_mask = np.zeros((100, 100), dtype=np.uint8)
    pose_mask[50, 50] = 255
    constraints.update(pose_mask)
    constrained = constraints.constrain(person_mask)
    assert constrained[50, 50] == 255
    assert constrained[0, 0] == 0


def test_missing_models_are_reported_as_unverified(tmp_path: Path) -> None:
    descriptions = describe_models(tmp_path)
    assert len(descriptions) == 4
    assert all(not model["exists"] for model in descriptions)
    assert all(not model["verified"] for model in descriptions)


def test_avatar_models_are_opt_in(tmp_path: Path) -> None:
    assert len(describe_models(tmp_path)) == 4
    descriptions = describe_models(tmp_path, include_avatar=True)
    assert len(descriptions) == 9
    assert any(model["name"].endswith("motion_extractor.pth") for model in descriptions)


def test_depth_models_are_opt_in(tmp_path: Path) -> None:
    descriptions = describe_models(tmp_path, include_depth=True)
    assert len(descriptions) == 7
    assert any(model["name"].endswith("model.safetensors") for model in descriptions)


def test_face_crop_is_square_and_pads_at_frame_edges() -> None:
    import numpy as np

    frame = np.arange(20 * 30 * 3, dtype=np.uint8).reshape(20, 30, 3)
    landmarks = [SourceLandmark(0.0, 0.0, 0.0), SourceLandmark(0.2, 0.3, 0.0)]
    cropped = crop_face_square(frame, landmarks, scale=3.0)
    assert cropped.shape[0] == cropped.shape[1]
    assert cropped.shape[2] == 3


def test_face_crop_geometry_is_smoothed_without_hiding_deliberate_motion() -> None:
    crop = TemporalFaceCrop(half_life_ms=100.0)
    initial = crop.update(FaceCropGeometry(100.0, 80.0, 60.0), 1000)
    jitter = crop.update(FaceCropGeometry(102.0, 78.0, 62.0), 1100)
    moved = crop.update(FaceCropGeometry(140.0, 100.0, 80.0), 1200)

    assert initial == FaceCropGeometry(100.0, 80.0, 60.0)
    assert jitter == FaceCropGeometry(101.0, 79.0, 61.0)
    assert jitter.center_x < moved.center_x < 140.0
    assert jitter.center_y < moved.center_y < 100.0
    assert jitter.size < moved.size < 80.0


def test_face_crop_geometry_resets_after_a_tracking_gap() -> None:
    crop = TemporalFaceCrop(half_life_ms=100.0, reset_after_ms=500)
    crop.update(FaceCropGeometry(100.0, 80.0, 60.0), 1000)

    reacquired = crop.update(FaceCropGeometry(200.0, 180.0, 120.0), 1500)

    assert reacquired == FaceCropGeometry(200.0, 180.0, 120.0)


def test_avatar_composition_returns_full_size_bgrx_frame() -> None:
    import numpy as np

    source = np.full((9, 16, 3), 10, dtype=np.uint8)
    animated = np.full((8, 8, 3), 200, dtype=np.uint8)
    output = compose_avatar_frame(source, animated, 16, 9)
    assert output.shape == (9, 16, 4)
    assert output.dtype == np.uint8
    assert output[4, 8, :3].tolist() == [200, 200, 200]
    assert output[4, 0, :3].tolist() == [10, 10, 10]


def test_avatar_identity_client_reads_the_selected_engine(monkeypatch) -> None:  # noqa: ANN001
    def respond(*_: object, **__: object) -> HttpResponse:
        return HttpResponse(b'{"identity":"liveportrait"}')

    monkeypatch.setattr("urllib.request.urlopen", respond)

    identity = VideoIdentityClient("http://127.0.0.1:8742")

    assert identity.selected_avatar_engine() == "liveportrait"


def test_video_identity_client_keeps_depth_distinct_and_can_refresh_immediately(
    monkeypatch,  # noqa: ANN001
) -> None:
    responses = iter(
        (
            b'{"identity":"depth-map","background_enabled":false}',
            b'{"identity":"camera","background_enabled":true}',
            b'{"identity":"camera","background_enabled":false}',
        )
    )

    def respond(*_: object, **__: object) -> HttpResponse:
        return HttpResponse(next(responses))

    monkeypatch.setattr("urllib.request.urlopen", respond)

    identity = VideoIdentityClient("http://127.0.0.1:8742")

    assert identity.selected_identity() == "depth-map"
    assert identity.selected_avatar_engine() is None
    assert identity.depth_usage() == "visualization"
    identity.invalidate()
    assert identity.depth_usage() == "mask-refinement"
    identity.invalidate()
    assert identity.depth_usage() is None


def test_avatar_publisher_tags_frames_with_the_rendering_engine(monkeypatch) -> None:  # noqa: ANN001
    import numpy as np

    published = []

    def respond(request, **_: object) -> HttpResponse:  # noqa: ANN001
        published.append(request)
        return HttpResponse()

    monkeypatch.setattr("urllib.request.urlopen", respond)
    frame = np.zeros((1, 2, 4), dtype=np.uint8)

    AvatarPublisher("http://127.0.0.1:8742").publish("stylized-3d", 42, 1234, frame)

    assert published[0].get_header("X-tarsier-avatar-engine") == "stylized-3d"


def test_depth_scale_stabilizes_bounds_across_frames() -> None:
    import numpy as np

    scale = TemporalDepthScale(smoothing=0.25)
    first = np.arange(100, dtype=np.float32).reshape(10, 10)
    second = first + 100

    first_far, first_near = scale.update(first)
    second_far, second_near = scale.update(second)

    assert first_far < second_far < first_far + 100
    assert first_near < second_near < first_near + 100


def test_depth_mask_refiner_suppresses_background_leaks_and_keeps_person_edges() -> None:
    import numpy as np

    depth = np.full((16, 16), 0.2, dtype=np.float32)
    depth[:, :8] = 0.8
    mask = np.zeros((16, 16), dtype=np.uint8)
    mask[:, :7] = 255
    mask[:, 7:9] = 128
    estimate = DepthEstimate(1, 1, depth, 0.0, 1.0)

    refined = DepthMaskRefiner(smoothing=1.0).refine(mask, estimate)

    assert refined[:, 7].mean() > 128
    assert refined[:, 8].mean() < 128
    assert np.all(refined[:, :7] == 255)
    assert np.all(refined[:, 9:] == 0)


def test_depth_input_width_preserves_aspect_ratio_and_model_patch_size() -> None:
    assert depth_input_width(640, 360, 252) == 448
    assert depth_input_width(1280, 720, 350) == 630


def test_depth_publisher_posts_raw_float32_values(monkeypatch) -> None:  # noqa: ANN001
    import numpy as np

    published = []

    def respond(request, **_: object) -> HttpResponse:  # noqa: ANN001
        published.append(request)
        return HttpResponse()

    monkeypatch.setattr("urllib.request.urlopen", respond)
    values = np.array([[0.25, 1.5], [2.75, 4.0]], dtype=np.float32)
    estimate = DepthEstimate(42, 1234, values, 0.25, 4.0)

    DepthPublisher("http://127.0.0.1:8742").publish(estimate)

    request = published[0]
    assert request.get_header("X-tarsier-depth-representation") == DEPTH_REPRESENTATION
    assert request.get_header("X-tarsier-depth-width") == "2"
    assert request.get_header("X-tarsier-depth-height") == "2"
    assert np.frombuffer(request.data, dtype="<f4").reshape(2, 2).tolist() == values.tolist()


def test_avatar_motion_maps_mediapipe_expressions() -> None:
    import numpy as np

    result = FaceResult(
        face_landmarks=[[SourceLandmark(0.5, 0.5, 0.0)]],
        face_blendshapes=[
            [
                FaceCategory("eyeBlinkLeft", 0.8),
                FaceCategory("eyeBlinkRight", 0.3),
                FaceCategory("jawOpen", 0.7),
                FaceCategory("mouthSmileLeft", 0.6),
                FaceCategory("browInnerUp", 0.5),
            ]
        ],
        facial_transformation_matrixes=[np.eye(4, dtype=np.float32)],
    )

    assert motion_from_mediapipe(result) == AvatarMotion(
        blink_left=0.8,
        blink_right=0.3,
        jaw_open=0.7,
        smile=0.6,
        brow_raise=0.5,
    )


def test_stylized_3d_renderer_produces_an_opaque_bgrx_frame() -> None:
    pytest.importorskip("moderngl")
    import numpy as np

    profile = Path(__file__).parents[2] / "assets/avatars/stylized-3d.json"
    with Stylized3DAvatarEngine(profile, 320, 180) as engine:
        neutral = engine.render(AvatarMotion.neutral())
        expressive = engine.render(AvatarMotion(jaw_open=1.0, blink_left=1.0, yaw=0.3))

    assert neutral.shape == (180, 320, 4)
    assert neutral.dtype == np.uint8
    assert np.all(neutral[:, :, 3] == 255)
    assert np.unique(neutral[:, :, :3].reshape(-1, 3), axis=0).shape[0] > 20
    assert not np.array_equal(neutral, expressive)
