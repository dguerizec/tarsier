from contextlib import suppress
from pathlib import Path
from threading import Event

from tarsier_perception.liveportrait.switching import PortraitSwitcher


def test_preparation_keeps_rendering_old_frames_and_commits_only_after_publish():
    started, release = Event(), Event()

    def prepare(path):
        started.set()
        assert release.wait(2)
        return path.name

    with PortraitSwitcher(
        0, "old", prepare, lambda frame, source: (frame, source), lambda *_: None
    ) as switcher:
        switcher.request(1, Path("new"))
        assert switcher.render(10)[0] == (10, "old")
        assert started.wait(2)
        assert switcher.render(11)[0] == (11, "old")
        assert switcher.render(12)[0] == (12, "old")
        release.set()
        switcher._future.result(timeout=2)
        output, version = switcher.render(13)
        assert output == (13, "new")
        assert version.revision == 1
        assert switcher.active.revision == 0  # A failed HTTP publication must not commit.
        assert switcher.render(14)[0] == (14, "new")
        switcher.accept(version.revision)
        assert switcher.active.revision == 1
        assert switcher.render(15)[0] == (15, "new")


def test_superseded_preparations_are_discarded_and_only_latest_is_loaded():
    started, release = Event(), Event()
    loaded = []

    def prepare(path):
        loaded.append(path.name)
        if path.name == "obsolete":
            started.set()
            assert release.wait(2)
        return path.name

    with PortraitSwitcher(
        0, "old", prepare, lambda frame, source: source, lambda *_: None
    ) as switcher:
        switcher.request(1, Path("obsolete"))
        switcher.render(None)
        assert started.wait(2)
        switcher.request(2, Path("skipped"))
        switcher.request(3, Path("latest"))
        release.set()
        switcher._future.result(timeout=2)
        assert switcher.render(None)[0] == "old"
        switcher._future.result(timeout=2)
        output, version = switcher.render(None)
        assert (output, version.revision) == ("latest", 3)
        assert loaded == ["obsolete", "latest"]


def test_prepare_failure_preserves_old_avatar_and_reports_once():
    errors = []

    def prepare(_):
        raise ValueError("bad portrait")

    with PortraitSwitcher(
        0, "old", prepare, lambda frame, source: source, lambda *error: errors.append(error)
    ) as switcher:
        switcher.request(1, Path("bad"))
        switcher.render(None)
        with suppress(ValueError):
            switcher._future.result(timeout=2)
        assert switcher.render(None)[0] == "old"
        assert switcher.render(None)[0] == "old"
        assert switcher.active.revision == 0
    assert errors == [(1, "bad portrait")]


def test_first_render_failure_does_not_replace_active_source():
    errors = []

    def render(frame, source):
        if source == "bad":
            raise RuntimeError("render failed")
        return source

    with PortraitSwitcher(
        0, "old", lambda _: "bad", render, lambda *error: errors.append(error)
    ) as switcher:
        switcher.request(1, Path("bad"))
        switcher.render(None)
        switcher._future.result(timeout=2)
        assert switcher.render(None)[0] == "old"
        switcher.accept(1)
        assert switcher.active.revision == 0
    assert errors == [(1, "render failed")]
