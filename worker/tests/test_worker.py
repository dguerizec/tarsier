from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from tarsier_perception.models import describe_models
from tarsier_perception.worker import normalize_gesture, select_gesture


@dataclass
class Category:
    category_name: str
    score: float


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
    assert select_gesture(
        [[Category("None", 0.91), Category("Open_Palm", 0.67)]]
    ) == ("open_palm", 0.67)


def test_missing_models_are_reported_as_unverified(tmp_path: Path) -> None:
    descriptions = describe_models(tmp_path)
    assert len(descriptions) == 2
    assert all(not model["exists"] for model in descriptions)
    assert all(not model["verified"] for model in descriptions)
